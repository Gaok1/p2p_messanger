use std::{
    fs::OpenOptions,
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

use arboard::Clipboard;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
        MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use get_if_addrs::{IfAddr, get_if_addrs};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph},
};

use tokio::sync::mpsc as tokio_mpsc;

use crate::net::{NetCommand, NetEvent};

mod components;
use components::{Button, ButtonAction, ClickTarget, ReceivedClickAction, ReceivedClickTarget};

mod state;
pub use state::*;

pub fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

pub fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
    net_tx: tokio_mpsc::UnboundedSender<NetCommand>,
    net_rx: Receiver<NetEvent>,
) -> io::Result<()> {
    let tick_rate = Duration::from_millis(100);

    loop {
        while let Ok(event) = net_rx.try_recv() {
            handle_net_event(app, event);
        }

        if app.needs_clear {
            terminal.clear()?;
            app.needs_clear = false;
        }

        terminal.draw(|frame| draw_ui(frame, app))?;

        if event::poll(tick_rate)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.peer_focus {
                        handle_peer_input_key(app, key.code, &net_tx);
                    } else if app.log_search_focus {
                        handle_log_search_key(app, key.code);
                    } else if app.history_search_focus {
                        handle_history_search_key(app, key.code);
                    } else {
                        match key.code {
                            KeyCode::Up => match app.active_tab {
                                ActiveTab::Downloads => app.scroll_history_up(1),
                                _ => app.scroll_logs_up(1),
                            },
                            KeyCode::Down => match app.active_tab {
                                ActiveTab::Downloads => app.scroll_history_down(1),
                                _ => app.scroll_logs_down(1),
                            },
                            KeyCode::PageUp => match app.active_tab {
                                ActiveTab::Downloads => {
                                    app.scroll_history_up(app.history_view_height.max(1))
                                }
                                _ => app.scroll_logs_up(app.logs_view_height.max(1)),
                            },
                            KeyCode::PageDown => match app.active_tab {
                                ActiveTab::Downloads => {
                                    app.scroll_history_down(app.history_view_height.max(1))
                                }
                                _ => app.scroll_logs_down(app.logs_view_height.max(1)),
                            },
                            KeyCode::Home => match app.active_tab {
                                ActiveTab::Downloads => app.history_scroll = 0,
                                _ => app.scroll_logs_top(),
                            },
                            KeyCode::End => match app.active_tab {
                                ActiveTab::Downloads => {
                                    app.history_scroll = app.max_history_scroll()
                                }
                                _ => app.scroll_logs_bottom(),
                            },
                            KeyCode::Char('/') => {
                                app.peer_focus = false;
                                app.log_search_focus = false;
                                app.history_search_focus = false;
                                if matches!(app.active_tab, ActiveTab::Downloads) {
                                    app.history_search_focus = true;
                                } else {
                                    app.log_search_focus = true;
                                }
                            }
                            KeyCode::Char('f') => {
                                if matches!(app.active_tab, ActiveTab::Downloads) {
                                    app.cycle_history_filter();
                                } else {
                                    app.cycle_log_filter();
                                }
                            }
                            KeyCode::Char('m') => {
                                app.mouse_capture_request = Some(!app.mouse_capture_enabled);
                            }
                            KeyCode::Char('r')
                                if matches!(app.active_tab, ActiveTab::Downloads) =>
                            {
                                app.refresh_history();
                            }
                            KeyCode::Char('1') => app.active_tab = ActiveTab::Transfers,
                            KeyCode::Char('2') => app.active_tab = ActiveTab::Downloads,
                            KeyCode::Char('3') => app.active_tab = ActiveTab::Events,
                            KeyCode::Tab => app.active_tab = app.active_tab.next(),
                            KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                            _ => {}
                        }
                    }
                }
                Event::Mouse(mouse) => handle_mouse_event(app, mouse, &net_tx),
                _ => {}
            }
        }

        if let Some(enabled) = app.mouse_capture_request.take() {
            set_mouse_capture(terminal, enabled)?;
            app.mouse_capture_enabled = enabled;
            if enabled {
                app.push_log(
                    "mouse capturado: cliques habilitados (pressione 'm' para liberar seleção)",
                );
            } else {
                app.push_log(
                    "mouse livre: selecione o texto no terminal (pressione 'm' para voltar)",
                );
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn set_mouse_capture(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    enabled: bool,
) -> io::Result<()> {
    if enabled {
        execute!(terminal.backend_mut(), EnableMouseCapture)?;
    } else {
        execute!(terminal.backend_mut(), DisableMouseCapture)?;
    }
    Ok(())
}

fn handle_net_event(app: &mut AppState, event: NetEvent) {
    match event {
        NetEvent::Log(message) => {
            let lower = message.to_ascii_lowercase();
            if lower.starts_with("stun indisponivel") {
                app.stun_status = Some("stun indisponivel".to_string());
            } else if lower.starts_with("stun erro") {
                app.stun_status = Some("stun falhou".to_string());
            } else if lower.starts_with("endpoint publico") {
                app.stun_status = None;
            }
            app.push_log(message);
        }
        NetEvent::Bound(addr) => {
            app.bind_addr = addr;
            app.local_ip = detect_local_ips(addr.ip());
            app.mode = app
                .mode
                .fallback(app.local_ip.has_v4(), app.local_ip.has_v6());
            app.public_endpoint = None;
            app.stun_status = Some("stun...".to_string());
            app.probe_status = None;
            app.needs_clear = true;
        }
        NetEvent::PublicEndpoint(endpoint) => {
            app.public_endpoint = Some(endpoint);
            app.stun_status = None;
            app.push_log(format!("endpoint publico {endpoint}"));
        }
        NetEvent::ProbeFinished { peer, ok, message } => {
            app.probe_status = Some(ProbeStatus {
                peer,
                message: message.clone(),
                ok: Some(ok),
            });
            let status = if ok { "teste ok" } else { "teste falhou" };
            app.push_log(format!("{status} {peer}: {message}"));
        }
        NetEvent::FileSent { file_id, path } => {
            if let Some(entry) = app
                .selected
                .iter_mut()
                .find(|entry| entry.file_id == Some(file_id) || entry.path == path)
            {
                entry.status = OutgoingStatus::Sent;
                if let Some(size) = entry.size {
                    entry.sent_bytes = size;
                }
                entry.rate_mbps = 0.0;
                entry.rate_last_at = None;
            }
            app.push_log(format!("enviado {}", path.display()));
        }
        NetEvent::FileReceived { file_id, path } => {
            if let Some(entry) = app
                .received
                .iter_mut()
                .find(|entry| entry.file_id == file_id)
            {
                entry.status = IncomingStatus::Done;
                entry.received_bytes = entry.size;
                entry.rate_mbps = 0.0;
                entry.rate_started_at = None;
            }
            app.push_log(format!("recebido {}", path.display()));
            app.push_history_entry(path);
        }
        NetEvent::SessionDir(path) => {
            app.push_log(format!("diretorio da sessao {}", path.display()));
        }
        NetEvent::SendStarted {
            file_id,
            path,
            size,
        } => {
            if let Some(entry) = app.selected.iter_mut().find(|entry| entry.path == path) {
                entry.file_id = Some(file_id);
                entry.size = Some(size);
                entry.sent_bytes = 0;
                entry.rate_mbps = 0.0;
                entry.rate_last_at = Some(Instant::now());
                entry.rate_last_bytes = 0;
                entry.status = OutgoingStatus::Sending;
            }
        }
        NetEvent::SendProgress {
            file_id,
            bytes_sent,
            size,
        } => {
            if let Some(entry) = app
                .selected
                .iter_mut()
                .find(|entry| entry.file_id == Some(file_id))
            {
                entry.size = Some(size);
                let now = Instant::now();
                let new_bytes = bytes_sent.min(size);
                if let Some(last_at) = entry.rate_last_at {
                    let dt = now.duration_since(last_at);
                    if dt >= Duration::from_millis(250) {
                        let delta = new_bytes.saturating_sub(entry.rate_last_bytes);
                        let dt_s = dt.as_secs_f64().max(0.001);
                        entry.rate_mbps = (delta as f64 * 8.0) / dt_s / 1_000_000.0;
                        entry.rate_last_at = Some(now);
                        entry.rate_last_bytes = new_bytes;
                    }
                } else {
                    entry.rate_last_at = Some(now);
                    entry.rate_last_bytes = new_bytes;
                }
                entry.sent_bytes = new_bytes;
                if matches!(entry.status, OutgoingStatus::Pending) {
                    entry.status = OutgoingStatus::Sending;
                }
            }
        }
        NetEvent::SendCanceled { file_id, path } => {
            if let Some(entry) = app
                .selected
                .iter_mut()
                .find(|entry| entry.file_id == Some(file_id) || entry.path == path)
            {
                entry.status = OutgoingStatus::Canceled;
                entry.rate_mbps = 0.0;
                entry.rate_last_at = None;
            }
            app.push_log(format!("cancelado {}", path.display()));
        }
        NetEvent::ReceiveStarted {
            file_id,
            path,
            size,
        } => {
            app.received.push(IncomingEntry {
                path,
                file_id,
                size,
                received_bytes: 0,
                status: IncomingStatus::Receiving,
                rate_mbps: 0.0,
                rate_started_at: Some(Instant::now()),
            });
        }
        NetEvent::ReceiveProgress {
            file_id,
            bytes_received,
            size,
        } => {
            if let Some(entry) = app
                .received
                .iter_mut()
                .find(|entry| entry.file_id == file_id)
            {
                let now = Instant::now();
                let new_bytes = bytes_received.min(size);
                entry.size = size;
                entry.received_bytes = new_bytes;

                if entry.rate_started_at.is_none() {
                    entry.rate_started_at = Some(now);
                }

                if let Some(started_at) = entry.rate_started_at {
                    let elapsed = now.duration_since(started_at).as_secs_f64();
                    if elapsed > 0.0 {
                        entry.rate_mbps = (new_bytes as f64 * 8.0) / elapsed / 1_000_000.0;
                    }
                }
                if matches!(entry.status, IncomingStatus::Done) {
                    entry.status = IncomingStatus::Receiving;
                }
            }
        }
        NetEvent::ReceiveCanceled { file_id, path } => {
            if let Some(entry) = app
                .received
                .iter_mut()
                .find(|entry| entry.file_id == file_id)
            {
                entry.status = IncomingStatus::Canceled;
                entry.rate_mbps = 0.0;
                entry.rate_started_at = None;
            }
            app.push_log(format!("recebimento cancelado {}", path.display()));
        }
        NetEvent::PeerConnecting(addr) => {
            app.connect_status = ConnectStatus::Connecting(addr);
            app.peer_addr = None;
            app.push_log(format!("conectando {addr}"));
        }
        NetEvent::PeerConnected(addr) => {
            app.peer_addr = Some(addr);
            app.connect_status = ConnectStatus::Connected(addr);
            app.push_log(format!("conectado {addr}"));
        }
        NetEvent::PeerDisconnected(addr) => {
            if app.peer_addr == Some(addr) {
                app.peer_addr = None;
            }
            app.connect_status = ConnectStatus::Disconnected(addr);
            app.push_log(format!("desconectado {addr}"));
        }
        NetEvent::PeerTimeout(addr) => {
            if app.peer_addr == Some(addr) {
                app.peer_addr = None;
            }
            app.connect_status = ConnectStatus::Timeout(addr);
            app.push_log(format!("tempo esgotado {addr}"));
        }
        NetEvent::ReceiveFailed { file_id, path } => {
            if let Some(entry) = app
                .received
                .iter_mut()
                .find(|entry| entry.file_id == file_id)
            {
                entry.status = IncomingStatus::Canceled;
                entry.rate_mbps = 0.0;
                entry.rate_started_at = None;
            }
            app.push_log(format!("falha ao receber {}", path.display()));
        }
        NetEvent::PublicEndpointObserved(endpoint) => {
            app.public_endpoint = Some(endpoint);
            app.push_log(format!("endpoint observado {endpoint}"));
        }
    }
}

fn handle_mouse_event(
    app: &mut AppState,
    mouse: MouseEvent,
    net_tx: &tokio_mpsc::UnboundedSender<NetCommand>,
) {
    if !app.mouse_capture_enabled {
        return;
    }

    let position = (mouse.column, mouse.row);

    match mouse.kind {
        MouseEventKind::Moved => {
            app.last_mouse = Some(position);
        }
        MouseEventKind::ScrollUp => {
            if matches!(app.active_tab, ActiveTab::Downloads)
                && point_in_rect(mouse.column, mouse.row, app.history_area)
            {
                app.scroll_history_up(3);
            } else if point_in_rect(mouse.column, mouse.row, app.logs_area) {
                app.scroll_logs_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if matches!(app.active_tab, ActiveTab::Downloads)
                && point_in_rect(mouse.column, mouse.row, app.history_area)
            {
                app.scroll_history_down(3);
            } else if point_in_rect(mouse.column, mouse.row, app.logs_area) {
                app.scroll_logs_down(3);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            app.last_mouse = Some(position);
            app.log_search_focus = false;
            if point_in_rect(mouse.column, mouse.row, app.peer_input_area) {
                app.peer_focus = true;
                return;
            }
            app.peer_focus = false;

            if point_in_rect(mouse.column, mouse.row, app.local_panel_area)
                || point_in_rect(mouse.column, mouse.row, app.public_panel_area)
            {
                app.mouse_capture_request = Some(false);
                return;
            }

            let received_targets = app.received_click_targets.clone();
            if handle_click_targets(&received_targets, position, app, net_tx) {
                return;
            }

            let buttons = app.buttons.clone();
            handle_click_targets(&buttons, position, app, net_tx);
        }
        _ => {}
    }
}

fn handle_click_targets<T: ClickTarget>(
    targets: &[T],
    position: (u16, u16),
    app: &mut AppState,
    net_tx: &tokio_mpsc::UnboundedSender<NetCommand>,
) -> bool {
    targets
        .iter()
        .find(|target| target.contains(position.0, position.1))
        .map(|target| {
            target.on_click(app, net_tx);
        })
        .is_some()
}

fn handle_button_action(
    app: &mut AppState,
    action: ButtonAction,
    net_tx: &tokio_mpsc::UnboundedSender<NetCommand>,
) {
    match action {
        ButtonAction::ConnectPeer => start_connect(app, net_tx),
        ButtonAction::ProbePeer => start_probe(app, net_tx),
        ButtonAction::SelectIpv4 => handle_mode_change(app, IpMode::Ipv4, net_tx),
        ButtonAction::SelectIpv6 => handle_mode_change(app, IpMode::Ipv6, net_tx),
        ButtonAction::ToggleMouseMode => {
            app.mouse_capture_request = Some(!app.mouse_capture_enabled);
        }
        ButtonAction::CopyLocalIp => copy_local_ip(app),
        ButtonAction::CopyPublicEndpoint => copy_public_endpoint(app),
        ButtonAction::PastePeerIp => paste_peer_ip(app),
        ButtonAction::AddFiles => {
            if let Some(files) = pick_files_dialog(app.mouse_capture_enabled) {
                for path in files {
                    app.selected.push(OutgoingEntry {
                        path,
                        file_id: None,
                        size: None,
                        sent_bytes: 0,
                        rate_mbps: 0.0,
                        rate_last_at: None,
                        rate_last_bytes: 0,
                        status: OutgoingStatus::Pending,
                    });
                }
            }
            app.needs_clear = true;
        }
        ButtonAction::SendFiles => {
            if app.selected.is_empty() {
                app.push_log("nenhum arquivo selecionado");
                return;
            }
            let files = app
                .selected
                .iter()
                .filter(|entry| matches!(entry.status, OutgoingStatus::Pending))
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>();
            if files.is_empty() {
                app.push_log("nenhum arquivo pendente");
                return;
            }
            if let Err(err) = net_tx.send(NetCommand::SendFiles(files)) {
                app.push_log(format!("erro ao enviar {err}"));
            }
        }
        ButtonAction::CancelTransfers => {
            if let Err(err) = net_tx.send(NetCommand::CancelTransfers) {
                app.push_log(format!("erro ao cancelar {err}"));
            }
        }
        ButtonAction::Quit => app.should_quit = true,
    }
}

fn handle_mode_change(
    app: &mut AppState,
    mode: IpMode,
    net_tx: &tokio_mpsc::UnboundedSender<NetCommand>,
) {
    if app.mode == mode {
        return;
    }

    app.select_mode(mode);
    app.connect_status = ConnectStatus::Idle;
    app.peer_addr = None;
    app.public_endpoint = None;
    app.stun_status = None;

    let new_bind = bind_for_mode(app.bind_addr, mode);
    if new_bind != app.bind_addr {
        app.bind_addr = new_bind;
        app.local_ip = detect_local_ips(new_bind.ip());
        if let Err(err) = net_tx.send(NetCommand::Rebind(new_bind)) {
            app.push_log(format!("erro ao trocar modo {err}"));
        }
    }
}

fn bind_for_mode(current: SocketAddr, mode: IpMode) -> SocketAddr {
    let port = current.port();
    match mode {
        IpMode::Ipv4 => match current.ip() {
            IpAddr::V4(addr) => SocketAddr::new(IpAddr::V4(addr), port),
            IpAddr::V6(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
        },
        IpMode::Ipv6 => match current.ip() {
            IpAddr::V6(addr) => SocketAddr::new(IpAddr::V6(addr), port),
            IpAddr::V4(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
        },
    }
}

fn handle_peer_input_key(
    app: &mut AppState,
    code: KeyCode,
    net_tx: &tokio_mpsc::UnboundedSender<NetCommand>,
) {
    match code {
        KeyCode::Esc => app.peer_focus = false,
        KeyCode::Enter => {
            app.peer_focus = false;
            start_connect(app, net_tx);
        }
        KeyCode::Backspace => {
            app.peer_input.pop();
            app.peer_label = None;
        }
        KeyCode::Char(c) => {
            if c.is_ascii() && app.peer_input.len() < MAX_PEER_INPUT {
                app.peer_input.push(c);
            }
            app.peer_label = None;
        }
        _ => {}
    }
}

fn handle_log_search_key(app: &mut AppState, key: KeyCode) {
    match key {
        KeyCode::Esc | KeyCode::Enter => {
            app.log_search_focus = false;
        }
        KeyCode::Backspace => {
            if !app.log_query.is_empty() {
                let mut query = app.log_query.clone();
                query.pop();
                app.set_log_query(query);
            }
        }
        KeyCode::Char(c) => {
            if app.log_query.len() < MAX_LOG_QUERY {
                let mut query = app.log_query.clone();
                query.push(c);
                app.set_log_query(query);
            }
        }
        KeyCode::Up => app.scroll_logs_up(1),
        KeyCode::Down => app.scroll_logs_down(1),
        KeyCode::PageUp => app.scroll_logs_up(app.logs_view_height.max(1)),
        KeyCode::PageDown => app.scroll_logs_down(app.logs_view_height.max(1)),
        KeyCode::Home => app.scroll_logs_top(),
        KeyCode::End => app.scroll_logs_bottom(),
        _ => {}
    }
}

fn handle_history_search_key(app: &mut AppState, key: KeyCode) {
    match key {
        KeyCode::Esc | KeyCode::Enter => {
            app.history_search_focus = false;
        }
        KeyCode::Backspace => {
            if !app.history_query.is_empty() {
                let mut query = app.history_query.clone();
                query.pop();
                app.set_history_query(query);
            }
        }
        KeyCode::Char(c) => {
            if app.history_query.len() < MAX_LOG_QUERY {
                let mut query = app.history_query.clone();
                query.push(c);
                app.set_history_query(query);
            }
        }
        KeyCode::Up => app.scroll_history_up(1),
        KeyCode::Down => app.scroll_history_down(1),
        KeyCode::PageUp => app.scroll_history_up(app.history_view_height.max(1)),
        KeyCode::PageDown => app.scroll_history_down(app.history_view_height.max(1)),
        KeyCode::Home => app.history_scroll = 0,
        KeyCode::End => app.history_scroll = app.max_history_scroll(),
        _ => {}
    }
}

fn copy_local_ip(app: &mut AppState) {
    let addr = match app.current_local_ip() {
        Some(addr) => addr,
        None => {
            app.push_log("ip local nao encontrado");
            return;
        }
    };
    let text = match addr {
        IpAddr::V4(v4) => format!("{}:{}", v4, app.bind_addr.port()),
        IpAddr::V6(v6) => format!("[{}]:{}", v6, app.bind_addr.port()),
    };
    match Clipboard::new() {
        Ok(mut clipboard) => match clipboard.set_text(text.clone()) {
            Ok(()) => app.push_log(format!("copiado {text}")),
            Err(err) => app.push_log(format!("erro no clipboard {err}")),
        },
        Err(err) => app.push_log(format!("erro no clipboard {err}")),
    }
}

fn copy_public_endpoint(app: &mut AppState) {
    let endpoint = match app.current_public_endpoint() {
        Some(endpoint) => endpoint,
        None => {
            app.push_log("endpoint publico nao encontrado");
            return;
        }
    };
    let text = endpoint.to_string();
    match Clipboard::new() {
        Ok(mut clipboard) => match clipboard.set_text(text.clone()) {
            Ok(()) => app.push_log(format!("copiado {text}")),
            Err(err) => app.push_log(format!("erro no clipboard {err}")),
        },
        Err(err) => app.push_log(format!("erro no clipboard {err}")),
    }
}

fn paste_peer_ip(app: &mut AppState) {
    let mut clipboard = match Clipboard::new() {
        Ok(clipboard) => clipboard,
        Err(err) => {
            app.push_log(format!("erro no clipboard {err}"));
            return;
        }
    };
    match clipboard.get_text() {
        Ok(text) => {
            let filtered = text
                .trim()
                .chars()
                .filter(|c| c.is_ascii() && !c.is_control())
                .take(MAX_PEER_INPUT)
                .collect::<String>();
            if filtered.is_empty() {
                app.push_log("clipboard vazio");
            } else {
                app.peer_input = filtered;
                app.peer_label = None;
                app.peer_focus = true;
                app.push_log("ip colado");
            }
        }
        Err(err) => app.push_log(format!("erro no clipboard {err}")),
    }
}

fn start_probe(app: &mut AppState, net_tx: &tokio_mpsc::UnboundedSender<NetCommand>) {
    match parse_peer_addr(&app.peer_input) {
        Some(addr) => {
            app.probe_status = Some(ProbeStatus {
                peer: addr,
                message: "testando conectividade...".to_string(),
                ok: None,
            });
            if let Err(err) = net_tx.send(NetCommand::ProbePeer(addr)) {
                app.push_log(format!("erro ao testar conexao {err}"));
            } else {
                app.push_log(format!("teste rapido iniciado para {addr}"));
            }
        }
        None => app.push_log("endereco do parceiro invalido"),
    }
}

fn start_connect(app: &mut AppState, net_tx: &tokio_mpsc::UnboundedSender<NetCommand>) {
    match parse_peer_addr(&app.peer_input) {
        Some(addr) => {
            if let Err(err) = net_tx.send(NetCommand::ConnectPeer(addr)) {
                app.push_log(format!("erro ao enviar conexao {err}"));
            } else {
                app.connect_status = ConnectStatus::Connecting(addr);
                app.peer_addr = None;
            }
        }
        None => app.push_log("endereco do parceiro invalido"),
    }
}

fn parse_peer_addr(input: &str) -> Option<SocketAddr> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(addr) = trimmed.parse() {
        return Some(addr);
    }

    None
}

fn status_color(theme: Theme, status: &ConnectStatus) -> Color {
    match status {
        ConnectStatus::Idle => theme.info,
        ConnectStatus::Connecting(_) => theme.warn,
        ConnectStatus::Connected(_) => theme.ok,
        ConnectStatus::Disconnected(_) => theme.warn,
        ConnectStatus::Timeout(_) => theme.warn,
    }
}

fn nat_tip_text(app: &AppState) -> String {
    let port = app.bind_addr.port();
    match (app.public_endpoint, app.stun_status.as_deref()) {
        (Some(endpoint), _) => {
            if endpoint.port() == port {
                format!("endpoint {endpoint} (UDP {port})")
            } else {
                format!("local {port} → {endpoint}")
            }
        }
        (None, Some(status)) => {
            format!("STUN: {status} · libere UDP {port}")
        }
        (None, None) => format!("STUN pendente · UDP {port}"),
    }
}

fn probe_summary(app: &AppState) -> String {
    match &app.probe_status {
        Some(status) => {
            let prefix = match status.ok {
                Some(true) => "teste OK",
                Some(false) => "teste falhou",
                None => "testando",
            };
            format!("{prefix} {} ({})", status.peer, status.message)
        }
        None => "sem teste rapido (use o botao Testar)".to_string(),
    }
}

fn root_bg(theme: Theme) -> Block<'static> {
    Block::default().style(Style::default().bg(theme.bg))
}

fn title_style(theme: Theme) -> Style {
    Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
}

fn block_with_title(theme: Theme, title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(title, title_style(theme)))
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.panel))
}

fn subtle_block(theme: Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.panel))
}

