mod net;
mod ui;

use std::net::SocketAddr;

use get_if_addrs::{IfAddr, get_if_addrs};

const DEFAULT_BIND_IPV6: &str = "[::]:0";
const DEFAULT_BIND_IPV4: &str = "0.0.0.0:0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();
    let (net_tx, net_rx, net_handle) =
        net::start_network(args.bind_addr, args.peer_addr, args.autotune);

    let mut terminal = ui::setup_terminal()?;
    let mut app = ui::AppState::new(args.bind_addr, args.peer_addr);
    let run_result = ui::run_app(&mut terminal, &mut app, net_tx.clone(), net_rx);
    ui::restore_terminal(&mut terminal)?;

    let _ = net_tx.send(net::NetCommand::Shutdown);
    let _ = net_handle.join();

    if let Err(err) = run_result {
        eprintln!("error: {err}");
    }

    Ok(())
}

struct CliArgs {
    bind_addr: SocketAddr,
    peer_addr: Option<SocketAddr>,
    autotune: net::AutotuneConfig,
}

fn parse_args() -> CliArgs {
    let mut bind_addr = default_bind_addr();
    let mut peer_addr = None;
    let mut autotune = net::AutotuneConfig::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                if let Some(addr) = args.next() {
                    if let Ok(parsed) = addr.parse() {
                        bind_addr = parsed;
                    }
                }
            }
            "--peer" => {
                if let Some(addr) = args.next() {
                    if let Ok(parsed) = addr.parse() {
                        peer_addr = Some(parsed);
                    }
                }
            }
            "--autotune-inflight" => {
                if let Some(value) = args.next() {
                    autotune.enabled = value != "off" && value != "0";
                }
            }
            "--autotune-gain" => {
                if let Some(value) = args.next() {
                    if let Ok(parsed) = value.parse() {
                        autotune.gain = parsed;
                    }
                }
            }
            "--autotune-max-window" => {
                if let Some(value) = args.next() {
                    if let Ok(parsed) = value.parse() {
                        autotune.max_window = parsed;
                    }
                }
            }
            _ => {}
        }
    }
    CliArgs {
        bind_addr,
        peer_addr,
        autotune,
    }
}

fn default_bind_addr() -> SocketAddr {
    std::env::var("PASTA_P2P_BIND")
        .ok()
        .and_then(|addr| addr.parse().ok())
        .or_else(|| {
            let default = if has_global_ipv6() {
                DEFAULT_BIND_IPV6
            } else {
                DEFAULT_BIND_IPV4
            };
            default.parse().ok()
        })
        .unwrap_or_else(|| "0.0.0.0:0".parse().expect("fallback de bind valido"))
}

fn has_global_ipv6() -> bool {
    let interfaces = match get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(_) => return false,
    };

    interfaces.iter().any(|iface| match &iface.addr {
        IfAddr::V6(v6) => {
            let addr = v6.ip;
            !(addr.is_loopback()
                || addr.is_multicast()
                || addr.is_unspecified()
                || addr.is_unicast_link_local()
                || addr.is_unique_local())
        }
        _ => false,
    })
}
