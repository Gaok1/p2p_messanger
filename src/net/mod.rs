use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use base64::Engine;
use bincode::Options;
use laminar::{Config, Socket, SocketEvent};

mod stun;
mod transfer;

use transfer::{IncomingFile, SendOutcome, handle_incoming_message, handle_send_punch};

const CHANNEL_ID: u8 = 0;
const CHUNK_SIZE: usize = 1024;
const PUNCH_TIMEOUT: Duration = Duration::from_secs(10);
const PUNCH_INTERVAL: Duration = Duration::from_millis(200);
const STUN_MAGIC_COOKIE: u32 = 0x2112A442;

fn log_transport_config(config: &Config, evt_tx: &Sender<NetEvent>) {
    let heartbeat = config
        .heartbeat_interval
        .map(|interval| format!("{interval:?}"))
        .unwrap_or_else(|| "desabilitado".to_string());
    let _ = evt_tx.send(NetEvent::Log(format!(
        "controle de trafego: confiavel/ordenado no canal {CHANNEL_ID}, max_packets_in_flight={}, fragment_size={}, max_packet_size={}, heartbeat={heartbeat}",
        config.max_packets_in_flight, config.fragment_size, config.max_packet_size
    )));
    let _ = evt_tx.send(NetEvent::Log(format!(
        "dados de arquivo: chunks de {CHUNK_SIZE} bytes com etiquetagem interna do laminar"
    )));
}

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