fn chip(theme: Theme, label: &str, color: Color) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        Style::default()
            .fg(theme.bg)
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )
}

fn button_style(theme: Theme, accent: Color, hover: bool, enabled: bool) -> Style {
    if !enabled {
        return Style::default()
            .fg(theme.muted)
            .bg(theme.panel)
            .add_modifier(Modifier::DIM);
    }

    if hover {
        Style::default()
            .fg(theme.bg)
            .bg(accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(accent)
            .bg(theme.panel)
            .add_modifier(Modifier::BOLD)
    }
}

fn mode_button_style(theme: Theme, active: bool, hover: bool, enabled: bool) -> Style {
    if active {
        return if enabled {
            Style::default()
                .fg(theme.bg)
                .bg(theme.ok)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(theme.bg)
                .bg(theme.warn)
                .add_modifier(Modifier::BOLD)
        };
    }

    if enabled {
        button_style(theme, theme.info, hover, enabled)
    } else {
        Style::default()
            .fg(theme.muted)
            .bg(theme.panel)
            .add_modifier(Modifier::DIM)
    }
}

fn primary_button_style(theme: Theme, hover: bool) -> Style {
    // Botão “principal” do header (conectar)
    if hover {
        Style::default()
            .fg(theme.bg)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.text)
            .bg(Color::Rgb(26, 30, 38))
            .add_modifier(Modifier::BOLD)
    }
}

