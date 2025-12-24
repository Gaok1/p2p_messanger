use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use laminar::{Socket, SocketEvent};

mod stun;
mod transfer;

use transfer::{IncomingFile, SendOutcome, handle_incoming_message, handle_send_punch};

const CHANNEL_ID: u8 = 0;
const CHUNK_SIZE: usize = 1024;
const PUNCH_TIMEOUT: Duration = Duration::from_secs(10);
const PUNCH_INTERVAL: Duration = Duration::from_millis(200);

/// Mensagens enviadas pela camada de rede para trafegar os arquivos.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
enum WireMessage {
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

/// Comandos enviados pela UI para a thread de rede.
pub enum NetCommand {
    ConnectPeer(SocketAddr),
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

    let mut socket = match Socket::bind(bind_addr) {
        Ok(socket) => socket,
        Err(err) => {
            let _ = evt_tx.send(NetEvent::Log(format!("erro ao abrir socket {err}")));
            return;
        }
    };

    let _ = evt_tx.send(NetEvent::Log(format!("escutando {bind_addr}")));

    let mut punch_state = initial_peer.map(PunchState::new);
    if let Some(peer) = punch_state.as_ref().map(|state| state.peer) {
        let _ = evt_tx.send(NetEvent::PeerConnecting(peer));
    }

    let mut connected_peer: Option<SocketAddr> = None;
    let mut session_dir: Option<PathBuf> = None;
    let mut incoming: HashMap<u64, IncomingFile> = HashMap::new();
    let mut next_file_id = 1u64;
    let mut next_punch_nonce = 1u64;
    let mut pending_cmds: Vec<NetCommand> = Vec::new();

    'outer: loop {
        if !pending_cmds.is_empty() {
            let queued = pending_cmds.drain(..).collect::<Vec<_>>();
            for cmd in queued {
                match handle_command(
                    cmd,
                    &mut connected_peer,
                    &mut punch_state,
                    &mut socket,
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

        while let Ok(cmd) = cmd_rx.try_recv() {
            match handle_command(
                cmd,
                &mut connected_peer,
                &mut punch_state,
                &mut socket,
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

        if let Some(state) = punch_state.as_mut() {
            if state.started.elapsed() >= PUNCH_TIMEOUT {
                let peer = state.peer;
                punch_state = None;
                let _ = evt_tx.send(NetEvent::PeerTimeout(peer));
            } else if state.last_sent.elapsed() >= PUNCH_INTERVAL {
                handle_send_punch(&mut socket, state.peer, next_punch_nonce, &evt_tx);
                next_punch_nonce = next_punch_nonce.wrapping_add(1);
                state.last_sent = Instant::now();
            }
        }

        while let Some(event) = socket.recv() {
            if let SocketEvent::Packet(packet) = event {
                let from = packet.addr();
                match bincode::deserialize::<WireMessage>(packet.payload()) {
                    Ok(message) => {
                        let new_peer = handle_incoming_message(
                            &mut socket,
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
                        let _ = evt_tx.send(NetEvent::Log(format!("erro ao decodificar {err}")));
                    }
                }
            }
        }

        socket.manual_poll(Instant::now());
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

#[cfg(test)]
mod tests {
    use super::stun::stun_server_list;

    #[test]
    fn stun_defaults_not_empty() {
        assert!(!stun_server_list("0.0.0.0:5000".parse().unwrap()).is_empty());
        assert!(!stun_server_list("[::]:5000".parse().unwrap()).is_empty());
    }
}
