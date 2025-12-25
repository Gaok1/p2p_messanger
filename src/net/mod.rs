use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};

use base64::Engine;
use bincode::Options;
use quinn::{Endpoint, ServerConfig};
use tokio::runtime::Runtime;
use tokio::sync::mpsc as tokio_mpsc;

mod stun;
mod transfer;

use transfer::{IncomingFile, SendOutcome, handle_incoming_message, send_files};

const CHUNK_SIZE: usize = 1024;

/// Mensagens enviadas pela camada de rede para trafegar os arquivos.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub(crate) enum WireMessage {
    Hello {
        version: u8,
    },
    Punch {
        nonce: u64,
    },
    Cancel {
        file_id: u64,
    },
    FileMeta {
        file_id: u64,
        name: String,
        size: u64,
    },
    FileChunk {
        file_id: u64,
        data: Vec<u8>,
    },
    FileDone {
        file_id: u64,
    },
}

fn bincode_options() -> impl Options {
    // Enforce a fixed width encoding to avoid any platform-specific differences when
    // serializing/deserializing `WireMessage` variants.
    bincode::DefaultOptions::new().with_fixint_encoding()
}

pub(crate) fn serialize_message(message: &WireMessage) -> bincode::Result<Vec<u8>> {
    bincode_options().serialize(message)
}

#[allow(dead_code)]
pub(crate) fn serialize_message_base64(message: &WireMessage) -> bincode::Result<String> {
    let bytes = serialize_message(message)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn deserialize_message(bytes: &[u8]) -> bincode::Result<WireMessage> {
    bincode_options().deserialize(bytes)
}

fn deserialize_message_base64(text: &str) -> Result<WireMessage, bincode::Error> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|err| Box::new(bincode::ErrorKind::Custom(err.to_string())))?;
    deserialize_message(&decoded)
}

fn decode_payload(payload: &[u8]) -> Result<WireMessage, String> {
    match deserialize_message(payload) {
        Ok(msg) => Ok(msg),
        Err(binary_err) => {
            let base64_attempt = std::str::from_utf8(payload)
                .ok()
                .and_then(|text| deserialize_message_base64(text).ok());

            if let Some(msg) = base64_attempt {
                Ok(msg)
            } else {
                Err(binary_err.to_string())
            }
        }
    }
}

/// Comandos enviados pela UI para a thread de rede.
pub enum NetCommand {
    ConnectPeer(SocketAddr),
    Rebind(SocketAddr),
    CancelTransfers,
    SendFiles(Vec<PathBuf>),
    Shutdown,
}

/// Eventos gerados pela thread de rede para atualizar a UI.
pub enum NetEvent {
    Log(String),
    PublicEndpoint(SocketAddr),
    FileSent {
        file_id: u64,
        path: PathBuf,
    },
    FileReceived {
        file_id: u64,
        path: PathBuf,
    },
    SessionDir(PathBuf),
    PeerConnecting(SocketAddr),
    PeerConnected(SocketAddr),
    PeerTimeout(SocketAddr),
    SendStarted {
        file_id: u64,
        path: PathBuf,
        size: u64,
    },
    SendProgress {
        file_id: u64,
        bytes_sent: u64,
        size: u64,
    },
    SendCanceled {
        file_id: u64,
        path: PathBuf,
    },
    ReceiveStarted {
        file_id: u64,
        path: PathBuf,
        size: u64,
    },
    ReceiveProgress {
        file_id: u64,
        bytes_received: u64,
        size: u64,
    },
    ReceiveCanceled {
        file_id: u64,
        path: PathBuf,
    },
}

/// Inicia a thread de rede e retorna os canais de comunicação com a UI.
pub fn start_network(
    bind_addr: SocketAddr,
    peer_addr: Option<SocketAddr>,
) -> (
    Sender<NetCommand>,
    Receiver<NetEvent>,
    thread::JoinHandle<()>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (evt_tx, evt_rx) = mpsc::channel();
    let handle = thread::spawn(move || run_network(bind_addr, peer_addr, cmd_rx, evt_tx));
    (cmd_tx, evt_rx, handle)
}

