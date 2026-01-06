
use std::{
    collections::HashMap,
    fs,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Instant,
};

use ratatui::layout::Rect;
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

use super::{append_log_to_file, detect_local_ips, format_log_line, infer_log_level};
use super::components::{Button, ReceivedClickTarget, ClickTarget};

pub const MAX_PEER_INPUT: usize = 120;
pub const MAX_LOGS: usize = 200;
pub const MAX_LOG_QUERY: usize = 120;
pub const PROGRESS_BAR_WIDTH: usize = 16;
pub const RECEIVED_OPEN_BUTTON_TEXT: &str = " Abrir ";
pub const RECEIVED_FOLDER_BUTTON_TEXT: &str = " Pasta ";
pub const LOG_FILE_PATH: &str = "p2p_logs.txt";
pub const DOWNLOADS_DIR: &str = "received";
const DOWNLOAD_META_FILE: &str = ".download_meta.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveTab {
    Transfers,
    Downloads,
    Events,
}

impl ActiveTab {
    pub fn next(self) -> Self {
        match self {
            ActiveTab::Transfers => ActiveTab::Downloads,
            ActiveTab::Downloads => ActiveTab::Events,
            ActiveTab::Events => ActiveTab::Transfers,
        }
    }
}

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
pub enum HistoryFilter {
    All,
    FromPeer,
    Recent,
    Large,
    Media,
    Docs,
}

