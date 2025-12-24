use std::{
    io,
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    sync::mpsc,
    thread,
    time::Duration,
};

use stunclient::StunClient;

/// STUN servers dedicados para IPv4.
const STUN_SERVERS_V4: [&str; 5] = [
    "stun.cloudflare.com:3478",
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.sipgate.net:3478",
    "stun.stunprotocol.net:3478",
];

/// STUN servers dedicados para IPv6.
const STUN_SERVERS_V6: [&str; 5] = [
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun.sipgate.net:3478",
    "stun.antisip.com:3478",
    "stun.callwithus.com:3478",
];

const STUN_TIMEOUT: Duration = Duration::from_secs(1);

/// Executa a detecção do endpoint público usando STUN com a família correta do bind.
pub fn detect_public_endpoint(bind_addr: SocketAddr) -> Result<Option<SocketAddr>, String> {
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
    let socket =
        UdpSocket::bind(bind_addr).map_err(|err| format!("falha ao abrir UDP para STUN: {err}"))?;
    let _ = socket.set_read_timeout(Some(STUN_TIMEOUT));

    let servers = stun_server_list(bind_addr);
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

pub(crate) fn stun_server_list(bind_addr: SocketAddr) -> Vec<String> {
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

    if bind_addr.is_ipv4() {
        STUN_SERVERS_V4
            .iter()
            .map(|item| item.to_string())
            .collect()
    } else {
        STUN_SERVERS_V6
            .iter()
            .map(|item| item.to_string())
            .collect()
    }
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
