use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::mpsc::Sender,
};

use quinn::{ReadError, RecvStream, VarInt};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter},
    sync::mpsc as tokio_mpsc,
    task,
};

const TRANSFER_BUFFER_SIZE: usize = 256 * 1024;
const PROGRESS_EMIT_BYTES: u64 = 1024 * 1024;
const PROGRESS_EMIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

use super::{
    NetCommand, NetEvent, OBSERVED_ENDPOINT_VERSION, PROTOCOL_VERSION, WireMessage,
    serialize_message,
};

/// Representa um arquivo que está chegando do par.
pub(crate) struct IncomingTransfer {
    pub path: PathBuf,
    pub handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Copy)]
pub(crate) enum SendOutcome {
    Completed,
    Canceled,
    Shutdown,
}

pub(crate) struct SendResult {
    pub outcome: SendOutcome,
    pub next_file_id: u64,
    pub pending_cmds: Vec<NetCommand>,
}

/// Processa mensagens recebidas pela rede, atualizando o estado e retornando
/// `true` se um novo par foi identificado.
pub(crate) async fn handle_incoming_message(
    connection: &quinn::Connection,
    message: WireMessage,
    from: SocketAddr,
    peer_addr: &mut Option<SocketAddr>,
    session_dir: &mut Option<PathBuf>,
    incoming: &mut HashMap<u64, IncomingTransfer>,
    public_endpoint: &mut Option<SocketAddr>,
    evt_tx: &Sender<NetEvent>,
) -> bool {
    let mut new_peer = false;
    if peer_addr.is_none() {
        *peer_addr = Some(from);
        new_peer = true;
    }

    match message {
        WireMessage::Hello { version } => {
            if session_dir.is_none() {
                if let Err(err) = ensure_session_dir(session_dir, evt_tx).await {
                    let _ = evt_tx.send(NetEvent::Log(format!("erro na sessao {err}")));
                }
            }
            if version >= OBSERVED_ENDPOINT_VERSION {
                let _ = send_message(
                    connection,
                    &WireMessage::ObservedEndpoint { addr: from },
                    evt_tx,
                )
                .await;
            }
        }
        WireMessage::ObservedEndpoint { addr } => {
            if *public_endpoint != Some(addr) {
                *public_endpoint = Some(addr);
                let _ = evt_tx.send(NetEvent::PublicEndpoint(addr));
            }
        }
        WireMessage::Punch { .. } => {
            let _ = send_message(
                connection,
                &WireMessage::Hello {
                    version: PROTOCOL_VERSION,
                },
                evt_tx,
            )
            .await;
        }
        WireMessage::Cancel { file_id } => {
            if let Some(entry) = incoming.remove(&file_id) {
                entry.handle.abort();
                let _ = evt_tx.send(NetEvent::ReceiveCanceled {
                    file_id,
                    path: entry.path,
                });
            }
        }
        WireMessage::ResumeQuery {
            file_id,
            name,
            size,
        } => match query_resume_offset(session_dir, &name, size, evt_tx).await {
            Ok(offset) => {
                let _ = send_message(
                    connection,
                    &WireMessage::ResumeAnswer {
                        file_id,
                        offset,
                        ok: true,
                        reason: None,
                    },
                    evt_tx,
                )
                .await;
            }
            Err(err) => {
                let _ = send_message(
                    connection,
                    &WireMessage::ResumeAnswer {
                        file_id,
                        offset: 0,
                        ok: false,
                        reason: Some(err.to_string()),
                    },
                    evt_tx,
                )
                .await;
            }
        },
        _ => {}
    }

    new_peer
}