impl HistoryFilter {
    pub fn next(self) -> Self {
        match self {
            HistoryFilter::All => HistoryFilter::FromPeer,
            HistoryFilter::FromPeer => HistoryFilter::Recent,
            HistoryFilter::Recent => HistoryFilter::Large,
            HistoryFilter::Large => HistoryFilter::Media,
            HistoryFilter::Media => HistoryFilter::Docs,
            HistoryFilter::Docs => HistoryFilter::All,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HistoryFilter::All => "todos",
            HistoryFilter::FromPeer => "do peer",
            HistoryFilter::Recent => "recentes",
            HistoryFilter::Large => "grandes",
            HistoryFilter::Media => "midia",
            HistoryFilter::Docs => "docs",
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

#[derive(Clone)]
pub struct DownloadEntry {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
    pub kind: DownloadKind,
    pub from_peer: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadKind {
    Document,
    Media,
    Archive,
    Binary,
    Other,
}

impl DownloadKind {
    pub fn label(self) -> &'static str {
        match self {
            DownloadKind::Document => "doc",
            DownloadKind::Media => "midia",
            DownloadKind::Archive => "pacote",
            DownloadKind::Binary => "binario",
            DownloadKind::Other => "outro",
        }
    }
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
        Self {
            bg: Color::Rgb(12, 14, 18),
            panel: Color::Rgb(18, 21, 27),
            border: Color::Rgb(45, 52, 65),
            text: Color::Rgb(230, 233, 240),
            muted: Color::Rgb(150, 158, 172),
            accent: Color::Rgb(99, 179, 237),
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

    pub active_tab: ActiveTab,
    pub mode: IpMode,

    pub connect_status: ConnectStatus,
    pub probe_status: Option<ProbeStatus>,

    pub local_ip: LocalIps,
    pub public_endpoint: Option<SocketAddr>,
    pub stun_status: Option<String>,

    pub mouse_capture_enabled: bool,
    pub mouse_capture_request: Option<bool>,

    // áreas
    pub received_click_targets: Vec<ReceivedClickTarget>,
    pub tab_click_targets: Vec<super::TabClickTarget>,
    pub ui_click_targets: Vec<super::UiClickTarget>,

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

    pub history_area: Rect,
    pub history_view_height: usize,
    pub history_scroll: usize,
    pub history_entries: Vec<DownloadEntry>,
    pub history_filter: HistoryFilter,
    pub history_query: String,
    pub history_query_lower: String,
    pub history_search_focus: bool,
    pub history_status: Option<String>,

    // botões
    pub buttons: Vec<Button>,
    pub peer_input_area: Rect,
    pub last_mouse: Option<(u16, u16)>,

    pub needs_clear: bool,
    pub should_quit: bool,
}

impl AppState {
    pub fn new(bind_addr: SocketAddr, peer_addr: Option<SocketAddr>, peer_label: Option<String>) -> Self {
        let (peer_addr, peer_input, connect_status) = match peer_addr {
            Some(addr) => (None, addr.to_string(), ConnectStatus::Connecting(addr)),
            None => (None, String::new(), ConnectStatus::Idle),
        };

        let local_ip = detect_local_ips(bind_addr.ip());
        let mode = if bind_addr.is_ipv4() { IpMode::Ipv4 } else { IpMode::Ipv6 };
        let mode = mode.fallback(local_ip.has_v4(), local_ip.has_v6());

        let mut app = Self {
            bind_addr,
            peer_addr,
            peer_label,
            peer_input,
            peer_focus: false,

            active_tab: ActiveTab::Transfers,
            mode,

            connect_status,
            probe_status: None,

            local_ip,
            public_endpoint: None,
            stun_status: None,

            mouse_capture_enabled: true,
            mouse_capture_request: None,

            received_click_targets: Vec::new(),
            tab_click_targets: Vec::new(),
            ui_click_targets: Vec::new(),

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

            history_area: Rect::default(),
            history_view_height: 0,
            history_scroll: 0,
            history_entries: Vec::new(),
            history_filter: HistoryFilter::All,
            history_query: String::new(),
            history_query_lower: String::new(),
            history_search_focus: false,
            history_status: None,

            buttons: Vec::new(),
            peer_input_area: Rect::default(),
            last_mouse: None,

            needs_clear: false,
            should_quit: false,
        };

        app.refresh_history();
        app
    }

    pub fn clear_focus(&mut self) {
        self.peer_focus = false;
        self.log_search_focus = false;
        self.history_search_focus = false;
    }

    pub fn set_active_tab(&mut self, tab: ActiveTab) {
        if self.active_tab != tab {
            self.active_tab = tab;
            self.clear_focus();
            self.needs_clear = true;
        }
    }

    pub fn refresh_history(&mut self) {
        let received_dir = resolve_downloads_dir();
        if !received_dir.exists() {
            self.history_entries.clear();
            self.history_status = Some(format!("pasta '{DOWNLOADS_DIR}' não encontrada"));
            return;
        }

        let meta = load_download_meta_index(&received_dir).by_path;
        let mut entries = Vec::new();
        let mut status = None;

        let mut stack = vec![received_dir.clone()];
        while let Some(dir) = stack.pop() {
            match fs::read_dir(&dir) {
                Ok(reader) => {
                    for item in reader.flatten() {
                        let path = item.path();
                        if path.is_dir() {
                            stack.push(path);
                            continue;
                        }

                        if is_download_meta_file(&path) {
                            continue;
                        }

                        let name = path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or_default()
                            .to_string();
                        let metadata = path.metadata().ok();
                        let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
                        let modified = metadata.and_then(|m| m.modified().ok());
                        let kind = classify_download(&path);
                        let from_peer = relative_download_key(&received_dir, &path)
                            .and_then(|key| meta.get(&key).cloned());

                        entries.push(DownloadEntry {
                            path,
                            name,
                            size,
                            modified,
                            kind,
                            from_peer,
                        });
                    }
                }
                Err(err) => {
                    status = Some(format!("erro ao ler {}: {err}", dir.display()));
                }
            }
        }
        entries.sort_by(|a, b| b.modified.cmp(&a.modified));

        self.history_entries = entries;
        self.history_status = status;
        self.history_scroll = 0;
    }

    pub fn push_history_entry(&mut self, path: PathBuf, from_peer: Option<String>) {
        if let Some(peer) = from_peer.as_deref() {
            let downloads_dir = resolve_downloads_dir();
            if let Err(err) = store_download_peer(&downloads_dir, &path, peer) {
                self.push_log(format!("erro ao salvar historico de downloads: {err}"));
            }
        }

        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let metadata = path.metadata().ok();
        let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = metadata.and_then(|m| m.modified().ok());
        let kind = classify_download(&path);

        self.history_entries.retain(|entry| entry.path != path);
        self.history_entries.insert(
            0,
            DownloadEntry {
                path,
                name,
                size,
                modified,
                kind,
                from_peer,
            },
        );
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
        self.visible_logs_len().saturating_sub(self.logs_view_height)
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
        self.logs.iter().filter(|entry| self.log_matches(entry)).count()
    }

    pub fn visible_logs(&self) -> Vec<&LogEntry> {
        self.logs.iter().filter(|entry| self.log_matches(entry)).collect()
    }

    pub fn log_matches(&self, entry: &LogEntry) -> bool {
        self.log_filter.matches(entry.level)
            && (self.log_query_lower.is_empty()
                || entry
                    .message
                    .to_ascii_lowercase()
                    .contains(&self.log_query_lower))
    }

    pub fn filtered_history(&self) -> Vec<&DownloadEntry> {
        let now = std::time::SystemTime::now();
        let recent_cutoff = now
            .checked_sub(std::time::Duration::from_secs(60 * 60 * 24 * 7))
            .unwrap_or(now);
        let peer_key = self
            .peer_addr
            .map(|addr| addr.to_string())
            .or_else(|| self.peer_input.trim().parse::<SocketAddr>().ok().map(|a| a.to_string()));

        self.history_entries
            .iter()
            .filter(|entry| {
                let matches_query = self.history_query_lower.is_empty()
                    || entry
                        .name
                        .to_ascii_lowercase()
                        .contains(&self.history_query_lower);

                let matches_filter = match self.history_filter {
                    HistoryFilter::All => true,
                    HistoryFilter::FromPeer => peer_key
                        .as_ref()
                        .is_some_and(|key| entry.from_peer.as_deref() == Some(key.as_str())),
                    HistoryFilter::Recent => entry.modified.is_some_and(|m| m >= recent_cutoff),
                    HistoryFilter::Large => entry.size >= 50 * 1_000_000,
                    HistoryFilter::Media => matches!(entry.kind, DownloadKind::Media),
                    HistoryFilter::Docs => matches!(entry.kind, DownloadKind::Document),
                };

                matches_query && matches_filter
            })
            .collect()
    }

    pub fn set_history_query(&mut self, query: String) {
        self.history_query = query;
        self.history_query_lower = self.history_query.to_ascii_lowercase();
        self.history_scroll = self.history_scroll.min(self.max_history_scroll());
    }

    pub fn cycle_history_filter(&mut self) {
        self.history_filter = self.history_filter.next();
        self.history_scroll = self.history_scroll.min(self.max_history_scroll());
    }

    pub fn max_history_scroll(&self) -> usize {
        self.filtered_history().len().saturating_sub(self.history_view_height)
    }

    pub fn set_history_view_height(&mut self, height: usize) {
        self.history_view_height = height;
        self.history_scroll = self.history_scroll.min(self.max_history_scroll());
    }

    pub fn scroll_history_up(&mut self, lines: usize) {
        self.history_scroll = self.history_scroll.saturating_sub(lines);
    }

    pub fn scroll_history_down(&mut self, lines: usize) {
        self.history_scroll = (self.history_scroll + lines).min(self.max_history_scroll());
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

    pub fn mode_label(&self) -> &'static str {
        match self.mode {
            IpMode::Ipv4 => "IPv4",
            IpMode::Ipv6 => "IPv6",
        }
    }

    pub fn current_local_ip(&self) -> Option<IpAddr> {
        self.local_ip.current_for_mode(self.mode)
    }

    pub fn current_public_endpoint(&self) -> Option<SocketAddr> {
        self.public_endpoint.and_then(|endpoint| match (self.mode, endpoint) {
            (IpMode::Ipv4, SocketAddr::V4(_)) => Some(endpoint),
            (IpMode::Ipv6, SocketAddr::V6(_)) => Some(endpoint),
            _ => None,
        })
    }
}

fn resolve_downloads_dir() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(DOWNLOADS_DIR)
}

#[derive(Serialize, Deserialize, Default)]
struct DownloadMetaIndex {
    by_path: HashMap<String, String>,
}

fn download_meta_path(downloads_dir: &Path) -> PathBuf {
    downloads_dir.join(DOWNLOAD_META_FILE)
}

fn load_download_meta_index(downloads_dir: &Path) -> DownloadMetaIndex {
    let path = download_meta_path(downloads_dir);
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return DownloadMetaIndex::default(),
    };

    serde_json::from_str(&text).unwrap_or_default()
}

fn save_download_meta_index(downloads_dir: &Path, index: &DownloadMetaIndex) -> io::Result<()> {
    let path = download_meta_path(downloads_dir);
    let tmp_path = downloads_dir.join(format!("{DOWNLOAD_META_FILE}.tmp"));
    let json = serde_json::to_vec_pretty(index)
        .unwrap_or_else(|_| br#"{"by_path":{}}"#.to_vec());

    fs::write(&tmp_path, json)?;
    if path.exists() {
        let _ = fs::remove_file(&path);
    }
    fs::rename(tmp_path, path)?;
    Ok(())
}

fn store_download_peer(downloads_dir: &Path, download_path: &Path, peer: &str) -> io::Result<()> {
    let Some(key) = relative_download_key(downloads_dir, download_path) else {
        return Ok(());
    };

    let mut index = load_download_meta_index(downloads_dir);
    index.by_path.insert(key, peer.to_string());
    save_download_meta_index(downloads_dir, &index)
}

fn is_download_meta_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.starts_with(DOWNLOAD_META_FILE))
}

fn relative_download_key(downloads_dir: &Path, download_path: &Path) -> Option<String> {
    let rel = download_path.strip_prefix(downloads_dir).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

fn classify_download(path: &PathBuf) -> DownloadKind {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if matches!(ext.as_str(), "mp3" | "flac" | "wav" | "mp4" | "mkv" | "mov" | "avi") {
        DownloadKind::Media
    } else if matches!(ext.as_str(), "pdf" | "doc" | "docx" | "txt" | "md" | "ppt" | "pptx") {
        DownloadKind::Document
    } else if matches!(ext.as_str(), "zip" | "tar" | "gz" | "xz" | "7z" | "rar") {
        DownloadKind::Archive
    } else if matches!(ext.as_str(), "exe" | "msi" | "apk" | "bin" | "appimage") {
        DownloadKind::Binary
    } else {
        DownloadKind::Other
    }
}