fn infer_log_level(message: &str) -> LogLevel {
    let lower = message.to_ascii_lowercase();
    if lower.contains("erro") || lower.contains("fail") {
        LogLevel::Error
    } else if lower.contains("cancel")
        || lower.contains("timeout")
        || lower.contains("esgotado")
        || lower.contains("indisponivel")
    {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

fn format_log_line(level: LogLevel, message: &str) -> String {
    format!("[{}] {message}", level.label())
}

fn log_style(theme: Theme, entry: &LogEntry) -> Style {
    match entry.level {
        LogLevel::Error => Style::default().fg(theme.danger),
        LogLevel::Warn => Style::default().fg(theme.warn),
        LogLevel::Info => Style::default().fg(theme.text),
    }
}

fn outgoing_status_color(theme: Theme, status: OutgoingStatus) -> Color {
    match status {
        OutgoingStatus::Pending => theme.info,
        OutgoingStatus::Sending => theme.warn,
        OutgoingStatus::Sent => theme.ok,
        OutgoingStatus::Canceled => theme.danger,
    }
}

fn incoming_status_color(theme: Theme, status: IncomingStatus) -> Color {
    match status {
        IncomingStatus::Receiving => theme.warn,
        IncomingStatus::Done => theme.ok,
        IncomingStatus::Canceled => theme.danger,
    }
}

fn format_bytes(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut idx = 0usize;
    while value >= 1024.0 && idx < units.len() - 1 {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{bytes} {}", units[idx])
    } else {
        format!("{:.1} {}", value, units[idx])
    }
}

fn format_eta(remaining_bytes: u64, rate_mbps: f64) -> Option<String> {
    if rate_mbps <= 0.0 {
        return None;
    }
    let rate_bps = rate_mbps * 1_000_000.0 / 8.0;
    if rate_bps <= 0.0 {
        return None;
    }
    let seconds = (remaining_bytes as f64) / rate_bps;
    if !seconds.is_finite() {
        return None;
    }

    let total_secs = seconds.ceil() as u64;
    let duration = Duration::from_secs(total_secs);
    let hours = duration.as_secs() / 3600;
    let minutes = (duration.as_secs() % 3600) / 60;
    let seconds = duration.as_secs() % 60;

    let text = if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    };

    Some(text)
}

fn progress_bar(bytes: u64, size: u64, width: usize) -> (String, u64) {
    if size == 0 {
        return (format!("[{}]", "░".repeat(width)), 0);
    }
    let ratio = (bytes as f64 / size as f64).clamp(0.0, 1.0);
    let filled = (ratio * width as f64).round() as usize;
    let filled = filled.min(width);
    let percent = (ratio * 100.0).round() as u64;
    let bar = format!(
        "[{}{}]",
        "█".repeat(filled),
        "░".repeat(width.saturating_sub(filled))
    );
    (bar, percent)
}

fn progress_details(current: u64, size: u64, percent: u64, rate_mbps: f64) -> String {
    let safe_current = current.min(size);
    let mut parts = vec![format!(
        "{} / {} ({}%)",
        format_bytes(safe_current),
        format_bytes(size),
        percent
    )];

    if rate_mbps > 0.0 {
        parts.push(format!("{rate_mbps:.1} Mbps"));
    }

    if let Some(eta) = format_eta(size.saturating_sub(safe_current), rate_mbps) {
        parts.push(format!("ETA {eta}"));
    }

    format!(" {} ", parts.join(" · "))
}

fn format_size(size: u64) -> String {
    if size == 0 {
        return "0 B".to_string();
    }

    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}

fn format_modified(modified: Option<std::time::SystemTime>) -> String {
    if let Some(time) = modified {
        if let Ok(duration) = time.elapsed() {
            let hours = duration.as_secs() / 3600;
            if hours < 1 {
                return "agora mesmo".to_string();
            }
            if hours < 24 {
                return format!("há {}h", hours);
            }
            let days = hours / 24;
            return format!("há {}d", days);
        }
    }
    "tempo desconhecido".to_string()
}

fn render_outgoing_item(theme: Theme, entry: &OutgoingEntry) -> ListItem<'static> {
    let sc = outgoing_status_color(theme, entry.status);

    let status = Span::styled(
        format!("{} ", entry.status.label()),
        Style::default().fg(sc).add_modifier(Modifier::BOLD),
    );

    let path = Span::styled(
        entry.path.display().to_string(),
        Style::default().fg(theme.text),
    );

    if let Some(size) = entry.size {
        let (bar, percent) = progress_bar(entry.sent_bytes, size, PROGRESS_BAR_WIDTH);
        let bar_span = Span::styled(bar, Style::default().fg(sc));
        let info = progress_details(entry.sent_bytes, size, percent, entry.rate_mbps);
        let info_span = Span::styled(info, Style::default().fg(theme.muted));
        ListItem::new(Line::from(vec![status, bar_span, info_span, path]))
    } else {
        ListItem::new(Line::from(vec![status, path]))
    }
}