fn run_network(
    bind_addr: SocketAddr,
    initial_peer: Option<SocketAddr>,
    cmd_rx: Receiver<NetCommand>,
    evt_tx: Sender<NetEvent>,
) {
    let runtime = Runtime::new().expect("runtime");
    runtime.block_on(async move {
        if let Err(err) = run_network_async(bind_addr, initial_peer, cmd_rx, evt_tx).await {
            eprintln!("net thread error: {err}");
        }
    });
}

async fn run_network_async(
    mut bind_addr: SocketAddr,
    initial_peer: Option<SocketAddr>,
    cmd_rx: Receiver<NetCommand>,
    evt_tx: Sender<NetEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    log_transport_config(&evt_tx);

    let mut pending_cmds: Vec<NetCommand> = Vec::new();

    let (mut endpoint, _cert) = loop {
        match make_endpoint(bind_addr) {
            Ok(ctx) => break ctx,
            Err(err) => {
                let _ = evt_tx.send(NetEvent::Log(format!("erro ao abrir endpoint {err}")));

                match cmd_rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(NetCommand::Rebind(new_bind)) => {
                        if new_bind != bind_addr {
                            bind_addr = new_bind;
                        }
                    }
                    Ok(NetCommand::Shutdown) => return Ok(()),
                    Ok(other) => pending_cmds.push(other),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                }
            }
        }
    };

    run_stun_detection(bind_addr, &evt_tx);

    let mut connected_peer: Option<SocketAddr> = None;
    let mut session_dir: Option<PathBuf> = None;
    let mut incoming: HashMap<u64, IncomingFile> = HashMap::new();
    let mut next_file_id = 1u64;
    let (mut inbound_tx, mut inbound_rx) = tokio_mpsc::unbounded_channel::<(WireMessage, SocketAddr)>();
    let mut reader_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut connection: Option<quinn::Connection> = None;

    if let Some(peer) = initial_peer {
        let _ = evt_tx.send(NetEvent::PeerConnecting(peer));
        if let Some(conn) = connect_peer(&mut endpoint, peer, &evt_tx).await {
            setup_connection_reader(
                conn.clone(),
                &mut inbound_tx,
                &evt_tx,
                &mut reader_task,
            );
            connected_peer = Some(conn.remote_address());
            connection = Some(conn);
            let _ = evt_tx.send(NetEvent::PeerConnected(peer));
        }
    }

    loop {
        if !pending_cmds.is_empty() {
            let queued: Vec<_> = pending_cmds.drain(..).collect();
            for cmd in queued {
                let exit = handle_command(
                    cmd,
                    &mut connected_peer,
                    &mut connection,
                    &mut next_file_id,
                    &evt_tx,
                    &cmd_rx,
                    &mut pending_cmds,
                    &mut endpoint,
                    &mut inbound_tx,
                    &mut reader_task,
                )
                .await?;
                if exit {
                    return Ok(());
                }
            }
        }

        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                NetCommand::Rebind(new_bind) => {
                    if new_bind != bind_addr {
                        let _ = evt_tx.send(NetEvent::Log(format!("reconfigurando bind para {new_bind}")));
                        match make_endpoint(new_bind) {
                            Ok((new_endpoint, _)) => {
                                endpoint = new_endpoint;
                                bind_addr = new_bind;
                                connection = None;
                                connected_peer = None;
                                session_dir = None;
                                incoming.clear();
                                next_file_id = 1;
                                run_stun_detection(bind_addr, &evt_tx);
                            }
                            Err(err) => {
                                let _ = evt_tx.send(NetEvent::Log(format!("erro ao reconfigurar {err}")));
                            }
                        }
                    }
                }
                other => {
                    let exit = handle_command(
                        other,
                        &mut connected_peer,
                        &mut connection,
                        &mut next_file_id,
                        &evt_tx,
                        &cmd_rx,
                        &mut pending_cmds,
                        &mut endpoint,
                        &mut inbound_tx,
                        &mut reader_task,
                    ).await?;
                    if exit { return Ok(()); }
                }
            }
        }

        tokio::select! {
            incoming = endpoint.accept() => {
                if let Some(connecting) = incoming {
                    match connecting.await {
                        Ok(new_conn) => {
                            connected_peer = Some(new_conn.remote_address());
                            setup_connection_reader(
                                new_conn.clone(),
                                &mut inbound_tx,
                                &evt_tx,
                                &mut reader_task,
                            );
                            connection = Some(new_conn.clone());
                            let _ = evt_tx.send(NetEvent::PeerConnected(new_conn.remote_address()));
                        }
                        Err(err) => {
                            let _ = evt_tx.send(NetEvent::Log(format!("erro ao aceitar {err}")));
                        }
                    }
                }
            }
            Some((message, from)) = async {
                match inbound_rx.recv().await {
                    Some(msg) => Some(msg),
                    None => None,
                }
            } => {
                if let Some(conn) = connection.as_ref() {
                    let new_peer = handle_incoming_message(
                        conn,
                        message,
                        from,
                        &mut connected_peer,
                        &mut session_dir,
                        &mut incoming,
                        &evt_tx,
                    ).await;
                    if new_peer {
                        let _ = evt_tx.send(NetEvent::PeerConnected(from));
                    }
                } else {
                    let _ = evt_tx.send(NetEvent::Log("mensagem recebida sem conexao".to_string()));
                }
            }
            else => {
                break;
            }
        }
    }

    let _ = reader_task.take().map(|t| t.abort());
    Ok(())
}

