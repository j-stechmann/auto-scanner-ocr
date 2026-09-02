//! TUI application state and event loop.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{
    Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::backend::pdf::BuildOutcome;
use crate::check::Report;
use crate::config::Config;
use crate::notify::{self, Urgency};
use crate::session::{self, Busy, Event, PageStatus, PageView, SessionMeta};

use super::overlays::{self, Confirm, Overlay};
use super::preview::PreviewWorker;
use super::ui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Sidebar,
    Preview,
    Text,
}

impl Pane {
    pub const ALL: [Pane; 3] = [Pane::Sidebar, Pane::Preview, Pane::Text];

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let i = Self::ALL.iter().position(|p| *p == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub fn title(self) -> &'static str {
        match self {
            Pane::Sidebar => "Pages",
            Pane::Preview => "Preview",
            Pane::Text => "Extracted text",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub dpi: u16,
    pub mode: String,
}

/// Status lines kept in the status pane (newest at bottom).
pub struct App {
    pub cfg: Config,
    pub settings: Settings,
    pub pages: Vec<PageView>,
    pub selected: usize,
    pub focus: Pane,
    pub status_lines: Vec<String>,
    pub status_scroll: usize,
    pub text_scroll: usize,
    pub meta: Option<SessionMeta>,
    pub device_label: String,
    pub overlay: Option<Overlay>,
    pub quit_requested: bool,
    pub last_result: Option<(PathBuf, u64, bool)>, // path, kb, searchable
    pub report: Option<Report>,
    pub diagnostics_request_tx: mpsc::Sender<mpsc::Sender<Report>>,
    pub langs_cache: Vec<String>,
    pub picker_available: bool,
    /// Pane geometry from the last frame (hit-testing + preview sync).
    pub pane_rects: Option<crate::tui::ui::PaneRects>,
    /// Preview grid cell rects from the last frame: (page id, cell rect),
    /// in draw order. Used for click-to-select on the contact sheet.
    pub preview_cells: Vec<(crate::session::PageId, Rect)>,
    /// Spinner frame counter (bumped on ticks).
    pub tick: u64,
}

impl App {
    pub fn new(
        cfg: Config,
        device_label: String,
        diagnostics_request_tx: mpsc::Sender<mpsc::Sender<Report>>,
    ) -> Self {
        let settings = Settings {
            dpi: cfg.dpi,
            mode: cfg.mode.clone(),
        };
        let mut status_lines = vec!["ready - press s to scan, ? for help".into()];
        // Only real language mixes (e.g. eng+deu) garble umlauts; a script
        // model like Latin alongside one language is safe (and fixes §).
        let lang_count = cfg
            .langs
            .split('+')
            .filter(|part| !part.is_empty() && !crate::config::is_script_lang(part))
            .count();
        if lang_count > 1 {
            status_lines.push(
                "hint: mixed OCR languages can garble umlauts (für→fiir) - press L to pick a single language".into(),
            );
        }
        Self {
            cfg,
            settings,
            pages: Vec::new(),
            selected: 0,
            focus: Pane::Sidebar,
            status_lines,
            status_scroll: 0,
            text_scroll: 0,
            meta: None,
            device_label,
            overlay: None,
            quit_requested: false,
            last_result: None,
            report: None,
            diagnostics_request_tx,
            langs_cache: Vec::new(),
            picker_available: false,
            pane_rects: None,
            preview_cells: Vec::new(),
            tick: 0,
        }
    }

    pub fn selected_page(&self) -> Option<&PageView> {
        self.pages.get(self.selected)
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_lines.push(msg.into());
        // Cap the buffer.
        if self.status_lines.len() > 500 {
            self.status_lines.drain(0..100);
        }
        self.status_scroll = 0;
    }

    pub fn sync_selection(&mut self) {
        if self.pages.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.pages.len() {
            self.selected = self.pages.len() - 1;
        }
    }

    pub fn busy(&self) -> Busy {
        self.meta.as_ref().map(|m| m.busy).unwrap_or(Busy::Idle)
    }

    /// Guard feedback for the footer: is this key action currently allowed?
    pub fn action_allowed(&self, action: Action) -> bool {
        let busy = self.busy();
        let jobs_running = self.meta.as_ref().map(|m| m.jobs_running).unwrap_or(0);
        match action {
            // Scanning overlaps with per-page processing (scanner is the
            // exclusive resource; jobs run in the background).
            Action::Scan => matches!(busy, Busy::Idle),
            Action::Rescan => {
                matches!(busy, Busy::Idle)
                    && self
                        .selected_page()
                        .is_some_and(|p| matches!(p.status, PageStatus::Ready | PageStatus::Failed))
            }
            Action::Rotate => {
                jobs_running == 0
                    && self
                        .selected_page()
                        .is_some_and(|p| matches!(p.status, PageStatus::Ready))
            }
            Action::Delete => !self.pages.is_empty(),
            Action::Reorder => !self.pages.is_empty(),
            Action::Finish => {
                !self.pages.is_empty()
                    && matches!(busy, Busy::Idle)
                    && self.pages.iter().all(|p| p.status == PageStatus::Ready)
            }
            Action::Open => self.last_result.is_some(),
            Action::Cancel => busy == Busy::Scanning,
            Action::Settings => busy != Busy::Finishing,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Scan,
    Rescan,
    Rotate,
    Delete,
    Reorder,
    Finish,
    Open,
    Cancel,
    Settings,
}

/// Commands the app sends to the session actor.
pub fn to_cmd(app: &App, action: CommandAction) -> Option<session::Cmd> {
    match action {
        CommandAction::ScanNext => Some(session::Cmd::ScanNext {
            dpi: app.settings.dpi,
            mode: app.settings.mode.clone(),
        }),
        CommandAction::Rescan(id) => Some(session::Cmd::Rescan(id as u32)),
        CommandAction::Rotate(id, cw) => Some(session::Cmd::Rotate(id as u32, cw)),
        CommandAction::Delete(id) => Some(session::Cmd::Delete(id as u32)),
        CommandAction::Move(from, to) => Some(session::Cmd::Move { from, to }),
        CommandAction::CancelScan => Some(session::Cmd::CancelScan),
        CommandAction::ListLangs => Some(session::Cmd::ListLangs),
    }
}

#[derive(Debug, Clone, Copy)]
pub enum CommandAction {
    ScanNext,
    Rescan(usize),
    Rotate(usize, bool),
    Delete(usize),
    Move(usize, usize),
    CancelScan,
    ListLangs,
}

/// Run the TUI. Owns the terminal, event stream, preview worker, and the
/// session actor's event channel.
pub struct TuiInit {
    pub cfg: Config,
    pub device_label: String,
    pub report: Option<Report>,
    pub picker: ratatui_image::picker::Picker,
    pub picker_available: bool,
}

pub async fn run_tui(
    mut terminal: DefaultTerminal,
    init: TuiInit,
    mut event_rx: mpsc::Receiver<Event>,
    cmd_tx: mpsc::Sender<session::Cmd>,
) -> Result<()> {
    let TuiInit {
        cfg,
        device_label,
        report: initial_report,
        picker,
        picker_available,
    } = init;
    let (diag_tx, mut diag_rx) = mpsc::channel::<mpsc::Sender<Report>>(4);
    let mut app = App::new(cfg.clone(), device_label, diag_tx);
    app.picker_available = picker_available;
    app.report = initial_report.clone();
    if initial_report.is_some_and(|r| !r.ok()) {
        app.overlay = Some(Overlay::Diagnostics);
    }

    let mut preview = PreviewWorker::new(picker);
    tracing::info!(
        "image preview protocol: {}",
        if picker_available {
            "native (sixel/kitty)"
        } else {
            "halfblocks"
        }
    );
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // Reconcile thumbnails against the page list, then poll completed
        // decodes/encodes before drawing so fresh results appear fast.
        // (Per-frame reconcile also makes the preview follow selection and
        // list changes from ANY source, not just session events.)
        preview.on_pages_changed(&app);
        let _preview_changed = preview.poll() | preview.poll_resizes();

        // Draw.
        terminal.draw(|f| ui::draw(f, &mut app, &mut preview))?;
        // Post-draw: kick per-cell re-encodes for the current grid geometry.
        if !app.preview_cells.is_empty() {
            preview.sync_cells(&app.preview_cells);
        }

        tokio::select! {
            // Terminal input.
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(ev)) => {
                        handle_event(&mut app, &cmd_tx, ev).await?;
                    }
                    Some(Err(e)) => return Err(anyhow::anyhow!("terminal event error: {e}")),
                    None => return Ok(()),
                }
            }
            // Session actor events.
            Some(ev) = event_rx.recv() => {
                handle_session_event(&mut app, ev).await;
            }
            // Diagnostics results.
            Some(tx) = diag_rx.recv() => {
                let report = crate::check::run_checks(&app.cfg).await;
                app.report = Some(report.clone());
                let _ = tx.send(report).await;
            }
            // Periodic tick: elapsed timers, spinner frames.
            _ = tick.tick() => {
                app.tick = app.tick.wrapping_add(1);
            }
        }

        if app.quit_requested && app.overlay.is_none() {
            break;
        }
    }