fn render_incoming_info_line(
    theme: Theme,
    entry: &IncomingEntry,
    list_area: Rect,
) -> ListItem<'static> {
    let sc = incoming_status_color(theme, entry.status);

    let filename = entry
        .path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| entry.path.display().to_string());

    // --- status agora é “texto em destaque” (sem fundo) ---
    let status_label = entry.status.label();
    let status_text = Span::styled(
        status_label,
        Style::default().fg(sc).add_modifier(Modifier::BOLD),
    );
    let status_len = status_label.chars().count();

    // Ícone compacto (sem emoji) pra leitura rápida
    let icon = match entry.status {
        IncomingStatus::Receiving => "↓",
        IncomingStatus::Done => "✓",
        IncomingStatus::Canceled => "×",
    };

    // Meta do lado direito: tamanho + taxa (se recebendo)
    let mut meta_parts: Vec<String> = Vec::new();
    if entry.size > 0 {
        meta_parts.push(format_bytes(entry.size));
    }
    if matches!(entry.status, IncomingStatus::Receiving) && entry.rate_mbps > 0.0 {
        meta_parts.push(format!("{:.1} Mbps", entry.rate_mbps));
    }
    let meta_text = meta_parts.join(" · ");
    let meta_len = meta_text.chars().count();

    let list_inner = inner_block_area(list_area);
    let available = list_inner.width as usize;
    if available == 0 {
        return ListItem::new(Line::from(Vec::<Span>::new()));
    }

    // Layout:
    // "│ " + icon + " " + status + " " + name + filler + (meta?) + " │"
    let left_len = 4; // '│' + ' ' + icon + ' '
    let right_len = 2; // ' ' + '│'
    let between_status_and_name = 1;

    let reserved = left_len
        + status_len
        + between_status_and_name
        + right_len
        + if meta_text.is_empty() {
            0
        } else {
            1 + meta_len
        }; // espaço + meta

    let name_area = available.saturating_sub(reserved);
    let name = truncate_keep_end(&filename, name_area);
    let name_len = name.chars().count();
    let filler_len = name_area.saturating_sub(name_len);

    let bar_style = Style::default().fg(sc).add_modifier(Modifier::BOLD);
    let right_bar_style = Style::default().fg(theme.border);

    let left_bar = Span::styled("│", bar_style);
    let icon_span = Span::styled(icon, bar_style);

    let name_span = Span::styled(
        name,
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
    );

    let filler = Span::raw(" ".repeat(filler_len));

    let mut spans = vec![
        left_bar,
        Span::raw(" "),
        icon_span,
        Span::raw(" "),
        status_text,
        Span::raw(" "),
        name_span,
        filler,
    ];

    if !meta_text.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            meta_text,
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::raw(" "));
    spans.push(Span::styled("│", right_bar_style));

    ListItem::new(Line::from(spans))
}

