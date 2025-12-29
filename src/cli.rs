use std::net::SocketAddr;

use get_if_addrs::{IfAddr, get_if_addrs};

use crate::{net, profiles};

const DEFAULT_BIND_IPV6: &str = "[::]:0";
const DEFAULT_BIND_IPV4: &str = "0.0.0.0:0";

pub struct CliArgs {
    pub bind_addr: SocketAddr,
    pub peer_addr: Option<SocketAddr>,
    pub autotune: net::AutotuneConfig,
    pub peer_label: Option<String>,
}

#[derive(Default)]
struct ProfileActions {
    to_load: Option<String>,
    to_save: Option<String>,
    to_delete: Option<String>,
    list_all: bool,
}

pub fn parse_args() -> CliArgs {
    let mut options = CliOptions::new(default_bind_addr());
    let mut remaining = std::env::args().skip(1);

    while let Some(arg) = remaining.next() {
        handle_argument(&mut options, arg, &mut remaining);
    }

    let mut store = profiles::ProfileStore::load().unwrap_or_default();
    process_profile_requests(&mut store, &mut options);

    CliArgs {
        bind_addr: options.bind_addr,
        peer_addr: options.peer_addr,
        autotune: options.autotune,
        peer_label: options.peer_label,
    }
}

struct CliOptions {
    bind_addr: SocketAddr,
    peer_addr: Option<SocketAddr>,
    autotune: net::AutotuneConfig,
    peer_label: Option<String>,
    peer_key: Option<String>,
    profile_actions: ProfileActions,
}

impl CliOptions {
    fn new(bind_addr: SocketAddr) -> Self {
        Self {
            bind_addr,
            peer_addr: None,
            autotune: net::AutotuneConfig::default(),
            peer_label: None,
            peer_key: None,
            profile_actions: ProfileActions::default(),
        }
    }
}

fn handle_argument(
    options: &mut CliOptions,
    arg: String,
    remaining: &mut impl Iterator<Item = String>,
) {
    match arg.as_str() {
        "--bind" => set_socket_addr(remaining.next(), |addr| options.bind_addr = addr),
        "--peer" => set_socket_addr(remaining.next(), |addr| options.peer_addr = Some(addr)),
        "--autotune-inflight" => set_autotune_flag(remaining.next(), options),
        "--autotune-gain" => set_autotune_gain(remaining.next(), options),
        "--autotune-max-window" => set_autotune_value(remaining.next(), |window| {
            options.autotune.max_window = window
        }),
        "--profile" => options.profile_actions.to_load = remaining.next(),
        "--profile-save" => options.profile_actions.to_save = remaining.next(),
        "--profile-delete" => options.profile_actions.to_delete = remaining.next(),
        "--profile-alias" => options.peer_label = remaining.next(),
        "--peer-key" => options.peer_key = remaining.next(),
        "--list-profiles" => options.profile_actions.list_all = true,
        _ => {}
    }
}

fn set_socket_addr(value: Option<String>, mut assign: impl FnMut(SocketAddr)) {
    if let Some(raw) = value {
        if let Ok(addr) = raw.parse() {
            assign(addr);
        }
    }
}

fn set_autotune_flag(value: Option<String>, options: &mut CliOptions) {
    if let Some(raw) = value {
        options.autotune.enabled = raw != "off" && raw != "0";
    }
}

fn set_autotune_value(value: Option<String>, mut assign: impl FnMut(u64)) {
    if let Some(raw) = value {
        if let Ok(parsed) = raw.parse() {
            assign(parsed);
        }
    }
}

fn set_autotune_gain(value: Option<String>, options: &mut CliOptions) {
    if let Some(raw) = value {
        if let Ok(parsed) = raw.parse() {
            options.autotune.gain = parsed;
        }
    }
}

fn process_profile_requests(store: &mut profiles::ProfileStore, options: &mut CliOptions) {
    handle_list_profiles(store, options.profile_actions.list_all);
    handle_delete_profile(store, options.profile_actions.to_delete.as_deref());
    load_requested_profile(store, options);
    save_requested_profile(store, options);
}

fn handle_list_profiles(store: &profiles::ProfileStore, list_all: bool) {
    if list_all {
        profiles::print_profiles(store);
        std::process::exit(0);
    }
}

fn handle_delete_profile(store: &mut profiles::ProfileStore, name: Option<&str>) {
    if let Some(name) = name {
        if store.remove(name) {
            report_profile_save_result(store.save(), |m| println!("perfil removido: {m}"), name);
        } else {
            eprintln!("perfil nao encontrado: {name}");
        }
        std::process::exit(0);
    }
}

fn load_requested_profile(store: &profiles::ProfileStore, options: &mut CliOptions) {
    if let Some(name) = &options.profile_actions.to_load {
        match store.find(name) {
            Some(profile) => apply_profile(profile, options),
            None => eprintln!("perfil nao encontrado: {name}"),
        }
    }
}

fn save_requested_profile(store: &mut profiles::ProfileStore, options: &mut CliOptions) {
    if let Some(name) = &options.profile_actions.to_save {
        match options.peer_addr {
            Some(peer) => {
                let profile = profiles::ConnectionProfile {
                    name: name.clone(),
                    peer,
                    alias: options.peer_label.clone(),
                    key: options.peer_key.clone(),
                };
                store.upsert(profile);
                report_profile_save_result(
                    store.save(),
                    |m| println!("perfil salvo: {m} -> {peer}"),
                    name,
                );
            }
            None => {
                eprintln!("--profile-save requer um --peer ou --profile");
                std::process::exit(2);
            }
        }
    }
}

fn report_profile_save_result(
    result: std::io::Result<()>,
    on_success: impl FnOnce(&str),
    name: &str,
) {
    match result {
        Ok(()) => on_success(name),
        Err(err) => eprintln!("erro ao salvar perfil {name}: {err}"),
    }
}

fn apply_profile(profile: profiles::ConnectionProfile, options: &mut CliOptions) {
    options.peer_addr = Some(profile.peer);
    if options.peer_label.is_none() {
        options
            .peer_label
            .clone_from(&profile.alias.or_else(|| Some(profile.name.clone())));
    }
    if options.peer_key.is_none() {
        options.peer_key.clone_from(&profile.key);
    }
}

fn default_bind_addr() -> SocketAddr {
    std::env::var("PASTA_P2P_BIND")
        .ok()
        .and_then(|addr| addr.parse().ok())
        .or_else(|| detect_preferred_bind().parse().ok())
        .unwrap_or_else(|| "0.0.0.0:0".parse().expect("fallback de bind valido"))
}

fn detect_preferred_bind() -> &'static str {
    if has_global_ipv6() {
        DEFAULT_BIND_IPV6
    } else {
        DEFAULT_BIND_IPV4
    }
}

fn has_global_ipv6() -> bool {
    let interfaces = match get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(_) => return false,
    };

    interfaces.iter().any(|iface| match &iface.addr {
        IfAddr::V6(v6) => is_global_ipv6(v6.ip),
        _ => false,
    })
}

fn is_global_ipv6(addr: std::net::Ipv6Addr) -> bool {
    !(addr.is_loopback()
        || addr.is_multicast()
        || addr.is_unspecified()
        || addr.is_unicast_link_local()
        || addr.is_unique_local())
}
