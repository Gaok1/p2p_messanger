use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read, Write},
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use chrono::Local;
use laminar::{Packet, Socket, SocketEvent};
use serde::{Deserialize, Serialize};
use stunclient::StunClient;

const CHANNEL_ID: u8 = 0;
const CHUNK_SIZE: usize = 1024;
const PUNCH_TIMEOUT: Duration = Duration::from_secs(10);
const PUNCH_INTERVAL: Duration = Duration::from_millis(200);
const STUN_TIMEOUT: Duration = Duration::from_secs(1);
const STUN_SERVERS: [&str; 5] = [
    "stun.cloudflare.com:53",
    "stun.cloudflare.com:3478",
    "stunserver2024.stunprotocol.org:3478",
    "stun.sipgate.net:3478",
    "stun.l.google.com:19302",
];

#[derive(Debug, Serialize, Deserialize)]
enum WireMessage {
    Hello { version: u8 },
    Punch { nonce: u64 },
    Cancel { file_id: u64 },
    FileMeta { file_id: u64, name: String, size: u64 },
    FileChunk { file_id: u64, data: Vec<u8> },
    FileDone { file_id: u64 },
}

pub enum NetCommand {
    ConnectPeer(SocketAddr),
    CancelTransfers,
    SendFiles(Vec<PathBuf>),
    Shutdown,
}

pub enum NetEvent {
    Log(String),
    PublicEndpoint(SocketAddr),
    FileSent { file_id: u64, path: PathBuf },
    FileReceived { file_id: u64, path: PathBuf },
    SessionDir(PathBuf),
    PeerConnecting(SocketAddr),
    PeerConnected(SocketAddr),
    PeerTimeout(SocketAddr),
    SendStarted { file_id: u64, path: PathBuf, size: u64 },
    SendProgress { file_id: u64, bytes_sent: u64, size: u64 },
    SendCanceled { file_id: u64, path: PathBuf },
    ReceiveStarted { file_id: u64, path: PathBuf, size: u64 },
    ReceiveProgress { file_id: u64, bytes_received: u64, size: u64 },
    ReceiveCanceled { file_id: u64, path: PathBuf },
}

struct IncomingFile {
    file: File,
    path: PathBuf,
    size: u64,
    bytes_written: u64,
}

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
    match detect_public_endpoint(bind_addr) {
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
                send_punch(&mut socket, state.peer, next_punch_nonce, &evt_tx);
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
                match send_files(
                    socket,
                    peer,
                    &files,
                    next_file_id,
                    evt_tx,
                    cmd_rx,
                    pending_cmds,
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

enum SendOutcome {
    Completed,
    Canceled,
    Shutdown,
}

fn poll_send_control(
    cmd_rx: &Receiver<NetCommand>,
    pending_cmds: &mut Vec<NetCommand>,
) -> SendOutcome {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            NetCommand::CancelTransfers => return SendOutcome::Canceled,
            NetCommand::Shutdown => return SendOutcome::Shutdown,
            other => pending_cmds.push(other),
        }
    }
    SendOutcome::Completed
}

fn send_cancel(socket: &mut Socket, peer: SocketAddr, file_id: u64, evt_tx: &Sender<NetEvent>) {
    send_message(socket, peer, &WireMessage::Cancel { file_id }, evt_tx);
}

