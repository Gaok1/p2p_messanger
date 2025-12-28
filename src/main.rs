mod net;
mod profiles;
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
    let mut app = ui::AppState::new(args.bind_addr, args.peer_addr, args.peer_label);
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
    peer_label: Option<String>,
}

fn parse_args() -> CliArgs {
    let mut bind_addr = default_bind_addr();
    let mut peer_addr = None;
    let mut autotune = net::AutotuneConfig::default();
    let mut peer_label = None;
    let mut peer_key = None;

    let mut profile_to_load = None;
    let mut profile_to_save = None;
    let mut profile_to_delete = None;
    let mut list_profiles = false;
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
            "--profile" => {
                if let Some(name) = args.next() {
                    profile_to_load = Some(name);
                }
            }
            "--profile-save" => {
                if let Some(name) = args.next() {
                    profile_to_save = Some(name);
                }
            }
            "--profile-delete" => {
                if let Some(name) = args.next() {
                    profile_to_delete = Some(name);
                }
            }
            "--profile-alias" => {
                if let Some(alias) = args.next() {
                    peer_label = Some(alias);
                }
            }
            "--peer-key" => {
                if let Some(key) = args.next() {
                    peer_key = Some(key);
                }
            }
            "--list-profiles" => list_profiles = true,
            _ => {}
        }
    }

    let mut store = profiles::ProfileStore::load().unwrap_or_default();

    if list_profiles {
        profiles::print_profiles(&store);
        std::process::exit(0);
    }

    if let Some(name) = profile_to_delete {
        if store.remove(&name) {
            if let Err(err) = store.save() {
                eprintln!("erro ao salvar perfis: {err}");
            } else {
                println!("perfil removido: {name}");
            }
        } else {
            eprintln!("perfil nao encontrado: {name}");
        }
        std::process::exit(0);
    }

    if let Some(name) = profile_to_load {
        match store.find(&name) {
            Some(profile) => {
                peer_addr = Some(profile.peer);
                if peer_label.is_none() {
                    peer_label = profile.alias.clone().or_else(|| Some(profile.name.clone()));
                }
                if peer_key.is_none() {
                    peer_key = profile.key.clone();
                }
            }
            None => eprintln!("perfil nao encontrado: {name}"),
        }
    }

    if let Some(name) = profile_to_save {
        match peer_addr {
            Some(peer) => {
                let profile = profiles::ConnectionProfile {
                    name: name.clone(),
                    peer,
                    alias: peer_label.clone(),
                    key: peer_key.clone(),
                };
                store.upsert(profile);
                if let Err(err) = store.save() {
                    eprintln!("erro ao salvar perfil {name}: {err}");
                } else {
                    println!("perfil salvo: {name} -> {peer}");
                }
            }
            None => {
                eprintln!("--profile-save requer um --peer ou --profile");
                std::process::exit(2);
            }
        }
    }

    CliArgs {
        bind_addr,
        peer_addr,
        autotune,
        peer_label,
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
