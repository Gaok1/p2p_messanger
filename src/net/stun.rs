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
const STUN_SERVERS_V4: [&str; 10] = [
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
    // Removidos: stun.ekiga.net / stun.voxgratia.org (NXDOMAIN em 2025-12)
];

/// Lista de servidores STUN para IPv6.
///
/// Importante: hostnames não são “dedicados” a uma família; muitos resolvem A e AAAA.
/// A seleção real por família acontece em `resolve_stun_server` (filtra por A vs AAAA).
const STUN_SERVERS_V6: [&str; 7] = [
    // Cloudflare
    "stun.cloudflare.com:3478",
    // Google (WebRTC)
    "stun.l.google.com:19302",
    "stun1.l.google.com:19302",
    "stun2.l.google.com:19302",
    "stun3.l.google.com:19302",
    "stun4.l.google.com:19302",
    // Outros
    "stun.nextcloud.com:443",
    // Removidos: stun.ekiga.net / stunserver.org / stun.voxgratia.org (NXDOMAIN em 2025-12)
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
    detect_public_endpoint_on_socket_inner(socket, bind_addr, None)
}

pub fn detect_public_endpoint_on_socket_with_trace(
    socket: &UdpSocket,
    bind_addr: SocketAddr,
    mut trace: impl FnMut(String),
) -> Result<Option<SocketAddr>, String> {
    detect_public_endpoint_on_socket_inner(socket, bind_addr, Some(&mut trace))
}