pub(crate) async fn handle_incoming_stream(
    file_id: u64,
    name: String,
    size: u64,
    offset: u64,
    from: SocketAddr,
    mut stream: RecvStream,
    session_dir: &mut Option<PathBuf>,
    incoming: &mut HashMap<u64, IncomingTransfer>,
    evt_tx: &Sender<NetEvent>,
    completion_tx: tokio_mpsc::UnboundedSender<u64>,
) -> io::Result<()> {
    let dir = ensure_session_dir(session_dir, evt_tx).await?;
    let safe_name = sanitize_file_name(&name);
    let (final_path, path) = resolve_incoming_paths(&dir, &safe_name).await;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .open(&path)
        .await?;

    let current_len = file.metadata().await?.len();
    if current_len > offset {
        file.set_len(offset).await?;
    } else if current_len < offset {
        let _ = evt_tx.send(NetEvent::ReceiveFailed {
            file_id,
            path: path.clone(),
        });
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("offset remoto {offset} maior que parcial local {current_len}"),
        ));
    }
    file.seek(std::io::SeekFrom::Start(offset)).await?;

    let _ = evt_tx.send(NetEvent::ReceiveStarted {
        file_id,
        path: path.clone(),
        size,
    });
    let _ = evt_tx.send(NetEvent::Log(format!(
        "recebendo {} (offset {offset})",
        path.display()
    )));

    let evt_tx = evt_tx.clone();
    let completion = completion_tx.clone();
    let task_path = path.clone();
    let handle = task::spawn(async move {
        let mut file = BufWriter::new(file);
        let mut received = offset;
        let mut reported = 0u64;
        let mut last_emit = std::time::Instant::now();
        let mut buffer = vec![0u8; TRANSFER_BUFFER_SIZE];

        loop {
            match stream.read(&mut buffer).await {
                Ok(Some(n)) if n == 0 => {
                    break;
                }
                Ok(Some(n)) => {
                    if let Err(err) = file.write_all(&buffer[..n]).await {
                        let _ = evt_tx.send(NetEvent::Log(format!("erro ao gravar {err}")));
                        let _ = evt_tx.send(NetEvent::ReceiveFailed {
                            file_id,
                            path: task_path.clone(),
                        });
                        let _ = completion.send(file_id);
                        return;
                    }
                    received += n as u64;
                    if should_emit_progress(received, reported, last_emit) {
                        reported = received;
                        last_emit = std::time::Instant::now();
                        let _ = evt_tx.send(NetEvent::ReceiveProgress {
                            file_id,
                            bytes_received: received,
                            size,
                        });
                    }
                }
                Ok(None) => {
                    let _ = file.flush().await;
                    if received > reported {
                        let _ = evt_tx.send(NetEvent::ReceiveProgress {
                            file_id,
                            bytes_received: received,
                            size,
                        });
                    }
                    if received == size {
                        if let Err(err) = tokio::fs::rename(&task_path, &final_path).await {
                            let _ = evt_tx
                                .send(NetEvent::Log(format!("erro ao finalizar arquivo {err}")));
                            let _ = evt_tx.send(NetEvent::ReceiveFailed {
                                file_id,
                                path: task_path.clone(),
                            });
                        } else {
                            let _ = evt_tx.send(NetEvent::FileReceived {
                                file_id,
                                path: final_path.clone(),
                                from,
                            });
                        }
                    } else {
                        let _ = evt_tx.send(NetEvent::Log(format!(
                            "tamanho divergente {received} bytes (esperado {size})"
                        )));
                    }
                    break;
                }
                Err(ReadError::Reset(_)) => {
                    let _ = evt_tx.send(NetEvent::ReceiveCanceled {
                        file_id,
                        path: task_path.clone(),
                    });
                    let _ = completion.send(file_id);
                    return;
                }
                Err(err) => {
                    let _ = evt_tx.send(NetEvent::Log(format!("erro na leitura do stream {err}")));
                    let _ = evt_tx.send(NetEvent::ReceiveFailed {
                        file_id,
                        path: task_path.clone(),
                    });
                    let _ = completion.send(file_id);
                    return;
                }
            }
        }

        let _ = completion.send(file_id);
    });

    incoming.insert(file_id, IncomingTransfer { path, handle });

    Ok(())
}

