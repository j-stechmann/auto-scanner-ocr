//! Modal overlays: help, diagnostics, language picker, confirmations.
//! All input reaches overlays first (modal routing per the UX review).

use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use tokio::sync::mpsc;

use super::app::App;

#[derive(Debug)]
pub enum Overlay {
    Help,
    Diagnostics,
    LangPicker(LangPicker),
    Confirm(Confirm),
}

/// What a confirm overlay resolves to when accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmKind {
    Quit,
    Finish,
    NewSession,
    DeletePage(usize),
    DeleteBusy,
}

#[derive(Debug)]
pub struct Confirm {
    pub kind: ConfirmKind,
    pub title: String,
    pub lines: Vec<String>,
    pub accept_label: String,
}

impl Confirm {
    pub fn quit() -> Self {
        Self {
            kind: ConfirmKind::Quit,
            title: "Quit?".into(),
            lines: vec![
                "Pages exist that are not part of a built PDF.".into(),
                "They will be lost.".into(),
            ],
            accept_label: "y quit anyway".into(),
        }
    }

    pub fn finish(path: std::path::PathBuf) -> Self {
        Self {
            kind: ConfirmKind::Finish,
            title: "Build searchable PDF?".into(),
            lines: vec![
                format!("Output: {}", path.display()),
                "OCR runs over all pages; this can take a while.".into(),
            ],
            accept_label: "Enter build".into(),
        }
    }

    pub fn new_session() -> Self {
        Self {
            kind: ConfirmKind::NewSession,
            title: "Start a new session?".into(),
            lines: vec!["All scanned pages of this session will be dropped.".into()],
            accept_label: "y new session".into(),
        }
    }

    pub fn delete_page(id: usize) -> Self {
        Self {
            kind: ConfirmKind::DeletePage(id),
            title: format!("Delete page {}?", id),
            lines: vec!["The scanned image will be removed.".into()],
            accept_label: "y delete".into(),
        }
    }

    pub fn delete_busy() -> Self {
        Self {
            kind: ConfirmKind::DeleteBusy,
            title: "Delete page while it is processing?".into(),
            lines: vec!["The running job will be cancelled.".into()],
            accept_label: "y delete".into(),
        }
    }
}

#[derive(Debug)]
pub struct LangPicker {
    pub available: Vec<String>,
    pub selected: Vec<String>,
    pub cursor: usize,
    pub loading: bool,
}

impl LangPicker {
    pub fn new(current: String) -> Self {
        let selected: Vec<String> = current.split('+').map(String::from).collect();
        Self {
            available: Vec::new(),
            selected,
            cursor: 0,
            loading: true,
        }
    }

    pub fn set_available(&mut self, mut langs: Vec<String>) {
        langs.sort();
        // Ensure currently selected langs exist in the list view.
        for s in &self.selected {
            if !langs.contains(s) {
                langs.push(s.clone());
            }
        }
        langs.sort();
        self.available = langs;
        self.loading = false;
        // Place the cursor on the first selected lang if possible.
        if let Some(idx) = self
            .available
            .iter()
            .position(|l| self.selected.first().is_some_and(|s| s == l))
        {
            self.cursor = idx;
        }
    }

    pub fn result(&self) -> String {
        self.selected.join("+")
    }

    fn toggle(&mut self) {
        if let Some(lang) = self.available.get(self.cursor).cloned() {
            if let Some(pos) = self.selected.iter().position(|s| *s == lang) {
                self.selected.remove(pos);
            } else {
                self.selected.push(lang);
            }
        }
    }
}

/// Handle a key press inside an overlay. Returns false when the overlay
/// should close; accepting confirmations dispatches actions.
pub async fn handle_key(
    app: &mut App,
    overlay: &mut Overlay,
    key: KeyEvent,
    cmd_tx: &mpsc::Sender<crate::session::Cmd>,
) -> bool {
    use KeyCode::*;
    let keep = match overlay {
        Overlay::Help => !matches!(key.code, Esc | Char('?') | Char('q')),
        Overlay::Diagnostics => {
            match key.code {
                Esc | Char('!') | Char('q') => false,
                Char('r') | Char('R') => {
                    // Re-run checks inline; update the app's report copy.
                    let report = crate::check::run_checks(&app.cfg).await;
                    let ok = report.ok();
                    app.report = Some(report);
                    app.set_status(if ok {
                        "all checks passed"
                    } else {
                        "problems found"
                    });
                    true
                }
                _ => true,
            }
        }
        Overlay::LangPicker(picker) => match key.code {
            Esc | Char('q') => false,
            Up | Char('k') => {
                if picker.cursor > 0 {
                    picker.cursor -= 1;
                }
                true
            }
            Down | Char('j') => {
                if picker.cursor + 1 < picker.available.len() {
                    picker.cursor += 1;
                }
                true
            }
            Char(' ') => {
                picker.toggle();
                true
            }
            Enter => {
                if !picker.selected.is_empty() {
                    app.cfg.langs = picker.result();
                    app.set_status(format!("langs: {}", app.cfg.langs));
                }
                false
            }
            _ => true,
        },
        Overlay::Confirm(confirm) => match key.code {
            Esc | Char('q') | Char('n') => false,
            Char('y') | Char('Y') | Enter => {
                let kind = confirm.kind.clone();
                accept_confirm(app, &kind, cmd_tx).await;
                false
            }
            _ => true,
        },
    };
    keep
}

async fn accept_confirm(
    app: &mut App,
    kind: &ConfirmKind,
    cmd_tx: &mpsc::Sender<crate::session::Cmd>,
) {
    use crate::session::Cmd;
    match kind {
        ConfirmKind::Quit => app.quit_requested = true,
        ConfirmKind::Finish => {
            let _ = cmd_tx.send(Cmd::Finish).await;
        }
        ConfirmKind::NewSession => {
            let _ = cmd_tx.send(Cmd::NewSession).await;
        }
        ConfirmKind::DeletePage(idx) => {
            if let Some(p) = app.pages.get(*idx) {
                let _ = cmd_tx.send(Cmd::Delete(p.id)).await;
            }
        }
        ConfirmKind::DeleteBusy => {}
    }
}

pub fn handle_mouse(app: &mut App, overlay: &mut Overlay, mouse: MouseEvent) {
    // Overlays swallow clicks; clicking outside closes them (common TUI
    // convention), except diagnostics where accidental dismissal hurts.
    let _ = (app, overlay);
    let _ = mouse;
}