fn detect_public_endpoint_on_socket_inner(
    socket: &UdpSocket,
    bind_addr: SocketAddr,
    mut trace: Option<&mut dyn FnMut(String)>,
) -> Result<Option<SocketAddr>, String> {
    if stun_disabled() {
        stun_trace(
            &mut trace,
            format!("stun trace: disabled (bind={bind_addr})"),
        );
        return Ok(None);
    }

    let start = Instant::now();
    let mut recv_count = 0usize;
    let mut parsed_count = 0usize;
    stun_trace(
        &mut trace,
        format!(
            "stun trace: start bind={bind_addr} read_timeout={}ms",
            STUN_READ_TIMEOUT.as_millis()
        ),
    );

    let _ = socket.set_read_timeout(Some(STUN_READ_TIMEOUT));
    let servers = stun_server_list(bind_addr);
    if servers.is_empty() {
        return Err("STUN sem servidores".to_string());
    }
    stun_trace(
        &mut trace,
        format!("stun trace: servers={}", servers.join(", ")),
    );

    let mut seed = txid_seed();
    let mut last_err = None;
    let mut resolve_failures = 0usize;
    let mut dns_failures = 0usize;
    let mut family_mismatch_failures = 0usize;

    #[derive(Clone)]
    struct SentRequest {
        server: String,
        addr: SocketAddr,
        txid: [u8; 12],
    }
    let mut sent_requests: Vec<SentRequest> = Vec::new();
    let total_servers = servers.len();
    for server in servers {
        let server_addrs = match resolve_stun_server_addrs(server.as_str(), bind_addr) {
            Ok(addrs) => addrs,
            Err(err) => {
                resolve_failures += 1;
                if is_dns_resolution_error(&err) {
                    dns_failures += 1;
                }
                if err.kind() == io::ErrorKind::AddrNotAvailable {
                    family_mismatch_failures += 1;
                }
                last_err = Some(format_stun_resolve_error(&server, &err));
                stun_trace(
                    &mut trace,
                    format!("stun trace: resolve {server} -> ERROR {err}"),
                );
                continue;
            }
        };
        stun_trace(
            &mut trace,
            format!(
                "stun trace: resolve {server} -> {}",
                fmt_addrs(&server_addrs)
            ),
        );

        let mut sent = false;
        for server_addr in server_addrs {
            let txid = next_transaction_id(&mut seed);
            let request = build_stun_request(txid);
            match socket.send_to(&request, server_addr) {
                Ok(_) => {
                    sent_requests.push(SentRequest {
                        server: server.clone(),
                        addr: server_addr,
                        txid,
                    });
                    sent = true;
                    stun_trace(
                        &mut trace,
                        format!(
                            "stun trace: send server={server} to={server_addr} txid={} bytes={}",
                            fmt_txid(&txid),
                            request.len()
                        ),
                    );
                    break;
                }
                Err(err) => {
                    last_err = Some(format_stun_send_error(
                        &server,
                        server_addr,
                        bind_addr,
                        &err,
                    ));
                    stun_trace(
                        &mut trace,
                        format!(
                            "stun trace: send ERROR to={server_addr} txid={} err={err}",
                            fmt_txid(&txid)
                        ),
                    );
                }
            }
        }
        if !sent {
            continue;
        }
    }

    if sent_requests.is_empty() {
        if resolve_failures == total_servers && dns_failures > 0 {
            return Err(dns_resolution_hint());
        }

        if resolve_failures == total_servers
            && family_mismatch_failures == total_servers
            && total_servers > 0
        {
            let want_label = if bind_addr.is_ipv4() { "IPv4" } else { "IPv6" };
            return Err(format!(
                "nenhum servidor STUN tem endereco {want_label} (todos resolveram apenas a outra familia); tente rebind para a outra familia ou configure `PASTA_P2P_STUN`"
            ));
        }

        return Err(match last_err {
            Some(err) if total_servers > 0 => {
                format!("{err} (servidores testados: {total_servers})")
            }
            Some(err) => err,
            None => "STUN sem servidores".to_string(),
        });
    }

    let overall_ms = STUN_TIMEOUT
        .as_millis()
        .saturating_mul(sent_requests.len() as u128);
    let overall = Duration::from_millis(overall_ms.min(u128::from(u64::MAX)) as u64);
    let deadline = Instant::now() + overall;
    let mut buf = [0u8; 1024];
    while Instant::now() < deadline {
        match socket.recv_from(&mut buf) {
            Ok((size, from)) => {
                recv_count = recv_count.saturating_add(1);
                stun_trace(
                    &mut trace,
                    format!("stun trace: recv from={from} bytes={size}"),
                );
                if let Some((txid, endpoint)) = parse_stun_response(&buf[..size]) {
                    parsed_count = parsed_count.saturating_add(1);
                    let matching = sent_requests.iter().find(|req| req.txid == txid);
                    stun_trace(
                        &mut trace,
                        format!(
                            "stun trace: parsed txid={} endpoint={endpoint} matched={}",
                            fmt_txid(&txid),
                            matching.is_some()
                        ),
                    );
                    if let Some(req) = matching
                        && ((bind_addr.is_ipv4() && endpoint.is_ipv4())
                            || (bind_addr.is_ipv6() && endpoint.is_ipv6()))
                    {
                        stun_trace(
                            &mut trace,
                            format!(
                                "stun trace: SUCCESS server={} to={} endpoint={endpoint} elapsed={}ms",
                                req.server,
                                req.addr,
                                start.elapsed().as_millis()
                            ),
                        );
                        return Ok(Some(endpoint));
                    }

                    stun_trace(
                        &mut trace,
                        "stun trace: ignore (txid desconhecido ou familia diferente)".to_string(),
                    );
                } else {
                    stun_trace(
                        &mut trace,
                        format!(
                            "stun trace: recv unparsed {}",
                            describe_stun_datagram(&buf[..size])
                        ),
                    );
                }
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) => {}
            Err(err) => {
                last_err = Some(format!("falha ao receber STUN: {err}"));
                stun_trace(&mut trace, format!("stun trace: recv ERROR {err}"));
            }
        }
    }

    stun_trace(
        &mut trace,
        format!(
            "stun trace: TIMEOUT elapsed={}ms sent={} recv={} parsed={}",
            start.elapsed().as_millis(),
            sent_requests.len(),
            recv_count,
            parsed_count
        ),
    );
    Err(last_err.unwrap_or_else(|| {
        let mut msg = format!(
            "STUN sem resposta (bind={bind_addr}, enviados: {}, recebidos: {}, parsed: {}, elapsed={}ms)",
            sent_requests.len(),
            recv_count,
            parsed_count,
            start.elapsed().as_millis()
        );
        if !sent_requests.is_empty() {
            msg.push_str(&format!(
                ", destinos: {}",
                fmt_addrs_compact(sent_requests.iter().map(|req| req.addr))
            ));
        }
        msg
    }))
}

