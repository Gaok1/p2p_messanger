use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Lista de servidores STUN para IPv4.
///
/// Importante: hostnames não são “dedicados” a uma família; muitos resolvem A e AAAA.
/// A seleção real por família acontece em `resolve_stun_server` (filtra por A vs AAAA).
const STUN_SERVERS_V4: [&str; 12] = [
    // Cloudflare
    "stun.cloudflare.com:3478",

    // Twilio (STUN global)
    "global.stun.twilio.com:3478",

    // Google (WebRTC)
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun2.l.google.com:19302",
    "stun3.l.google.com:19302",
    "stun4.l.google.com:19302",

    // Outros
    "stun.sipgate.net:3478",
    "stun.nextcloud.com:443",
    "stun.zoiper.com:3478",
    "stun.ekiga.net:3478",
    "stun.voxgratia.org:3478",
];

/// Lista de servidores STUN para IPv6.
///
/// Importante: hostnames não são “dedicados” a uma família; muitos resolvem A e AAAA.
/// A seleção real por família acontece em `resolve_stun_server` (filtra por A vs AAAA).
const STUN_SERVERS_V6: [&str; 13] = [
    // Cloudflare
    "stun.cloudflare.com:3478",

    // Twilio (STUN global)
    "global.stun.twilio.com:3478",

    // Google (WebRTC)
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun2.l.google.com:19302",
    "stun3.l.google.com:19302",
    "stun4.l.google.com:19302",

    // Outros
    "stun.sipgate.net:3478",
    "stun.nextcloud.com:443",
    "stun.zoiper.com:3478",
    "stun.ekiga.net:3478",
    "stunserver.org:3478",
    "stun.voxgratia.org:3478",
];

const STUN_TIMEOUT: Duration = Duration::from_secs(1);
pub const STUN_READ_TIMEOUT: Duration = Duration::from_millis(200);
const STUN_MAGIC_COOKIE: u32 = 0x2112A442;
const STUN_BINDING_REQUEST: u16 = 0x0001;
const STUN_BINDING_SUCCESS: u16 = 0x0101;
const STUN_ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const STUN_ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

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
    let _ = socket.set_read_timeout(Some(STUN_READ_TIMEOUT));
    detect_public_endpoint_on_socket(&socket, bind_addr)
}

