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

use super::components::{
    Button, ButtonAction, ClickTarget, ReceivedClickAction, ReceivedClickTarget,
};
use super::{append_log_to_file, detect_local_ips, format_log_line, infer_log_level};

pub const MAX_PEER_INPUT: usize = 120;
pub const MAX_LOGS: usize = 200;
pub const MAX_LOG_QUERY: usize = 120;
pub const PROGRESS_BAR_WIDTH: usize = 16;
pub const RECEIVED_OPEN_BUTTON_TEXT: &str = " Abrir ";
pub const RECEIVED_FOLDER_BUTTON_TEXT: &str = " Pasta ";
pub const LOG_FILE_PATH: &str = "p2p_logs.txt";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERRO",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevelFilter {
    All,
    Info,
    Warn,
    Error,
}

impl LogLevelFilter {
    pub fn next(self) -> Self {
        match self {
            LogLevelFilter::All => LogLevelFilter::Info,
            LogLevelFilter::Info => LogLevelFilter::Warn,
            LogLevelFilter::Warn => LogLevelFilter::Error,
            LogLevelFilter::Error => LogLevelFilter::All,
        }
    }

    pub fn matches(self, level: LogLevel) -> bool {
        match self {
            LogLevelFilter::All => true,
            LogLevelFilter::Info => matches!(level, LogLevel::Info),
            LogLevelFilter::Warn => matches!(level, LogLevel::Warn),
            LogLevelFilter::Error => matches!(level, LogLevel::Error),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LogLevelFilter::All => "todos",
            LogLevelFilter::Info => "info",
            LogLevelFilter::Warn => "warn",
            LogLevelFilter::Error => "erro",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpMode {
    Ipv4,
    Ipv6,
}

impl IpMode {
    pub fn fallback(self, has_v4: bool, has_v6: bool) -> Self {
        match self {
            IpMode::Ipv4 if has_v4 => IpMode::Ipv4,
            IpMode::Ipv6 if has_v6 => IpMode::Ipv6,
            _ => {
                if has_v4 {
                    IpMode::Ipv4
                } else {
                    IpMode::Ipv6
                }
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
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

    pub fn current_for_mode(&self, mode: IpMode) -> Option<IpAddr> {
        match mode {
            IpMode::Ipv4 => self.v4.map(IpAddr::V4),
            IpMode::Ipv6 => self.v6.map(IpAddr::V6),
        }
    }
}

#[derive(Clone)]
pub enum ConnectStatus {
    Idle,
    Connecting(SocketAddr),
    Connected(SocketAddr),
    Disconnected(SocketAddr),
    Timeout(SocketAddr),
}

impl ConnectStatus {
    pub fn label(&self) -> String {
        match self {
            ConnectStatus::Idle => "aguardando".to_string(),
            ConnectStatus::Connecting(addr) => format!("conectando {addr}"),
            ConnectStatus::Connected(addr) => format!("conectado {addr}"),
            ConnectStatus::Disconnected(addr) => format!("desconectado {addr}"),
            ConnectStatus::Timeout(addr) => format!("tempo esgotado {addr}"),
        }
    }
}

#[derive(Clone)]
pub struct ProbeStatus {
    pub peer: SocketAddr,
    pub message: String,
    pub ok: Option<bool>,
}

#[derive(Clone)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
}

#[derive(Clone)]
pub struct OutgoingEntry {
    pub path: PathBuf,
    pub file_id: Option<u64>,
    pub size: Option<u64>,
    pub sent_bytes: u64,
    pub rate_mbps: f64,
    pub rate_last_at: Option<Instant>,
    pub rate_last_bytes: u64,
    pub status: OutgoingStatus,
}

#[derive(Clone, Copy)]
pub enum OutgoingStatus {
    Pending,
    Sending,
    Sent,
    Canceled,
}

impl OutgoingStatus {
    pub fn label(self) -> &'static str {
        match self {
            OutgoingStatus::Pending => "pendente",
            OutgoingStatus::Sending => "enviando",
            OutgoingStatus::Sent => "enviado",
            OutgoingStatus::Canceled => "cancelado",
        }
    }
}

#[derive(Clone)]
pub struct IncomingEntry {
    pub path: PathBuf,
    pub file_id: u64,
    pub size: u64,
    pub received_bytes: u64,
    pub status: IncomingStatus,
    pub rate_mbps: f64,
    pub rate_started_at: Option<Instant>,
}

#[derive(Clone, Copy)]
pub enum IncomingStatus {
    Receiving,
    Done,
    Canceled,
}

impl IncomingStatus {
    pub fn label(self) -> &'static str {
        match self {
            IncomingStatus::Receiving => "recebendo",
            IncomingStatus::Done => "recebido",
            IncomingStatus::Canceled => "cancelado",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub panel: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub info: Color,
    pub ok: Color,
    pub warn: Color,
    pub danger: Color,
}

impl Theme {
    pub fn default_dark() -> Self {
        // Paleta curta e consistente (RGB) pra ficar “aesthetic” sem poluir.
        Self {
            bg: Color::Rgb(12, 14, 18),
            panel: Color::Rgb(18, 21, 27),
            border: Color::Rgb(45, 52, 65),
            text: Color::Rgb(230, 233, 240),
            muted: Color::Rgb(150, 158, 172),
            accent: Color::Rgb(99, 179, 237), // azul/ciano elegante
            info: Color::Rgb(130, 170, 255),
            ok: Color::Rgb(120, 210, 160),
            warn: Color::Rgb(240, 200, 120),
            danger: Color::Rgb(240, 120, 120),
        }
    }
}

pub struct AppState {
    pub bind_addr: SocketAddr,
    pub peer_addr: Option<SocketAddr>,
    pub peer_label: Option<String>,
    pub peer_input: String,
    pub peer_focus: bool,
    pub mode: IpMode,
    pub connect_status: ConnectStatus,
    pub probe_status: Option<ProbeStatus>,
    pub local_ip: LocalIps,
    pub public_endpoint: Option<SocketAddr>,
    pub stun_status: Option<String>,
    pub mouse_capture_enabled: bool,
    pub mouse_capture_request: Option<bool>,
    pub local_panel_area: Rect,
    pub public_panel_area: Rect,
    pub received_click_targets: Vec<ReceivedClickTarget>,
    pub selected: Vec<OutgoingEntry>,
    pub received: Vec<IncomingEntry>,
    pub logs: Vec<LogEntry>,
    pub log_filter: LogLevelFilter,
    pub log_query: String,
    pub log_query_lower: String,
    pub log_search_focus: bool,
    pub logs_scroll: usize,
    pub logs_view_height: usize,
    pub logs_area: Rect,
    pub buttons: Vec<Button>,
    pub peer_input_area: Rect,
    pub last_mouse: Option<(u16, u16)>,
    pub needs_clear: bool,
    pub should_quit: bool,
}

impl AppState {
    pub fn new(
        bind_addr: SocketAddr,
        peer_addr: Option<SocketAddr>,
        peer_label: Option<String>,
    ) -> Self {
        let (peer_addr, peer_input, connect_status) = match peer_addr {
            Some(addr) => (None, addr.to_string(), ConnectStatus::Connecting(addr)),
            None => (None, String::new(), ConnectStatus::Idle),
        };
        let local_ip = detect_local_ips(bind_addr.ip());
        let mode = if bind_addr.is_ipv4() {
            IpMode::Ipv4
        } else {
            IpMode::Ipv6
        };
        let mode = mode.fallback(local_ip.has_v4(), local_ip.has_v6());
        Self {
            bind_addr,
            peer_addr,
            peer_label,
            peer_input,
            peer_focus: false,
            mode,
            connect_status,
            probe_status: None,
            local_ip,
            public_endpoint: None,
            stun_status: None,
            mouse_capture_enabled: true,
            mouse_capture_request: None,
            local_panel_area: Rect::default(),
            public_panel_area: Rect::default(),
            received_click_targets: Vec::new(),
            selected: Vec::new(),
            received: Vec::new(),
            logs: Vec::new(),
            log_filter: LogLevelFilter::All,
            log_query: String::new(),
            log_query_lower: String::new(),
            log_search_focus: false,
            logs_scroll: 0,
            logs_view_height: 0,
            logs_area: Rect::default(),
            buttons: Vec::new(),
            peer_input_area: Rect::default(),
            last_mouse: None,
            needs_clear: false,
            should_quit: false,
        }
    }

    pub fn push_log(&mut self, message: impl Into<String>) {
        let message = message.into();
        let level = infer_log_level(&message);
        self.push_log_with_level(level, message);
    }

    pub fn push_log_with_level(&mut self, level: LogLevel, message: impl Into<String>) {
        let message = message.into();
        let formatted = format_log_line(level, &message);
        let was_at_bottom = self.logs_scroll == self.max_logs_scroll();
        self.logs.push(LogEntry { level, message });
        if self.logs.len() > MAX_LOGS {
            let excess = self.logs.len() - MAX_LOGS;
            self.logs.drain(0..excess);
            self.logs_scroll = self.logs_scroll.saturating_sub(excess);
        }

        self.adjust_logs_scroll(was_at_bottom);

        if let Err(err) = append_log_to_file(&formatted) {
            eprintln!("failed to write log file: {err}");
        }
    }

    pub fn max_logs_scroll(&self) -> usize {
        self.visible_logs_len()
            .saturating_sub(self.logs_view_height)
    }

    pub fn set_logs_view_height(&mut self, height: usize) {
        self.logs_view_height = height;
        self.logs_scroll = self.logs_scroll.min(self.max_logs_scroll());
    }

    pub fn set_log_filter(&mut self, filter: LogLevelFilter) {
        if self.log_filter != filter {
            let stick_to_bottom = self.logs_scroll == self.max_logs_scroll();
            self.log_filter = filter;
            self.adjust_logs_scroll(stick_to_bottom);
        }
    }

    pub fn cycle_log_filter(&mut self) {
        let next = self.log_filter.next();
        self.set_log_filter(next);
    }

    pub fn set_log_query(&mut self, query: String) {
        let stick_to_bottom = self.logs_scroll == self.max_logs_scroll();
        self.log_query = query;
        self.log_query_lower = self.log_query.to_ascii_lowercase();
        self.adjust_logs_scroll(stick_to_bottom);
    }

    pub fn adjust_logs_scroll(&mut self, stick_to_bottom: bool) {
        if stick_to_bottom {
            self.logs_scroll = self.max_logs_scroll();
        } else {
            self.logs_scroll = self.logs_scroll.min(self.max_logs_scroll());
        }
    }

    pub fn visible_logs_len(&self) -> usize {
        self.logs
            .iter()
            .filter(|entry| self.log_matches(entry))
            .count()
    }

    pub fn visible_logs(&self) -> Vec<&LogEntry> {
        self.logs
            .iter()
            .filter(|entry| self.log_matches(entry))
            .collect()
    }

    pub fn log_matches(&self, entry: &LogEntry) -> bool {
        self.log_filter.matches(entry.level)
            && (self.log_query_lower.is_empty()
                || entry
                    .message
                    .to_ascii_lowercase()
                    .contains(&self.log_query_lower))
    }

    pub fn scroll_logs_up(&mut self, lines: usize) {
        self.logs_scroll = self.logs_scroll.saturating_sub(lines);
    }

    pub fn scroll_logs_down(&mut self, lines: usize) {
        self.logs_scroll = (self.logs_scroll + lines).min(self.max_logs_scroll());
    }

    pub fn scroll_logs_top(&mut self) {
        self.logs_scroll = 0;
    }

    pub fn scroll_logs_bottom(&mut self) {
        self.logs_scroll = self.max_logs_scroll();
    }

    pub fn mode_supported(&self, mode: IpMode) -> bool {
        match mode {
            IpMode::Ipv4 => self.local_ip.has_v4(),
            IpMode::Ipv6 => self.local_ip.has_v6(),
        }
    }

    pub fn select_mode(&mut self, mode: IpMode) {
        if self.mode != mode {
            self.mode = mode;
            if !self.mode_supported(mode) {
                let label = match mode {
                    IpMode::Ipv4 => "IPv4",
                    IpMode::Ipv6 => "IPv6",
                };
                self.push_log(format!("modo {label} sem ip local"));
            }
            self.needs_clear = true;
        }
    }

    pub fn current_local_ip(&self) -> Option<IpAddr> {
        self.local_ip.current_for_mode(self.mode)
    }

    pub fn current_public_endpoint(&self) -> Option<SocketAddr> {
        self.public_endpoint
            .and_then(|endpoint| match (self.mode, endpoint) {
                (IpMode::Ipv4, SocketAddr::V4(_)) => Some(endpoint),
                (IpMode::Ipv6, SocketAddr::V6(_)) => Some(endpoint),
                _ => None,
            })
    }
}