pub(crate) fn stun_server_list(bind_addr: SocketAddr) -> Vec<String> {
    if stun_disabled() {
        return Vec::new();
    }

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

fn resolve_stun_server_addrs(server: &str, bind_addr: SocketAddr) -> io::Result<Vec<SocketAddr>> {
    let want_v4 = bind_addr.is_ipv4();
    let resolved: Vec<SocketAddr> = server.to_socket_addrs()?.collect();
    if resolved.is_empty() {
        return Err(io::Error::new(io::ErrorKind::Other, "STUN sem endereco"));
    }

    let selected = select_stun_addrs(resolved.iter().copied(), want_v4);
    if selected.is_empty() {
        let want_label = if want_v4 { "IPv4" } else { "IPv6" };
        return Err(io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            format!(
                "STUN sem endereco {want_label} (resolvido: {})",
                fmt_addrs(&resolved)
            ),
        ));
    }

    Ok(selected)
}

fn stun_disabled() -> bool {
    let Ok(value) = std::env::var("PASTA_P2P_STUN") else {
        return false;
    };

    let trimmed = value.trim();
    if trimmed.is_empty() {
        return true;
    }

    matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "0" | "off" | "false" | "disable" | "disabled" | "none"
    )
}

fn is_dns_resolution_error(err: &io::Error) -> bool {
    if matches!(err.kind(), io::ErrorKind::NotFound) {
        return true;
    }

    // Windows winsock errors:
    // 11001 (WSAHOST_NOT_FOUND), 11002 (WSATRY_AGAIN),
    // 11003 (WSANO_RECOVERY), 11004 (WSANO_DATA).
    matches!(err.raw_os_error(), Some(11001 | 11002 | 11003 | 11004))
}

fn dns_resolution_hint() -> String {
    "falha ao resolver servidores STUN (DNS indisponível/host desconhecido). \
dica: verifique DNS/proxy/firewall ou defina `PASTA_P2P_STUN` com uma lista acessível (hostname ou IP:porta)"
        .to_string()
}

fn format_stun_resolve_error(server: &str, err: &io::Error) -> String {
    let mut msg = match err.kind() {
        io::ErrorKind::AddrNotAvailable => format!("STUN {server} sem endereco compativel: {err}"),
        _ => format!("falha ao resolver STUN {server}: {err}"),
    };

    if err.kind() == io::ErrorKind::AddrNotAvailable {
        msg.push_str(" (dica: esse host provavelmente nao tem AAAA/A para a familia atual; tente outro servidor)");
    } else if is_dns_resolution_error(err) {
        msg.push_str(" (dica: DNS indisponível; ajuste DNS/proxy/firewall ou use `PASTA_P2P_STUN` com servidor acessível)");
    }
    msg
}

pub(crate) fn stun_trace_enabled() -> bool {
    let Ok(value) = std::env::var("PASTA_P2P_STUN_TRACE") else {
        return false;
    };

    let trimmed = value.trim();
    if trimmed.is_empty() {
        return true;
    }

    matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "1" | "on" | "true" | "yes" | "enable" | "enabled" | "trace" | "debug"
    )
}

fn stun_trace(trace: &mut Option<&mut dyn FnMut(String)>, msg: String) {
    if let Some(trace) = trace.as_mut() {
        trace(msg);
    }
}

fn fmt_txid(txid: &[u8; 12]) -> String {
    let mut out = String::with_capacity(24);
    for b in txid {
        use std::fmt::Write;
        let _ = write!(&mut out, "{:02x}", b);
    }
    out
}

fn describe_stun_datagram(data: &[u8]) -> String {
    if data.len() < 4 {
        return format!("len={}", data.len());
    }

    let msg_type = u16::from_be_bytes([data[0], data[1]]);
    let msg_len = u16::from_be_bytes([data[2], data[3]]);
    if data.len() < 8 {
        return format!(
            "len={} msg_type=0x{msg_type:04x} msg_len={msg_len}",
            data.len()
        );
    }

    let cookie = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    if data.len() < 20 {
        return format!(
            "len={} msg_type=0x{msg_type:04x} msg_len={msg_len} cookie=0x{cookie:08x}",
            data.len()
        );
    }

    let mut txid = [0u8; 12];
    txid.copy_from_slice(&data[8..20]);
    format!(
        "len={} msg_type=0x{msg_type:04x} msg_len={msg_len} cookie=0x{cookie:08x} txid={}",
        data.len(),
        fmt_txid(&txid)
    )
}