    // Persisted cleanup happens in main via the session actor's Drop.
    Ok(())
}

async fn handle_session_event(app: &mut App, ev: Event) {
    match ev {
        Event::Pages { pages, meta } => {
            let selection_was_valid = app.selected < pages.len();
            let old_selected_id = app.pages.get(app.selected).map(|p| p.id);
            app.pages = pages;
            app.meta = Some(meta);
            // Keep selection stable across list changes.
            if let Some(id) = old_selected_id {
                if let Some(idx) = app.pages.iter().position(|p| p.id == id) {
                    app.selected = idx;
                } else if selection_was_valid {
                    app.selected = app.selected.min(app.pages.len().saturating_sub(1));
                }
            }
            app.sync_selection();
        }
        Event::Status(msg) => app.set_status(msg),
        Event::Finished {
            outcome,
            path,
            size_kb,
        } => match outcome {
            Some(BuildOutcome::Searchable) => {
                app.last_result = Some((path.clone(), size_kb, true));
                app.set_status(format!("done: {} ({size_kb} KB)", path.display()));
                notify::notify(
                    app.cfg.notify,
                    "Scan complete",
                    &path.to_string_lossy(),
                    Urgency::Normal,
                )
                .await;
            }
            Some(BuildOutcome::WithoutTextLayer) => {
                app.last_result = Some((path.clone(), size_kb, false));
                app.set_status(format!(
                    "done (no text layer - ocrmypdf failed): {}",
                    path.display()
                ));
                notify::notify(
                    app.cfg.notify,
                    "Scan complete (no text layer)",
                    &path.to_string_lossy(),
                    Urgency::Critical,
                )
                .await;
            }
            None => {
                notify::notify(
                    app.cfg.notify,
                    "PDF build failed",
                    "See the log for details",
                    Urgency::Critical,
                )
                .await;
            }
        },
        Event::Langs(langs) => {
            app.langs_cache = langs;
            if let Some(Overlay::LangPicker(picker)) = &mut app.overlay {
                picker.set_available(app.langs_cache.clone());
            }
        }
    }
}