pub(crate) async fn ensure_session_dir(
    session_dir: &mut Option<PathBuf>,
    evt_tx: &Sender<NetEvent>,
) -> io::Result<PathBuf> {
    if let Some(dir) = session_dir.clone() {
        return Ok(dir);
    }

    let dir = std::env::current_dir()?.join("received");
    tokio::fs::create_dir_all(&dir).await?;
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

async fn unique_download_path(dir: &Path, file_name: &str) -> PathBuf {
    let mut candidate = dir.join(file_name);
    if !tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
        return candidate;
    }

    let base = Path::new(file_name);
    let stem = base
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("file");
    let ext = base.extension().and_then(|value| value.to_str());

    for idx in 2..=9999_u32 {
        let next_name = match ext {
            Some(ext) => format!("{stem} ({idx}).{ext}"),
            None => format!("{stem} ({idx})"),
        };
        candidate = dir.join(next_name);
        if !tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
            return candidate;
        }
    }

    candidate
}

async fn resolve_incoming_paths(dir: &Path, file_name: &str) -> (PathBuf, PathBuf) {
    let final_path = unique_download_path(dir, file_name).await;
    let mut part_name = final_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file.bin")
        .to_string();
    part_name.push_str(".part");
    let part_path = final_path.with_file_name(part_name);
    (final_path, part_path)
}

async fn query_resume_offset(
    session_dir: &mut Option<PathBuf>,
    name: &str,
    size: u64,
    evt_tx: &Sender<NetEvent>,
) -> io::Result<u64> {
    let dir = ensure_session_dir(session_dir, evt_tx).await?;
    let safe_name = sanitize_file_name(name);
    let (_final_path, part_path) = resolve_incoming_paths(&dir, &safe_name).await;

    match tokio::fs::metadata(&part_path).await {
        Ok(meta) => {
            let current = meta.len();
            Ok(current.min(size))
        }
        Err(_) => Ok(0),
    }
}

/// Envia um pacote de controle confiável para o par.
pub(crate) async fn send_message(
    connection: &quinn::Connection,
    message: &WireMessage,
    evt_tx: &Sender<NetEvent>,
) -> Result<(), quinn::WriteError> {
    match serialize_message(message) {
        Ok(payload) => {
            let mut stream = connection.open_uni().await?;
            if let Err(err) = write_framed(&mut stream, &payload).await {
                let _ = evt_tx.send(NetEvent::Log(format!("erro ao enviar {err}")));
            }
            let _ = stream.finish();
            Ok(())
        }
        Err(err) => {
            let _ = evt_tx.send(NetEvent::Log(format!("erro ao serializar {err}")));
            Ok(())
        }
    }
}

async fn write_wire_message_framed(
    stream: &mut quinn::SendStream,
    message: &WireMessage,
    evt_tx: &Sender<NetEvent>,
) -> io::Result<()> {
    match serialize_message(message) {
        Ok(payload) => write_framed(stream, &payload).await,
        Err(err) => {
            let _ = evt_tx.send(NetEvent::Log(format!("erro ao serializar {err}")));
            Ok(())
        }
    }
}

async fn write_framed(stream: &mut quinn::SendStream, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    Ok(())
}

