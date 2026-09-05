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
use crate::check::{self, Report};
use crate::config::{Config, PreviewOcr};
use crate::notify::{self, Urgency};
use crate::session::{self, Busy, Event, PageStatus, PageView, SessionMeta};

use super::overlays::{self, Confirm, ConfirmKind, Overlay};
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
    /// Header label; "detecting..." until background scanner detection
    /// resolves, then the device label (or "no scanner" on failure).
    pub device_label: String,
    pub overlay: Option<Overlay>,
    pub quit_requested: bool,
    pub last_result: Option<(PathBuf, u64, bool)>, // path, kb, searchable
    pub report: Option<Report>,
    /// Request channel for non-blocking check runs (diagnostics re-run):
    /// the overlay queues a token; the run_tui select loop consumes it,
    /// arms the in-flight guard and spawns the suite.
    pub diagnostics_request_tx: mpsc::Sender<()>,
    /// Startup exit-code verdict: None = quit while detection was still
    /// running (neutral exit 0), Some(true) = usable device and no failed
    /// checks, Some(false) = failed checks or no scanner. Set ONLY by the
    /// startup final delivery (never by fast reports or manual re-runs).
    pub startup_report_ok: Option<bool>,
    /// True while the actor's device is still unknown (detection running).
    /// Gates scanning; `s` presses buffer instead of firing.
    pub device_known: bool,
    /// A scan intent buffered while detection was still running; fired by
    /// the tick once the device is known (never re-fired afterwards).
    pub pending_scan: bool,
    /// True while an async check run is in flight (guards double-`r`).
    pub checks_in_flight: bool,
    /// True once a manual diagnostics re-run's report has been applied.
    /// Startup reports arriving after that are out-of-order — the re-run's
    /// data is strictly newer — and are ignored wholesale (see
    /// `apply_report`).
    pub rerun_seen: bool,
    pub langs_cache: Vec<String>,
    pub picker_available: bool,
    /// Pane geometry from the last frame (hit-testing + preview sync).
    pub pane_rects: Option<crate::tui::ui::PaneRects>,
    /// Preview grid cell rects from the last frame: (page id, cell rect),
    /// in draw order. Used for click-to-select on the contact sheet.
    pub preview_cells: Vec<(crate::session::PageId, Rect)>,
    /// Spinner frame counter (bumped on ticks).
    pub tick: u64,
    /// True while a spawned system save dialog is still open: `f` must not
    /// stack a second native dialog on top (the first one's result would
    /// arrive out of order). Cleared when its outcome is consumed.
    pub dialog_in_flight: bool,
    /// Inbox for the system save-dialog task: `f` spawns the native dialog
    /// (zenity/kdialog/yad) and it reports the outcome here — see
    /// `SaveChoice`. Consumed by the run_tui select loop.
    pub finish_tx: mpsc::Sender<crate::backend::filedialog::SaveChoice>,
}