#[derive(Debug)]
enum UiAction {
    None,
    Quit,
}

async fn handle_event(
    app: &mut App,
    cmd_tx: &mpsc::Sender<session::Cmd>,
    ev: CtEvent,
) -> Result<()> {
    // Modal routing: overlays swallow everything first.
    if app.overlay.is_some() {
        let mut overlay = app.overlay.take().expect("overlay present");
        match ev {
            CtEvent::Key(key) if key.kind == KeyEventKind::Press => {
                if overlays::handle_key(app, &mut overlay, key, cmd_tx).await {
                    app.overlay = Some(overlay);
                }
            }
            CtEvent::Mouse(mouse) => overlays::handle_mouse(app, &mut overlay, mouse),
            _ => app.overlay = Some(overlay),
        }
        return Ok(());
    }

    match ev {
        CtEvent::Key(key) if key.kind == KeyEventKind::Press => {
            match handle_key(app, key, cmd_tx).await {
                UiAction::Quit => {
                    if app.pages.iter().any(|p| p.status != PageStatus::Failed) {
                        app.overlay = Some(Overlay::Confirm(Confirm::quit()));
                    } else {
                        app.quit_requested = true;
                    }
                }
                UiAction::None => {}
            }
        }
        CtEvent::Mouse(mouse) => handle_mouse(app, mouse, cmd_tx).await,
        _ => {}
    }
    Ok(())
}