/// Processa a fila de comandos durante um envio para detectar cancelamentos ou encerramentos.
pub(crate) fn handle_send_control(
    cmd_rx: &mut tokio_mpsc::UnboundedReceiver<NetCommand>,
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
pub(crate) async fn send_files(
    connection: &quinn::Connection,
    _peer: SocketAddr,
    files: &[PathBuf],
    mut next_file_id: u64,
    evt_tx: &Sender<NetEvent>,
    cmd_rx: &mut tokio_mpsc::UnboundedReceiver<NetCommand>,
    chunk_size: usize,
) -> io::Result<SendResult> {
    let mut pending_cmds: Vec<NetCommand> = Vec::new();
    let _ = send_message(
        connection,
        &WireMessage::Hello {
            version: PROTOCOL_VERSION,
        },
        evt_tx,
    )
    .await;

    for path in files {
        match handle_send_control(cmd_rx, &mut pending_cmds) {
            SendOutcome::Canceled => {
                return Ok(SendResult {
                    outcome: SendOutcome::Canceled,
                    next_file_id,
                    pending_cmds,
                });
            }
            SendOutcome::Shutdown => {
                return Ok(SendResult {
                    outcome: SendOutcome::Shutdown,
                    next_file_id,
                    pending_cmds,
                });
            }
            SendOutcome::Completed => {}
        }

        let mut file = File::open(path).await?;
        let metadata = file.metadata().await?;
        let size = metadata.len();
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("file.bin")
            .to_string();
        let file_id = next_file_id;
        next_file_id = next_file_id.wrapping_add(1);

        let _ = send_message(
            connection,
            &WireMessage::ResumeQuery {
                file_id,
                name: name.clone(),
                size,
            },
            evt_tx,
        )
        .await;

        let mut stream = match connection.open_uni().await {
            Ok(stream) => stream,
            Err(err) => {
                let _ = evt_tx.send(NetEvent::Log(format!("erro ao abrir stream {err}")));
                continue;
            }
        };

        let _ = write_wire_message_framed(
            &mut stream,
            &WireMessage::FileMeta {
                file_id,
                name,
                size,
                offset: 0,
            },
            evt_tx,
        )
        .await;
        let _ = evt_tx.send(NetEvent::SendStarted {
            file_id,
            path: path.clone(),
            size,
        });

        let mut buffer = vec![0u8; chunk_size.max(TRANSFER_BUFFER_SIZE)];
        let mut sent_bytes = 0u64;
        let mut reported = 0u64;
        let mut last_emit = std::time::Instant::now();
        let mut success = true;
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }

            if let Err(err) = stream.write_all(&buffer[..read]).await {
                let _ = evt_tx.send(NetEvent::Log(format!("erro ao enviar {err}")));
                success = false;
                break;
            }
            sent_bytes += read as u64;
            if should_emit_progress(sent_bytes, reported, last_emit) {
                reported = sent_bytes;
                last_emit = std::time::Instant::now();
                let _ = evt_tx.send(NetEvent::SendProgress {
                    file_id,
                    bytes_sent: sent_bytes,
                    size,
                });
            }

            match handle_send_control(cmd_rx, &mut pending_cmds) {
                SendOutcome::Canceled => {
                    let _ = stream.reset(VarInt::from_u32(0));
                    let _ =
                        send_message(connection, &WireMessage::Cancel { file_id }, evt_tx).await;
                    let _ = evt_tx.send(NetEvent::SendCanceled {
                        file_id,
                        path: path.clone(),
                    });
                    return Ok(SendResult {
                        outcome: SendOutcome::Canceled,
                        next_file_id,
                        pending_cmds,
                    });
                }
                SendOutcome::Shutdown => {
                    let _ = stream.reset(VarInt::from_u32(0));
                    let _ =
                        send_message(connection, &WireMessage::Cancel { file_id }, evt_tx).await;
                    return Ok(SendResult {
                        outcome: SendOutcome::Shutdown,
                        next_file_id,
                        pending_cmds,
                    });
                }
                SendOutcome::Completed => {}
            }
        }

        if success {
            if sent_bytes > reported {
                let _ = evt_tx.send(NetEvent::SendProgress {
                    file_id,
                    bytes_sent: sent_bytes,
                    size,
                });
            }
            let _ = stream.finish();
            let _ = evt_tx.send(NetEvent::FileSent {
                file_id,
                path: path.clone(),
            });
        }
    }

    Ok(SendResult {
        outcome: SendOutcome::Completed,
        next_file_id,
        pending_cmds,
    })
}

fn should_emit_progress(total: u64, reported: u64, last_emit: std::time::Instant) -> bool {
    total.saturating_sub(reported) >= PROGRESS_EMIT_BYTES
        || last_emit.elapsed() >= PROGRESS_EMIT_INTERVAL
}