fn render_incoming_action_line(
    theme: Theme,
    entry: &IncomingEntry,
    list_area: Rect,
    row_idx: usize,
    hover: Option<(u16, u16)>,
) -> ListItem<'static> {
    let sc = incoming_status_color(theme, entry.status);

    let show_actions = matches!(entry.status, IncomingStatus::Done) && entry.path.exists();
    let list_inner = inner_block_area(list_area);
    let available = list_inner.width as usize;

    let open_rect = received_open_button_rect(list_area, row_idx);
    let folder_rect = received_folder_button_rect(list_area, row_idx);
    let open_hover = show_actions
        && open_rect.width > 0
        && hover.is_some_and(|(x, y)| point_in_rect(x, y, open_rect));
    let folder_hover = show_actions
        && folder_rect.width > 0
        && hover.is_some_and(|(x, y)| point_in_rect(x, y, folder_rect));

    let reserved_actions = if show_actions && open_rect.width > 0 && folder_rect.width > 0 {
        1 + RECEIVED_OPEN_BUTTON_TEXT.chars().count()
            + 1
            + RECEIVED_FOLDER_BUTTON_TEXT.chars().count()
    } else {
        0
    };

    let prefix_text = "  ";
    let prefix_len = prefix_text.chars().count();

    let (bar, percent) = progress_bar(entry.received_bytes, entry.size, PROGRESS_BAR_WIDTH);
    let bar_len = bar.chars().count();
    let bar_span = Span::styled(bar, Style::default().fg(sc));

    let info_text = progress_details(entry.received_bytes, entry.size, percent, entry.rate_mbps);
    let info_len = info_text.chars().count();
    let info_span = Span::styled(info_text, Style::default().fg(theme.muted));

    let filler_len = available.saturating_sub(prefix_len + bar_len + info_len + reserved_actions);
    let filler = Span::raw(" ".repeat(filler_len));

    let prefix = Span::styled(prefix_text, Style::default().fg(theme.muted));

    let mut line = vec![prefix, bar_span, info_span, filler];
    if show_actions && open_rect.width > 0 && folder_rect.width > 0 {
        let open_bg = if open_hover { theme.accent } else { theme.ok };
        let folder_bg = if folder_hover {
            theme.accent
        } else {
            theme.info
        };
        line.push(Span::raw(" "));
        line.push(Span::styled(
            RECEIVED_OPEN_BUTTON_TEXT,
            Style::default()
                .fg(theme.bg)
                .bg(open_bg)
                .add_modifier(Modifier::BOLD),
        ));
        line.push(Span::raw(" "));
        line.push(Span::styled(
            RECEIVED_FOLDER_BUTTON_TEXT,
            Style::default()
                .fg(theme.bg)
                .bg(folder_bg)
                .add_modifier(Modifier::BOLD),
        ));
    }

    ListItem::new(Line::from(line))
}

fn max_received_entries_for_area(list_area: Rect) -> usize {
    let inner = inner_block_area(list_area);
    (inner.height as usize) / 2
}

fn build_received_view<'a>(
    received: &'a [IncomingEntry],
    max_entries: usize,
) -> Vec<&'a IncomingEntry> {
    received.iter().rev().take(max_entries).collect()
}

fn draw_ui(frame: &mut Frame, app: &mut AppState) {
    let theme = Theme::default_dark();

    // "pinta" o fundo inteiro (evita ficar com blocos soltos)
    frame.render_widget(root_bg(theme), frame.size());

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(24), // header + destaque
            Constraint::Length(3),  // tabs
            Constraint::Min(12),    // content
            Constraint::Length(4),  // buttons
        ])
        .split(frame.size());

    let header_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(20), Constraint::Length(4)])
        .split(outer[0]);

    let (peer_input_area, mut header_buttons) =
        render_connection_panel(frame, header_chunks[0], app, app.last_mouse, theme);
    app.peer_input_area = peer_input_area;

    render_status_overview(frame, header_chunks[1], app, theme);
    render_tab_bar(frame, outer[1], app, theme);

    match app.active_tab {
        ActiveTab::Transfers => render_transfer_tab(frame, outer[2], app, theme),
        ActiveTab::Downloads => render_downloads_tab(frame, outer[2], app, theme),
        ActiveTab::Events => render_events_tab(frame, outer[2], app, theme),
    }

    let mut buttons = Vec::new();
    buttons.append(&mut header_buttons);
    buttons.extend(render_buttons(frame, outer[3], app, theme));
    app.buttons = buttons;
}

fn render_status_overview(frame: &mut Frame, area: Rect, app: &AppState, theme: Theme) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(area);

    let connect = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Vínculo ", Style::default().fg(theme.muted)),
            chip(theme, app.mode_label(), theme.accent),
        ]),
        Line::from(vec![Span::styled(
            app.connect_status.label(),
            Style::default()
                .fg(status_color(theme, &app.connect_status))
                .add_modifier(Modifier::BOLD),
        )]),
    ])
    .block(block_with_title(theme, "Conexão"))
    .alignment(Alignment::Center)
    .style(Style::default().bg(theme.panel));

    let nat = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Portas ", Style::default().fg(theme.muted)),
            Span::styled(app.bind_addr.port().to_string(), title_style(theme)),
        ]),
        Line::from(vec![Span::styled(
            nat_tip_text(app),
            Style::default().fg(theme.text),
        )]),
    ])
    .block(block_with_title(theme, "Rede / NAT"))
    .style(Style::default().bg(theme.panel));

    let probe = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Autodiagnóstico ", Style::default().fg(theme.muted)),
            chip(theme, "F", theme.info),
            Span::raw("  (r para atualizar histórico)"),
        ]),
        Line::from(vec![Span::styled(
            probe_summary(app),
            Style::default().fg(theme.text),
        )]),
    ])
    .block(block_with_title(theme, "Saúde do nó"))
    .style(Style::default().bg(theme.panel));

    frame.render_widget(connect, columns[0]);
    frame.render_widget(nat, columns[1]);
    frame.render_widget(probe, columns[2]);
}

fn render_tab_bar(frame: &mut Frame, area: Rect, app: &AppState, theme: Theme) {
    let tabs = [
        ("1 Transferências", ActiveTab::Transfers),
        ("2 Downloads", ActiveTab::Downloads),
        ("3 Eventos", ActiveTab::Events),
    ];

    let sections = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(area);

    for ((label, tab), chunk) in tabs.into_iter().zip(sections.iter()) {
        let active = app.active_tab == tab;
        let style = if active {
            Style::default()
                .fg(theme.bg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text).bg(theme.panel)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border))
            .style(Style::default().bg(theme.panel));
        let tab_widget = Paragraph::new(label)
            .alignment(Alignment::Center)
            .style(style)
            .block(block);
        frame.render_widget(tab_widget, *chunk);
    }
}

