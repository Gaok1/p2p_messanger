use std::net::SocketAddr;
use std::path::PathBuf;

/// Comandos enviados pela UI para a thread de rede.
pub enum NetCommand {
    ConnectPeer(SocketAddr),
    ProbePeer(SocketAddr),
    Rebind(SocketAddr),
    CancelTransfers,
    SendFiles(Vec<PathBuf>),
    /// Mensagem interna: resposta de resume encaminhada para a tarefa de envio.
    /// A UI nao deve gerar este comando.
    InternalResumeAnswer {
        file_id: u64,
        offset: u64,
        ok: bool,
        reason: Option<String>,
    },
    Shutdown,
}

/// Eventos gerados pela thread de rede para atualizar a UI.
pub enum NetEvent {
    Log(String),
    Bound(SocketAddr),
    PublicEndpoint(SocketAddr),
    ProbeFinished {
        peer: SocketAddr,
        ok: bool,
        message: String,
    },
    FileSent {
        file_id: u64,
        path: PathBuf,
    },
    FileReceived {
        file_id: u64,
        path: PathBuf,
        from: SocketAddr,
    },
    SessionDir(PathBuf),
    PeerConnecting(SocketAddr),
    PeerConnected(SocketAddr),
    PeerDisconnected(SocketAddr),
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
    ReceiveFailed {
        file_id: u64,
        path: PathBuf,
    },
    PublicEndpointObserved(SocketAddr),
}
