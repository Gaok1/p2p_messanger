mod net;
mod ui;

use std::net::SocketAddr;

const DEFAULT_BIND_IPV6: &str = "[::]:0";
const DEFAULT_BIND_IPV4: &str = "0.0.0.0:0";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (bind_addr, peer_addr) = parse_args();
    let (net_tx, net_rx, net_handle) = net::start_network(bind_addr, peer_addr);

    let mut terminal = ui::setup_terminal()?;
    let mut app = ui::AppState::new(bind_addr, peer_addr);
    let run_result = ui::run_app(&mut terminal, &mut app, net_tx.clone(), net_rx);
    ui::restore_terminal(&mut terminal)?;

    let _ = net_tx.send(net::NetCommand::Shutdown);
    let _ = net_handle.join();

    if let Err(err) = run_result {
        eprintln!("error: {err}");
    }

    Ok(())
}

fn parse_args() -> (SocketAddr, Option<SocketAddr>) {
    let mut bind_addr = default_bind_addr();
    let mut peer_addr = None;
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
            _ => {}
        }
    }
    (bind_addr, peer_addr)
}

fn default_bind_addr() -> SocketAddr {
    std::env::var("PASTA_P2P_BIND")
        .ok()
        .and_then(|addr| addr.parse().ok())
        .or_else(|| DEFAULT_BIND_IPV6.parse().ok())
        .or_else(|| DEFAULT_BIND_IPV4.parse().ok())
        .unwrap_or_else(|| "0.0.0.0:0".parse().expect("fallback de bind valido"))
}