fn render_transfer_tab(frame: &mut Frame, area: Rect, app: &mut AppState, theme: Theme) {
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(area);

    let selected_items = app
        .selected
        .iter()
        .map(|e| render_outgoing_item(theme, e))
        .collect::<Vec<_>>();

    let selected = List::new(selected_items)
        .block(block_with_title(theme, "arquivos (saída)"))
        .style(Style::default().bg(theme.panel));

    frame.render_widget(selected, body[0]);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(body[1]);

    let max_entries = max_received_entries_for_area(right[0]).min(24);
    let received_view = build_received_view(&app.received, max_entries);
    let mut received_items = Vec::new();
    for (entry_idx, entry) in received_view.iter().enumerate() {
        received_items.push(render_incoming_info_line(theme, entry, right[0]));
        received_items.push(render_incoming_action_line(
            theme,
            entry,
            right[0],
            entry_idx * 2 + 1,
            app.last_mouse,
        ));
    }

    let received = List::new(received_items)
        .block(block_with_title(theme, "recebendo (entrada)"))
        .style(Style::default().bg(theme.panel));

    app.received_click_targets = build_received_click_targets(right[0], &received_view);
    frame.render_widget(received, right[0]);

    let logs_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(right[1]);

    let query_text = if app.log_query.is_empty() {
        "(todas)".to_string()
    } else {
        app.log_query.clone()
    };
    let query_color = if app.log_search_focus {
        theme.accent
    } else {
        theme.text
    };
    let filter_line = Line::from(vec![
        Span::styled("Filtro (F): ", Style::default().fg(theme.muted)),
        Span::styled(
            app.log_filter.label(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Busca (/): ", Style::default().fg(theme.muted)),
        Span::styled(query_text, Style::default().fg(query_color)),
    ]);

    let logs_filter = Paragraph::new(filter_line).style(Style::default().bg(theme.panel));
    frame.render_widget(logs_filter, logs_chunks[0]);

    app.logs_area = logs_chunks[1];
    app.set_logs_view_height(logs_chunks[1].height.saturating_sub(2) as usize);

    let visible_logs = app.visible_logs();
    let start = app.logs_scroll.min(app.max_logs_scroll());
    let end = (start + app.logs_view_height).min(visible_logs.len());
    let log_items = visible_logs[start..end]
        .iter()
        .map(|entry| {
            ListItem::new(Span::styled(
                format_log_line(entry.level, &entry.message),
                log_style(theme, entry),
            ))
        })
        .collect::<Vec<_>>();

    let logs = List::new(log_items)
        .block(block_with_title(theme, "eventos"))
        .style(Style::default().bg(theme.panel));

    frame.render_widget(logs, logs_chunks[1]);
}

fn render_events_tab(frame: &mut Frame, area: Rect, app: &mut AppState, theme: Theme) {
    let layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    let summary = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("Peer: ", Style::default().fg(theme.muted)),
            Span::styled(
                app.peer_label
                    .clone()
                    .unwrap_or_else(|| "sem apelido".to_string()),
                title_style(theme),
            ),
        ]),
        Line::from(vec![
            Span::styled("Status: ", Style::default().fg(theme.muted)),
            Span::styled(
                app.connect_status.label(),
                Style::default().fg(status_color(theme, &app.connect_status)),
            ),
        ]),
        Line::from(vec![
            Span::styled("Diagnóstico: ", Style::default().fg(theme.muted)),
            Span::styled(probe_summary(app), Style::default().fg(theme.text)),
        ]),
    ])
    .block(block_with_title(theme, "Contexto da sessão"))
    .style(Style::default().bg(theme.panel));

    frame.render_widget(summary, layout[0]);

    let log_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)])
        .split(layout[1]);

    let query_text = if app.log_query.is_empty() {
        "(todas)".to_string()
    } else {
        app.log_query.clone()
    };
    let query_color = if app.log_search_focus {
        theme.accent
    } else {
        theme.text
    };
    let filter_line = Line::from(vec![
        Span::styled("Filtro (F): ", Style::default().fg(theme.muted)),
        Span::styled(
            app.log_filter.label(),
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Busca (/): ", Style::default().fg(theme.muted)),
        Span::styled(query_text, Style::default().fg(query_color)),
    ]);

    let logs_filter = Paragraph::new(filter_line).style(Style::default().bg(theme.panel));
    frame.render_widget(logs_filter, log_layout[0]);

    app.logs_area = log_layout[1];
    app.set_logs_view_height(log_layout[1].height.saturating_sub(2) as usize);

    let visible_logs = app.visible_logs();
    let start = app.logs_scroll.min(app.max_logs_scroll());
    let end = (start + app.logs_view_height).min(visible_logs.len());
    let log_items = visible_logs[start..end]
        .iter()
        .map(|entry| {
            ListItem::new(Span::styled(
                format_log_line(entry.level, &entry.message),
                log_style(theme, entry),
            ))
        })
        .collect::<Vec<_>>();

    let logs = List::new(log_items)
        .block(block_with_title(theme, "cronologia"))
        .style(Style::default().bg(theme.panel));

    frame.render_widget(logs, log_layout[1]);
}