fn handle_incoming_message(
    socket: &mut Socket,
    message: WireMessage,
    from: SocketAddr,
    peer_addr: &mut Option<SocketAddr>,
    session_dir: &mut Option<PathBuf>,
    incoming: &mut HashMap<u64, IncomingFile>,
    evt_tx: &Sender<NetEvent>,
) -> bool {
    let mut new_peer = false;
    if peer_addr.is_none() {
        *peer_addr = Some(from);
        new_peer = true;
    }

    match message {
        WireMessage::Hello { .. } => {
            if session_dir.is_none() {
                if let Err(err) = ensure_session_dir(session_dir, evt_tx) {
                    let _ = evt_tx.send(NetEvent::Log(format!("erro na sessao {err}")));
                }
            }
        }
        WireMessage::Punch { .. } => {
            send_message(socket, from, &WireMessage::Hello { version: 1 }, evt_tx);
        }
        WireMessage::Cancel { file_id } => {
            if let Some(entry) = incoming.remove(&file_id) {
                let _ = fs::remove_file(&entry.path);
                let _ = evt_tx.send(NetEvent::ReceiveCanceled {
                    file_id,
                    path: entry.path,
                });
            }
        }
        WireMessage::FileMeta {
            file_id,
            name,
            size,
        } => {
            let dir = match ensure_session_dir(session_dir, evt_tx) {
                Ok(dir) => dir,
                Err(err) => {
                    let _ = evt_tx.send(NetEvent::Log(format!("erro na sessao {err}")));
                    return new_peer;
                }
            };
            let safe_name = sanitize_file_name(&name);
            let path = dir.join(safe_name);
            match File::create(&path) {
                Ok(file) => {
                    let _ = evt_tx.send(NetEvent::ReceiveStarted {
                        file_id,
                        path: path.clone(),
                        size,
                    });
                    incoming.insert(
                        file_id,
                        IncomingFile {
                            file,
                            path: path.clone(),
                            size,
                            bytes_written: 0,
                        },
                    );
                    let _ = evt_tx.send(NetEvent::Log(format!("recebendo {}", path.display())));
                }
                Err(err) => {
                    let _ = evt_tx.send(NetEvent::Log(format!("erro ao criar arquivo {err}")));
                }
            }
        }
        WireMessage::FileChunk { file_id, data } => {
            if let Some(entry) = incoming.get_mut(&file_id) {
                if let Err(err) = entry.file.write_all(&data) {
                    let _ = evt_tx.send(NetEvent::Log(format!("erro ao gravar {err}")));
                } else {
                    entry.bytes_written += data.len() as u64;
                    let _ = evt_tx.send(NetEvent::ReceiveProgress {
                        file_id,
                        bytes_received: entry.bytes_written,
                        size: entry.size,
                    });
                }
            }
        }
        WireMessage::FileDone { file_id } => {
            if let Some(mut entry) = incoming.remove(&file_id) {
                let _ = entry.file.flush();
                let _ = evt_tx.send(NetEvent::FileReceived {
                    file_id,
                    path: entry.path.clone(),
                });
                if entry.bytes_written != entry.size {
                    let _ = evt_tx.send(NetEvent::Log(format!(
                        "tamanho divergente {} bytes",
                        entry.bytes_written
                    )));
                }
            }
        }
    }

    new_peer
}

fn ensure_session_dir(
    session_dir: &mut Option<PathBuf>,
    evt_tx: &Sender<NetEvent>,
) -> io::Result<PathBuf> {
    if let Some(dir) = session_dir.clone() {
        return Ok(dir);
    }

    let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let dir = std::env::current_dir()?.join("received").join(timestamp);
    fs::create_dir_all(&dir)?;
    *session_dir = Some(dir.clone());
    let _ = evt_tx.send(NetEvent::SessionDir(dir.clone()));
    Ok(dir)
}

fn sanitize_file_name(name: &str) -> String {
    Path::new(name)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("file.bin")
        .to_string()
}

fn send_punch(socket: &mut Socket, peer: SocketAddr, nonce: u64, evt_tx: &Sender<NetEvent>) {
    match bincode::serialize(&WireMessage::Punch { nonce }) {
        Ok(payload) => {
            if let Err(err) = socket.send(Packet::unreliable(peer, payload)) {
                let _ = evt_tx.send(NetEvent::Log(format!("erro ao enviar punch {err}")));
            }
        }
        Err(err) => {
            let _ = evt_tx.send(NetEvent::Log(format!("erro ao serializar {err}")));
        }
    }
}

fn send_message(
    socket: &mut Socket,
    peer: SocketAddr,
    message: &WireMessage,
    evt_tx: &Sender<NetEvent>,
) {
    match bincode::serialize(message) {
        Ok(payload) => {
            if let Err(err) = socket.send(Packet::reliable_ordered(peer, payload, Some(CHANNEL_ID)))
            {
                let _ = evt_tx.send(NetEvent::Log(format!("erro ao enviar {err}")));
            }
        }
        Err(err) => {
            let _ = evt_tx.send(NetEvent::Log(format!("erro ao serializar {err}")));
        }
    }
}