async fn handle_key(
    app: &mut App,
    key: ratatui::crossterm::event::KeyEvent,
    cmd_tx: &mpsc::Sender<session::Cmd>,
) -> UiAction {
    use KeyCode::*;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if ctrl && key.code == Char('c') {
        return UiAction::Quit;
    }

    match key.code {
        // ---------------- global
        Char('?') => {
            app.overlay = Some(Overlay::Help);
        }
        Char('!') => {
            app.overlay = Some(Overlay::Diagnostics);
        }
        Char('q') => return UiAction::Quit,
        Tab => {
            app.focus = app.focus.next();
            app.text_scroll = 0;
        }
        BackTab => {
            app.focus = app.focus.prev();
            app.text_scroll = 0;
        }

        // ---------------- scanning
        Char('s') => {
            send(app, cmd_tx, CommandAction::ScanNext).await;
        }
        Esc | Char('c') if app.action_allowed(Action::Cancel) => {
            send(app, cmd_tx, CommandAction::CancelScan).await;
        }

        // ---------------- page ops (sidebar selection)
        Char('r') => {
            if let Some(p) = app.selected_page() {
                send(app, cmd_tx, CommandAction::Rescan(p.id as usize)).await;
            }
        }
        Char('R') => {
            if let Some(p) = app.selected_page() {
                send(app, cmd_tx, CommandAction::Rotate(p.id as usize, true)).await;
            }
        }
        Char('<') => {
            if let Some(p) = app.selected_page() {
                send(app, cmd_tx, CommandAction::Rotate(p.id as usize, false)).await;
            }
        }
        Char('d') => {
            if let Some(p) = app.selected_page() {
                let needs_confirm = matches!(p.status, PageStatus::Ready | PageStatus::Processing);
                if needs_confirm {
                    app.overlay = Some(Overlay::Confirm(Confirm::delete_page(p.id as usize)));
                } else {
                    send(app, cmd_tx, CommandAction::Delete(p.id as usize)).await;
                }
            }
        }
        Char('J') | Right if app.focus == Pane::Sidebar => {
            let sel = app.selected;
            if sel + 1 < app.pages.len() {
                send(app, cmd_tx, CommandAction::Move(sel, sel + 1)).await;
                app.selected = sel + 1;
            }
        }
        Char('K') | Left if app.focus == Pane::Sidebar => {
            let sel = app.selected;
            if sel > 0 {
                send(app, cmd_tx, CommandAction::Move(sel, sel - 1)).await;
                app.selected = sel - 1;
            }
        }
        Char('j') | Down if app.focus == Pane::Sidebar => {
            if app.selected + 1 < app.pages.len() {
                app.selected += 1;
                app.text_scroll = 0;
            }
        }
        Char('k') | Up if app.focus == Pane::Sidebar => {
            if app.selected > 0 {
                app.selected -= 1;
                app.text_scroll = 0;
            }
        }
        Char('j') | Down if app.focus == Pane::Text => {
            app.text_scroll = app.text_scroll.saturating_add(1);
        }
        Char('k') | Up if app.focus == Pane::Text => {
            app.text_scroll = app.text_scroll.saturating_sub(1);
        }

        // ---------------- finish / open / session
        Char('f') => {
            let path = app
                .meta
                .as_ref()
                .map(|m| m.output_path.clone())
                .unwrap_or_default();
            app.overlay = Some(Overlay::Confirm(Confirm::finish(path)));
        }
        Char('o') => {
            if let Some((path, _, _)) = &app.last_result {
                open_result(path).await;
            }
        }
        Char('n') => {
            app.overlay = Some(Overlay::Confirm(Confirm::new_session()));
        }

        // ---------------- settings
        Char('m') if app.action_allowed(Action::Settings) => {
            app.settings.mode = match app.settings.mode.as_str() {
                "gray" => "color".to_string(),
                "color" => "lineart".to_string(),
                _ => "gray".to_string(),
            };
            app.set_status(format!("mode: {}", app.settings.mode));
        }
        Char('+') | Char('=') if app.action_allowed(Action::Settings) => {
            app.settings.dpi = next_dpi(app.settings.dpi, 1);
            app.set_status(format!("dpi: {}", app.settings.dpi));
        }
        Char('-') | Char('_') if app.action_allowed(Action::Settings) => {
            app.settings.dpi = next_dpi(app.settings.dpi, -1);
            app.set_status(format!("dpi: {}", app.settings.dpi));
        }
        Char('L') => {
            app.overlay = Some(Overlay::LangPicker(super::overlays::LangPicker::new(
                app.cfg.langs.clone(),
            )));
            send(app, cmd_tx, CommandAction::ListLangs).await;
        }

        // digits jump to page N
        Char(d @ '1'..='9') => {
            let n = d.to_digit(10).unwrap_or(1) as usize;
            if n <= app.pages.len() {
                app.selected = n - 1;
                app.text_scroll = 0;
            }
        }

        _ => {}
    }
    UiAction::None
}