fn render_downloads_tab(frame: &mut Frame, area: Rect, app: &mut AppState, theme: Theme) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(6),
        ])
        .split(area);

    app.logs_area = Rect::default();
    app.set_logs_view_height(0);

    let search_line = Line::from(vec![
        Span::styled("Buscar (/): ", Style::default().fg(theme.muted)),
        Span::styled(
            if app.history_query.is_empty() {
                "(todos)".to_string()
            } else {
                app.history_query.clone()
            },
            Style::default()
                .fg(if app.history_search_focus {
                    theme.accent
                } else {
                    theme.text
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  Filtro (F): ", Style::default().fg(theme.muted)),
        Span::styled(app.history_filter.label(), title_style(theme)),
        Span::styled("  Atualizar (R)", Style::default().fg(theme.muted)),
    ]);

    let search = Paragraph::new(search_line)
        .block(block_with_title(theme, "histórico de downloads"))
        .style(Style::default().bg(theme.panel));
    frame.render_widget(search, layout[0]);

    let status_text = app
        .history_status
        .clone()
        .unwrap_or_else(|| format!("{} itens registrados", app.history_entries.len()));
    let status = Paragraph::new(status_text)
        .block(subtle_block(theme))
        .style(Style::default().fg(theme.muted).bg(theme.panel));
    frame.render_widget(status, layout[1]);

    let list_area = layout[2];
    app.history_area = list_area;
    let inner = inner_block_area(list_area);
    let item_height = 2_usize;
    app.set_history_view_height((inner.height as usize / item_height).max(1));

    let entries = app.filtered_history();
    let start = app.history_scroll.min(app.max_history_scroll());
    let end = (start + app.history_view_height).min(entries.len());
    let window = &entries[start..end];

    let mut items = Vec::new();
    for entry in window {
        let subtitle = format!(
            "{}  ·  {}  ·  {}",
            format_size(entry.size),
            format_modified(entry.modified),
            entry.kind.label()
        );

        items.push(ListItem::new(vec![
            Line::from(Span::styled(
                truncate_keep_end(&entry.name, inner.width as usize - 4),
                Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(subtitle, Style::default().fg(theme.muted))),
        ]));
    }

    if items.is_empty() {
        items.push(ListItem::new(Span::styled(
            "nenhum download encontrado",
            Style::default().fg(theme.muted),
        )));
    }

    let history_list = List::new(items)
        .block(block_with_title(theme, "downloads concluídos"))
        .style(Style::default().bg(theme.panel));

    frame.render_widget(history_list, list_area);
}

fn render_connection_panel(
    frame: &mut Frame,
    area: Rect,
    app: &mut AppState,
    hover: Option<(u16, u16)>,
    theme: Theme,
) -> (Rect, Vec<Button>) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(4),
        ])
        .split(area);

    // Linha 1: escolha IPv4 / IPv6
    let row_modes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(0),
        ])
        .split(rows[0]);

    // Alternador IPv4 / IPv6
    let ipv4_button = Button {
        label: "IPv4".to_string(),
        area: row_modes[0],
        action: ButtonAction::SelectIpv4,
    };

    let ipv6_button = Button {
        label: "IPv6".to_string(),
        area: row_modes[1],
        action: ButtonAction::SelectIpv6,
    };

    let mode_status_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(12)])
        .split(row_modes[2])[1];

    let mouse_mode_button = Button {
        label: if app.mouse_capture_enabled {
            "Copy[M]".to_string()
        } else {
            "Static[M]".to_string()
        },
        area: mode_status_area,
        action: ButtonAction::ToggleMouseMode,
    };

    let ipv4_hover = hover
        .map(|(x, y)| point_in_rect(x, y, ipv4_button.area))
        .unwrap_or(false);
    let ipv6_hover = hover
        .map(|(x, y)| point_in_rect(x, y, ipv6_button.area))
        .unwrap_or(false);

    let ipv4_enabled = app.mode_supported(IpMode::Ipv4);
    let ipv6_enabled = app.mode_supported(IpMode::Ipv6);

    let ipv4_style = mode_button_style(theme, app.mode == IpMode::Ipv4, ipv4_hover, ipv4_enabled);
    let ipv6_style = mode_button_style(theme, app.mode == IpMode::Ipv6, ipv6_hover, ipv6_enabled);

    let mouse_mode_hover = hover
        .map(|(x, y)| point_in_rect(x, y, mouse_mode_button.area))
        .unwrap_or(false);
    let mouse_mode_active = !app.mouse_capture_enabled;
    let mouse_mode_style = mode_button_style(theme, mouse_mode_active, mouse_mode_hover, true);

    let ipv4_widget = Paragraph::new(ipv4_button.label.as_str())
        .alignment(Alignment::Center)
        .style(ipv4_style)
        .block(subtle_block(theme));
    let ipv6_widget = Paragraph::new(ipv6_button.label.as_str())
        .alignment(Alignment::Center)
        .style(ipv6_style)
        .block(subtle_block(theme));
    let mouse_mode_widget = Paragraph::new(mouse_mode_button.label.as_str())
        .alignment(Alignment::Center)
        .style(mouse_mode_style)
        .block(subtle_block(theme));

    frame.render_widget(ipv4_widget, ipv4_button.area);
    frame.render_widget(ipv6_widget, ipv6_button.area);
    frame.render_widget(mouse_mode_widget, mouse_mode_button.area);

    // Linha 2: input + colar + conectar + testar
    let row_top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(20),
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(12),
        ])
        .split(rows[1]);

    // Linha 2: meu ip + copiar
    let row_mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(12)])
        .split(rows[2]);

    // Linha 4: endpoint publico + copiar
    let row_public = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(12)])
        .split(rows[3]);

    // Linha 5: status (chips)
    let row_status = rows[4];

    // Input
    let input_title = "ip do parceiro";

    let placeholder = if matches!(app.mode, IpMode::Ipv4) {
        "ex: 192.0.2.10:12345"
    } else {
        "ex: [2001:db8::1]:12345"
    };
    let input_text = if app.peer_input.is_empty() {
        placeholder.to_string()
    } else {
        app.peer_input.clone()
    };

    let mut input_style = Style::default().fg(if app.peer_input.is_empty() {
        theme.muted
    } else {
        theme.text
    });

    if app.peer_focus {
        input_style = input_style
            .fg(theme.text)
            .bg(Color::Rgb(30, 35, 45))
            .add_modifier(Modifier::BOLD);
    }

    let input_block =
        block_with_title(theme, input_title).border_style(Style::default().fg(if app.peer_focus {
            theme.accent
        } else {
            theme.border
        }));

    let input = Paragraph::new(input_text)
        .style(input_style)
        .block(input_block);

    frame.render_widget(input, row_top[0]);

    // Colar
    let paste_button = Button {
        label: "Colar".to_string(),
        area: row_top[1],
        action: ButtonAction::PastePeerIp,
    };

    let paste_hover = hover
        .map(|(x, y)| point_in_rect(x, y, paste_button.area))
        .unwrap_or(false);

    let paste_style = button_style(theme, theme.info, paste_hover, true);

    let paste_widget = Paragraph::new(paste_button.label.as_str())
        .alignment(Alignment::Center)
        .style(paste_style)
        .block(subtle_block(theme));

    frame.render_widget(paste_widget, paste_button.area);

    // Conectar
    let connect_label = match &app.connect_status {
        ConnectStatus::Connecting(_) => "Conectando",
        ConnectStatus::Connected(_) => "Reconectar",
        ConnectStatus::Disconnected(_) => "Reconectar",
        ConnectStatus::Timeout(_) => "Tentar",
        ConnectStatus::Idle => "Conectar",
    };

    let connect_button = Button {
        label: connect_label.to_string(),
        area: row_top[2],
        action: ButtonAction::ConnectPeer,
    };

    let connect_hover = hover
        .map(|(x, y)| point_in_rect(x, y, connect_button.area))
        .unwrap_or(false);

    let connect_style = primary_button_style(theme, connect_hover);

    let connect_widget = Paragraph::new(connect_button.label.as_str())
        .alignment(Alignment::Center)
        .style(connect_style)
        .block(subtle_block(theme));

    frame.render_widget(connect_widget, connect_button.area);

    // Testar conexao rapida
    let probe_button = Button {
        label: "Testar".to_string(),
        area: row_top[3],
        action: ButtonAction::ProbePeer,
    };

    let probe_hover = hover
        .map(|(x, y)| point_in_rect(x, y, probe_button.area))
        .unwrap_or(false);
    let probe_enabled = !app.peer_input.trim().is_empty();
    let probe_style = button_style(theme, theme.info, probe_hover, probe_enabled);

    let probe_widget = Paragraph::new(probe_button.label.as_str())
        .alignment(Alignment::Center)
        .style(probe_style)
        .block(subtle_block(theme));

    frame.render_widget(probe_widget, probe_button.area);

    // Meu IP
    let local_text = app
        .current_local_ip()
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| "nao encontrado".to_string());

    let local_line = Line::from(vec![
        Span::styled("meu ip: ", Style::default().fg(theme.muted)),
        Span::styled(
            local_text,
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
    ]);

    let local_panel = Paragraph::new(local_line).block(block_with_title(theme, "local"));
    frame.render_widget(local_panel, row_mid[0]);
    app.local_panel_area = row_mid[0];

    // Copiar
    let copy_button = Button {
        label: "Copiar".to_string(),
        area: row_mid[1],
        action: ButtonAction::CopyLocalIp,
    };

    let copy_hover = hover
        .map(|(x, y)| point_in_rect(x, y, copy_button.area))
        .unwrap_or(false);

    let copy_enabled = app.current_local_ip().is_some();

    let copy_style = button_style(theme, theme.accent, copy_hover, copy_enabled);

    let copy_widget = Paragraph::new(copy_button.label.as_str())
        .alignment(Alignment::Center)
        .style(copy_style)
        .block(subtle_block(theme));

    frame.render_widget(copy_widget, copy_button.area);

    // Endpoint publico (STUN)
    let public_text = match (app.current_public_endpoint(), app.stun_status.as_deref()) {
        (Some(addr), _) => addr.to_string(),
        (None, Some(status)) => status.to_string(),
        (None, None) => "aguardando STUN".to_string(),
    };

    let public_line = Line::from(vec![
        Span::styled("publico: ", Style::default().fg(theme.muted)),
        Span::styled(
            public_text,
            Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        ),
    ]);

    let public_panel = Paragraph::new(public_line).block(block_with_title(theme, "publico"));
    frame.render_widget(public_panel, row_public[0]);
    app.public_panel_area = row_public[0];

    let copy_public_button = Button {
        label: "Copiar pub".to_string(),
        area: row_public[1],
        action: ButtonAction::CopyPublicEndpoint,
    };

    let copy_public_hover = hover
        .map(|(x, y)| point_in_rect(x, y, copy_public_button.area))
        .unwrap_or(false);

    let copy_public_enabled = app.current_public_endpoint().is_some();
    let copy_public_style = button_style(theme, theme.info, copy_public_hover, copy_public_enabled);

    let copy_public_widget = Paragraph::new(copy_public_button.label.as_str())
        .alignment(Alignment::Center)
        .style(copy_public_style)
        .block(subtle_block(theme));

    frame.render_widget(copy_public_widget, copy_public_button.area);

    // Status (chips)
    let peer_text = if let Some(label) = &app.peer_label {
        match app.peer_addr {
            Some(addr) => format!("{label} ({addr})"),
            None if !app.peer_input.trim().is_empty() => {
                format!("{label} ({})", app.peer_input.trim())
            }
            None => label.clone(),
        }
    } else if let Some(addr) = app.peer_addr {
        addr.to_string()
    } else if !app.peer_input.trim().is_empty() {
        app.peer_input.trim().to_string()
    } else {
        "none".to_string()
    };

    let st = status_color(theme, &app.connect_status);

    let status_line = Line::from(vec![
        Span::styled("local ", Style::default().fg(theme.muted)),
        Span::styled(app.bind_addr.to_string(), Style::default().fg(theme.text)),
        Span::styled("   ", Style::default().fg(theme.muted)),
        Span::styled("parceiro ", Style::default().fg(theme.muted)),
        Span::styled(peer_text, Style::default().fg(theme.text)),
        Span::styled("   ", Style::default().fg(theme.muted)),
        chip(theme, &app.connect_status.label(), st),
    ]);

    let status = Paragraph::new(status_line).block(block_with_title(theme, "status"));
    frame.render_widget(status, row_status);

    let row_assist = rows[5];
    let detected_v4 = app
        .local_ip
        .v4
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "nenhum".to_string());
    let detected_v6 = app
        .local_ip
        .v6
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "nenhum".to_string());
    let detected_line = Line::from(vec![
        Span::styled("v4 ", Style::default().fg(theme.muted)),
        Span::styled(format!("{detected_v4}"), Style::default().fg(theme.text)),
        Span::styled(" · ", Style::default().fg(theme.muted)),
        Span::styled("v6 ", Style::default().fg(theme.muted)),
        Span::styled(format!("{detected_v6}"), Style::default().fg(theme.text)),
        Span::styled(" · ", Style::default().fg(theme.muted)),
        Span::styled(
            format!("UDP {}", app.bind_addr.port()),
            Style::default().fg(theme.muted),
        ),
    ]);

    let assist_line = Line::from(vec![
        Span::styled("Ajuda: ", Style::default().fg(theme.muted)),
        Span::styled(nat_tip_text(app), Style::default().fg(theme.text)),
        Span::raw(" "),
        Span::styled("•", Style::default().fg(theme.muted)),
        Span::raw(" "),
        Span::styled(probe_summary(app), Style::default().fg(theme.text)),
    ]);

    let assistant = Paragraph::new(vec![detected_line, assist_line])
        .block(block_with_title(theme, "assistente de rede"))
        .style(Style::default().bg(theme.panel));

    frame.render_widget(assistant, row_assist);

    let buttons = vec![
        ipv4_button,
        ipv6_button,
        mouse_mode_button,
        connect_button,
        probe_button,
        paste_button,
        copy_button,
        copy_public_button,
    ];
    (row_top[0], buttons)
}

