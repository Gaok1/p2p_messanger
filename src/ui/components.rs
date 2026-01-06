use std::path::PathBuf;

use ratatui::layout::Rect;
use tokio::sync::mpsc::UnboundedSender;

use crate::net::NetCommand;

use super::{AppState, open_path_in_default_app, reveal_path_in_file_manager};

#[derive(Clone, Copy)]
pub enum ReceivedClickAction {
    Open,
    RevealInFolder,
}

#[derive(Clone)]
pub struct ReceivedClickTarget {
    pub area: Rect,
    pub path: PathBuf,
    pub action: ReceivedClickAction,
}

#[derive(Clone, Copy)]
pub enum ButtonAction {
    ConnectPeer,
    ProbePeer,
    SelectIpv4,
    SelectIpv6,
    ToggleMouseMode,
    CopyLocalIp,
    CopyPublicEndpoint,
    PastePeerIp,
    AddFiles,
    SendFiles,
    CancelTransfers,
    Quit,
}

#[derive(Clone)]
pub struct Button {
    pub label: String,
    pub area: Rect,
    pub action: ButtonAction,
}

pub trait ClickTarget {
    fn area(&self) -> Rect;
    fn on_click(&self, app: &mut AppState, net_tx: &UnboundedSender<NetCommand>);

    fn contains(&self, x: u16, y: u16) -> bool {
        let rect = self.area();
        x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
    }
}

impl ClickTarget for Button {
    fn area(&self) -> Rect {
        self.area
    }

    fn on_click(&self, app: &mut AppState, net_tx: &UnboundedSender<NetCommand>) {
        super::handle_button_action(app, self.action, net_tx);
    }
}

impl ClickTarget for ReceivedClickTarget {
    fn area(&self) -> Rect {
        self.area
    }

    fn on_click(&self, app: &mut AppState, _net_tx: &UnboundedSender<NetCommand>) {
        let action = self.action;
        let path = self.path.clone();
        if !path.exists() {
            app.push_log(format!("arquivo não encontrado {}", path.display()));
            return;
        }
        let result = match action {
            ReceivedClickAction::Open => open_path_in_default_app(&path),
            ReceivedClickAction::RevealInFolder => reveal_path_in_file_manager(&path),
        };

        match result {
            Ok(()) => match action {
                ReceivedClickAction::Open => app.push_log(format!("abrindo {}", path.display())),
                ReceivedClickAction::RevealInFolder => {
                    app.push_log(format!("abrindo pasta {}", path.display()))
                }
            },
            Err(err) => match action {
                ReceivedClickAction::Open => {
                    app.push_log(format!("erro ao abrir {}: {err}", path.display()))
                }
                ReceivedClickAction::RevealInFolder => {
                    app.push_log(format!("erro ao abrir pasta {}: {err}", path.display()))
                }
            },
        }
    }
}