async fn handle_command(
    cmd: NetCommand,
    connected_peer: &mut Option<SocketAddr>,
    connection: &mut Option<quinn::Connection>,
    next_file_id: &mut u64,
    evt_tx: &Sender<NetEvent>,
    cmd_rx: &Receiver<NetCommand>,
    pending_cmds: &mut Vec<NetCommand>,
    endpoint: &mut Endpoint,
    inbound_tx: &mut tokio_mpsc::UnboundedSender<(WireMessage, SocketAddr)>,
    reader_task: &mut Option<tokio::task::JoinHandle<()>>,
) -> io::Result<bool> {
    match cmd {
        NetCommand::ConnectPeer(addr) => {
            *connected_peer = Some(addr);
            let _ = evt_tx.send(NetEvent::PeerConnecting(addr));
            if let Some(conn) = connect_peer(endpoint, addr, evt_tx).await {
                setup_connection_reader(conn.clone(), inbound_tx, evt_tx, reader_task);
                *connection = Some(conn);
                let _ = evt_tx.send(NetEvent::PeerConnected(addr));
            }
        }
        NetCommand::Rebind(_) => {}
        NetCommand::CancelTransfers => {
            let _ = evt_tx.send(NetEvent::Log("cancelamento solicitado".to_string()));
        }
        NetCommand::SendFiles(files) => {
            if let (Some(peer), Some(conn)) = (*connected_peer, connection) {
                match send_files(
                    conn,
                    peer,
                    &files,
                    next_file_id,
                    evt_tx,
                    cmd_rx,
                    pending_cmds,
                    CHUNK_SIZE,
                ).await {
                    Ok(SendOutcome::Completed) => {}
                    Ok(SendOutcome::Canceled) => {
                        let _ = evt_tx.send(NetEvent::Log("transferencia cancelada".to_string()));
                    }
                    Ok(SendOutcome::Shutdown) => return Ok(true),
                    Err(err) => {
                        let _ = evt_tx.send(NetEvent::Log(format!("erro ao enviar {err}")));
                    }
                }
            } else {
                let _ = evt_tx.send(NetEvent::Log("parceiro nao definido".to_string()));
            }
        }
        NetCommand::Shutdown => return Ok(true),
    }
    Ok(false)
}

fn run_stun_detection(bind_addr: SocketAddr, evt_tx: &Sender<NetEvent>) {
    match stun::detect_public_endpoint(bind_addr) {
        Ok(Some(endpoint)) => {
            let _ = evt_tx.send(NetEvent::PublicEndpoint(endpoint));
            let _ = evt_tx.send(NetEvent::Log(format!("endpoint publico {endpoint}")));
        }
        Ok(None) => {
            let _ = evt_tx.send(NetEvent::Log("stun indisponivel".to_string()));
        }
        Err(err) => {
            let _ = evt_tx.send(NetEvent::Log(format!("stun erro {err}")));
        }
    }
}