fn looks_like_stun(payload: &[u8]) -> bool {
    payload.len() >= 8 && payload[4..8] == STUN_MAGIC_COOKIE.to_be_bytes()
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

/// Estado temporário utilizado enquanto tentamos perfurar o NAT.
struct PunchState {
    peer: SocketAddr,
    started: Instant,
    last_sent: Instant,
}

impl PunchState {
    fn new(peer: SocketAddr) -> Self {
        let now = Instant::now();
        Self {
            peer,
            started: now,
            last_sent: now - PUNCH_INTERVAL,
        }
    }
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
    let mut bind_addr = bind_addr;
    // Aumentamos `max_packets_in_flight` para evitar gargalos de ~500 KB em
    // transferências confiáveis. O valor padrão (512) limita a quantidade de
    // pacotes confiáveis simultâneos e acabava esgotando quando enviávamos
    // arquivos maiores, fazendo a fila travar. Com 4096 pacotes, liberamos
    // espaço suficiente para fluxos de megabytes mantendo a janela de
    // retransmissão do Laminar.
    let config = Config {
        heartbeat_interval: Some(Duration::from_secs(2)),
        max_packets_in_flight: 4096,
        ..Config::default()
    };
    log_transport_config(&config, &evt_tx);
    run_stun_detection(bind_addr, &evt_tx);
    let mut socket = match bind_socket(bind_addr, &config, &evt_tx) {
        Some(socket) => Some(socket),
        None => return,
    };

    let mut punch_state = initial_peer.map(PunchState::new);
    if let Some(peer) = punch_state.as_ref().map(|state| state.peer) {
        let _ = evt_tx.send(NetEvent::PeerConnecting(peer));
    }

    let mut connected_peer: Option<SocketAddr> = None;
    let mut session_dir: Option<PathBuf> = None;
    let mut incoming: HashMap<u64, IncomingFile> = HashMap::new();
    let mut next_file_id = 1u64;
    let mut next_punch_nonce = 1u64;
    let mut stun_warning_logged = false;
    let mut pending_cmds: Vec<NetCommand> = Vec::new();

    'outer: loop {
        if !pending_cmds.is_empty() {
            let queued = pending_cmds.drain(..).collect::<Vec<_>>();
            for cmd in queued {
                match cmd {
                    NetCommand::Rebind(new_bind) => {
                        if !apply_rebind(&mut socket, &mut bind_addr, new_bind, &config, &evt_tx) {
                            break 'outer;
                        }
                        connected_peer = None;
                        punch_state = None;
                        session_dir = None;
                        incoming.clear();
                        next_file_id = 1;
                        next_punch_nonce = 1;
                    }
                    other => {
                        let Some(socket_ref) = socket.as_mut() else {
                            break 'outer;
                        };
                        match handle_command(
                            other,
                            &mut connected_peer,
                            &mut punch_state,
                            socket_ref,
                            &mut next_file_id,
                            &evt_tx,
                            &cmd_rx,
                            &mut pending_cmds,
                        ) {
                            Ok(true) => break 'outer,
                            Ok(false) => {}
                            Err(err) => {
                                let _ = evt_tx.send(NetEvent::Log(format!("erro interno {err}")));
                                break 'outer;
                            }
                        }
                    }
                }
            }
        }

        while let Ok(cmd) = cmd_rx.try_recv() {
            match cmd {
                NetCommand::Rebind(new_bind) => {
                    if !apply_rebind(&mut socket, &mut bind_addr, new_bind, &config, &evt_tx) {
                        break 'outer;
                    }
                    connected_peer = None;
                    punch_state = None;
                    session_dir = None;
                    incoming.clear();
                    next_file_id = 1;
                    next_punch_nonce = 1;
                }
                other => {
                    let Some(socket_ref) = socket.as_mut() else {
                        break 'outer;
                    };
                    match handle_command(
                        other,
                        &mut connected_peer,
                        &mut punch_state,
                        socket_ref,
                        &mut next_file_id,
                        &evt_tx,
                        &cmd_rx,
                        &mut pending_cmds,
                    ) {
                        Ok(true) => break 'outer,
                        Ok(false) => {}
                        Err(err) => {
                            let _ = evt_tx.send(NetEvent::Log(format!("erro interno {err}")));
                            break 'outer;
                        }
                    }
                }
            }
        }

        if let Some(socket_ref) = socket.as_mut() {
            if let Some(state) = punch_state.as_mut() {
                if state.started.elapsed() >= PUNCH_TIMEOUT {
                    let peer = state.peer;
                    punch_state = None;
                    let _ = evt_tx.send(NetEvent::PeerTimeout(peer));
                } else if state.last_sent.elapsed() >= PUNCH_INTERVAL {
                    handle_send_punch(socket_ref, state.peer, next_punch_nonce, &evt_tx);
                    next_punch_nonce = next_punch_nonce.wrapping_add(1);
                    state.last_sent = Instant::now();
                }
            }

            while let Some(event) = socket_ref.recv() {
                if let SocketEvent::Packet(packet) = event {
                    let from = packet.addr();
                    match decode_payload(packet.payload()) {
                        Ok(message) => {
                            let new_peer = handle_incoming_message(
                                socket_ref,
                                message,
                                from,
                                &mut connected_peer,
                                &mut session_dir,
                                &mut incoming,
                                &evt_tx,
                                CHANNEL_ID,
                            );
                            if new_peer {
                                if let Some(state) = punch_state.as_mut() {
                                    if state.peer == from || state.peer.ip() == from.ip() {
                                        state.peer = from;
                                        punch_state = None;
                                    }
                                }
                                let _ = evt_tx.send(NetEvent::PeerConnected(from));
                            }
                        }
                        Err(err) => {
                            if looks_like_stun(packet.payload()) {
                                if !stun_warning_logged {
                                    let _ = evt_tx.send(NetEvent::Log(
                                        "ignorado pacote STUN recebido apos detecao".to_string(),
                                    ));
                                    stun_warning_logged = true;
                                }
                            } else {
                                let base64_preview = base64::engine::general_purpose::STANDARD
                                    .encode(packet.payload());
                                let preview = base64_preview.chars().take(48).collect::<String>();
                                let _ = evt_tx.send(NetEvent::Log(format!(
                                    "erro ao decodificar {err}; payload(base64)={preview}"
                                )));
                            }
                        }
                    }
                }
            }

            socket_ref.manual_poll(Instant::now());
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn handle_command(
    cmd: NetCommand,
    connected_peer: &mut Option<SocketAddr>,
    punch_state: &mut Option<PunchState>,
    socket: &mut Socket,
    next_file_id: &mut u64,
    evt_tx: &Sender<NetEvent>,
    cmd_rx: &Receiver<NetCommand>,
    pending_cmds: &mut Vec<NetCommand>,
) -> io::Result<bool> {
    match cmd {
        NetCommand::ConnectPeer(addr) => {
            *connected_peer = None;
            *punch_state = Some(PunchState::new(addr));
            let _ = evt_tx.send(NetEvent::PeerConnecting(addr));
        }
        NetCommand::Rebind(_) => {}
        NetCommand::CancelTransfers => {
            let _ = evt_tx.send(NetEvent::Log("cancelamento solicitado".to_string()));
        }
        NetCommand::SendFiles(files) => {
            if let Some(peer) = *connected_peer {
                match transfer::send_files(
                    socket,
                    peer,
                    &files,
                    next_file_id,
                    evt_tx,
                    cmd_rx,
                    pending_cmds,
                    CHANNEL_ID,
                    CHUNK_SIZE,
                ) {
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

fn bind_socket(
    bind_addr: SocketAddr,
    config: &Config,
    evt_tx: &Sender<NetEvent>,
) -> Option<Socket> {
    match Socket::bind_with_config(bind_addr, config.clone()) {
        Ok(socket) => {
            let _ = evt_tx.send(NetEvent::Log(format!("escutando {bind_addr}")));
            Some(socket)
        }
        Err(err) => {
            let _ = evt_tx.send(NetEvent::Log(format!("erro ao abrir socket {err}")));
            None
        }
    }
}

fn apply_rebind(
    socket: &mut Option<Socket>,
    bind_addr: &mut SocketAddr,
    new_bind: SocketAddr,
    config: &Config,
    evt_tx: &Sender<NetEvent>,
) -> bool {
    if new_bind == *bind_addr {
        return true;
    }

    let _ = evt_tx.send(NetEvent::Log(format!(
        "reconfigurando bind para {new_bind}"
    )));
    *socket = None;
    run_stun_detection(new_bind, evt_tx);
    let new_socket = match bind_socket(new_bind, config, evt_tx) {
        Some(socket) => socket,
        None => return false,
    };
    *socket = Some(new_socket);
    *bind_addr = new_bind;
    true
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