fn send_files(
    socket: &mut Socket,
    peer: SocketAddr,
    files: &[PathBuf],
    next_file_id: &mut u64,
    evt_tx: &Sender<NetEvent>,
    cmd_rx: &Receiver<NetCommand>,
    pending_cmds: &mut Vec<NetCommand>,
) -> io::Result<SendOutcome> {
    send_message(
        socket,
        peer,
        &WireMessage::Hello { version: 1 },
        evt_tx,
    );

    for path in files {
        match poll_send_control(cmd_rx, pending_cmds) {
            SendOutcome::Canceled => return Ok(SendOutcome::Canceled),
            SendOutcome::Shutdown => return Ok(SendOutcome::Shutdown),
            SendOutcome::Completed => {}
        }

        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        let size = metadata.len();
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("file.bin")
            .to_string();
        let file_id = *next_file_id;
        *next_file_id += 1;

        send_message(
            socket,
            peer,
            &WireMessage::FileMeta {
                file_id,
                name,
                size,
            },
            evt_tx,
        );
        let _ = evt_tx.send(NetEvent::SendStarted {
            file_id,
            path: path.clone(),
            size,
        });

        let mut buffer = vec![0u8; CHUNK_SIZE];
        let mut sent_bytes = 0u64;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            send_message(
                socket,
                peer,
                &WireMessage::FileChunk {
                    file_id,
                    data: buffer[..read].to_vec(),
                },
                evt_tx,
            );
            sent_bytes += read as u64;
            let _ = evt_tx.send(NetEvent::SendProgress {
                file_id,
                bytes_sent: sent_bytes,
                size,
            });
            socket.manual_poll(Instant::now());

            match poll_send_control(cmd_rx, pending_cmds) {
                SendOutcome::Canceled => {
                    send_cancel(socket, peer, file_id, evt_tx);
                    let _ = evt_tx.send(NetEvent::SendCanceled {
                        file_id,
                        path: path.clone(),
                    });
                    return Ok(SendOutcome::Canceled);
                }
                SendOutcome::Shutdown => {
                    send_cancel(socket, peer, file_id, evt_tx);
                    return Ok(SendOutcome::Shutdown);
                }
                SendOutcome::Completed => {}
            }
        }

        send_message(
            socket,
            peer,
            &WireMessage::FileDone { file_id },
            evt_tx,
        );
        let _ = evt_tx.send(NetEvent::FileSent {
            file_id,
            path: path.clone(),
        });
    }

    Ok(SendOutcome::Completed)
}

fn detect_public_endpoint(bind_addr: SocketAddr) -> Result<Option<SocketAddr>, String> {
    const OVERALL: Duration = Duration::from_secs(6);

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let res = detect_public_endpoint_inner(bind_addr);
        let _ = tx.send(res);
    });

    match rx.recv_timeout(OVERALL) {
        Ok(res) => res,
        Err(_) => Ok(None),
    }
}

fn detect_public_endpoint_inner(bind_addr: SocketAddr) -> Result<Option<SocketAddr>, String> {
    let socket = UdpSocket::bind(bind_addr)
        .map_err(|err| format!("falha ao abrir UDP para STUN: {err}"))?;
    let _ = socket.set_read_timeout(Some(STUN_TIMEOUT));

    let servers = stun_server_list();
    if servers.is_empty() {
        return Err("STUN sem servidores".to_string());
    }

    let mut last_err = None;
    for server in servers {
        let server_addr = match resolve_stun_server(server.as_str(), bind_addr) {
            Ok(addr) => addr,
            Err(err) => {
                last_err = Some(format!("falha ao resolver STUN {server}: {err}"));
                continue;
            }
        };

        let client = StunClient::new(server_addr);
        match client.query_external_address(&socket) {
            Ok(addr) => return Ok(Some(addr)),
            Err(err) => {
                last_err = Some(format!("falha STUN {server}: {err}"));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| "STUN sem servidores".to_string()))
}

fn stun_server_list() -> Vec<String> {
    if let Ok(value) = std::env::var("PASTA_P2P_STUN") {
        let list = value
            .split(',')
            .map(|item| item.trim())
            .filter(|item| !item.is_empty())
            .map(|item| item.to_string())
            .collect::<Vec<_>>();
        if !list.is_empty() {
            return list;
        }
    }

    STUN_SERVERS.iter().map(|item| item.to_string()).collect()
}

fn resolve_stun_server(server: &str, bind_addr: SocketAddr) -> io::Result<SocketAddr> {
    let want_v4 = bind_addr.is_ipv4();
    let mut first = None;
    for addr in server.to_socket_addrs()? {
        if first.is_none() {
            first = Some(addr);
        }
        if want_v4 == addr.is_ipv4() {
            return Ok(addr);
        }
    }
    first.ok_or_else(|| io::Error::new(io::ErrorKind::Other, "STUN sem endereco"))
}
