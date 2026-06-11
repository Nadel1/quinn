//! This example demonstrates an HTTP client that requests files from a server.
//!
//! Checkout the `README.md` for guidance.

use std::{
    fs,
    io::{self, Write},
    net::{SocketAddr, ToSocketAddrs},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use clap::Parser;
use quinn::{
    AckFrequencyConfig, VarInt,
    congestion::{BbrConfig, CubicConfig, NewRenoConfig},
};
use quinn_proto::crypto::rustls::QuicClientConfig;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tracing::{error, info};
use url::Url;

/// HTTP/0.9 over QUIC client
#[derive(Parser, Debug)]
#[clap(name = "client")]
struct Opt {
    /// Perform NSS-compatible TLS key logging to the file specified in `SSLKEYLOGFILE`.
    #[clap(long = "keylog")]
    keylog: bool,

    url: Url,

    /// Override hostname used for certificate verification
    #[clap(long = "host")]
    host: Option<String>,

    /// Custom certificate authority to trust, in DER format
    #[clap(long = "ca")]
    ca: Option<PathBuf>,

    /// Simulate NAT rebinding after connecting
    #[clap(long = "rebind")]
    rebind: bool,

    /// Address to bind on
    #[clap(long = "bind", default_value = "[::]:0")]
    bind: SocketAddr,

    /// initial rtt, set to some big number for long latency
    #[clap(long = "initial-rtt", default_value = "333")]
    initial_rtt: u64,

    /// idle timeout, set to some big number for long latency
    #[clap(long = "idle-timeout", default_value = "100000")]
    idle_timeout: u64,

    /// number requests
    #[clap(long = "requests", default_value = "1")]
    requests: u64,

    /// number requests
    #[clap(long = "max-ack-delay", default_value = "1")]
    max_ack_delay: u64,

    /// congestion control
    #[clap(long = "congestion-control", default_value = "newReno")]
    congestion_control: Vec<String>,

    ///ack-threshold
    #[clap(long = "ack-eliciting-threshold", default_value = "10")]
    ack_eliciting_threshold: u32,

    ///requested_max_ack_delay
    #[clap(long = "requested-max-ack-delay", default_value = "1000")]
    requested_max_ack_delay: u64,

    ///initial cwnd
    /// cubic recommended value: `min(10 * max_datagram_size, max(2 * max_datagram_size, 14720))`, max datagram size =1200
    /// bbr recommended value: `min(10 * max_datagram_size, max(2 * max_datagram_size, 14720))`
    #[clap(long = "initial-cwnd", default_value = "12000")]
    initial_cwnd: u64,

    #[clap(long = "logging-file", default_value = "client.csv")]
    logging_name: String,
}

#[allow(unused)]
pub const ALPN_QUIC_HTTP: &[&[u8]] = &[b"hq-29", b"hq-interop"];

fn main() {
    tracing::subscriber::set_global_default(
        tracing_subscriber::FmtSubscriber::builder()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .finish(),
    )
    .unwrap();
    let opt = Opt::parse();
    let code = {
        if let Err(e) = run(opt) {
            eprintln!("ERROR: {e}");
            1
        } else {
            0
        }
    };
    ::std::process::exit(code);
}

#[tokio::main]
async fn run(options: Opt) -> Result<()> {
    let url = options.url;
    let url_host = strip_ipv6_brackets(url.host_str().unwrap());
    let remote = (url_host, url.port().unwrap_or(4433))
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow!("couldn't resolve to an address"))?;

    let mut roots = rustls::RootCertStore::empty();
    if let Some(ca_path) = options.ca {
        roots.add(CertificateDer::from(fs::read(ca_path)?))?;
    } else {
        let dirs = directories_next::ProjectDirs::from("org", "quinn", "quinn-examples").unwrap();
        match fs::read(dirs.data_local_dir().join("cert.der")) {
            Ok(cert) => {
                roots.add(CertificateDer::from(cert))?;
            }
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
                info!("local server certificate not found");
            }
            Err(e) => {
                error!("failed to open local server certificate: {}", e);
            }
        }
    }
    let mut client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(SkipServerVerification::new())
        .with_no_client_auth();
    client_crypto.alpn_protocols = ALPN_QUIC_HTTP.iter().map(|&x| x.into()).collect();
    if options.keylog {
        client_crypto.key_log = Arc::new(rustls::KeyLogFile::new());
    }

    let mut client_config =
        quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(client_crypto)?));
    let mut transport_config = quinn::TransportConfig::default();
    let initial_rtt = options.initial_rtt;
    let idle_timeout = options.idle_timeout;
    let requests = options.requests;
    let logging_name = options.logging_name;
    let mut ack_freq = AckFrequencyConfig::default();

    let threshold = VarInt::from_u32(options.ack_eliciting_threshold);
    ack_freq.ack_eliciting_threshold(threshold);
    ack_freq.max_ack_delay(Some(Duration::from_micros(options.requested_max_ack_delay)));
    transport_config
        .initial_rtt(Duration::from_millis(initial_rtt))
        .max_idle_timeout(Some(
            Duration::from_millis(idle_timeout).try_into().unwrap(),
        ))
        .logging_file(logging_name);

    transport_config.ack_frequency_config(Some(ack_freq));
    let mut ack_freq_config = quinn::AckFrequencyConfig::default();
    ack_freq_config.max_ack_delay(Some(Duration::from_millis(options.max_ack_delay)));
    transport_config.ack_frequency_config(Some(ack_freq_config));
    transport_config.stream_receive_window(VarInt::MAX);
    transport_config.send_window(u64::MAX);
    let ccontrol = options.congestion_control.join(" ");

    match ccontrol.as_str() {
        "newReno" => {
            transport_config.congestion_controller_factory(Arc::new(NewRenoConfig::default()))
        }
        "cubic" => {
            println!("-----------using cubic in client!--------------");
            let mut cubic_config: CubicConfig = CubicConfig::default();
            cubic_config.initial_window(options.initial_cwnd); //change window
            transport_config.congestion_controller_factory(Arc::new(cubic_config))
        }
        "bbr" => {
            println!("-----------using bbr in client!--------------");
            let mut bbr_config = BbrConfig::default();
            bbr_config.initial_window(options.initial_cwnd);
            transport_config.congestion_controller_factory(Arc::new(bbr_config))
        }
        _ => {
            println!("----using new reno in client-----");
            transport_config.congestion_controller_factory(Arc::new(NewRenoConfig::default()))
        }
    };
    client_config.transport_config(Arc::new(transport_config));
    let mut endpoint = quinn::Endpoint::client(options.bind)?;
    endpoint.set_default_client_config(client_config);

    let request = format!("GET {}\r\n", url.path());
    let start = Instant::now();
    let rebind = options.rebind;
    let host = options.host.as_deref().unwrap_or(url_host);

    eprintln!("connecting to {host} at {remote}");

    let conn = endpoint
        .connect(remote, host)?
        .await
        .map_err(|e| anyhow!("failed to connect: {}", e))?;
    eprintln!("connected at {:?}", start.elapsed());

    for _n in 0..requests {
        //println!("Sending request {}",n);
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| anyhow!("failed to open stream: {}", e))?;
        if rebind {
            let socket = std::net::UdpSocket::bind("[::]:0").unwrap();
            let addr = socket.local_addr().unwrap();
            eprintln!("rebinding to {addr}");
            endpoint.rebind(socket).expect("rebind failed");
        }

        send.write_all(request.as_bytes())
            .await
            .map_err(|e| anyhow!("failed to send request: {}", e))?;
        send.finish().unwrap();
        let response_start = Instant::now();
        eprintln!("request sent at {:?}", response_start - start);
        let resp = recv
            .read_to_end(usize::MAX)
            .await
            .map_err(|e| anyhow!("failed to read response: {}", e))?;
        //let resp = timeout(Duration::from_secs(5),recv.read_to_end(usize::MAX)).await.map_err(|e| anyhow!("failed to read response: {}", e))?;
        //match resp{
        //    Ok(value)=>value,
        //    Err(e)=>Vec<u8>
        //}
        let duration = response_start.elapsed();
        eprintln!(
            "response received in {:?} - {} KiB/s",
            duration,
            resp.len() as f32 / (duration_secs(&duration) * 1024.0)
        );
        //io::stdout().write_all(&resp).unwrap();
        //io::stdout().flush().unwrap();
    }
    conn.close(0u32.into(), b"done");

    // Give the server a fair chance to receive the close packet
    endpoint.wait_idle().await;

    Ok(())
}

fn strip_ipv6_brackets(host: &str) -> &str {
    // An ipv6 url looks like eg https://[::1]:4433/Cargo.toml, wherein the host [::1] is the
    // ipv6 address ::1 wrapped in brackets, per RFC 2732. This strips those.
    if host.starts_with('[') && host.ends_with(']') {
        &host[1..host.len() - 1]
    } else {
        host
    }
}

fn duration_secs(x: &Duration) -> f32 {
    x.as_secs() as f32 + x.subsec_nanos() as f32 * 1e-9
}

/// Dummy certificate verifier that treats any certificate as valid.
/// NOTE, such verification is vulnerable to MITM attacks, but convenient for testing.
#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
    }
}

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}