use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read, Write},
    net::SocketAddr,
    path::Path,
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    time::Instant,
};

use chrono::Local;
use laminar::{Packet, Socket};

use super::{NetCommand, NetEvent, WireMessage, serialize_message};

/// Representa um arquivo que está chegando do par.
pub(crate) struct IncomingFile {
    pub file: File,
    pub path: PathBuf,
    pub size: u64,
    pub bytes_written: u64,
}

#[derive(Clone, Copy)]
pub(crate) enum SendOutcome {
    Completed,
    Canceled,
    Shutdown,
}

/// Processa mensagens recebidas pela rede, atualizando o estado e retornando
/// `true` se um novo par foi identificado.
pub(crate) fn handle_incoming_message(
    socket: &mut Socket,
    message: WireMessage,
    from: SocketAddr,
    peer_addr: &mut Option<SocketAddr>,
    session_dir: &mut Option<PathBuf>,
    incoming: &mut HashMap<u64, IncomingFile>,
    evt_tx: &Sender<NetEvent>,
    channel_id: u8,
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
            send_message(
                socket,
                from,
                &WireMessage::Hello { version: 1 },
                evt_tx,
                channel_id,
            );
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

/// Envia mensagens de perfuração periódicas para manter o socket aberto.
pub(crate) fn handle_send_punch(
    socket: &mut Socket,
    peer: SocketAddr,
    nonce: u64,
    evt_tx: &Sender<NetEvent>,
) {
    match serialize_message(&WireMessage::Punch { nonce }) {
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

/// Envia um pacote de controle confiável para o par.
pub(crate) fn send_message(
    socket: &mut Socket,
    peer: SocketAddr,
    message: &WireMessage,
    evt_tx: &Sender<NetEvent>,
    channel_id: u8,
) {
    match serialize_message(message) {
        Ok(payload) => {
            if let Err(err) = socket.send(Packet::reliable_ordered(peer, payload, Some(channel_id)))
            {
                let _ = evt_tx.send(NetEvent::Log(format!("erro ao enviar {err}")));
            }
        }
        Err(err) => {
            let _ = evt_tx.send(NetEvent::Log(format!("erro ao serializar {err}")));
        }
    }
}

/// Processa a fila de comandos durante um envio para detectar cancelamentos ou encerramentos.
pub(crate) fn handle_send_control(
    cmd_rx: &Receiver<NetCommand>,
    pending_cmds: &mut Vec<NetCommand>,
) -> SendOutcome {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            NetCommand::CancelTransfers => return SendOutcome::Canceled,
            NetCommand::Shutdown => return SendOutcome::Shutdown,
            NetCommand::Rebind(addr) => {
                pending_cmds.push(NetCommand::Rebind(addr));
                return SendOutcome::Canceled;
            }
            other => pending_cmds.push(other),
        }
    }
    SendOutcome::Completed
}

/// Envia os arquivos para o par conectado respeitando cancelamentos da UI.
pub(crate) fn send_files(
    socket: &mut Socket,
    peer: SocketAddr,
    files: &[PathBuf],
    next_file_id: &mut u64,
    evt_tx: &Sender<NetEvent>,
    cmd_rx: &Receiver<NetCommand>,
    pending_cmds: &mut Vec<NetCommand>,
    channel_id: u8,
    chunk_size: usize,
) -> io::Result<SendOutcome> {
    send_message(
        socket,
        peer,
        &WireMessage::Hello { version: 1 },
        evt_tx,
        channel_id,
    );

    for path in files {
        match handle_send_control(cmd_rx, pending_cmds) {
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
            channel_id,
        );
        let _ = evt_tx.send(NetEvent::SendStarted {
            file_id,
            path: path.clone(),
            size,
        });

        let mut buffer = vec![0u8; chunk_size];
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
                channel_id,
            );
            sent_bytes += read as u64;
            let _ = evt_tx.send(NetEvent::SendProgress {
                file_id,
                bytes_sent: sent_bytes,
                size,
            });
            socket.manual_poll(Instant::now());

            match handle_send_control(cmd_rx, pending_cmds) {
                SendOutcome::Canceled => {
                    send_cancel(socket, peer, file_id, evt_tx, channel_id);
                    let _ = evt_tx.send(NetEvent::SendCanceled {
                        file_id,
                        path: path.clone(),
                    });
                    return Ok(SendOutcome::Canceled);
                }
                SendOutcome::Shutdown => {
                    send_cancel(socket, peer, file_id, evt_tx, channel_id);
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
            channel_id,
        );
        let _ = evt_tx.send(NetEvent::FileSent {
            file_id,
            path: path.clone(),
        });
    }

    Ok(SendOutcome::Completed)
}

fn send_cancel(
    socket: &mut Socket,
    peer: SocketAddr,
    file_id: u64,
    evt_tx: &Sender<NetEvent>,
    channel_id: u8,
) {
    send_message(
        socket,
        peer,
        &WireMessage::Cancel { file_id },
        evt_tx,
        channel_id,
    );
}