impl App {
    pub fn new(
        cfg: Config,
        diagnostics_request_tx: mpsc::Sender<()>,
        finish_tx: mpsc::Sender<crate::backend::filedialog::SaveChoice>,
    ) -> Self {
        let settings = Settings {
            dpi: cfg.dpi,
            mode: cfg.mode.clone(),
        };
        let mut status_lines = vec!["detecting scanner - press ? for help".into()];
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
            device_label: "detecting...".to_string(),
            overlay: None,
            quit_requested: false,
            last_result: None,
            report: None,
            diagnostics_request_tx,
            startup_report_ok: None,
            device_known: false,
            pending_scan: false,
            checks_in_flight: false,
            rerun_seen: false,
            langs_cache: Vec::new(),
            picker_available: false,
            pane_rects: None,
            preview_cells: Vec::new(),
            tick: 0,
            dialog_in_flight: false,
            finish_tx,
        }
    }

    /// Whether scanning may start: the actor's device must be known and
    /// all other preconditions met. Mirrors the actor-side guard,
    /// including the deferred-delete block.
    fn scan_allowed(&self) -> bool {
        self.device_known
            && matches!(self.busy(), Busy::Idle)
            && !self.meta.as_ref().is_some_and(|m| m.finished)
            && !self
                .pages
                .iter()
                .any(|p| p.status == PageStatus::DeletePending)
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

    /// Quit needs a confirm only when real work could still be lost:
    /// any page holding a captured image while the session hasn't been
    /// built into a PDF. Failed captures count as contentless (quitting
    /// deletes their files along with the session dir, the dialog just
    /// can't promise a PDF for them), and a finished session holds only
    /// inert post-build stubs.
    pub fn needs_quit_confirm(&self) -> bool {
        !self.meta.as_ref().is_some_and(|m| m.finished)
            && self.pages.iter().any(|p| {
                matches!(
                    p.status,
                    PageStatus::Ready
                        | PageStatus::Processing
                        | PageStatus::Scanning
                        | PageStatus::DeletePending
                )
            })
    }

    /// Guard feedback for the footer: is this key action currently allowed?
    pub fn action_allowed(&self, action: Action) -> bool {
        let busy = self.busy();
        // After a successful build the session dir is gone: page commands
        // would run against missing images. Mirror the actor's guards here.
        let finished = self.meta.as_ref().is_some_and(|m| m.finished);
        match action {
            // Scanning overlaps with per-page processing (scanner is the
            // exclusive resource; jobs run in the background). Requires a
            // known device (background detection must have delivered one).
            Action::Scan => self.scan_allowed(),
            // Mirrors the actor guard: a live per-page job (preview OCR or
            // rotate, visible as text_pending or a non-Ready status) blocks
            // rescan/rotate of that page.
            Action::Rescan => {
                self.device_known
                    && matches!(busy, Busy::Idle)
                    && !finished
                    && self.selected_page().is_some_and(|p| {
                        matches!(p.status, PageStatus::Ready | PageStatus::Failed)
                            && !p.text_pending
                    })
            }
            Action::Rotate => {
                !finished
                    && self
                        .selected_page()
                        .is_some_and(|p| matches!(p.status, PageStatus::Ready) && !p.text_pending)
            }
            Action::Delete => !self.pages.is_empty(),
            Action::Reorder => !self.pages.is_empty(),
            Action::Finish => {
                // A system save dialog is blocking in the background: the
                // outcome is still pending, so `f` must not stack a second
                // dialog (and the footer greys the key accordingly).
                !self.dialog_in_flight
                    && !self.pages.is_empty()
                    && matches!(busy, Busy::Idle)
                    && !finished
                    && self.pages.iter().all(|p| {
                        // Preview OCR for the text pane never gates the
                        // build (the PDF text layer comes from ocrmypdf).
                        p.status == PageStatus::Ready
                            || (p.status == PageStatus::Processing
                                && p.stage.is_some_and(|s| s == session::Stage::Ocr))
                    })
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
        CommandAction::Rescan(id) => Some(session::Cmd::Rescan {
            id: id as u32,
            dpi: app.settings.dpi,
            mode: app.settings.mode.clone(),
        }),
        CommandAction::Rotate(id, cw) => Some(session::Cmd::Rotate(id as u32, cw)),
        CommandAction::Delete(id) => Some(session::Cmd::Delete(id as u32)),
        CommandAction::Move(from, to) => Some(session::Cmd::Move { from, to }),
        CommandAction::CancelScan => Some(session::Cmd::CancelScan),
        CommandAction::ListLangs => Some(session::Cmd::ListLangs),
        CommandAction::NewSession => Some(session::Cmd::NewSession),
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
    NewSession,
}

/// Run the TUI. Owns the terminal, event stream, preview worker, and the
/// session actor's event channel. Returns whether the startup environment
/// finished OK (device found, no failed checks); None (quit while
/// detection was still running) maps to a neutral success. The caller maps
/// this to the process exit code.
pub struct TuiInit {
    pub cfg: Config,
    pub picker: ratatui_image::picker::Picker,
    pub picker_available: bool,
    /// Report inbox ends: the startup preflight task sends the fast report
    /// first, then the final report; diagnostics re-runs send theirs here
    /// too. One consumer branch keeps the ordering rules in one place.
    pub report_tx: mpsc::Sender<Report>,
    pub report_rx: mpsc::Receiver<Report>,
}

pub async fn run_tui(
    mut terminal: DefaultTerminal,
    init: TuiInit,
    mut event_rx: mpsc::Receiver<Event>,
    cmd_tx: mpsc::Sender<session::Cmd>,
) -> Result<bool> {
    let TuiInit {
        cfg,
        picker,
        picker_available,
        report_tx,
        report_rx,
    } = init;
    let mut report_rx = report_rx;
    let (diag_tx, mut diag_rx) = mpsc::channel::<()>(4);
    // Save-dialog inbox: `f` spawns the native dialog task; its outcome
    // arrives here (see SaveChoice).
    let (finish_tx, mut finish_rx) = mpsc::channel::<crate::backend::filedialog::SaveChoice>(1);
    let mut app = App::new(cfg.clone(), diag_tx, finish_tx);
    app.picker_available = picker_available;

    let mut preview = PreviewWorker::new(picker);
    tracing::info!(
        "image preview protocol: {}",
        if app.picker_available {
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
                    None => return Ok(app.startup_report_ok.unwrap_or(true)),
                }
            }
            // Session actor events.
            Some(ev) = event_rx.recv() => {
                handle_session_event(&mut app, ev).await;
            }
            // Preflight/diagnostics reports (startup fast+final, manual
            // re-runs). One inbox keeps the ordering rules in one place.
            Some(report) = report_rx.recv() => {
                apply_report(&mut app, report, &cmd_tx).await;
            }
            // Diagnostics re-run requests (r in the overlay): run the full
            // check suite in a background task so the UI never freezes.
            // The guard is armed only here — the overlay just queues a
            // token; if one is stale (guard already set) it's dropped.
            Some(()) = diag_rx.recv() => {
                if !app.checks_in_flight {
                    app.checks_in_flight = true;
                    app.set_status("re-running checks...");
                    let cfg = app.cfg.clone();
                    let report_tx = report_tx.clone();
                    tokio::spawn(async move {
                        let report = check::run_checks(&cfg).await;
                        let _ = report_tx.send(report).await;
                    });
                }
            }
            // System save-dialog result for the finish flow (f key).
            Some(chosen) = finish_rx.recv() => {
                handle_dialog_result(&mut app, chosen, &cmd_tx).await;
            }
            // Periodic tick: elapsed timers, spinner frames, and the lazy
            // preview-OCR request for the selected page.
            _ = tick.tick() => {
                app.tick = app.tick.wrapping_add(1);
                fire_pending_scan(&mut app, &cmd_tx).await;
                request_text_if_needed(&app, &cmd_tx).await;
            }
        }

        if app.quit_requested && app.overlay.is_none() {
            break;
        }
    }

    // Quitting deletes the session dir (via the session actor's Drop):
    // un-built pages are gone unless a PDF build was in flight, in which
    // case the dir survives for the next startup's sweep.
    // Exit verdict: None means quit during detection (neutral success);
    // Some(false) is the only failing verdict (failed checks / no scanner).
    Ok(app.startup_report_ok.unwrap_or(true))
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
            // A quit confirm opened while a build was in flight may now be
            // stale (PDF finished meanwhile, pages became inert stubs):
            // drop it instead of claiming pages "will be lost". The user
            // presses q again to actually quit.
            let stale_quit = matches!(
                app.overlay,
                Some(Overlay::Confirm(Confirm {
                    kind: ConfirmKind::Quit,
                    ..
                }))
            ) && !app.needs_quit_confirm();
            if stale_quit {
                app.overlay = None;
            }
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

/// A report arrived (startup fast/final or a manual diagnostics re-run).
/// Ordering matters: store -> header label -> actor device -> exit-code
/// flag -> auto-open overlay -> fire a buffered scan.
///
/// Staleness rule: a manual re-run is strictly newer than anything the
/// startup preflight produced (it re-checks the same environment later).
/// So once a re-run report has been applied, a later-arriving startup
/// report (e.g. a slow startup `scanimage -L` final landing after the
/// re-run delivered its device) is out-of-order and ignored wholesale —
/// otherwise it could clobber `device_known`, re-lock scanning and drop
/// a buffered scan on a healthy machine. Re-run reports never arrive out
/// of order among themselves: `checks_in_flight` serializes them.
///
/// Exit-code exception: if the verdict is still undecided and a stale
/// StartupFinal is dropped, the re-run's data decides it instead — the
/// machine state it measured IS the startup outcome by then. Without
/// this, a quit after both startup detection and re-run failed would
/// exit 0 (neutral) instead of 1.
async fn apply_report(app: &mut App, report: Report, cmd_tx: &mpsc::Sender<session::Cmd>) {
    let settled = report.settled();
    let device = report.device.clone();
    let source = report.source;
    if source == check::ReportSource::ReRun {
        app.checks_in_flight = false;
        app.rerun_seen = true;
    } else if app.rerun_seen {
        // Stale startup report after a re-run applied: drop it entirely.
        // If the exit verdict is still undecided, the re-run's (newer)
        // stored data decides it — its device + check results, never the
        // stale report's — see the doc above.
        if app.startup_report_ok.is_none() && settled {
            app.startup_report_ok = app.report.as_ref().map(|r| r.device.is_some() && r.ok());
        }
        return;
    }
    app.report = Some(report);

    if !settled {
        // Fast (still-detecting) report. Real failures in it (missing
        // binaries etc.) are worth surfacing immediately, but never steal
        // focus from a dialog the user opened meanwhile.
        if !app.report.as_ref().is_some_and(|r| r.ok()) && app.overlay.is_none() {
            app.overlay = Some(Overlay::Diagnostics);
        }
        return;
    }

    // Final/settled report: the scanner question is answered.
    if let Some(d) = &device {
        app.device_label = check::device_label(Some(d));
        app.device_known = true;
        // Delivery to the actor (it ignores empty/duplicate names).
        let _ = cmd_tx.send(session::Cmd::SetDevice(d.name.clone())).await;
    } else {
        app.device_label = "no scanner".to_string();
        app.device_known = false;
    }

    // Startup exit-code semantics: the verdict starts as None (quit while
    // still detecting -> neutral exit 0) and is decided exactly once, by
    // the startup final report: ok (device + no failures) -> Some(true),
    // else Some(false). Manual re-runs and fast reports never touch it.
    if app.startup_report_ok.is_none() && source == check::ReportSource::StartupFinal {
        app.startup_report_ok =
            Some(device.is_some() && app.report.as_ref().is_some_and(|r| r.ok()));
    }

    // Auto-open diagnostics on failure (final reports AND fast reports
    // with real fails), guarded so a user-opened overlay is preserved.
    if !app.report.as_ref().is_some_and(|r| r.ok()) && app.overlay.is_none() {
        app.overlay = Some(Overlay::Diagnostics);
        if device.is_none() {
            app.set_status("no scanner found - see diagnostics (press ! to reopen)");
        }
    }

    // A scan intent buffered during detection: fire it now that the device
    // is known, or drop it with a hint when detection found nothing.
    if app.pending_scan {
        if device.is_some() {
            app.set_status("scanner ready - starting buffered scan");
            // The tick fires it (re-checks guards); keep the buffer set.
        } else {
            app.pending_scan = false;
            app.set_status("no scanner found - buffered scan dropped");
        }
    }
}

/// Tick-driven buffered-scan fire: self-healing (unlike an event-triggered
/// fire, a tick can't be lost). Re-checks all scan guards at fire time.
async fn fire_pending_scan(app: &mut App, cmd_tx: &mpsc::Sender<session::Cmd>) {
    if !app.pending_scan || !app.scan_allowed() {
        return;
    }
    app.pending_scan = false;
    let dpi = app.settings.dpi;
    let mode = app.settings.mode.clone();
    let _ = cmd_tx.send(session::Cmd::ScanNext { dpi, mode }).await;
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
                    if app.needs_quit_confirm() {
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
            if app.scan_allowed() {
                send(app, cmd_tx, CommandAction::ScanNext).await;
            } else if !app.device_known {
                // Detection still running: buffer the intent (fired by the
                // tick once the device arrives) instead of a doomed scan.
                if !app.pending_scan {
                    app.pending_scan = true;
                    app.set_status("waiting for scanner - scan will start when detected");
                }
            } else if app.busy() == Busy::Finishing {
                app.set_status("blocked: building PDF - scan once it finishes");
            } else if app.meta.as_ref().is_some_and(|m| m.finished) {
                app.set_status("blocked: PDF already built - press n for a new session");
            } else if app.busy() == Busy::Scanning {
                // Parity with the actor guard; previously this fell through
                // silently (footer greying only).
                app.set_status("blocked: scanner busy - press Esc to cancel");
            } else if app
                .pages
                .iter()
                .any(|p| p.status == PageStatus::DeletePending)
            {
                app.set_status("blocked: waiting for deferred delete");
            }
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
        Char('f') if app.action_allowed(Action::Finish) => {
            // Delegate path choice to the system save dialog (zenity/
            // kdialog/yad) in a background task: the dialog is a blocking
            // native window and must never run in the select loop. The
            // outcome is delivered via finish_tx; without any dialog tool
            // the plain confirm dialog (default path) is used. The
            // dialog-in-flight flag is part of action_allowed(Finish), so
            // the keypress guard and the footer stay in sync and a second
            // dialog can never stack while one is open.
            let path = app
                .meta
                .as_ref()
                .map(|m| m.output_path.clone())
                .unwrap_or_default();
            let dir = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| app.cfg.output.clone());
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let event_tx = app.finish_tx.clone();
            app.dialog_in_flight = true;
            // Panic guard: the dialog task must ALWAYS report an outcome, or
            // `dialog_in_flight` would wedge and disable `f` for the rest of
            // the run. The inner task's panic (tokio catches it and surfaces
            // it through the JoinHandle) is mapped to Unavailable, which
            // routes to the plain confirm dialog — the finish flow degrades,
            // it never breaks.
            let inner = tokio::spawn(crate::backend::filedialog::save_dialog(
                dir,
                filename,
                "Save PDF as",
            ));
            tokio::spawn(async move {
                let chosen = match inner.await {
                    Ok(chosen) => chosen,
                    Err(join_err) => {
                        tracing::error!("save dialog task failed: {join_err}");
                        crate::backend::filedialog::SaveChoice::Unavailable
                    }
                };
                let _ = event_tx.send(chosen).await;
            });
        }
        Char('o') => {
            if let Some((path, _, _)) = &app.last_result {
                open_result(path).await;
            }
        }
        Char('n') => {
            // Post-finish the pages are inert stubs of a built PDF: nothing
            // to lose, so start the new session directly.
            if app.meta.as_ref().is_some_and(|m| m.finished) {
                send(app, cmd_tx, CommandAction::NewSession).await;
            } else {
                app.overlay = Some(Overlay::Confirm(Confirm::new_session()));
            }
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

/// Lazy preview OCR: when the selected Ready page has no text yet, ask the
/// actor to extract it (called on every tick). The actor validates
/// idempotently and silently — duplicates here are harmless; it never
/// pushes "blocked" status lines for this command.
async fn request_text_if_needed(app: &App, cmd_tx: &mpsc::Sender<session::Cmd>) {
    if app.cfg.preview_ocr != PreviewOcr::Lazy
        || app.busy() == Busy::Finishing
        // Post-finish the session dir is gone; the actor drops these
        // requests, so stop sending them (and the filesystem probing
        // behind the actor's existence check).
        || app.meta.as_ref().is_some_and(|m| m.finished)
    {
        return;
    }
    if let Some(p) = app.selected_page() {
        // `text.is_some()` is the guard (not non-empty): lazy OCR
        // legitimately produces Some("") which must not re-trigger. A
        // failed attempt for the current image is also not retried (the
        // actor would drop it silently anyway); rescan/rotate bumps
        // image_gen and re-arms the request.
        if p.status == PageStatus::Ready
            && p.text.is_none()
            && !p.text_pending
            && p.ocr_failed_gen != Some(p.image_gen)
        {
            let _ = cmd_tx.send(session::Cmd::RequestText(p.id)).await;
        }
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

/// Result of the system save-dialog task for `f`: `Chosen(path)` = user
/// picked a target (overwrite already confirmed inside the dialog),
/// `Cancelled` = the user dismissed the dialog (do nothing — they have not
/// changed their mind about building, and must not be railroaded into the
/// default-path confirm), `Unavailable` = no dialog tool installed, or the
/// tool could not run (no display, e.g. SSH without forwarding — exit-1
/// display failures are discriminated by stderr). Unavailable falls back
/// to the plain confirm dialog with the reserved default path.
async fn handle_dialog_result(
    app: &mut App,
    chosen: crate::backend::filedialog::SaveChoice,
    cmd_tx: &mpsc::Sender<session::Cmd>,
) {
    use crate::backend::filedialog::SaveChoice;
    app.dialog_in_flight = false;
    match chosen {
        SaveChoice::Chosen(out) => {
            app.set_status(format!("saving to {}", out.display()));
            let _ = cmd_tx
                .send(session::Cmd::FinishTo {
                    out,
                    // The dialog's own overwrite prompt already asked.
                    overwrite: true,
                })
                .await;
        }
        SaveChoice::Cancelled => {
            app.set_status("save dialog cancelled - press f to try again");
        }
        SaveChoice::Unavailable => {
            // The TUI stayed interactive while the dialog ran, so the user
            // may have opened an overlay meanwhile (? / ! / quit confirm).
            // Same guard as apply_report: never steal their overlay.
            let path = app
                .meta
                .as_ref()
                .map(|m| m.output_path.clone())
                .unwrap_or_default();
            let confirm = Overlay::Confirm(Confirm::finish(path));
            if app.overlay.is_none() {
                app.overlay = Some(confirm);
            } else {
                app.set_status("save dialog unavailable - press f to retry");
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

    /// App with throwaway channels; meta/pages are set per-test.
    fn test_app() -> App {
        let (diag_tx, _diag_rx) = mpsc::channel(4);
        let (finish_tx, _finish_rx) = mpsc::channel(1);
        App::new(Config::default(), diag_tx, finish_tx)
    }

    fn ready_page(id: u32) -> PageView {
        PageView {
            id,
            status: PageStatus::Ready,
            stage: None,
            stage_started: None,
            image: Some(std::path::PathBuf::from(format!("/tmp/page_{id}.png"))),
            image_gen: 1,
            text: None,
            text_pending: false,
            ocr_failed_gen: None,
            error: None,
            dpi: 300,
            mode: "gray".into(),
            rotated: false,
        }
    }

    fn meta(finished: bool) -> SessionMeta {
        SessionMeta {
            busy: Busy::Idle,
            busy_since: None,
            jobs_running: 0,
            output_path: "/tmp/out.pdf".into(),
            dirty: !finished,
            finished,
        }
    }

    #[test]
    fn quit_confirm_rules() {
        // No pages / no meta: never confirm.
        let mut app = test_app();
        assert!(!app.needs_quit_confirm(), "empty app quits silently");

        // Ready page, session not finished: confirm.
        app.pages = vec![ready_page(1)];
        app.meta = Some(meta(false));
        assert!(app.needs_quit_confirm(), "un-built pages need confirm");

        // Failed pages only: no confirm (nothing the dialog cares about).
        let mut failed = ready_page(1);
        failed.status = PageStatus::Failed;
        app.pages = vec![failed];
        assert!(!app.needs_quit_confirm(), "failed pages quit silently");

        // Finished session: inert stubs, no confirm.
        app.pages = vec![ready_page(1)];
        app.meta = Some(meta(true));
        assert!(!app.needs_quit_confirm(), "finished session quits silently");

        // meta missing (before first snapshot) but pages present: fall
        // back to the pages check.
        app.meta = None;
        assert!(app.needs_quit_confirm(), "missing meta defers to pages");
    }

    #[tokio::test]
    async fn stale_quit_confirm_dismisses_after_finish() {
        // The quit dialog opened while the build ran; the Pages snapshot
        // after completion must dismiss it instead of claiming pages
        // "will be lost".
        let mut app = test_app();
        app.pages = vec![ready_page(1)];
        app.meta = Some(meta(false));
        app.overlay = Some(Overlay::Confirm(Confirm::quit()));
        assert!(app.needs_quit_confirm());

        handle_session_event(
            &mut app,
            Event::Pages {
                pages: vec![ready_page(1)],
                meta: meta(true),
            },
        )
        .await;
        assert!(app.overlay.is_none(), "stale quit dialog dismissed");
    }

    #[tokio::test]
    async fn quit_confirm_survives_unrelated_page_events() {
        // A confirm that is still valid must NOT be dismissed by Pages
        // snapshots, and other overlay kinds stay untouched.
        let mut app = test_app();
        app.pages = vec![ready_page(1)];
        app.meta = Some(meta(false));
        app.overlay = Some(Overlay::Confirm(Confirm::quit()));

        handle_session_event(
            &mut app,
            Event::Pages {
                pages: vec![ready_page(1)],
                meta: meta(false),
            },
        )
        .await;
        assert!(
            matches!(app.overlay, Some(Overlay::Confirm(_))),
            "still-valid confirm survives"
        );
    }

    fn device(name: &str) -> crate::check::Device {
        crate::check::Device {
            name: name.into(),
            label: "Test Scanner".into(),
        }
    }

    /// A report whose scanner item carries the given status. `source`
    /// defaults to the startup final report (the exit-flag path).
    fn report_with(device: Option<crate::check::Device>, status: crate::check::Status) -> Report {
        report_with_source(device, status, crate::check::ReportSource::StartupFinal)
    }

    fn report_with_source(
        device: Option<crate::check::Device>,
        status: crate::check::Status,
        source: crate::check::ReportSource,
    ) -> Report {
        let mut r = Report::default();
        r.items.push(crate::check::CheckItem {
            what: "scanner".into(),
            status,
            detail: String::new(),
            hint: None,
            pending_detail: None,
        });
        r.device = device;
        r.source = source;
        r
    }

    #[tokio::test]
    async fn report_arrival_sets_device_and_exit_flag() {
        let mut app = test_app();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);

        // Fast (pending) report: device stays unknown, no SetDevice, no
        // verdict.
        apply_report(
            &mut app,
            report_with(None, crate::check::Status::Pending),
            &cmd_tx,
        )
        .await;
        assert!(!app.device_known);
        assert_eq!(app.device_label, "detecting...");
        assert!(app.startup_report_ok.is_none());
        assert!(cmd_rx.try_recv().is_err(), "no SetDevice on fast report");

        // Final report with device: label, device_known, SetDevice, flag.
        apply_report(
            &mut app,
            report_with(Some(device("hpaio:/usb/x")), crate::check::Status::Ok),
            &cmd_tx,
        )
        .await;
        assert_eq!(app.device_label, "Test Scanner");
        assert!(app.device_known);
        assert_eq!(app.startup_report_ok, Some(true));
        match cmd_rx.try_recv() {
            Ok(session::Cmd::SetDevice(name)) => assert_eq!(name, "hpaio:/usb/x"),
            other => panic!("expected SetDevice, got {other:?}"),
        }
    }

    /// A failing startup report (e.g. no scanner) decides the verdict as
    /// Some(false) -> exit 1; quit-during-detection (None) stays neutral.
    #[tokio::test]
    async fn failed_startup_report_sets_failing_verdict() {
        let mut app = test_app();
        apply_report(
            &mut app,
            report_with(None, crate::check::Status::Fail),
            &mpsc::channel(1).0,
        )
        .await;
        assert_eq!(app.startup_report_ok, Some(false));

        // A later report must not flip the decided verdict; in reality a
        // late plug-in arrives via a ReRun report, which never touches the
        // flag. (The is_none() guard holds for any source.)
        apply_report(
            &mut app,
            report_with(Some(device("hpaio:/usb/x")), crate::check::Status::Ok),
            &mpsc::channel(1).0,
        )
        .await;
        assert_eq!(app.startup_report_ok, Some(false));
    }

    #[tokio::test]
    async fn report_failure_opens_diagnostics_only_when_unobstructed() {
        let mut app = test_app();
        apply_report(
            &mut app,
            report_with(None, crate::check::Status::Fail),
            &mpsc::channel(1).0,
        )
        .await;
        assert!(app.overlay.is_some(), "failure auto-opens diagnostics");
        assert_eq!(app.device_label, "no scanner");
        assert_eq!(app.startup_report_ok, Some(false));

        // A user-opened overlay is never clobbered by auto-open.
        let mut app = test_app();
        app.overlay = Some(Overlay::Help);
        apply_report(
            &mut app,
            report_with(None, crate::check::Status::Fail),
            &mpsc::channel(1).0,
        )
        .await;
        assert!(
            matches!(app.overlay, Some(Overlay::Help)),
            "auto-open must not clobber a user dialog"
        );
    }

    #[tokio::test]
    async fn buffered_scan_fires_on_tick_after_device_arrives() {
        let mut app = test_app();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
        app.pending_scan = true;
        assert!(!app.scan_allowed(), "no device yet");

        apply_report(
            &mut app,
            report_with(Some(device("hpaio:/x")), crate::check::Status::Ok),
            &cmd_tx,
        )
        .await;
        assert!(app.device_known);
        assert!(app.pending_scan, "still buffered until the tick fires");

        fire_pending_scan(&mut app, &cmd_tx).await;
        assert!(!app.pending_scan);
        // First message is the SetDevice from apply_report, second the scan.
        let _ = cmd_rx.recv().await;
        match cmd_rx.recv().await {
            Some(session::Cmd::ScanNext { dpi, mode }) => {
                assert_eq!(dpi, Config::default().dpi);
                assert_eq!(mode, "gray");
            }
            other => panic!("expected ScanNext, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn buffered_scan_dropped_when_detection_fails() {
        let mut app = test_app();
        app.pending_scan = true;
        apply_report(
            &mut app,
            report_with(None, crate::check::Status::Fail),
            &mpsc::channel(1).0,
        )
        .await;
        assert!(!app.pending_scan, "no device -> buffered intent dropped");
        assert!(!app.device_known);
    }

    #[tokio::test]
    async fn scan_not_allowed_while_device_unknown() {
        let app = test_app();
        assert!(!app.scan_allowed());
        assert!(!app.action_allowed(Action::Scan));
    }

    /// Regression: a manual re-run (r in diagnostics) must re-deliver the
    /// device instead of wiping it. Previously run_checks returned
    /// device: None, so apply_report flipped device_known off on a healthy
    /// machine and scan_allowed stayed false forever (scan locked out
    /// until restart).
    #[tokio::test]
    async fn rerun_report_keeps_device_known() {
        let mut app = test_app();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);

        apply_report(
            &mut app,
            report_with(Some(device("hpaio:/usb/x")), crate::check::Status::Ok),
            &cmd_tx,
        )
        .await;
        assert!(app.device_known);

        apply_report(
            &mut app,
            report_with_source(
                Some(device("hpaio:/usb/x")),
                crate::check::Status::Ok,
                crate::check::ReportSource::ReRun,
            ),
            &cmd_tx,
        )
        .await;
        assert!(app.device_known, "re-run must not wipe the known device");
        assert_eq!(app.device_label, "Test Scanner");
        assert!(app.scan_allowed());
        assert_eq!(
            app.startup_report_ok,
            Some(true),
            "verdict unchanged by re-run"
        );
        // Second SetDevice (duplicate names are an actor no-op).
        let _ = cmd_rx.recv().await;
        match cmd_rx.try_recv() {
            Ok(session::Cmd::SetDevice(name)) => assert_eq!(name, "hpaio:/usb/x"),
            other => panic!("expected SetDevice from re-run, got {other:?}"),
        }
    }

    /// The exit-code flag is set only by the startup final report; a
    /// successful manual re-run while still detecting must not set it.
    #[tokio::test]
    async fn rerun_report_does_not_set_startup_exit_flag() {
        let mut app = test_app();
        apply_report(
            &mut app,
            report_with_source(
                Some(device("hpaio:/usb/x")),
                crate::check::Status::Ok,
                crate::check::ReportSource::ReRun,
            ),
            &mpsc::channel(1).0,
        )
        .await;
        assert!(app.device_known);
        assert_eq!(
            app.startup_report_ok, None,
            "re-run must not set the startup flag"
        );
    }

    /// A startup report landing mid-re-run must not clear the re-run
    /// guard (otherwise a second r could start a concurrent re-run whose
    /// older report could arrive last and win).
    #[tokio::test]
    async fn startup_report_does_not_clear_rerun_guard() {
        let mut app = test_app();
        app.checks_in_flight = true;
        apply_report(
            &mut app,
            report_with(Some(device("hpaio:/x")), crate::check::Status::Ok),
            &mpsc::channel(1).0,
        )
        .await;
        assert!(
            app.checks_in_flight,
            "startup report must not unlock re-runs"
        );

        apply_report(
            &mut app,
            report_with_source(
                None,
                crate::check::Status::Fail,
                crate::check::ReportSource::ReRun,
            ),
            &mpsc::channel(1).0,
        )
        .await;
        assert!(!app.checks_in_flight, "re-run arrival clears the guard");
    }

    /// Regression: a slow startup final report (e.g. flaky scanimage -L
    /// with device: None) landing AFTER a manual re-run already delivered
    /// a device must not clobber it (device_known flipped off, header
    /// "no scanner", scanning re-locked, buffered scan dropped). The
    /// re-run's data is strictly newer — the stale startup report is
    /// ignored wholesale.
    #[tokio::test]
    async fn stale_startup_final_after_rerun_is_ignored() {
        let mut app = test_app();
        let (cmd_tx, mut cmd_rx) = mpsc::channel(8);

        // Startup fast report applied normally.
        apply_report(
            &mut app,
            report_with(None, crate::check::Status::Pending),
            &cmd_tx,
        )
        .await;
        assert!(!app.device_known);

        // User presses r: re-run finds the scanner and delivers it.
        apply_report(
            &mut app,
            report_with_source(
                Some(device("hpaio:/usb/x")),
                crate::check::Status::Ok,
                crate::check::ReportSource::ReRun,
            ),
            &cmd_tx,
        )
        .await;
        assert!(app.device_known);
        let _ = cmd_rx.recv().await; // SetDevice

        // The slow startup final report lands last with no device.
        apply_report(
            &mut app,
            report_with(None, crate::check::Status::Fail),
            &cmd_tx,
        )
        .await;
        assert!(
            app.device_known,
            "stale startup final must not wipe the re-run device"
        );
        assert_eq!(app.device_label, "Test Scanner");
        assert!(app.scan_allowed(), "scanning stays unlocked");
        // The stale report must not clobber the stored report either.
        assert_eq!(
            app.report.as_ref().map(|r| r.source),
            Some(crate::check::ReportSource::ReRun)
        );
        // And no second SetDevice/no actor downgrade was sent.
        assert!(
            cmd_rx.try_recv().is_err(),
            "stale report must not touch the actor"
        );
    }

    /// A startup fast report landing after a re-run is equally stale and
    /// must be dropped (its only effect would be a spurious diagnostics
    /// auto-open).
    #[tokio::test]
    async fn stale_startup_fast_after_rerun_is_ignored() {
        let mut app = test_app();
        apply_report(
            &mut app,
            report_with_source(
                Some(device("hpaio:/usb/x")),
                crate::check::Status::Ok,
                crate::check::ReportSource::ReRun,
            ),
            &mpsc::channel(1).0,
        )
        .await;
        assert!(app.device_known);
        app.overlay = None;

        apply_report(
            &mut app,
            report_with(None, crate::check::Status::Pending),
            &mpsc::channel(1).0,
        )
        .await;
        assert!(
            app.overlay.is_none(),
            "stale fast report must not auto-open diagnostics"
        );
        assert!(app.device_known);
    }

    /// When a stale StartupFinal is dropped while the exit verdict is
    /// still undecided, the re-run's (newer) data decides it: a failed
    /// re-run -> Some(false) so a quit afterwards doesn't exit 0.
    #[tokio::test]
    async fn stale_startup_final_lets_rerun_decide_verdict() {
        let mut app = test_app();

        // Failed re-run applied (no scanner anywhere); verdict undecided.
        apply_report(
            &mut app,
            report_with_source(
                None,
                crate::check::Status::Fail,
                crate::check::ReportSource::ReRun,
            ),
            &mpsc::channel(1).0,
        )
        .await;
        assert_eq!(app.startup_report_ok, None);

        // Stale startup final (also failed) lands last: dropped, but the
        // verdict must now be decided from the re-run's data.
        apply_report(
            &mut app,
            report_with(None, crate::check::Status::Fail),
            &mpsc::channel(1).0,
        )
        .await;
        assert_eq!(
            app.startup_report_ok,
            Some(false),
            "dropped stale final must not leave the verdict undecided on a failed machine"
        );
    }

    /// The verdict fallback never overrides an already-decided verdict.
    #[tokio::test]
    async fn stale_startup_final_does_not_override_decided_verdict() {
        let mut app = test_app();

        // Re-run found a scanner (healthy machine); verdict undecided.
        apply_report(
            &mut app,
            report_with_source(
                Some(device("hpaio:/usb/x")),
                crate::check::Status::Ok,
                crate::check::ReportSource::ReRun,
            ),
            &mpsc::channel(1).0,
        )
        .await;
        assert_eq!(app.startup_report_ok, None);

        // Stale failing startup final: dropped; the re-run data decides
        // -> Some(true).
        apply_report(
            &mut app,
            report_with(None, crate::check::Status::Fail),
            &mpsc::channel(1).0,
        )
        .await;
        assert_eq!(app.startup_report_ok, Some(true));
    }

    /// Mirrors the actor guard: a deferred delete (delete requested while
    /// scanning) blocks a new scan.
    #[test]
    fn scan_blocked_while_delete_pending() {
        let mut app = test_app();
        app.device_known = true;
        app.meta = Some(meta(false));
        let mut p = ready_page(1);
        p.status = PageStatus::DeletePending;
        app.pages = vec![p];
        assert!(!app.scan_allowed());
        assert!(!app.action_allowed(Action::Scan));
    }

    /// The dialog-in-flight flag is part of action_allowed(Finish): while a
    /// system save dialog is pending, `f` is blocked and the footer greys it.
    #[test]
    fn finish_blocked_while_dialog_in_flight() {
        let mut app = test_app();
        app.pages = vec![ready_page(1)];
        app.meta = Some(meta(false));
        assert!(app.action_allowed(Action::Finish));

        app.dialog_in_flight = true;
        assert!(
            !app.action_allowed(Action::Finish),
            "pending dialog must block a second `f` and grey the key"
        );

        app.dialog_in_flight = false;
        assert!(app.action_allowed(Action::Finish));
    }

    /// The Unavailable fallback never steals an overlay the user opened
    /// while the (blocking) system save dialog was pending; instead it
    /// reports via the status line. `f` remains available for a retry.
    #[tokio::test]
    async fn dialog_unavailable_preserves_open_overlay() {
        let mut app = test_app();
        app.pages = vec![ready_page(1)];
        app.meta = Some(meta(false));
        app.overlay = Some(Overlay::Confirm(Confirm::quit()));

        handle_dialog_result(
            &mut app,
            crate::backend::filedialog::SaveChoice::Unavailable,
            &mpsc::channel(1).0,
        )
        .await;
        let overlay_kind = match &app.overlay {
            Some(Overlay::Confirm(c)) => Some(c.kind.clone()),
            _ => None,
        };
        assert!(
            matches!(overlay_kind, Some(ConfirmKind::Quit)),
            "user-opened overlay must survive the Unavailable fallback"
        );
        assert!(!app.dialog_in_flight, "flag released");
        assert!(app.action_allowed(Action::Finish), "f available to retry");

        // With no overlay open, Unavailable installs the finish confirm.
        app.overlay = None;
        handle_dialog_result(
            &mut app,
            crate::backend::filedialog::SaveChoice::Unavailable,
            &mpsc::channel(1).0,
        )
        .await;
        assert!(matches!(app.overlay, Some(Overlay::Confirm(_))));
    }
}