fn render_buttons(frame: &mut Frame, area: Rect, app: &AppState, theme: Theme) -> Vec<Button> {
    let row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Min(0),
        ])
        .split(area);

    let has_pending = app
        .selected
        .iter()
        .any(|e| matches!(e.status, OutgoingStatus::Pending));

    let buttons = vec![
        Button {
            label: "Adicionar".to_string(),
            area: row[0],
            action: ButtonAction::AddFiles,
        },
        Button {
            label: "Enviar".to_string(),
            area: row[1],
            action: ButtonAction::SendFiles,
        },
        Button {
            label: "Cancelar".to_string(),
            area: row[2],
            action: ButtonAction::CancelTransfers,
        },
        Button {
            label: "Sair".to_string(),
            area: row[3],
            action: ButtonAction::Quit,
        },
    ];

    for button in &buttons {
        let is_hover = app
            .last_mouse
            .map(|(x, y)| point_in_rect(x, y, button.area))
            .unwrap_or(false);

        let (accent, enabled) = match button.action {
            ButtonAction::AddFiles => (theme.accent, true),
            ButtonAction::SendFiles => (theme.ok, has_pending),
            ButtonAction::CancelTransfers => (theme.warn, true),
            ButtonAction::Quit => (theme.danger, true),
            _ => (theme.accent, true),
        };

        let style = if matches!(button.action, ButtonAction::Quit) {
            // Quit: hover bem claro
            if is_hover {
                Style::default()
                    .fg(theme.bg)
                    .bg(theme.danger)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.danger)
                    .bg(theme.panel)
                    .add_modifier(Modifier::BOLD)
            }
        } else {
            button_style(theme, accent, is_hover, enabled)
        };

        let widget = Paragraph::new(button.label.as_str())
            .alignment(Alignment::Center)
            .style(style)
            .block(subtle_block(theme));

        frame.render_widget(widget, button.area);
    }

    buttons
}

fn point_in_rect(x: u16, y: u16, rect: Rect) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

fn inner_block_area(area: Rect) -> Rect {
    if area.width <= 2 || area.height <= 2 {
        return Rect::default();
    }
    Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width - 2,
        height: area.height - 2,
    }
}

fn received_open_button_rect(list_area: Rect, idx: usize) -> Rect {
    let inner = inner_block_area(list_area);
    let open_w = RECEIVED_OPEN_BUTTON_TEXT.chars().count() as u16;
    let folder_w = RECEIVED_FOLDER_BUTTON_TEXT.chars().count() as u16;
    let total_w = open_w + 1 + folder_w;
    if inner.width <= total_w || inner.height == 0 || idx >= inner.height as usize {
        return Rect::default();
    }
    Rect {
        x: inner.x + (inner.width - total_w),
        y: inner.y + idx as u16,
        width: open_w,
        height: 1,
    }
}

fn received_folder_button_rect(list_area: Rect, idx: usize) -> Rect {
    let inner = inner_block_area(list_area);
    let open_w = RECEIVED_OPEN_BUTTON_TEXT.chars().count() as u16;
    let folder_w = RECEIVED_FOLDER_BUTTON_TEXT.chars().count() as u16;
    let total_w = open_w + 1 + folder_w;
    if inner.width <= total_w || inner.height == 0 || idx >= inner.height as usize {
        return Rect::default();
    }
    Rect {
        x: inner.x + (inner.width - total_w) + open_w + 1,
        y: inner.y + idx as u16,
        width: folder_w,
        height: 1,
    }
}

fn build_received_click_targets(
    list_area: Rect,
    received: &[&IncomingEntry],
) -> Vec<ReceivedClickTarget> {
    let inner = inner_block_area(list_area);
    if inner.height == 0 || inner.width == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    for (entry_idx, entry) in received.iter().copied().enumerate() {
        if !matches!(entry.status, IncomingStatus::Done) {
            continue;
        }
        if !entry.path.exists() {
            continue;
        }

        let action_row = entry_idx * 2 + 1;
        let open_rect = received_open_button_rect(list_area, action_row);
        if open_rect.width > 0 {
            out.push(ReceivedClickTarget {
                area: open_rect,
                path: entry.path.clone(),
                action: ReceivedClickAction::Open,
            });
        }

        let folder_rect = received_folder_button_rect(list_area, action_row);
        if folder_rect.width > 0 {
            out.push(ReceivedClickTarget {
                area: folder_rect,
                path: entry.path.clone(),
                action: ReceivedClickAction::RevealInFolder,
            });
        }
    }
    out
}

fn truncate_keep_end(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max <= 3 {
        return text
            .chars()
            .rev()
            .take(max)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
    }

    let tail_len = max - 3;
    let tail = text
        .chars()
        .rev()
        .take(tail_len)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("...{tail}")
}

fn open_path_in_default_app(path: &std::path::Path) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(path).spawn()?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn()?;
        return Ok(());
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
        return Ok(());
    }
}

fn reveal_path_in_file_manager(path: &std::path::Path) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path)
            .spawn()?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .args(["-R"])
            .arg(path)
            .spawn()?;
        return Ok(());
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let dir = path.parent().unwrap_or(path);
        std::process::Command::new("xdg-open").arg(dir).spawn()?;
        return Ok(());
    }
}

fn pick_files_dialog(mouse_capture_enabled: bool) -> Option<Vec<PathBuf>> {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);
    let files = rfd::FileDialog::new().pick_files();
    if mouse_capture_enabled {
        let _ = execute!(stdout, EnterAlternateScreen, EnableMouseCapture);
    } else {
        let _ = execute!(stdout, EnterAlternateScreen, DisableMouseCapture);
    }
    let _ = enable_raw_mode();
    files
}

fn detect_local_ips(preferred: IpAddr) -> LocalIps {
    let interfaces = match get_if_addrs() {
        Ok(interfaces) => interfaces,
        Err(err) => {
            eprintln!("failed to list interfaces: {err}");
            return LocalIps::default();
        }
    };

    let mut best_v4 = None;
    let mut best_v6_global = None;
    let mut best_v6_local = None;

    for iface in interfaces {
        match iface.addr {
            IfAddr::V4(v4) => {
                let addr = v4.ip;
                if addr.is_loopback() || addr.is_link_local() || addr.is_broadcast() {
                    continue;
                }
                if best_v4.is_none() {
                    best_v4 = Some(IpAddr::V4(addr));
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
                        best_v6_local = Some(IpAddr::V6(addr));
                    }
                    continue;
                }
                if best_v6_global.is_none() {
                    best_v6_global = Some(IpAddr::V6(addr));
                }
            }
        }
    }

    let v6 = match preferred {
        IpAddr::V4(_) => best_v6_global.or(best_v6_local),
        IpAddr::V6(_) => best_v6_global.or(best_v6_local),
    };

    LocalIps {
        v4: best_v4.and_then(|ip| match ip {
            IpAddr::V4(v4) => Some(v4),
            _ => None,
        }),
        v6: v6.and_then(|ip| match ip {
            IpAddr::V6(v6) => Some(v6),
            _ => None,
        }),
    }
}

fn append_log_to_file(message: &str) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_FILE_PATH)?;
    writeln!(file, "{message}")
}