async fn send(app: &App, cmd_tx: &mpsc::Sender<session::Cmd>, action: CommandAction) {
    if let Some(cmd) = to_cmd(app, action) {
        let _ = cmd_tx.send(cmd).await;
    }
}

pub fn next_dpi(current: u16, dir: i32) -> u16 {
    use crate::config::DPI_PRESETS;
    // Exactly on a preset: step by one slot. Off-preset: snap directly to
    // the nearest preset in the requested direction.
    match DPI_PRESETS.iter().position(|d| *d == current) {
        Some(idx) => {
            let new_idx = (idx as i32 + dir).clamp(0, DPI_PRESETS.len() as i32 - 1);
            DPI_PRESETS[new_idx as usize]
        }
        None => {
            if dir >= 0 {
                let next = DPI_PRESETS.iter().find(|d| **d > current);
                next.copied().unwrap_or(DPI_PRESETS[DPI_PRESETS.len() - 1])
            } else {
                DPI_PRESETS
                    .iter()
                    .rev()
                    .find(|d| **d < current)
                    .copied()
                    .unwrap_or(DPI_PRESETS[0])
            }
        }
    }
}

async fn open_result(path: &std::path::Path) {
    if crate::backend::which("xdg-open").is_none() {
        tracing::warn!("xdg-open not found");
        return;
    }
    let p = path.to_path_buf();
    // Fire and forget; never a TUI child (no TTY inheritance).
    tokio::spawn(async move {
        let _ = tokio::process::Command::new("xdg-open")
            .arg(&p)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    });
}

async fn handle_mouse(
    app: &mut App,
    mouse: ratatui::crossterm::event::MouseEvent,
    cmd_tx: &mpsc::Sender<session::Cmd>,
) {
    let pos = (mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(pane) = ui::hit_test(app, pos) {
                app.focus = pane;
                match pane {
                    Pane::Sidebar => {
                        if let Some(idx) = ui::sidebar_index_at(app, pos) {
                            app.selected = idx;
                            app.text_scroll = 0;
                        }
                    }
                    // Clicking a contact-sheet cell selects that page (ids
                    // make this exact, unlike the sidebar's offset guess).
                    Pane::Preview => {
                        if let Some(id) = ui::preview_cell_at(app, pos) {
                            if let Some(idx) = app.pages.iter().position(|p| p.id == id) {
                                app.selected = idx;
                                app.text_scroll = 0;
                            }
                        }
                    }
                    Pane::Text => {}
                }
            }
        }
        MouseEventKind::ScrollUp => match ui::hit_test(app, pos) {
            // Sidebar and Preview grid both move the selection.
            Some(Pane::Sidebar) | Some(Pane::Preview) => {
                if app.selected > 0 {
                    app.selected -= 1;
                }
            }
            Some(Pane::Text) => app.text_scroll = app.text_scroll.saturating_sub(1),
            None => {}
        },
        MouseEventKind::ScrollDown => match ui::hit_test(app, pos) {
            Some(Pane::Sidebar) | Some(Pane::Preview) => {
                if app.selected + 1 < app.pages.len() {
                    app.selected += 1;
                }
            }
            Some(Pane::Text) => app.text_scroll = app.text_scroll.saturating_add(1),
            None => {}
        },
        _ => {}
    }
    let _ = cmd_tx; // reserved for future click actions
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_preset_cycling() {
        assert_eq!(next_dpi(150, 1), 200);
        assert_eq!(next_dpi(200, 1), 300);
        assert_eq!(next_dpi(300, 1), 600);
        assert_eq!(next_dpi(600, 1), 600); // clamp at top
        assert_eq!(next_dpi(600, -1), 300);
        assert_eq!(next_dpi(150, -1), 150); // clamp at bottom
        assert_eq!(next_dpi(250, 1), 300); // snap up to next preset
        assert_eq!(next_dpi(300, -1), 200);
    }

    #[test]
    fn pane_cycling() {
        assert_eq!(Pane::Sidebar.next(), Pane::Preview);
        assert_eq!(Pane::Preview.next(), Pane::Text);
        assert_eq!(Pane::Text.next(), Pane::Sidebar);
        assert_eq!(Pane::Sidebar.prev(), Pane::Text);
    }
}
