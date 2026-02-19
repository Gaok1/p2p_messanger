use std::net::SocketAddr;
use std::path::PathBuf;

use base64::Engine;
use bincode::Options;
use quinn::RecvStream;
use tokio::sync::Mutex;

use super::journal::TransferJournal;

fn bincode_options() -> impl Options {
    bincode::DefaultOptions::new().with_fixint_encoding()
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum WireMessage {
    Hello {
        version: u8,
    },
    /// Handshake de identidade no nível da aplicação.
    ///
    /// - `pubkey`: chave pública Ed25519 (32 bytes)
    /// - `challenge`: desafio aleatório (32 bytes)
    /// - `label`: opcional, apenas para UI/log (não tem efeito de segurança)
    IdentityInit {
        version: u8,
        pubkey: Vec<u8>,
        challenge: [u8; 32],
        label: Option<String>,
    },
    /// Resposta ao IdentityInit: assinatura do challenge recebido.
    IdentityAck {
        pubkey: Vec<u8>,
        signature: Vec<u8>,
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
        offset: u64,
    },
    ResumeQuery {
        file_id: u64,
        name: String,
        size: u64,
    },
    ResumeAnswer {
        file_id: u64,
        offset: u64,
        ok: bool,
        reason: Option<String>,
    },
    FileChunk {
        file_id: u64,
        data: Vec<u8>,
    },
    FileDone {
        file_id: u64,
    },
    ObservedEndpoint {
        addr: SocketAddr,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

pub enum InboundFrame {
    Control(WireMessage, SocketAddr),
    FileStream {
        file_id: u64,
        name: String,
        size: u64,
        offset: u64,
        from: SocketAddr,
        stream: RecvStream,
    },
}

pub fn serialize_message(message: &WireMessage) -> bincode::Result<Vec<u8>> {
    bincode_options().serialize(message)
}

#[allow(dead_code)]
pub fn serialize_message_base64(message: &WireMessage) -> bincode::Result<String> {
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

pub fn decode_payload(payload: &[u8]) -> Result<WireMessage, String> {
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

pub fn spawn_send_task(
    files: Vec<PathBuf>,
    connection: &Option<quinn::Connection>,
    connected_peer: Option<SocketAddr>,
    peer_id: Option<String>,
    next_file_id: u64,
    evt_tx: &std::sync::mpsc::Sender<crate::net::NetEvent>,
    journal: std::sync::Arc<Mutex<TransferJournal>>,
) -> Option<(
    tokio::task::JoinHandle<std::io::Result<crate::net::transfer::SendResult>>,
    tokio::sync::mpsc::UnboundedSender<crate::net::NetCommand>,
)> {
    let Some(peer) = connected_peer else {
        let _ = evt_tx.send(crate::net::NetEvent::Log(
            "nenhum par conectado para enviar arquivos".to_string(),
        ));
        return None;
    };

    let Some(connection) = connection else {
        let _ = evt_tx.send(crate::net::NetEvent::Log(
            "nenhuma conexao ativa para enviar arquivos".to_string(),
        ));
        return None;
    };

    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<crate::net::NetCommand>();
    let conn = connection.clone();
    let evt_tx_clone = evt_tx.clone();
    let peer_id = peer_id.unwrap_or_else(|| peer.to_string());
    let journal = journal.clone();

    let handle = tokio::spawn(async move {
        crate::net::transfer::send_files(
            &conn,
            peer,
            peer_id,
            &files,
            next_file_id,
            &evt_tx_clone,
            &mut cmd_rx,
            crate::net::transfer_chunk_size(),
            journal,
        )
        .await
    });

    Some((handle, cmd_tx))
}