fn log_transport_config(evt_tx: &Sender<NetEvent>) {
    let _ = evt_tx.send(NetEvent::Log("transporte QUIC: streams confiaveis com TLS 1.3".to_string()));
    let _ = evt_tx.send(NetEvent::Log(format!(
        "dados de arquivo: streams unidirecionais com chunks de {CHUNK_SIZE} bytes"
    )));
}

fn make_endpoint(bind_addr: SocketAddr) -> Result<(Endpoint, Vec<u8>), Box<dyn std::error::Error>> {
    let cert = rcgen::generate_simple_self_signed(["pasta-p2p.local".into()])?;
    let cert_der = cert.serialize_der()?;
    let priv_key = cert.serialize_private_key_der();

    let server_config = make_server_config(cert_der.clone(), priv_key.clone())?;
    let client_config = make_client_config();
    let mut endpoint = Endpoint::server(server_config, bind_addr)?;
    endpoint.set_default_client_config(client_config);
    Ok((endpoint, cert_der))
}

fn make_server_config(cert_der: Vec<u8>, key_der: Vec<u8>) -> Result<ServerConfig, Box<dyn std::error::Error>> {
    let cert_chain = vec![quinn::rustls::pki_types::CertificateDer::from(cert_der)];
    let key = quinn::rustls::pki_types::PrivateKeyDer::Pkcs8(key_der.into());
    let mut server_config = ServerConfig::with_single_cert(cert_chain, key)?;
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(5)));
    if let Ok(timeout) = quinn::IdleTimeout::try_from(Duration::from_secs(60)) {
        transport.max_idle_timeout(Some(timeout));
    }
    server_config.transport = std::sync::Arc::new(transport);
    Ok(server_config)
}

fn make_client_config() -> quinn::ClientConfig {
    use quinn::rustls::{self, client::danger, DigitallySignedStruct, SignatureScheme};

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

    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
        .expect("quic client config");

    quinn::ClientConfig::new(std::sync::Arc::new(crypto))
}

async fn connect_peer(endpoint: &mut Endpoint, peer: SocketAddr, evt_tx: &Sender<NetEvent>) -> Option<quinn::Connection> {
    match endpoint.connect(peer, "pasta") {
        Ok(connecting) => match connecting.await {
            Ok(connection) => Some(connection),
            Err(err) => {
                let _ = evt_tx.send(NetEvent::Log(format!("erro ao conectar {err}")));
                None
            }
        },
        Err(err) => {
            let _ = evt_tx.send(NetEvent::Log(format!("erro ao iniciar conexao {err}")));
            None
        }
    }
}

fn setup_connection_reader(
    connection: quinn::Connection,
    inbound_tx: &mut tokio_mpsc::UnboundedSender<(WireMessage, SocketAddr)>,
    evt_tx: &Sender<NetEvent>,
    reader_task: &mut Option<tokio::task::JoinHandle<()>>,
) {
    let inbound = inbound_tx.clone();
    let evt_tx = evt_tx.clone();
    let handle = tokio::spawn(async move {
        loop {
            match connection.accept_uni().await {
                Ok(mut stream) => {
                    match read_framed(&mut stream).await {
                        Ok(payload) => match decode_payload(&payload) {
                            Ok(message) => {
                                let _ = inbound.send((message, connection.remote_address()));
                            }
                            Err(err) => {
                                let base64_preview = base64::engine::general_purpose::STANDARD
                                    .encode(&payload);
                                let preview = base64_preview.chars().take(48).collect::<String>();
                                let _ = evt_tx.send(NetEvent::Log(format!(
                                    "erro ao decodificar {err}; payload(base64)={preview}"
                                )));
                            }
                        },
                        Err(err) => {
                            let _ = evt_tx.send(NetEvent::Log(format!("stream encerrado {err}")));
                            break;
                        }
                    }
                }
                Err(err) => {
                    let _ = evt_tx.send(NetEvent::Log(format!("conexao encerrada {err}")));
                    break;
                }
            }
        }
    });
    *reader_task = Some(handle);
}

async fn read_framed(stream: &mut quinn::RecvStream) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::stun::stun_server_list;

    #[test]
    fn stun_defaults_not_empty() {
        assert!(!stun_server_list("0.0.0.0:5000".parse().unwrap()).is_empty());
        assert!(!stun_server_list("[::]:5000".parse().unwrap()).is_empty());
    }
}