fn select_stun_addrs(
    addrs: impl IntoIterator<Item = SocketAddr>,
    want_v4: bool,
) -> Vec<SocketAddr> {
    let mut selected = Vec::new();
    for addr in addrs {
        let candidate = match (want_v4, addr) {
            (true, SocketAddr::V4(_)) => Some(addr),
            (true, SocketAddr::V6(v6)) => v6
                .ip()
                .to_ipv4_mapped()
                .map(|v4| SocketAddr::new(IpAddr::V4(v4), v6.port())),
            (false, SocketAddr::V6(v6)) => {
                // Evita endereços IPv4-mapped (ex: ::ffff:1.2.3.4) quando o usuário pediu IPv6,
                // porque em sockets dual-stack isso pode acabar enviando via IPv4.
                if v6.ip().to_ipv4_mapped().is_some() {
                    None
                } else {
                    Some(SocketAddr::V6(v6))
                }
            }
            (false, SocketAddr::V4(_)) => None,
        };

        if let Some(candidate) = candidate {
            if !selected.contains(&candidate) {
                selected.push(candidate);
            }
        }
    }
    selected
}

fn fmt_addrs(addrs: &[SocketAddr]) -> String {
    const MAX: usize = 6;
    let mut out = String::new();
    for (idx, addr) in addrs.iter().take(MAX).enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(&addr.to_string());
    }
    if addrs.len() > MAX {
        out.push_str(", ...");
    }
    out
}

fn fmt_addrs_compact(addrs: impl IntoIterator<Item = SocketAddr>) -> String {
    const MAX: usize = 6;
    let mut groups: Vec<(SocketAddr, usize)> = Vec::new();
    for addr in addrs {
        if let Some((_, count)) = groups.iter_mut().find(|(candidate, _)| candidate == &addr) {
            *count = count.saturating_add(1);
        } else {
            groups.push((addr, 1));
        }
    }

    let mut out = String::new();
    for (idx, (addr, count)) in groups.iter().take(MAX).enumerate() {
        if idx > 0 {
            out.push_str(", ");
        }
        out.push_str(&addr.to_string());
        if *count > 1 {
            out.push_str(&format!(" (x{count})"));
        }
    }
    if groups.len() > MAX {
        out.push_str(", ...");
    }
    out
}

fn format_stun_send_error(
    server: &str,
    server_addr: SocketAddr,
    bind_addr: SocketAddr,
    err: &io::Error,
) -> String {
    let mut msg = format!("falha STUN {server} ({server_addr}): {err}");
    if err.kind() == io::ErrorKind::AddrNotAvailable {
        let hint = if server_addr.is_ipv6() {
            " (EADDRNOTAVAIL: IPv6 sem rota/endereco; tente modo IPv4)"
        } else {
            " (EADDRNOTAVAIL: sem IPv4 valido; verifique interface/rota ou mude pra IPv6)"
        };
        msg.push_str(hint);
    } else if err.kind() == io::ErrorKind::InvalidInput
        && (server_addr.is_ipv4() != bind_addr.is_ipv4())
    {
        msg.push_str(" (familia IP diferente do bind; verifique modo IPv4/IPv6)");
    }
    msg
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

#[cfg(test)]
mod tests {
    use super::select_stun_addrs;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    #[test]
    fn select_stun_addrs_filters_by_family() {
        let input = vec![
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 3478),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 3478),
        ];

        let v4 = select_stun_addrs(input.iter().copied(), true);
        assert_eq!(v4.len(), 1);
        assert!(v4[0].is_ipv4());

        let v6 = select_stun_addrs(input.iter().copied(), false);
        assert_eq!(v6.len(), 1);
        assert!(v6[0].is_ipv6());
    }

    #[test]
    fn select_stun_addrs_accepts_ipv4_mapped_ipv6_for_v4() {
        let mapped = Ipv6Addr::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 1, 2, 3, 4]);
        let input = vec![SocketAddr::new(IpAddr::V6(mapped), 3478)];
        let v4 = select_stun_addrs(input, true);
        assert_eq!(
            v4,
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 3478)]
        );
    }
}
