//! Modal overlays: help, diagnostics, language picker, confirmations.
//! All input reaches overlays first (modal routing per the UX review).

use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
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
    /// Build to an explicit user-chosen path (system save dialog). The
    /// actor retargets the reserved placeholder; `overwrite` is already
    /// confirmed by the dialog's own prompt.
    Finish {
        out: std::path::PathBuf,
        overwrite: bool,
    },
    NewSession,
    DeletePage(usize),
    DeleteBusy,
    /// Crop page `id` to `rect` (original image pixels; destructive).
    Crop {
        id: crate::session::PageId,
        rect: crate::backend::pdf::CropRect,
    },
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

    /// Plain confirm for the default path (no save-dialog tool installed).
    pub fn finish(path: std::path::PathBuf) -> Self {
        Self {
            kind: ConfirmKind::Finish {
                out: path.clone(),
                overwrite: false,
            },
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

    pub fn crop(id: crate::session::PageId, rect: crate::backend::pdf::CropRect) -> Self {
        Self {
            kind: ConfirmKind::Crop { id, rect },
            title: format!("Crop page {id}?"),
            lines: vec![
                format!(
                    "Keep {}x{} px of the page; removed pixels are",
                    rect.w, rect.h
                ),
                "discarded (rescan restores them).".into(),
            ],
            accept_label: "Enter crop".into(),
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
                    // Non-blocking re-run: queue a request token; the
                    // run_tui select loop picks it up, arms the in-flight
                    // guard and runs the full suite as a background task.
                    // Never await anything here — this handler runs inside
                    // the select loop, so awaiting a response that only
                    // that loop can produce would deadlock the whole UI.
                    if !app.checks_in_flight {
                        let _ = app.diagnostics_request_tx.send(()).await;
                    }
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

pub(crate) async fn accept_confirm(
    app: &mut App,
    kind: &ConfirmKind,
    cmd_tx: &mpsc::Sender<crate::session::Cmd>,
) {
    use crate::session::Cmd;
    match kind {
        ConfirmKind::Quit => app.quit_requested = true,
        ConfirmKind::Finish { out, overwrite } => {
            let _ = cmd_tx
                .send(Cmd::FinishTo {
                    out: out.clone(),
                    overwrite: *overwrite,
                })
                .await;
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
        ConfirmKind::Crop { id, rect } => {
            let _ = cmd_tx
                .send(Cmd::Crop {
                    id: *id,
                    x: rect.x,
                    y: rect.y,
                    w: rect.w,
                    h: rect.h,
                })
                .await;
            // The actor bumps image_gen on completion; the editor stays
            // open and its worker re-decodes from the same (path, gen)
            // reconcile the thumbnails use. The tool closes; a rejected
            // command (guard) surfaces as a status line.
            if let Some(e) = app.editor.as_mut() {
                e.crop = None;
                e.drag = None;
                e.esc_pending = false;
            }
        }
    }
}

/// Handle a mouse event inside an overlay. Returns true when the overlay
/// should stay open; a left click outside the dialog rect closes it
/// (common TUI convention), except Diagnostics where accidental dismissal
/// hurts. Mere pointer movement, releases, drags and scrolling never
/// close — mouse capture delivers a `Moved` event for every pointer step,
/// so without the movement rule any dialog would vanish as soon as the
/// pointer crosses the terminal.
pub fn handle_mouse(app: &mut App, overlay: &mut Overlay, mouse: MouseEvent) -> bool {
    // Down(Left) outside the dialog is the only closing gesture. The
    // catch-all covers Up/Drag (a press inside dragged out must not close
    // on release), Moved, Scroll* (incl. ScrollLeft/ScrollRight) and
    // right/middle buttons.
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return true;
    }
    // Missing rect (no draw yet) or degenerate rect (tiny terminal, nothing
    // visible to click on): never close.
    let Some(rect) = app.overlay_rect.filter(|r| r.width > 0 && r.height > 0) else {
        return true;
    };
    let inside = mouse.column >= rect.x
        && mouse.column < rect.x + rect.width
        && mouse.row >= rect.y
        && mouse.row < rect.y + rect.height;
    inside || matches!(overlay, Overlay::Diagnostics)
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::KeyModifiers;
    use ratatui::layout::Rect;

    use super::*;

    /// App with throwaway channels (mirrors app.rs's test_app()).
    fn test_app() -> App {
        let (diag_tx, _diag_rx) = mpsc::channel(4);
        let (finish_tx, _finish_rx) = mpsc::channel(1);
        App::new(crate::config::Config::default(), diag_tx, finish_tx)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Dialog rect at (10, 5) sized 20x6, set the way ui::draw would.
    fn app_with_rect() -> App {
        let mut app = test_app();
        app.overlay_rect = Some(Rect::new(10, 5, 20, 6));
        app
    }

    #[test]
    fn pointer_movement_never_closes() {
        // The reported bug: with mouse capture on, every Moved event was
        // routed here and dropped the overlay. Movement must keep it.
        let mut app = app_with_rect();
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Down(MouseButton::Middle),
        ] {
            assert!(
                handle_mouse(&mut app, &mut Overlay::Help, mouse(kind, 0, 0)),
                "{kind:?} must not close the overlay"
            );
        }
    }

    #[test]
    fn left_click_inside_keeps() {
        let mut app = app_with_rect();
        // Every corner + center of Rect(10, 5, 20, 6): x 10..29, y 5..10.
        for (col, row) in [(10, 5), (29, 5), (10, 10), (29, 10), (19, 7)] {
            assert!(handle_mouse(
                &mut app,
                &mut Overlay::Confirm(Confirm::quit()),
                mouse(MouseEventKind::Down(MouseButton::Left), col, row)
            ));
        }
    }

    #[test]
    fn left_click_outside_closes_except_diagnostics() {
        let mut app = app_with_rect();
        // Just outside the right edge and below the bottom edge.
        for col_row in [(31, 7), (19, 12), (5, 5)] {
            let click = mouse(
                MouseEventKind::Down(MouseButton::Left),
                col_row.0,
                col_row.1,
            );
            assert!(!handle_mouse(&mut app, &mut Overlay::Help, click));
            assert!(!handle_mouse(
                &mut app,
                &mut Overlay::Confirm(Confirm::quit()),
                click
            ));
            assert!(!handle_mouse(
                &mut app,
                &mut Overlay::LangPicker(LangPicker::new("eng".into())),
                click
            ));
            // Diagnostics never mouse-closes (accidental dismissal hurts).
            assert!(handle_mouse(&mut app, &mut Overlay::Diagnostics, click));
        }
    }

    #[test]
    fn missing_or_degenerate_rect_never_closes() {
        // No draw yet (rect None): nothing to click on — keep.
        let mut app = test_app();
        assert!(handle_mouse(
            &mut app,
            &mut Overlay::Help,
            mouse(MouseEventKind::Down(MouseButton::Left), 0, 0)
        ));
        // Degenerate rect (tiny terminal): invisible dialog — keep.
        for degenerate in [Rect::new(0, 0, 0, 6), Rect::new(0, 0, 20, 0)] {
            app.overlay_rect = Some(degenerate);
            assert!(handle_mouse(
                &mut app,
                &mut Overlay::Help,
                mouse(MouseEventKind::Down(MouseButton::Left), 0, 0)
            ));
        }
    }
}
