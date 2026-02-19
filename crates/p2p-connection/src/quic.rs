use std::{
    io,
    net::SocketAddr,
    time::{Duration, Instant},
};

use quinn::{Endpoint, EndpointConfig, ServerConfig};

use crate::{AutotuneConfig, stun};

const DEFAULT_SERVER_NAME: &str = "pasta";

#[derive(Debug, Clone)]
pub enum ConnectError {
    Start { peer: SocketAddr, error: String },
    Handshake { peer: SocketAddr, error: String },
    Timeout { peer: SocketAddr, timeout: Duration },
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::Start { peer, error } => {
                write!(f, "erro ao iniciar conexao {peer}: {error}")
            }
            ConnectError::Handshake { peer, error } => write!(f, "erro ao conectar {peer}: {error}"),
            ConnectError::Timeout { peer, timeout } => write!(
                f,
                "tempo esgotado ao conectar {peer} ({}s)",
                timeout.as_secs().max(1)
            ),
        }
    }
}

impl std::error::Error for ConnectError {}

pub fn make_endpoint(
    bind_addr: SocketAddr,
    target_window: u64,
    autotune: &AutotuneConfig,
    log: Option<&mut dyn FnMut(String)>,
) -> Result<(Endpoint, Vec<u8>, Result<Option<SocketAddr>, String>), Box<dyn std::error::Error>> {
    let cert = rcgen::generate_simple_self_signed(["pasta-p2p.local".into()])?;
    let cert_der = cert.serialize_der()?;
    let priv_key = cert.serialize_private_key_der();

    let server_config = make_server_config(
        cert_der.clone(),
        priv_key.clone(),
        make_transport_config(target_window, autotune),
    )?;
    let client_config = make_client_config(make_transport_config(target_window, autotune));

    let socket = match std::net::UdpSocket::bind(bind_addr) {
        Ok(socket) => socket,
        Err(err) if err.kind() == io::ErrorKind::AddrInUse && bind_addr.port() != 0 => {
            let fallback = SocketAddr::new(bind_addr.ip(), 0);
            std::net::UdpSocket::bind(fallback)?
        }
        Err(err) => return Err(Box::new(err)),
    };
    let local_addr = socket.local_addr()?;

    let stun_result = match log {
        Some(log) if stun::stun_trace_enabled() => {
            stun::detect_public_endpoint_on_socket_with_trace(&socket, local_addr, move |line| {
                log(line);
            })
        }
        Some(log) => {
            let mut trace_lines: Vec<String> = Vec::new();
            let result = stun::detect_public_endpoint_on_socket_with_trace(
                &socket,
                local_addr,
                |line| trace_lines.push(line),
            );
            if result.is_err() {
                for line in trace_lines {
                    log(line);
                }
            }
            result
        }
        None => stun::detect_public_endpoint_on_socket(&socket, local_addr),
    };
    let _ = socket.set_read_timeout(None);

    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(server_config),
        socket,
        std::sync::Arc::new(quinn::TokioRuntime),
    )?;
    endpoint.set_default_client_config(client_config);
    Ok((endpoint, cert_der, stun_result))
}

fn make_transport_config(target_window: u64, autotune: &AutotuneConfig) -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(5)));
    if let Ok(timeout) = quinn::IdleTimeout::try_from(Duration::from_secs(60)) {
        transport.max_idle_timeout(Some(timeout));
    }

    configure_flow_control(&mut transport, target_window, autotune);
    transport
}

fn make_server_config(
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    transport: quinn::TransportConfig,
) -> Result<ServerConfig, Box<dyn std::error::Error>> {
    use quinn::rustls;
    use std::sync::Arc;

    let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der)];
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(key_der.into());

    let mut crypto = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(cert_chain, key)?;
    crypto.alpn_protocols = vec![b"hq-29".to_vec()];

    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)?;
    let mut server_config = ServerConfig::with_crypto(Arc::new(crypto));
    server_config.transport = std::sync::Arc::new(transport);
    Ok(server_config)
}

fn make_client_config(mut transport: quinn::TransportConfig) -> quinn::ClientConfig {
    use quinn::rustls::{self, DigitallySignedStruct, SignatureScheme, client::danger};

    #[derive(Debug)]
    struct SkipVerifier;
    impl danger::ServerCertVerifier for SkipVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<danger::ServerCertVerified, rustls::Error> {
            Ok(danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<danger::HandshakeSignatureValid, rustls::Error> {
            Ok(danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<danger::HandshakeSignatureValid, rustls::Error> {
            Ok(danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    let mut crypto = rustls::ClientConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("tls13")
    .dangerous()
    .with_custom_certificate_verifier(std::sync::Arc::new(SkipVerifier))
    .with_no_client_auth();

    crypto.alpn_protocols = vec![b"hq-29".to_vec()];

    let crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).expect("quic client config");

    let mut client = quinn::ClientConfig::new(std::sync::Arc::new(crypto));
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(32));
    client.transport_config(std::sync::Arc::new(transport));
    client
}

fn configure_flow_control(
    transport: &mut quinn::TransportConfig,
    target_window: u64,
    autotune: &AutotuneConfig,
) {
    let clamped = autotune.clamp_target(target_window);
    let flow_window = clamped.min(quinn::VarInt::MAX.into_inner());
    let flow_window = quinn::VarInt::from_u64(flow_window)
        .expect("flow control window within QUIC varint bounds");
    transport.stream_receive_window(flow_window);
    transport.receive_window(flow_window);
    transport.send_window(clamped);
}

pub fn apply_autotune_target(
    connection: &quinn::Connection,
    target_window: u64,
    autotune: &AutotuneConfig,
) {
    let clamped = autotune.clamp_target(target_window);
    let flow_window = quinn::VarInt::from_u64(clamped.min(quinn::VarInt::MAX.into_inner()))
        .expect("flow control window within QUIC varint bounds");
    connection.set_receive_window(flow_window);
    connection.set_send_window(clamped);
}

pub async fn connect_peer(
    endpoint: &Endpoint,
    peer: SocketAddr,
    timeout: Duration,
) -> Result<quinn::Connection, ConnectError> {
    let connecting = endpoint
        .connect(peer, DEFAULT_SERVER_NAME)
        .map_err(|err| ConnectError::Start {
            peer,
            error: err.to_string(),
        })?;

    match tokio::time::timeout(timeout, connecting).await {
        Ok(Ok(connection)) => Ok(connection),
        Ok(Err(err)) => Err(ConnectError::Handshake {
            peer,
            error: err.to_string(),
        }),
        Err(_) => Err(ConnectError::Timeout { peer, timeout }),
    }
}

pub async fn quick_probe_peer(
    endpoint: &Endpoint,
    peer: SocketAddr,
    timeout: Duration,
) -> Result<Duration, String> {
    let started = Instant::now();
    let connecting = endpoint
        .connect(peer, DEFAULT_SERVER_NAME)
        .map_err(|err| format!("erro ao iniciar teste {err}"))?;

    match tokio::time::timeout(timeout, connecting).await {
        Ok(Ok(connection)) => {
            let elapsed = started.elapsed();
            connection.close(0u32.into(), b"probe");
            Ok(elapsed)
        }
        Ok(Err(err)) => Err(format!("erro ao conectar: {err}")),
        Err(_) => Err(format!(
            "sem resposta em {}s (firewall/NAT?)",
            timeout.as_secs().max(1)
        )),
    }
}
