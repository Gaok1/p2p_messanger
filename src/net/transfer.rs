use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, Sender},
};

use chrono::Local;
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
};

use super::{
    NetCommand, NetEvent, WireMessage, serialize_message,
};

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
pub(crate) async fn handle_incoming_message(
    connection: &quinn::Connection,
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
                if let Err(err) = ensure_session_dir(session_dir, evt_tx).await {
                    let _ = evt_tx.send(NetEvent::Log(format!("erro na sessao {err}")));
                }
            }
        }
        WireMessage::Punch { .. } => {
            let _ = send_message(
                connection,
                &WireMessage::Hello { version: 1 },
                evt_tx,
            )
            .await;
        }
        WireMessage::Cancel { file_id } => {
            if let Some(entry) = incoming.remove(&file_id) {
                let _ = tokio::fs::remove_file(&entry.path).await;
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
            let dir = match ensure_session_dir(session_dir, evt_tx).await {
                Ok(dir) => dir,
                Err(err) => {
                    let _ = evt_tx.send(NetEvent::Log(format!("erro na sessao {err}")));
                    return new_peer;
                }
            };
            let safe_name = sanitize_file_name(&name);
            let path = dir.join(safe_name);
            match File::create(&path).await {
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
                match entry.file.write_all(&data).await {
                    Ok(()) => {
                        entry.bytes_written += data.len() as u64;
                        let _ = evt_tx.send(NetEvent::ReceiveProgress {
                            file_id,
                            bytes_received: entry.bytes_written,
                            size: entry.size,
                        });
                    }
                    Err(err) => {
                        let _ = evt_tx.send(NetEvent::Log(format!("erro ao gravar {err}")));
                    }
                }
            }
        }
        WireMessage::FileDone { file_id } => {
            if let Some(entry) = incoming.get(&file_id) {
                if entry.bytes_written == entry.size {
                    let _ = evt_tx.send(NetEvent::FileReceived {
                        file_id,
                        path: entry.path.clone(),
                    });
                } else {
                    let _ = evt_tx.send(NetEvent::Log(format!(
                        "tamanho divergente {} bytes",
                        entry.bytes_written
                    )));
                }
            }
            incoming.remove(&file_id);
        }
    }

    new_peer
}

async fn ensure_session_dir(
    session_dir: &mut Option<PathBuf>,
    evt_tx: &Sender<NetEvent>,
) -> io::Result<PathBuf> {
    if let Some(dir) = session_dir.clone() {
        return Ok(dir);
    }

    let timestamp = Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let dir = std::env::current_dir()?.join("received").join(timestamp);
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

async fn write_framed(stream: &mut quinn::SendStream, payload: &[u8]) -> io::Result<()> {
    let len = payload.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    Ok(())
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
pub(crate) async fn send_files(
    connection: &quinn::Connection,
    _peer: SocketAddr,
    files: &[PathBuf],
    next_file_id: &mut u64,
    evt_tx: &Sender<NetEvent>,
    cmd_rx: &Receiver<NetCommand>,
    pending_cmds: &mut Vec<NetCommand>,
    chunk_size: usize,
) -> io::Result<SendOutcome> {
    let _ = send_message(connection, &WireMessage::Hello { version: 1 }, evt_tx).await;

    for path in files {
        match handle_send_control(cmd_rx, pending_cmds) {
            SendOutcome::Canceled => return Ok(SendOutcome::Canceled),
            SendOutcome::Shutdown => return Ok(SendOutcome::Shutdown),
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
        let file_id = *next_file_id;
        *next_file_id += 1;

        let _ = send_message(
            connection,
            &WireMessage::FileMeta {
                file_id,
                name,
                size,
            },
            evt_tx,
        )
        .await;
        let _ = evt_tx.send(NetEvent::SendStarted {
            file_id,
            path: path.clone(),
            size,
        });

        let mut buffer = vec![0u8; chunk_size];
        let mut sent_bytes = 0u64;
        loop {
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }

            let _ = send_message(
                connection,
                &WireMessage::FileChunk {
                    file_id,
                    data: buffer[..read].to_vec(),
                },
                evt_tx,
            )
            .await;
            sent_bytes += read as u64;
            let _ = evt_tx.send(NetEvent::SendProgress {
                file_id,
                bytes_sent: sent_bytes,
                size,
            });

            match handle_send_control(cmd_rx, pending_cmds) {
                SendOutcome::Canceled => {
                    let _ = send_message(
                        connection,
                        &WireMessage::Cancel { file_id },
                        evt_tx,
                    )
                    .await;
                    let _ = evt_tx.send(NetEvent::SendCanceled {
                        file_id,
                        path: path.clone(),
                    });
                    return Ok(SendOutcome::Canceled);
                }
                SendOutcome::Shutdown => {
                    let _ = send_message(
                        connection,
                        &WireMessage::Cancel { file_id },
                        evt_tx,
                    )
                    .await;
                    return Ok(SendOutcome::Shutdown);
                }
                SendOutcome::Completed => {}
            }
        }

        let _ = send_message(connection, &WireMessage::FileDone { file_id }, evt_tx).await;
        let _ = evt_tx.send(NetEvent::FileSent {
            file_id,
            path: path.clone(),
        });
    }

    Ok(SendOutcome::Completed)
}