/// Executa a detecção do endpoint público usando um socket já aberto/bindado.
///
/// Útil quando a porta do QUIC já está ocupada (Windows), mas precisamos usar a mesma porta
/// para que o endpoint público seja válido.
pub fn detect_public_endpoint_on_socket(
    socket: &UdpSocket,
    bind_addr: SocketAddr,
) -> Result<Option<SocketAddr>, String> {
    let _ = socket.set_read_timeout(Some(STUN_READ_TIMEOUT));
    let servers = stun_server_list(bind_addr);
    if servers.is_empty() {
        return Err("STUN sem servidores".to_string());
    }

    let mut seed = txid_seed();
    let mut last_err = None;
    let mut requests = Vec::new();
    for server in servers {
        let server_addr = match resolve_stun_server(server.as_str(), bind_addr) {
            Ok(addr) => addr,
            Err(err) => {
                last_err = Some(format!("falha ao resolver STUN {server}: {err}"));
                continue;
            }
        };

        let txid = next_transaction_id(&mut seed);
        let request = build_stun_request(txid);
        if let Err(err) = socket.send_to(&request, server_addr) {
            last_err = Some(format!("falha STUN {server}: {err}"));
            continue;
        }
        requests.push(txid);
    }

    if requests.is_empty() {
        return Err(last_err.unwrap_or_else(|| "STUN sem servidores".to_string()));
    }

    let overall_ms = STUN_TIMEOUT
        .as_millis()
        .saturating_mul(requests.len() as u128);
    let overall = Duration::from_millis(overall_ms.min(u128::from(u64::MAX)) as u64);
    let deadline = Instant::now() + overall;
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((size, _)) => {
                if let Some((txid, endpoint)) = parse_stun_response(&buf[..size]) {
                    if requests.iter().any(|id| id == &txid)
                        && ((bind_addr.is_ipv4() && endpoint.is_ipv4())
                            || (bind_addr.is_ipv6() && endpoint.is_ipv6()))
                    {
                        return Ok(Some(endpoint));
                    }
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(err) => {
                last_err = Some(format!("falha ao receber STUN: {err}"));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| "STUN sem resposta".to_string()))
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

fn txid_seed() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn next_transaction_id(seed: &mut u128) -> [u8; 12] {
    *seed = seed.wrapping_add(1);
    let bytes = seed.to_be_bytes();
    let mut id = [0u8; 12];
    id.copy_from_slice(&bytes[4..]);
    id
}

fn build_stun_request(txid: [u8; 12]) -> [u8; 20] {
    let mut buf = [0u8; 20];
    buf[0..2].copy_from_slice(&STUN_BINDING_REQUEST.to_be_bytes());
    buf[2..4].copy_from_slice(&0u16.to_be_bytes());
    buf[4..8].copy_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
    buf[8..20].copy_from_slice(&txid);
    buf
}

fn parse_stun_response(data: &[u8]) -> Option<([u8; 12], SocketAddr)> {
    if data.len() < 20 {
        return None;
    }

    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    if msg_type != STUN_BINDING_SUCCESS {
        return None;
    }

    let msg_len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let end = 20usize.saturating_add(msg_len);
    if data.len() < end {
        return None;
    }

    let magic = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if magic != STUN_MAGIC_COOKIE {
        return None;
    }

    let mut txid = [0u8; 12];
    txid.copy_from_slice(&data[8..20]);

    let mut offset = 20usize;
    while offset + 4 <= end {
        let attr_type = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let attr_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        let value_start = offset + 4;
        let value_end = value_start.saturating_add(attr_len);
        if value_end > end {
            return None;
        }

        let value = &data[value_start..value_end];
        let addr = match attr_type {
            STUN_ATTR_XOR_MAPPED_ADDRESS => parse_xor_mapped_address(value, &txid),
            STUN_ATTR_MAPPED_ADDRESS => parse_mapped_address(value),
            _ => None,
        };
        if let Some(addr) = addr {
            return Some((txid, addr));
        }

        let padded_len = (attr_len + 3) & !3;
        offset = value_start.saturating_add(padded_len);
    }

    None
}

fn parse_mapped_address(value: &[u8]) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }

    let family = value[1];
    let port = u16::from_be_bytes([value[2], value[3]]);
    match family {
        0x01 => {
            if value.len() < 8 {
                return None;
            }
            let ip = Ipv4Addr::new(value[4], value[5], value[6], value[7]);
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        }
        0x02 => {
            if value.len() < 20 {
                return None;
            }
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&value[4..20]);
            let ip = Ipv6Addr::from(bytes);
            Some(SocketAddr::new(IpAddr::V6(ip), port))
        }
        _ => None,
    }
}

fn parse_xor_mapped_address(value: &[u8], txid: &[u8; 12]) -> Option<SocketAddr> {
    if value.len() < 4 {
        return None;
    }

    let family = value[1];
    let port = u16::from_be_bytes([value[2], value[3]]) ^ (STUN_MAGIC_COOKIE >> 16) as u16;
    let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
    match family {
        0x01 => {
            if value.len() < 8 {
                return None;
            }
            let ip = Ipv4Addr::new(
                value[4] ^ cookie[0],
                value[5] ^ cookie[1],
                value[6] ^ cookie[2],
                value[7] ^ cookie[3],
            );
            Some(SocketAddr::new(IpAddr::V4(ip), port))
        }
        0x02 => {
            if value.len() < 20 {
                return None;
            }
            let mut mask = [0u8; 16];
            mask[..4].copy_from_slice(&cookie);
            mask[4..].copy_from_slice(txid);
            let mut bytes = [0u8; 16];
            for i in 0..16 {
                bytes[i] = value[4 + i] ^ mask[i];
            }
            let ip = Ipv6Addr::from(bytes);
            Some(SocketAddr::new(IpAddr::V6(ip), port))
        }
        _ => None,
    }
}
