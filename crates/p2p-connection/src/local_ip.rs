use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use get_if_addrs::{IfAddr, get_if_addrs};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocalIps {
    pub v4: Option<Ipv4Addr>,
    pub v6: Option<Ipv6Addr>,
}

impl LocalIps {
    pub fn has_v4(&self) -> bool {
        self.v4.is_some()
    }

    pub fn has_v6(&self) -> bool {
        self.v6.is_some()
    }
}

pub fn detect_local_ips(_preferred: IpAddr) -> LocalIps {
    let interfaces = match get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(_) => return LocalIps::default(),
    };

    let mut best_v4: Option<Ipv4Addr> = None;
    let mut best_v6_global: Option<Ipv6Addr> = None;
    let mut best_v6_local: Option<Ipv6Addr> = None;

    for iface in interfaces {
        match iface.addr {
            IfAddr::V4(v4) => {
                let addr = v4.ip;
                if addr.is_loopback() || addr.is_link_local() || addr.is_broadcast() {
                    continue;
                }
                if best_v4.is_none() {
                    best_v4 = Some(addr);
                }
            }
            IfAddr::V6(v6) => {
                let addr = v6.ip;
                if addr.is_loopback() || addr.is_multicast() || addr.is_unspecified() {
                    continue;
                }
                if addr.is_unicast_link_local() {
                    continue;
                }
                if addr.is_unique_local() {
                    if best_v6_local.is_none() {
                        best_v6_local = Some(addr);
                    }
                    continue;
                }
                if best_v6_global.is_none() {
                    best_v6_global = Some(addr);
                }
            }
        }
    }

    LocalIps {
        v4: best_v4,
        v6: best_v6_global.or(best_v6_local),
    }
}

pub fn has_global_ipv6() -> bool {
    let interfaces = match get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(_) => return false,
    };

    interfaces.iter().any(|iface| match &iface.addr {
        IfAddr::V6(v6) => is_global_ipv6(v6.ip),
        _ => false,
    })
}

pub fn is_global_ipv6(addr: Ipv6Addr) -> bool {
    !(addr.is_loopback()
        || addr.is_multicast()
        || addr.is_unspecified()
        || addr.is_unicast_link_local()
        || addr.is_unique_local())
}

