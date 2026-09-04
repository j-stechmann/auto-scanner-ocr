//! Session actor: owns the page list, runs jobs, enforces guards.
//!
//! Architecture (per the concurrency review + UX findings):
//! - The actor exclusively owns `Vec<Page>`; the TUI never touches it.
//! - TUI -> actor: `mpsc<Cmd>`. Actor -> TUI: `mpsc<Event>` (try_send; the
//!   UI is never allowed to block the actor).
//! - Long work runs as spawned JOBS (scan, per-page OCR, rotate, PDF
//!   build). The actor loop only selects on commands + job completions and
//!   never awaits a long operation inline, so commands (delete, cancel,
//!   new session, quit) are handled within microseconds while work runs.
//! - The scanner is the single serialized resource (`Busy::Scanning`).
//!   The capture job ends as soon as the image lands (plus optional
//!   unpaper), so the scanner is free while the page's preview OCR runs as
//!   its own job — the next scan may start immediately.
//! - Per-page preview OCR (tesseract txt for the text pane) never gates
//!   anything: the final PDF's text layer comes from ocrmypdf at finish.
//!   Under `preview_ocr = lazy` it only runs on demand for the viewed page.
//! - Every state change ships a `SessionMeta` snapshot with the pages so
//!   the footer/header always show current facts (busy badge, output path,
//!   dirty flag). Live elapsed timers are computed by the UI from
//!   `Instant`s — no actor round-trips for ticking.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::backend::pdf::{self, BuildOutcome};
use crate::backend::scan;
use crate::config::{Cleanup, Config, PreviewOcr};

/// Unique per-page id (never reused; reorder never renames files).
pub type PageId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageStatus {
    /// scanimage running.
    Scanning,
    /// unpaper/rotate/ocr running.
    Processing,
    /// Image ready (preview text may be absent under lazy/off OCR).
    Ready,
    /// Scan or processing failed (message in `error`).
    Failed,
    /// Deletion requested while busy; removed when the job ends.
    DeletePending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Scan,
    Clean,
    Ocr,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Scan => "scanning",
            Stage::Clean => "cleaning",
            Stage::Ocr => "ocr",
        }
    }
}

/// Full page state, owned by the actor.
#[derive(Debug, Clone)]
pub struct Page {
    pub id: PageId,
    pub status: PageStatus,
    pub dpi: u16,
    pub mode: String,
    pub image: Option<PathBuf>,
    pub text: Option<String>,
    /// True while a preview-OCR job for this page is running (the text pane
    /// shows "extracting text…"). Only set under lazy preview OCR.
    pub text_pending: bool,
    /// Generation of the image whose preview OCR last failed. Stops the lazy
    /// tick from re-requesting a hopeless extraction forever (persistent
    /// tesseract failure: missing language data, corrupt image). Keyed to
    /// `image_gen`, so any new image content (rescan/rotate) implicitly
    /// clears it. Never fails the page.
    pub ocr_failed_gen: Option<u32>,
    pub error: Option<String>,
    pub stage: Option<Stage>,
    /// When the current stage started (drives the live elapsed timer).
    pub stage_started: Option<Instant>,
    pub rotated: bool,
    /// True when this page's image was fully cleaned+deskewed by unpaper
    /// (legacy mode, gray/lineart, unpaper succeeded). Pages without it get
    /// deskew/clean from ocrmypdf at finish.
    pub unpaper_deskewed: bool,
    /// True when the scanner rejected the requested settings and a fallback
    /// attempt (dropping --mode and/or --resolution) succeeded instead; the
    /// recorded dpi may then differ from the actual image scale.
    pub used_fallback: bool,
    /// Bumped when the image content changes (rotate/rescan) so the preview
    /// worker knows to re-encode even at the same path.
    pub image_gen: u32,
}

/// Immutable view for the UI.
#[derive(Debug, Clone)]
pub struct PageView {
    pub id: PageId,
    pub status: PageStatus,
    pub stage: Option<Stage>,
    pub stage_started: Option<Instant>,
    pub image: Option<PathBuf>,
    pub image_gen: u32,
    pub text: Option<String>,
    pub text_pending: bool,
    pub ocr_failed_gen: Option<u32>,
    pub error: Option<String>,
    pub dpi: u16,
    pub mode: String,
    pub rotated: bool,
}

impl PageView {
    /// Stage label; the UI appends live elapsed seconds from `stage_started`.
    pub fn stage_label(&self) -> String {
        match self.status {
            PageStatus::Scanning => Stage::Scan.label().to_string(),
            PageStatus::Processing => self
                .stage
                .map(|s| s.label())
                .unwrap_or("processing")
                .to_string(),
            PageStatus::Ready => "ready".into(),
            PageStatus::Failed => "failed".into(),
            PageStatus::DeletePending => "deleting".into(),
        }
    }
}

/// Commands TUI -> actor.
#[derive(Debug)]
pub enum Cmd {
    /// Capture the next page with the given settings.
    ScanNext { dpi: u16, mode: String },
    /// Cancel an in-flight scan.
    CancelScan,
    /// Rescan page (non-destructive: old image kept until success).
    Rescan(PageId),
    /// Rotate page image 90° CW (false = CCW); re-OCRs only under eager
    /// preview OCR (lazy re-extracts on demand).
    Rotate(PageId, bool),
    /// Delete page (kills job if processing; deferred if scanning).
    Delete(PageId),
    /// Move page within the list (index-based).
    Move { from: usize, to: usize },
    /// Build the final PDF; actor refuses while busy.
    Finish,
    /// Reset the session (drop all pages, new output path).
    NewSession,
    /// Extract the selected page's preview text on demand (lazy mode).
    /// Sent by the TUI tick loop; the actor validates silently — this is
    /// never surfaced as "blocked" (the tick re-sends until it applies).
    RequestText(PageId),
    /// Query installed tesseract languages (reply via event).
    ListLangs,
    /// Deliver the SANE device resolved by background detection. Accepted
    /// silently and idempotently (re-application with the same name is a
    /// no-op; a different name replaces it with a status note; empty names
    /// are ignored so a failed re-detection can't downgrade a live device).
    SetDevice(String),
}

/// Session-level facts sent with every page snapshot so the UI's header and
/// footer are always current.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    /// Exclusive resources: the scanner (Scanning) and the PDF build.
    pub busy: Busy,
    /// When the current exclusive job started (UI shows elapsed time).
    pub busy_since: Option<Instant>,
    /// Number of per-page jobs (unpaper/OCR/rotate) currently running.
    pub jobs_running: usize,
    pub output_path: PathBuf,
    pub dirty: bool,
    /// True after a successful finish: the session dir is deleted (pages
    /// remain in the list as inert stubs), so preview-OCR requests are
    /// pointless. A *failed* build leaves the dir intact and this false.
    pub finished: bool,
}

/// Events actor -> TUI.
#[derive(Debug)]
pub enum Event {
    Pages {
        pages: Vec<PageView>,
        meta: SessionMeta,
    },
    Status(String),
    Finished {
        outcome: Option<BuildOutcome>,
        path: PathBuf,
        size_kb: u64,
    },
    Langs(Vec<String>),
}

/// Exclusive resources only. Per-page processing does NOT occupy this: the
/// scanner is idle while pages clean/OCR, so the next scan may start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Idle,
    Scanning,
    Finishing,
}

/// A long-running unit of work, executed as a spawned task.
enum Job {
    Scan {
        id: PageId,
        is_rescan: bool,
        dpi: u16,
        mode: String,
        path: PathBuf,
        /// Process settings: the job runs capture -> optional unpaper only;
        /// preview OCR runs as its own `Job::OcrText` afterwards so the
        /// scanner is free as soon as the capture ends.
        cleanup: Cleanup,
        unpaper_extra_args: Vec<String>,
        token: CancellationToken,
    },
    /// Per-page preview OCR for the TUI text pane (eager after capture or
    /// lazy on demand). Never gates the PDF build.
    OcrText {
        id: PageId,
        image: PathBuf,
        /// Set when the job was spawned; echoed back so the completion
        /// handler can verify the page's image is unchanged. Rarely needed:
        /// guards block rescan/rotate while an OCR job runs, and finish
        /// only unblocks them after cancelling the job — but the cancelled
        /// job's completion can still arrive after a post-finish rescan
        /// bumped the gen.
        image_gen: u32,
        langs: String,
        dir: PathBuf,
        token: CancellationToken,
    },
    Rotate {
        id: PageId,
        image: PathBuf,
        cw: bool,
        /// True when the rotated image should be re-OCRed for the text pane
        /// (eager preview). False: text is invalidated and re-extracted on
        /// demand under lazy mode (skipped entirely under off).
        reocr: bool,
        langs: String,
        dir: PathBuf,
        token: CancellationToken,
    },
    Finish {
        plan: pdf::BuildPlan,
    },
}

/// What a finished job reports back to the actor.
enum JobDone {
    Scan {
        id: PageId,
        is_rescan: bool,
        /// Final image path (possibly the unpaper `_clean` variant).
        image: PathBuf,
        /// Legacy-unpaper fully cleaned this page (see Page::unpaper_deskewed).
        unpaper_deskewed: bool,
        /// Scanner rejected requested settings; fallback attempt succeeded.
        used_fallback: bool,
        result: anyhow::Result<()>,
    },
    OcrText {
        id: PageId,
        /// Image the text was extracted from, plus the `image_gen` at spawn
        /// time: completions whose generation no longer matches the page are
        /// treated as stale and dropped. Mostly unreachable (`jobs.contains_key`
        /// blocks rescan/rotate while an OCR job runs), but reachable after a
        /// finish: the cancelled job's completion can arrive once the build is
        /// done and a rescan has already bumped the gen.
        image: PathBuf,
        image_gen: u32,
        result: anyhow::Result<String>,
    },
    Rotate {
        id: PageId,
        /// Ok(Some(text)) = rotated + re-OCRed; Ok(None) = rotated without
        /// re-OCR (text invalidated); Err(msg) = failed/cancelled.
        reocr: bool,
        result: Result<Option<String>, String>,
    },
    Finish {
        result: anyhow::Result<BuildOutcome>,
    },
}

pub struct Session {
    cfg: Config,
    device: String,
    dir: PathBuf,
    /// Where `sessions/` lives; resolved once at construction so `new_session`
    /// never re-reads the environment mid-session.
    sessions_root: PathBuf,
    /// flock on `<dir>/.lock`, held for the session's lifetime. The kernel
    /// releases it on process death (crash, signal) without running any
    /// code — that is the ownership marker the startup sweep tests. Closing
    /// (dropping) the fd releases it on graceful paths.
    lock: std::fs::File,
    out_pdf: PathBuf,
    pages: Vec<Page>,
    next_id: PageId,
    busy: Busy,
    busy_since: Option<Instant>,
    /// Scan job cancellation (single scanner resource).
    scan_token: Option<CancellationToken>,
    /// Per-page job cancellation tokens (preview OCR/rotate). Keyed by page
    /// id; a token can be superseded by a newer job for the same page, so
    /// completion handlers remove entries only on token identity match.
    jobs: HashMap<PageId, CancellationToken>,
    /// True after a successful finish removed the session dir: pages remain
    /// in the list but their images are gone, so preview-OCR requests must
    /// not spawn. Reset by `new_session` (and never set on a failed build,
    /// which keeps the dir — lazy OCR resumes there).
    finished: bool,
    event_tx: mpsc::Sender<Event>,
    job_tx: mpsc::UnboundedSender<JobDone>,
}

impl Session {
    fn with_channels(
        cfg: Config,
        device: String,
        event_tx: mpsc::Sender<Event>,
        job_tx: mpsc::UnboundedSender<JobDone>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&cfg.output)?;
        let out_pdf = pdf::reserve_output_path(&cfg.output, pdf::stamp_now())?;
        let sessions_root = state_dir().join("sessions");
        let (dir, lock) = match create_session_dir(&sessions_root) {
            Ok(v) => v,
            Err(e) => {
                // No Drop runs when construction fails: release the output
                // reservation by hand or every failed launch leaks a
                // zero-byte placeholder into the output dir.
                pdf::release_reservation(&out_pdf);
                return Err(e);
            }
        };
        Ok(Self {
            cfg,
            device,
            dir,
            sessions_root,
            lock,
            out_pdf,
            pages: Vec::new(),
            next_id: 1,
            busy: Busy::Idle,
            busy_since: None,
            scan_token: None,
            jobs: HashMap::new(),
            finished: false,
            event_tx,
            job_tx,
        })
        .inspect(Self::record_reservation)
    }

    // ------------------------------------------------------------- helpers

    /// Record the output reservation in the session dir. The marker makes
    /// the reservation recoverable by the startup sweep under EVERY death
    /// mode (crash/SIGKILL/OOM included): owner dead => zero-byte
    /// placeholder released, non-empty file (the built PDF) kept. Written
    /// at reservation time, not at quit time — a hard kill runs no code.
    /// Removed with the dir on a successful finish / normal teardown.
    fn record_reservation(&self) {
        let path = self
            .out_pdf
            .canonicalize()
            .unwrap_or_else(|_| self.out_pdf.clone());
        if let Err(e) = std::fs::write(
            self.dir.join(".pending_out"),
            path.to_string_lossy().as_bytes(),
        ) {
            // Best effort by design, but a failed marker means a hard kill
            // later will leak the zero-byte placeholder (the sweep has
            // nothing to read): make that diagnosable.
            tracing::warn!(
                "could not record output reservation in {}: {e} \
                 (a hard kill would leak the placeholder)",
                self.dir.display()
            );
        }
    }

    fn push(&self, ev: Event) {
        let _ = self.event_tx.try_send(ev);
    }

    fn notify_pages(&self) {
        self.push(Event::Pages {
            pages: self.views(),
            meta: self.meta(),
        });
    }

    fn status(&self, msg: impl Into<String>) {
        self.push(Event::Status(msg.into()));
    }

    pub fn views(&self) -> Vec<PageView> {
        self.pages
            .iter()
            .map(|p| PageView {
                id: p.id,
                status: p.status,
                stage: p.stage,
                stage_started: p.stage_started,
                image: p.image.clone(),
                image_gen: p.image_gen,
                text: p.text.clone(),
                text_pending: p.text_pending,
                ocr_failed_gen: p.ocr_failed_gen,
                error: p.error.clone(),
                dpi: p.dpi,
                mode: p.mode.clone(),
                rotated: p.rotated,
            })
            .collect()
    }

    pub fn meta(&self) -> SessionMeta {
        SessionMeta {
            busy: self.busy,
            busy_since: self.busy_since,
            jobs_running: self.jobs.len(),
            output_path: self.out_pdf.clone(),
            dirty: self.dirty(),
            finished: self.finished,
        }
    }

    /// True while quitting could still lose scan results: work in flight
    /// or pages holding captured images. Mirrors the quit-confirm rule
    /// (`App::needs_quit_confirm`): failed pages count as contentless
    /// (quitting deletes their files along with the session dir, and the
    /// dialog ignores them too), and a finished session holds only inert
    /// stubs of a built PDF.
    pub fn dirty(&self) -> bool {
        if self.finished {
            return false;
        }
        self.busy != Busy::Idle
            || !self.jobs.is_empty()
            || self.pages.iter().any(|p| {
                matches!(
                    p.status,
                    PageStatus::Scanning | PageStatus::Processing | PageStatus::Ready
                )
            })
    }

    pub fn output_path(&self) -> &std::path::Path {
        &self.out_pdf
    }

    // -------------------------------------------------------------- guards

    /// UX guards: Err(reason) is shown in the status line; the footer greys
    /// the corresponding key. Per-page processing does NOT block scanning —
    /// the scanner is idle while pages clean/OCR (parity with the Python
    /// tool's background processing).
    pub fn guard(&self, cmd: &Cmd) -> Result<(), String> {
        match cmd {
            Cmd::ScanNext { .. } => {
                if self.device.is_empty() {
                    // finished + empty device is unreachable through real
                    // state transitions (a build needs pages needs a
                    // device); the branch is defensive so a future state
                    // bug can't produce a misleading message.
                    return Err(match self.finished {
                        true => "no scanner detected".into(),
                        false => "scanner detection still running".into(),
                    });
                }
                if self.busy == Busy::Scanning {
                    return Err("scanner busy - press Esc to cancel".into());
                }
                if self.busy == Busy::Finishing {
                    return Err("building PDF - scan once it finishes".into());
                }
                if self.finished {
                    return Err("PDF already built - press n for a new session".into());
                }
                if self
                    .pages
                    .iter()
                    .any(|p| p.status == PageStatus::DeletePending)
                {
                    return Err("waiting for deferred delete".into());
                }
                Ok(())
            }
            Cmd::Rescan(id) => {
                if self.device.is_empty() {
                    // Defensive: see the ScanNext guard note.
                    return Err(match self.finished {
                        true => "no scanner detected".into(),
                        false => "scanner detection still running".into(),
                    });
                }
                if self.busy == Busy::Scanning {
                    return Err("scanner busy - press Esc to cancel".into());
                }
                if self.busy == Busy::Finishing {
                    return Err("building PDF - rescan once it finishes".into());
                }
                if self.finished {
                    return Err("PDF already built - press n for a new session".into());
                }
                if self.jobs.contains_key(id) {
                    return Err("page busy - rescan once its text is extracted".into());
                }
                match self.pages.iter().find(|p| p.id == *id).map(|p| p.status) {
                    Some(PageStatus::Ready) | Some(PageStatus::Failed) => Ok(()),
                    Some(_) => Err("page busy - rescan after it finishes".into()),
                    None => Err("no such page".into()),
                }
            }
            Cmd::Rotate(id, _) => {
                if self.busy == Busy::Finishing {
                    return Err("building PDF - rotate once it finishes".into());
                }
                if self.finished {
                    return Err("PDF already built - press n for a new session".into());
                }
                if self.jobs.contains_key(id) {
                    return Err("page busy - rotate after its text is extracted".into());
                }
                match self.pages.iter().find(|p| p.id == *id).map(|p| p.status) {
                    Some(PageStatus::Ready) => Ok(()),
                    Some(_) => Err("only ready pages can be rotated".into()),
                    None => Err("no such page".into()),
                }
            }
            Cmd::CancelScan => {
                if self.busy == Busy::Scanning {
                    Ok(())
                } else {
                    Err("no scan in progress".into())
                }
            }
            Cmd::Delete(_) | Cmd::Move { .. } => {
                // Files must stay put while the build reads them.
                if self.busy == Busy::Finishing {
                    return Err("building PDF - wait for it to finish".into());
                }
                Ok(())
            }
            Cmd::Finish => {
                if self.pages.is_empty() {
                    return Err("no pages scanned yet".into());
                }
                if self.busy == Busy::Finishing {
                    return Err("already building".into());
                }
                if self.busy == Busy::Scanning {
                    return Err("scan in progress - finish after it completes".into());
                }
                if self.finished {
                    return Err("PDF already built - press n for a new session".into());
                }
                if let Some(p) = self.pages.iter().find(|p| p.status == PageStatus::Failed) {
                    return Err(format!(
                        "page {} failed - rescan (r) or delete (d) first",
                        p.id
                    ));
                }
                // Preview OCR for the text pane never gates the build: the
                // PDF's text layer comes from ocrmypdf at finish. Only
                // rotate/unpaper work (stage != Ocr) must complete first.
                if let Some(p) = self.pages.iter().find(|p| {
                    matches!(p.status, PageStatus::Processing)
                        && p.stage.is_some_and(|s| s != Stage::Ocr)
                }) {
                    let stage = p.stage.map(|s| s.label()).unwrap_or("processing");
                    return Err(format!("page {} still {stage} - finish once done", p.id));
                }
                if self
                    .pages
                    .iter()
                    .any(|p| p.status == PageStatus::DeletePending)
                {
                    return Err("waiting for deferred delete".into());
                }
                Ok(())
            }
            Cmd::NewSession => {
                if self.busy == Busy::Idle && self.jobs.is_empty() {
                    Ok(())
                } else {
                    Err("busy - try again after current work finishes".into())
                }
            }
            // Validated silently in the handler; never "blocked" — the lazy
            // tick re-sends until the request applies.
            Cmd::RequestText(_) => Ok(()),
            Cmd::ListLangs => Ok(()),
            // Applied silently in the handler (idempotent); never "blocked".
            Cmd::SetDevice(_) => Ok(()),
        }
    }

    // -------------------------------------------------------- command loop

    pub async fn handle(&mut self, cmd: Cmd) {
        if let Err(reason) = self.guard(&cmd) {
            self.status(format!("blocked: {reason}"));
            return;
        }
        match cmd {
            Cmd::ScanNext { dpi, mode } => self.start_scan(dpi, mode, None),
            Cmd::CancelScan => self.cancel_scan(),
            Cmd::Rescan(id) => {
                if let Some(p) = self.pages.iter().find(|p| p.id == id) {
                    self.start_scan(p.dpi, p.mode.clone(), Some(id));
                }
            }
            Cmd::Rotate(id, cw) => self.start_rotate(id, cw),
            Cmd::Delete(id) => self.delete(id),
            Cmd::Move { from, to } => self.move_page(from, to),
            Cmd::Finish => self.start_finish(),
            Cmd::NewSession => self.new_session(),
            Cmd::RequestText(id) => self.request_text(id),
            Cmd::ListLangs => {
                let langs = scan::available_langs().await.unwrap_or_default();
                self.push(Event::Langs(langs));
            }
            Cmd::SetDevice(name) => self.set_device(name),
        }
    }

    /// Apply a resolved device (background detection). Silent + idempotent:
    /// same name = no-op; a different name replaces it (user re-ran
    /// diagnostics after swapping scanners) with a status note; empty is
    /// ignored so a failed detection can never clear a working device.
    /// Also pushes a Pages snapshot so the UI observes readiness via meta.
    fn set_device(&mut self, name: String) {
        if name.is_empty() || name == self.device {
            return;
        }
        let replaced = !self.device.is_empty();
        self.device = name;
        tracing::info!("session device set: {}", self.device);
        if replaced {
            self.status(format!("scanner changed: {}", self.device));
        } else {
            self.status(format!("scanner ready: {}", self.device));
        }
        self.notify_pages();
    }

    /// A job finished; update state and chain the next stage.
    pub(self) async fn handle_job_done(&mut self, done: JobDone) {
        match done {
            JobDone::Scan {
                id,
                is_rescan,
                image,
                unpaper_deskewed,
                used_fallback,
                result,
            } => self.on_scan_done(
                id,
                is_rescan,
                image,
                unpaper_deskewed,
                used_fallback,
                result,
            ),
            JobDone::OcrText {
                id,
                image,
                image_gen,
                result,
            } => self.on_ocr_text_done(id, image, image_gen, result),
            JobDone::Rotate { id, reocr, result } => self.on_rotate_done(id, reocr, result),
            JobDone::Finish { result } => self.on_finish_done(result),
        }
    }

    // ------------------------------------------------------------- actions

    /// Start a scan job (new page when `rescan_of` is None, else rescan).
    fn start_scan(&mut self, dpi: u16, mode: String, rescan_of: Option<PageId>) {
        self.busy = Busy::Scanning;
        self.busy_since = Some(Instant::now());
        let token = CancellationToken::new();
        self.scan_token = Some(token.clone());

        match rescan_of {
            None => {
                let id = self.next_id;
                self.next_id += 1;
                let path = self.dir.join(format!("page_{id:03}.png"));
                self.pages.push(Page {
                    id,
                    status: PageStatus::Scanning,
                    dpi,
                    mode: mode.clone(),
                    image: None,
                    text: None,
                    text_pending: false,
                    ocr_failed_gen: None,
                    error: None,
                    stage: Some(Stage::Scan),
                    stage_started: Some(Instant::now()),
                    rotated: false,
                    unpaper_deskewed: false,
                    used_fallback: false,
                    image_gen: 0,
                });
                self.notify_pages();
                self.status(format!(
                    "scanning page {} ({dpi}dpi {mode})…",
                    self.pages.len()
                ));
                self.spawn_job(Job::Scan {
                    id,
                    is_rescan: false,
                    dpi,
                    mode,
                    path,
                    cleanup: self.cfg.cleanup,
                    unpaper_extra_args: self.cfg.unpaper_extra_args.clone(),
                    token,
                });
            }
            Some(id) => {
                let path = self.dir.join(format!("page_{id:03}.rescan.png"));
                if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                    p.status = PageStatus::Scanning;
                    p.stage = Some(Stage::Scan);
                    p.stage_started = Some(Instant::now());
                    p.error = None;
                    p.image_gen += 1;
                    p.ocr_failed_gen = None;
                }
                self.notify_pages();
                self.status(format!("rescanning page {id} ({dpi}dpi {mode})…"));
                self.spawn_job(Job::Scan {
                    id,
                    is_rescan: true,
                    dpi,
                    mode,
                    path,
                    cleanup: self.cfg.cleanup,
                    unpaper_extra_args: self.cfg.unpaper_extra_args.clone(),
                    token,
                });
            }
        }
    }

    fn spawn_job(&self, job: Job) {
        let job_tx = self.job_tx.clone();
        let device = self.device.clone();
        tokio::spawn(async move {
            let done = match job {
                Job::Scan {
                    id,
                    is_rescan,
                    dpi,
                    mode,
                    path,
                    cleanup,
                    unpaper_extra_args,
                    token,
                } => {
                    // Capture -> optional unpaper only; preview OCR runs as
                    // its own job (Job::OcrText) so the scanner is free as
                    // soon as the capture ends.
                    let result = scan::scan_page(&device, dpi, &mode, &path, &token).await;
                    match result {
                        Ok(scan::ScanOutcome { used_fallback }) => {
                            // The backend already warns on fallback scans
                            // with the metadata caveat; the status line at
                            // JobDone handling surfaces it to the UI.
                            let (image, unpaper_deskewed) =
                                pdf::maybe_unpaper(&path, cleanup, &unpaper_extra_args, &mode)
                                    .await;
                            JobDone::Scan {
                                id,
                                is_rescan,
                                image,
                                unpaper_deskewed,
                                used_fallback,
                                result: Ok(()),
                            }
                        }
                        Err(e) => JobDone::Scan {
                            id,
                            is_rescan,
                            image: path,
                            unpaper_deskewed: false,
                            used_fallback: false,
                            result: Err(e),
                        },
                    }
                }
                Job::OcrText {
                    id,
                    image,
                    image_gen,
                    langs,
                    dir,
                    token,
                } => {
                    let result = scan::ocr_text_cancellable(&image, &langs, &dir, &token).await;
                    JobDone::OcrText {
                        id,
                        image,
                        image_gen,
                        result,
                    }
                }
                Job::Rotate {
                    id,
                    image,
                    cw,
                    reocr,
                    langs,
                    dir,
                    token,
                } => {
                    let result = match pdf::rotate_png(&image, cw).await {
                        Ok(()) if reocr => {
                            if token.is_cancelled() {
                                Err("cancelled".to_string())
                            } else {
                                match scan::ocr_text_cancellable(&image, &langs, &dir, &token).await
                                {
                                    Ok(text) => Ok(Some(text)),
                                    Err(e) if e.to_string() == "cancelled" => {
                                        Err("cancelled".into())
                                    }
                                    Err(e) => Err(format!("re-OCR failed: {e:#}")),
                                }
                            }
                        }
                        Ok(()) => Ok(None),
                        Err(e) => Err(format!("rotate failed: {e:#}")),
                    };
                    JobDone::Rotate { id, reocr, result }
                }
                Job::Finish { plan } => JobDone::Finish {
                    result: pdf::build_pdf(&plan).await,
                },
            };
            let _ = job_tx.send(done);
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn on_scan_done(
        &mut self,
        id: PageId,
        is_rescan: bool,
        image: PathBuf,
        unpaper_deskewed: bool,
        used_fallback: bool,
        result: anyhow::Result<()>,
    ) {
        self.scan_token = None;
        self.busy = Busy::Idle;
        self.busy_since = None;

        match result {
            Ok(()) => {
                // Deferred delete requested while scanning?
                if self.finish_delete_if_pending(id) {
                    self.status("scan cancelled and page deleted");
                    self.notify_pages();
                    return;
                }
                // Page gone entirely (double-delete race): nothing to do.
                if !self.pages.iter().any(|p| p.id == id) {
                    self.notify_pages();
                    return;
                }
                if is_rescan {
                    // Remove the old images only now (non-destructive rescan).
                    let old_image = self
                        .pages
                        .iter()
                        .find(|p| p.id == id)
                        .and_then(|p| p.image.clone());
                    if let Some(img) = old_image {
                        let _ = std::fs::remove_file(&img);
                        let _ = std::fs::remove_file(clean_variant(&img));
                    }
                }
                // The capture is done; the text pane content will be
                // refreshed by the OCR job (or left empty under lazy/off).
                let image_gen = {
                    let p = self
                        .pages
                        .iter_mut()
                        .find(|p| p.id == id)
                        .expect("page exists (checked above)");
                    p.image = Some(image.clone());
                    p.unpaper_deskewed = unpaper_deskewed;
                    p.used_fallback = used_fallback;
                    p.text = None;
                    p.text_pending = false;
                    p.ocr_failed_gen = None;
                    p.image_gen += 1;
                    p.image_gen
                };
                match self.cfg.preview_ocr {
                    PreviewOcr::Eager => {
                        // Transition to the OCR stage (own elapsed timer) and
                        // spawn the preview-OCR job; the page completes when
                        // it finishes. The scanner is already free.
                        if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                            p.status = PageStatus::Processing;
                            p.stage = Some(Stage::Ocr);
                            p.stage_started = Some(Instant::now());
                        }
                        let token = CancellationToken::new();
                        self.jobs.insert(id, token.clone());
                        self.spawn_job(Job::OcrText {
                            id,
                            image,
                            image_gen,
                            langs: self.cfg.langs.clone(),
                            dir: self.dir.clone(),
                            token,
                        });
                    }
                    PreviewOcr::Lazy | PreviewOcr::Off => {
                        // Straight to Ready: no transient processing frame.
                        if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                            p.status = PageStatus::Ready;
                            p.stage = None;
                            p.stage_started = None;
                        }
                    }
                }
                self.notify_pages();
                // Only the scanner fallback needs surfacing here: unpaper
                // skips are per config (off/conservative by design, legacy
                // color pages because unpaper is grayscale-only) or logged
                // as warnings by the backend, and ocrmypdf always deskews
                // at finish.
                let note = if used_fallback {
                    " - scanner rejected resolution/mode; page size may differ"
                } else {
                    ""
                };
                // Under eager the "ready" line comes from on_ocr_text_done;
                // here the capture itself just finished.
                if self.cfg.preview_ocr == PreviewOcr::Eager {
                    self.status(format!("page {id} captured{note}"));
                } else {
                    self.status(format!("page {id} ready{note}"));
                }
            }
            Err(e) => {
                let cancelled = e.to_string() == "cancelled";
                if cancelled {
                    if is_rescan {
                        // Rescan keeps the old page.
                        if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                            p.status = PageStatus::Ready;
                            p.stage = None;
                            p.stage_started = None;
                        }
                        self.status("rescan cancelled; old page kept");
                        self.notify_pages();
                    } else {
                        self.pages.retain(|p| p.id != id);
                        self.status("scan cancelled");
                        self.notify_pages();
                    }
                } else {
                    if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                        p.status = PageStatus::Failed;
                        p.error = Some(format!("{e:#}"));
                        p.stage = None;
                        p.stage_started = None;
                    }
                    self.status(format!(
                        "{} failed: {e:#}",
                        if is_rescan { "rescan" } else { "scan" }
                    ));
                    self.notify_pages();
                }
            }
        }
    }

    /// On-demand preview OCR (lazy mode): extract the text for one page.
    /// Validates silently — the TUI tick re-sends until the request
    /// applies, so nothing here may push "blocked" status lines.
    fn request_text(&mut self, id: PageId) {
        if self.cfg.preview_ocr != PreviewOcr::Lazy
            || self.busy == Busy::Finishing
            // Post-finish the session dir is deleted; page stubs must not
            // respawn tesseract (the TUI tick keeps sending until this
            // guard's meta twin stops it in the UI).
            || self.finished
        {
            return;
        }
        // Cap concurrent OCR jobs (flipping through many pages must not
        // spawn unbounded tesseracts); the tick re-requests later.
        if self.jobs.len() >= 2 {
            tracing::debug!("preview OCR request deferred: job cap reached");
            return;
        }
        if self.jobs.contains_key(&id) {
            return;
        }
        let Some(page) = self.pages.iter().find(|p| p.id == id) else {
            return;
        };
        if page.status != PageStatus::Ready
            || page.text.is_some()
            || page.text_pending
            // A previous attempt for this exact image failed (missing
            // language data, corrupt file, …): don't respawn tesseract on
            // every tick. A rescan/rotate bumps image_gen and re-arms.
            || page.ocr_failed_gen == Some(page.image_gen)
        {
            return;
        }
        // The image must still exist on disk. After a successful finish the
        // session dir is deleted but the pages linger in the list; without
        // this check the lazy tick would respawn tesseract on the missing
        // file forever (failing + spamming status ~4x/sec).
        let image = match page.image.as_ref() {
            Some(img) if img.exists() => img.clone(),
            _ => return,
        };
        let image_gen = page.image_gen;
        if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
            p.text_pending = true;
        }
        let token = CancellationToken::new();
        self.jobs.insert(id, token.clone());
        self.notify_pages();
        tracing::debug!("preview OCR requested for page {id}");
        self.spawn_job(Job::OcrText {
            id,
            image,
            image_gen,
            langs: self.cfg.langs.clone(),
            dir: self.dir.clone(),
            token,
        });
    }

    /// Preview-OCR job finished. Invariants (per the concurrency review):
    /// - cancelled jobs never fail the page;
    /// - OCR errors never set Failed (the PDF text layer is independent);
    /// - stale completions (image/gen mismatch) are dropped — rare: guards
    ///   block rescan/rotate while an OCR job runs, but a finish cancels
    ///   the job and a post-finish rescan can bump the gen before the
    ///   cancelled job's completion arrives;
    /// - the jobs entry is removed only when this completion still matches
    ///   the page (guards keep at most one OCR job per page alive, so the
    ///   generation check identifies the entry's owner).
    fn on_ocr_text_done(
        &mut self,
        id: PageId,
        image: PathBuf,
        image_gen: u32,
        result: anyhow::Result<String>,
    ) {
        // Remove the jobs entry unless the page still exists with a
        // different generation (that means a newer job for the page owns
        // the entry; guards keep at most one job per page alive).
        let page_gen = self.pages.iter().find(|p| p.id == id).map(|p| p.image_gen);
        match page_gen {
            None => {
                // Page deleted meanwhile: the entry can't belong to anyone
                // else; leaving it would block NewSession forever.
                self.jobs.remove(&id);
            }
            Some(gen) if gen == image_gen => {
                self.jobs.remove(&id);
            }
            Some(_) => {}
        }
        if self.finish_delete_if_pending(id) {
            self.status("page deleted");
            self.notify_pages();
            return;
        }
        let Some(p) = self.pages.iter_mut().find(|p| p.id == id) else {
            return;
        };
        let current = p.image.as_deref() == Some(image.as_path()) && p.image_gen == image_gen;
        // Finish the OCR stage transition unless the page moved on.
        let finish_stage = |p: &mut Page| {
            p.text_pending = false;
            if p.status == PageStatus::Processing && p.stage == Some(Stage::Ocr) {
                p.status = PageStatus::Ready;
                p.stage = None;
                p.stage_started = None;
            }
        };
        match result {
            Ok(text) if current => {
                p.text = Some(text);
                let was_processing = p.status == PageStatus::Processing;
                finish_stage(p);
                if was_processing && self.busy != Busy::Finishing {
                    self.status(format!("page {id} ready"));
                }
            }
            Ok(_) => {
                // Stale success (image changed meanwhile): drop the result.
                tracing::debug!("dropping stale preview OCR result for page {id}");
                finish_stage(p);
            }
            Err(e) if e.to_string() == "cancelled" => {
                // Cancelled (finish/delete): complete the stage transition
                // but never fail the page.
                finish_stage(p);
            }
            Err(e) if current => {
                // Non-cancelled OCR failure: page still becomes ready with
                // no text — preview text must not block anything. Record
                // the failed generation so the lazy tick doesn't retry the
                // same hopeless image forever (status spam).
                finish_stage(p);
                p.ocr_failed_gen = Some(image_gen);
                self.status(format!("preview OCR failed for page {id}: {e:#}"));
            }
            Err(_) => {
                // Stale failure: image changed meanwhile; nothing to do.
                finish_stage(p);
            }
        }
        self.notify_pages();
    }

    fn start_rotate(&mut self, id: PageId, cw: bool) {
        let Some(page) = self.pages.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let Some(image) = page.image.clone() else {
            return;
        };
        let reocr = self.cfg.preview_ocr == PreviewOcr::Eager;
        if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
            p.status = PageStatus::Processing;
            p.stage = Some(Stage::Clean);
            p.stage_started = Some(Instant::now());
            // Under lazy/off the text pane is not re-OCRed; drop the stale
            // pre-rotation text here (on_rotate_done only overwrites text
            // when a re-OCR actually ran). Lazy re-extracts on demand.
            if !reocr {
                p.text = None;
                p.text_pending = false;
                // New image content: re-arm the lazy auto-retry.
                p.ocr_failed_gen = None;
            }
        }
        let token = CancellationToken::new();
        self.jobs.insert(id, token.clone());
        self.notify_pages();
        self.spawn_job(Job::Rotate {
            id,
            image,
            cw,
            reocr,
            langs: self.cfg.langs.clone(),
            dir: self.dir.clone(),
            token,
        });
    }

    fn on_rotate_done(&mut self, id: PageId, reocr: bool, result: Result<Option<String>, String>) {
        self.jobs.remove(&id);
        if self.finish_delete_if_pending(id) {
            self.status("page deleted");
            self.notify_pages();
            return;
        }
        if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
            match result {
                Ok(text) => {
                    p.status = PageStatus::Ready;
                    p.stage = None;
                    p.stage_started = None;
                    p.rotated = true;
                    p.image_gen += 1;
                    // Re-OCR ran: refresh the pane text (Ok(None) = tesseract
                    // found nothing). Without re-OCR the text was already
                    // invalidated in start_rotate; lazy mode re-extracts it
                    // on demand from the rotated image.
                    if reocr && text.is_some() {
                        p.text = text;
                    }
                }
                Err(e) if e == "cancelled" => {
                    p.status = PageStatus::Ready;
                    p.stage = None;
                    p.stage_started = None;
                }
                Err(e) => {
                    p.status = PageStatus::Failed;
                    p.error = Some(e);
                    p.stage = None;
                    p.stage_started = None;
                }
            }
        }
        self.notify_pages();
        self.status(format!("page {id} rotated"));
    }

    fn start_finish(&mut self) {
        self.busy = Busy::Finishing;
        self.busy_since = Some(Instant::now());
        // Cancel any outstanding per-page jobs: at this point every `jobs`
        // entry is a preview-OCR token (a live rotate job would have left
        // the page in Processing(stage=Clean), which the Finish guard
        // rejects). The token cancel kills the tesseract child; removing
        // the entries keeps jobs_running clean during the build and makes
        // the later remove_dir_all race-free.
        for (_, token) in self.jobs.drain() {
            token.cancel();
        }
        self.notify_pages();
        self.status("building searchable PDF…");

        // Per-page DPI (pages may be scanned at different resolutions within
        // one session via the +/- presets); (image path, dpi) pairs.
        let pages: Vec<(PathBuf, u16)> = self
            .pages
            .iter()
            .filter_map(|p| p.image.clone().map(|img| (img, p.dpi)))
            .collect();
        // Pages NOT fully cleaned+deskewed by unpaper (cleanup off/failed,
        // color pages in legacy mode): ocrmypdf deskews/cleans them.
        let any_page_needing_cleanup = self.pages.iter().any(|p| !p.unpaper_deskewed);
        let manually_rotated = self.pages.iter().any(|p| p.rotated);

        let plan = pdf::BuildPlan {
            pages,
            any_page_needing_cleanup,
            manually_rotated,
            langs: self.cfg.langs.clone(),
            out_pdf: self.out_pdf.clone(),
        };
        self.spawn_job(Job::Finish { plan });
    }

    fn on_finish_done(&mut self, result: anyhow::Result<BuildOutcome>) {
        self.busy = Busy::Idle;
        self.busy_since = None;
        match &result {
            Ok(outcome) => {
                let size = pdf::size_kb(&self.out_pdf);
                let _ = std::fs::remove_dir_all(&self.dir);
                // The dir is gone: mark the session finished so the lazy
                // tick stops re-requesting preview text for the lingering
                // page stubs.
                self.finished = true;
                self.status(format!(
                    "done: {} ({} KB) - o to open",
                    self.out_pdf.display(),
                    size
                ));
                self.push(Event::Finished {
                    outcome: Some(*outcome),
                    path: self.out_pdf.clone(),
                    size_kb: size,
                });
            }
            Err(e) => {
                self.status(format!("PDF build failed: {e:#}"));
                self.push(Event::Finished {
                    outcome: None,
                    path: self.out_pdf.clone(),
                    size_kb: 0,
                });
            }
        }
        self.notify_pages();
    }

    /// If the page was marked DeletePending, finish the deletion now.
    /// Returns true when the page was removed.
    fn finish_delete_if_pending(&mut self, id: PageId) -> bool {
        let was_pending = self
            .pages
            .iter()
            .find(|p| p.id == id)
            .is_some_and(|p| p.status == PageStatus::DeletePending);
        if was_pending {
            if let Some(page) = self.pages.iter().find(|p| p.id == id).cloned() {
                remove_page_files(&page);
            }
            self.pages.retain(|p| p.id != id);
        }
        was_pending
    }

    fn delete(&mut self, id: PageId) {
        let Some(page) = self.pages.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        match page.status {
            PageStatus::Scanning => {
                // Defer; cancel the scan so it ends promptly.
                if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                    p.status = PageStatus::DeletePending;
                }
                if let Some(t) = &self.scan_token {
                    t.cancel();
                }
                self.notify_pages();
                self.status("deleting page after scan ends…");
            }
            PageStatus::Processing | PageStatus::DeletePending => {
                // Cancel that page's job; its completion handler finishes
                // the deletion (and the job checks the pending flag).
                // DeletePending: a second `d` — just re-cancel the token.
                let was_processing = page.status == PageStatus::Processing;
                if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                    p.status = PageStatus::DeletePending;
                }
                if let Some(t) = self.jobs.get(&id) {
                    t.cancel();
                }
                self.notify_pages();
                if was_processing {
                    self.status("deleting page…");
                }
            }
            _ => {
                // Ready/Failed page — but under lazy preview OCR a job may
                // still be running for it: cancel so no tesseract writes
                // into the session dir after (or races) the build.
                if let Some(t) = self.jobs.get(&id) {
                    t.cancel();
                    self.jobs.remove(&id);
                }
                remove_page_files(&page);
                self.pages.retain(|p| p.id != id);
                self.notify_pages();
                self.status(format!("page {id} deleted"));
            }
        }
    }

    fn move_page(&mut self, from: usize, to: usize) {
        if from >= self.pages.len() || to >= self.pages.len() || from == to {
            return;
        }
        let p = self.pages.remove(from);
        self.pages.insert(to, p);
        self.notify_pages();
        self.status(format!("moved page {} → {}", from + 1, to + 1));
    }

    fn cancel_scan(&mut self) {
        if let Some(t) = &self.scan_token {
            t.cancel();
            self.status("cancelling scan…");
        }
    }

    fn new_session(&mut self) {
        // Acquire the new (dir, lock) and reserve the new output path FIRST,
        // BEFORE touching the old session: on any I/O failure the old session
        // stays fully intact (pages, dir, lock, reservation) and the error is
        // surfaced in the status line instead of unwinding (a panic here would
        // kill the actor task and leave the TUI running against a dead
        // channel — and the old dir would be deleted by the unwinding Drop).
        let (new_dir, new_lock) = match create_session_dir(&self.sessions_root) {
            Ok(v) => v,
            Err(e) => {
                self.status(format!("new session failed: {e:#}"));
                return;
            }
        };
        let new_out = match pdf::reserve_output_path(&self.cfg.output, pdf::stamp_now()) {
            Ok(v) => v,
            Err(e) => {
                // Roll the staged dir back: a created-but-unowned dir would
                // linger as a live-looking lock holder otherwise.
                let _ = std::fs::remove_dir_all(&new_dir);
                drop(new_lock);
                self.status(format!("new session failed: {e:#}"));
                return;
            }
        };
        // All fallible steps done: tear the old session down. The guard
        // guarantees no scan job is writing into the old dir (NewSession
        // requires Idle + no jobs).
        // Release the OLD reservation first: the old dir carries its
        // `.pending_out` marker, so a hard kill between a placeholder
        // release and the dir removal must not find a live placeholder
        // behind a marker — releasing first leaves marker + missing file
        // (the sweep's release is a no-op there). A built PDF (non-empty)
        // is never touched.
        pdf::release_reservation(&self.out_pdf);
        let old_dir = std::mem::replace(&mut self.dir, new_dir);
        let old_lock = std::mem::replace(&mut self.lock, new_lock);
        // Delete the old dir while holding its lock so a concurrent sweep
        // cannot claim it; dropping the fd releases the (unlinked) lock.
        // Same-stamp repeat (new session pressed within the second of a
        // successful finish — `on_finish_done` already deleted the dir and
        // the old lock fd refers to an unlinked inode): skip the removal,
        // or it would delete the dir we JUST created. The general
        // post-finish case is also covered: the stale dir is already gone.
        if old_dir != self.dir {
            let _ = std::fs::remove_dir_all(&old_dir);
        }
        drop(old_lock);
        self.pages.clear();
        // Guard requires an empty jobs map; drain-and-cancel is defense in
        // depth (kills any straggler instead of orphaning its token).
        for (_, token) in self.jobs.drain() {
            token.cancel();
        }
        self.busy = Busy::Idle;
        self.busy_since = None;
        self.finished = false;
        self.out_pdf = new_out;
        // Re-record the marker for the new reservation in the new dir.
        self.record_reservation();
        self.notify_pages();
        self.status("new session started");
    }
}

fn remove_page_files(page: &Page) {
    if let Some(img) = &page.image {
        let _ = std::fs::remove_file(img);
        let _ = std::fs::remove_file(clean_variant(img));
    }
}

fn clean_variant(path: &std::path::Path) -> PathBuf {
    // Mirror of the Python `_clean.png` naming.
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    match (stem, ext) {
        (Some(stem), Some(ext)) => path.with_file_name(format!("{stem}_clean.{ext}")),
        (Some(stem), None) => path.with_file_name(format!("{stem}_clean")),
        _ => path.to_path_buf(),
    }
}

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(crate::config::PROGRAM)
}

/// Create a session dir under `root` and flock it. The lock file is the
/// ownership marker: it stays flocked (LOCK_EX|LOCK_NB) while this process
/// lives, so the startup sweep of another instance sees the dir as taken —
/// even when this instance is suspended (suspended time never made a dir
/// look stale; mtime used to). On process death the kernel releases the
/// flock, so crash/signal leftovers are sweepable immediately, with no age
/// threshold anywhere.
///
/// `stamp_now()` has 1-second resolution, so candidates are claimed via
/// create_dir, not adopted: EEXIST means the name has a prior occupant —
/// a live same-second instance (whose held lock would surface as
/// WouldBlock one call later anyway) or one that launched and died. A
/// dead owner's dir must never be adopted: this instance's
/// `record_reservation` would overwrite its `.pending_out` marker,
/// orphaning the zero-byte placeholder that marker is the only cleanup
/// path for, and its stray page images would ride along. Ladder past
/// taken names with `_2`, `_3`... suffixes (mirroring the output-path
/// ladder) and leave the corpse for the next startup sweep.
pub fn create_session_dir(root: &std::path::Path) -> Result<(PathBuf, std::fs::File)> {
    std::fs::create_dir_all(root)?;
    let base = pdf::stamp_now();
    let mut candidate = base.clone();
    let mut n = 2;
    loop {
        let dir = root.join(&candidate);
        match std::fs::create_dir(&dir).and_then(|()| lock_dir(&dir)) {
            Ok(lock) => return Ok((dir, lock)),
            Err(err)
                if err.kind() == std::io::ErrorKind::AlreadyExists
                    || err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        // Taken (EEXIST: prior occupant, live or dead), contested (WouldBlock:
        // a concurrent sweep holds the fresh dir's lock just before deleting
        // it), or vanished mid-flight (ENOENT: sweep removed it between our
        // create and lock open). All mean: try the next suffix.
        candidate = format!("{base}_{n}");
        n += 1;
    }
}

/// Open (or create) `<dir>/.lock` and take an exclusive non-blocking flock.
/// An existing lock held by another process surfaces as `WouldBlock`.
fn lock_dir(dir: &std::path::Path) -> std::io::Result<std::fs::File> {
    let lock_path = dir.join(".lock");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

impl Drop for Session {
    fn drop(&mut self) {
        // Cancel any in-flight scan/per-page jobs (their tokens; the tasks
        // themselves die with the runtime). The Finish job has no token: a
        // build in flight is STILL READING the session dir, so deleting it
        // here would turn a quit into hard page loss. Leave the dir; the
        // flock is freed at process death and the next launch's sweep
        // removes it (or the build wins the teardown race and writes the
        // PDF after all).
        if self.busy == Busy::Finishing {
            // The reservation must also survive for now: ocrmypdf may be
            // mid-write into the placeholder, so releasing it here could
            // delete a real PDF. The .pending_out marker (written at
            // reservation time) already covers this; leave the dir for the
            // next startup's sweep, which releases the placeholder once the
            // dir's lock is freed (see sweep_stale_sessions).
            tracing::info!("quit during PDF build; session dir left for the next sweep");
            return;
        }
        for (_, token) in self.jobs.drain() {
            token.cancel();
        }
        if let Some(t) = self.scan_token.take() {
            t.cancel();
        }
        // Idle/Scanning path. Under Scanning a capture may still be writing
        // into the dir being removed: harmless at process exit (open writes
        // continue on the unlinked inode; reopened writes fail into the
        // job's error path, which a dead channel discards). Deleting under
        // the held lock keeps a concurrent sweep out; the fd drops right
        // after. A successful finish already removed the dir (ENOENT
        // ignored).
        // Release the reservation BEFORE deleting the dir: the dir holds the
        // `.pending_out` marker, so a hard kill between the two statements
        // would otherwise orphan a zero-byte placeholder with no marker for
        // the sweep. The other order leaks nothing — a crash here leaves
        // dir+marker intact around an already-released placeholder (the
        // sweep's release is a no-op on the missing file).
        pdf::release_reservation(&self.out_pdf);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Sweep session dirs left behind by crashes/signals at startup. For each
/// directory under `root`, test its `.lock` with a non-blocking flock:
/// acquired => the owning process is dead => delete the dir; EWOULDBLOCK =>
/// a live instance (this one or another) still holds it => keep; the lock
/// file missing => legacy (pre-flock) leftover => delete. Files directly in
/// the root are left alone. Best effort: I/O errors are logged and skipped.
fn sweep_stale_sessions(root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Bind the acquired lock BEFORE deleting: the guard must stay held
        // through remove_dir_all, or a simultaneously-sweeping second
        // instance could flock the same dir and race us. Only two outcomes
        // authorize deletion: the flock succeeded (either the dead owner's
        // freed lock, or a `.lock` marker we just created for a legacy
        // lockless dir — both prove no live owner), or a live instance
        // holds it (skip). Any other lock error (EACCES, EISDIR, EMFILE,
        // ...) is NOT proof of a dead owner — skip rather than risk
        // sweeping a live instance's dir.
        let guard = match lock_dir(&path) {
            // Live instance holds the lock.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            // Unreadable/invalid lock file: cannot prove a dead owner.
            Err(_) => continue,
            // Dead owner (free pre-existing lock) or legacy dir (we just
            // created the marker): both sweepable, and the guard blocks a
            // racing second sweeper meanwhile.
            Ok(file) => file,
        };
        // A quit during a PDF build defers its output reservation here
        // (see Drop): release the zero-byte placeholder now that the owner
        // is confirmed dead. Missing file / non-empty file => no-op.
        if let Ok(pending) = std::fs::read_to_string(path.join(".pending_out")) {
            pdf::release_reservation(std::path::Path::new(pending.trim()));
            let _ = std::fs::remove_file(path.join(".pending_out"));
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => tracing::info!("swept dead-owner session dir {}", path.display()),
            Err(e) => tracing::warn!("could not sweep {}: {e}", path.display()),
        }
        drop(guard);
    }
}

/// Run the session actor loop: commands + job completions.
async fn actor_loop(
    mut session: Session,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    mut job_rx: mpsc::UnboundedReceiver<JobDone>,
) {
    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(cmd) => session.handle(cmd).await,
                    None => break,
                }
            }
            done = job_rx.recv() => {
                match done {
                    Some(done) => session.handle_job_done(done).await,
                    None => break,
                }
            }
        }
    }
}

/// Spawn the actor with its channels; returns (cmd sender, event receiver).
pub fn spawn(cfg: Config, device: String) -> Result<(mpsc::Sender<Cmd>, mpsc::Receiver<Event>)> {
    // Startup sweep: crash/signal leftovers and legacy pre-flock dirs.
    // Quit deletes its own dir (except during a build), so anything with a
    // dead owner here is crash debris. Live instances' locks block the
    // sweep, so a second launch can no longer delete a running one's
    // session files.
    sweep_stale_sessions(&state_dir().join("sessions"));
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(64);
    let (event_tx, event_rx) = mpsc::channel::<Event>(256);
    let (job_tx, job_rx) = mpsc::unbounded_channel::<JobDone>();
    let session = Session::with_channels(cfg, device, event_tx, job_tx)?;
    tokio::spawn(actor_loop(session, cmd_rx, job_rx));
    Ok((cmd_tx, event_rx))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes every XDG_STATE_HOME set -> build -> restore sequence.
    /// Cargo runs #[tokio::test]s on parallel threads and env is
    /// process-global, so unguarded tests would race (same-second stamps
    /// would even share one directory).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Build a Session wired to throwaway channels. XDG_STATE_HOME is
    /// pointed at a per-test tempdir before `with_channels` so sessions
    /// never collide in (or pollute) the real `~/.local/state` —
    /// `stamp_now()` has 1-second resolution, so same-second tests would
    /// otherwise share one directory.
    fn test_session(preview_ocr: PreviewOcr) -> (Session, tempfile::TempDir) {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", dir.path());
        let cfg = Config {
            preview_ocr,
            output: dir.path().join("out"),
            ..Config::default()
        };
        let (event_tx, _event_rx) = mpsc::channel(256);
        let (job_tx, _job_rx) = mpsc::unbounded_channel();
        let session = Session::with_channels(cfg, "fake:/test".into(), event_tx, job_tx)
            .expect("session with temp state dir");
        match prev {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        // _event_rx/_job_rx stay alive so the channels don't close. Note:
        // request_text/eager paths spawn a real tesseract child against the
        // fake image; the child is killed on runtime drop and the result is
        // discarded by the throwaway job channel.
        (session, dir)
    }

    /// Fabricate a completed scan job result for page `id` with an existing
    /// image file (on_scan_done checks nothing on disk, but keep it honest).
    fn scan_ok(id: PageId, image: PathBuf) -> JobDone {
        JobDone::Scan {
            id,
            is_rescan: false,
            image,
            unpaper_deskewed: false,
            used_fallback: false,
            result: Ok(()),
        }
    }

    fn ocr_ok(id: PageId, image: PathBuf, image_gen: u32, text: &str) -> JobDone {
        JobDone::OcrText {
            id,
            image,
            image_gen,
            result: Ok(text.to_string()),
        }
    }

    #[tokio::test]
    async fn lazy_scan_goes_straight_to_ready_without_text() {
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        let p = &s.pages[0];
        assert_eq!(p.status, PageStatus::Ready);
        assert_eq!(p.stage, None);
        assert_eq!(p.text, None);
        assert!(!p.text_pending);
        assert_eq!(s.busy, Busy::Idle, "scanner must be free after capture");
        assert!(s.jobs.is_empty());
    }

    #[tokio::test]
    async fn eager_scan_spawns_ocr_job_then_completes() {
        let (mut s, _dir) = test_session(PreviewOcr::Eager);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        {
            let p = &s.pages[0];
            assert_eq!(p.status, PageStatus::Processing);
            assert_eq!(p.stage, Some(Stage::Ocr));
            assert!(p.stage_started.is_some(), "OCR stage gets its own timer");
        }
        assert_eq!(s.jobs.len(), 1, "OCR token registered for cancellation");
        assert_eq!(s.busy, Busy::Idle, "scanner free during preview OCR");
        // The job "finishes": text applies, page goes Ready.
        s.handle_job_done(ocr_ok(1, img, 1, "hello")).await;
        let p = &s.pages[0];
        assert_eq!(p.status, PageStatus::Ready);
        assert_eq!(p.text.as_deref(), Some("hello"));
        assert!(!p.text_pending);
        assert!(s.jobs.is_empty());
    }

    #[tokio::test]
    async fn request_text_fills_lazy_text_and_is_idempotent() {
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;

        s.request_text(1);
        assert!(s.pages[0].text_pending);
        assert_eq!(s.jobs.len(), 1);

        // Duplicate request while pending is a no-op (no second job).
        s.request_text(1);
        assert_eq!(s.jobs.len(), 1);

        // Off-config never spawns even when asked.
        s.handle_job_done(ocr_ok(1, img.clone(), 1, "text")).await;
        assert_eq!(s.pages[0].text.as_deref(), Some("text"));

        // Empty extraction result must not re-trigger (Some("") is text).
        s.request_text(1);
        assert!(!s.pages[0].text_pending);
        assert!(s.jobs.is_empty());
    }

    #[tokio::test]
    async fn request_text_ignored_under_off() {
        let (mut s, _dir) = test_session(PreviewOcr::Off);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img)).await;
        s.request_text(1);
        assert!(!s.pages[0].text_pending);
        assert!(s.jobs.is_empty(), "off mode never spawns preview OCR");
    }

    #[tokio::test]
    async fn ocr_error_never_fails_the_page() {
        let (mut s, _dir) = test_session(PreviewOcr::Eager);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        s.handle_job_done(JobDone::OcrText {
            id: 1,
            image: img,
            image_gen: 1,
            result: Err(anyhow::anyhow!("tesseract exploded")),
        })
        .await;
        let p = &s.pages[0];
        assert_eq!(p.status, PageStatus::Ready, "OCR errors stay cosmetic");
        assert!(p.text.is_none());
        assert!(!p.text_pending);
        // Finish stays permitted despite the OCR failure.
        assert!(s.guard(&Cmd::Finish).is_ok());
    }

    #[tokio::test]
    async fn cancelled_ocr_during_finish_does_not_fail_page() {
        let (mut s, _dir) = test_session(PreviewOcr::Eager);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        s.start_finish();
        assert_eq!(s.busy, Busy::Finishing);
        assert!(s.jobs.is_empty(), "finish cancels and drains OCR jobs");
        // Late completion arrives after the cancel.
        s.handle_job_done(JobDone::OcrText {
            id: 1,
            image: img,
            image_gen: 1,
            result: Err(anyhow::anyhow!("cancelled")),
        })
        .await;
        let p = &s.pages[0];
        assert_eq!(p.status, PageStatus::Ready, "cancelled != failed");
        assert!(!p.text_pending);
    }

    #[tokio::test]
    async fn finish_guard_ignores_ocr_stage_but_not_clean() {
        let (mut s, _dir) = test_session(PreviewOcr::Eager);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img)).await;
        // Page in OCR stage: finish allowed (PDF layer independent).
        assert!(s.guard(&Cmd::Finish).is_ok());
        // Page in rotate (clean) stage: finish rejected.
        if let Some(p) = s.pages.iter_mut().find(|p| p.id == 1) {
            p.stage = Some(Stage::Clean);
        }
        assert!(s.guard(&Cmd::Finish).is_err());
    }

    #[tokio::test]
    async fn stale_ocr_result_is_dropped_after_rotate() {
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        // Simulate rotate having bumped image_gen while the OCR job ran.
        if let Some(p) = s.pages.iter_mut().find(|p| p.id == 1) {
            p.image_gen += 1;
        }
        s.handle_job_done(ocr_ok(1, img, 1, "stale text")).await;
        assert!(s.pages[0].text.is_none(), "stale result dropped");
        assert!(!s.pages[0].text_pending);
    }

    #[tokio::test]
    async fn rotate_under_lazy_invalidates_text() {
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        s.request_text(1);
        s.handle_job_done(ocr_ok(1, img.clone(), 1, "pre-rotation"))
            .await;
        assert_eq!(s.pages[0].text.as_deref(), Some("pre-rotation"));

        s.start_rotate(1, true);
        assert!(
            s.pages[0].text.is_none(),
            "stale pre-rotation text dropped synchronously"
        );
        // The rotate job reports Ok(None) with reocr=false: text stays gone.
        s.handle_job_done(JobDone::Rotate {
            id: 1,
            reocr: false,
            result: Ok(None),
        })
        .await;
        assert_eq!(s.pages[0].status, PageStatus::Ready);
        assert!(s.pages[0].text.is_none());
        // Lazy re-extract works from the (rotated) image afterwards.
        s.request_text(1);
        assert!(s.pages[0].text_pending);
    }

    #[tokio::test]
    async fn delete_ready_page_cancels_running_ocr() {
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        s.request_text(1);
        assert_eq!(s.jobs.len(), 1);
        let token = s.jobs.get(&1).cloned().unwrap();
        s.delete(1);
        assert!(token.is_cancelled(), "delete cancels the OCR job");
        assert!(s.jobs.is_empty(), "no orphaned token");
        assert!(s.pages.is_empty());
        // Late completion for the deleted page must not panic or linger.
        s.handle_job_done(ocr_ok(1, img, 1, "ghost")).await;
        assert!(s.pages.is_empty());
        assert!(s.jobs.is_empty());
    }

    #[tokio::test]
    async fn deferred_delete_completes_on_ocr_done() {
        let (mut s, _dir) = test_session(PreviewOcr::Eager);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        // Delete arrives while the eager OCR job runs -> deferred.
        s.delete(1);
        assert_eq!(s.pages[0].status, PageStatus::DeletePending);
        s.handle_job_done(ocr_ok(1, img, 1, "text")).await;
        assert!(s.pages.is_empty(), "deferred delete completed on ocr done");
        assert!(s.jobs.is_empty());
    }

    #[tokio::test]
    async fn rescan_rejected_while_ocr_job_runs() {
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img)).await;
        s.request_text(1);
        assert!(s.guard(&Cmd::Rescan(1)).is_err());
        // A different page can still be scanned meanwhile.
        assert!(s
            .guard(&Cmd::ScanNext {
                dpi: 300,
                mode: "gray".into()
            })
            .is_ok());
    }

    #[tokio::test]
    async fn jobs_map_cleared_when_page_deleted_mid_ocr() {
        // Page deleted while its OCR runs: the completion must still remove
        // the jobs entry (otherwise NewSession stays blocked forever).
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        s.request_text(1);
        // Simulate the page vanishing without the delete path touching the
        // jobs map (the double-delete hole).
        s.pages.retain(|p| p.id != 1);
        s.handle_job_done(ocr_ok(1, img, 1, "ghost")).await;
        assert!(s.jobs.is_empty(), "orphaned entry removed");
        assert!(s.guard(&Cmd::NewSession).is_ok());
    }

    /// Session with no device yet (fresh startup state, pre-SetDevice).
    fn test_session_undetected() -> (Session, tempfile::TempDir) {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", dir.path());
        let cfg = Config {
            output: dir.path().join("out"),
            ..Config::default()
        };
        let (event_tx, _event_rx) = mpsc::channel(256);
        let (job_tx, _job_rx) = mpsc::unbounded_channel();
        let session = Session::with_channels(cfg, String::new(), event_tx, job_tx)
            .expect("session with temp state dir");
        match prev {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        (session, dir)
    }

    #[tokio::test]
    async fn scan_blocked_until_device_is_set() {
        let (mut s, _dir) = test_session_undetected();
        // Empty device: guard rejects scanning before detection delivers.
        assert_eq!(
            s.guard(&Cmd::ScanNext {
                dpi: 300,
                mode: "gray".into()
            }),
            Err("scanner detection still running".into())
        );
        assert!(s.guard(&Cmd::Rescan(1)).is_err());
        assert!(s.device.is_empty());

        s.handle(Cmd::SetDevice("hpaio:/usb/x".into())).await;
        assert_eq!(s.device, "hpaio:/usb/x");
        assert!(s
            .guard(&Cmd::ScanNext {
                dpi: 300,
                mode: "gray".into()
            })
            .is_ok());
    }

    /// Post-build with no device ever delivered (startup + re-runs both
    /// failed to find a scanner): the message must not claim detection is
    /// still running.
    #[tokio::test]
    async fn device_message_reflects_finished_state() {
        let (mut s, _dir) = test_session_undetected();
        s.finished = true;
        let expect = Err("no scanner detected".into());
        assert_eq!(
            s.guard(&Cmd::ScanNext {
                dpi: 300,
                mode: "gray".into()
            }),
            expect
        );
        assert_eq!(s.guard(&Cmd::Rescan(1)), expect);
    }

    #[tokio::test]
    async fn set_device_is_idempotent_and_never_empties() {
        let (mut s, _dir) = test_session_undetected();
        s.handle(Cmd::SetDevice("hpaio:/usb/x".into())).await;
        // Same name again: silent no-op.
        s.handle(Cmd::SetDevice("hpaio:/usb/x".into())).await;
        assert_eq!(s.device, "hpaio:/usb/x");
        // Empty name (failed re-detection) never clears a working device.
        s.handle(Cmd::SetDevice(String::new())).await;
        assert_eq!(s.device, "hpaio:/usb/x");
        // A different name replaces it (scanner swapped mid-session).
        s.handle(Cmd::SetDevice("escl:http://y".into())).await;
        assert_eq!(s.device, "escl:http://y");
    }

    #[tokio::test]
    async fn ocr_job_cap_bounds_concurrency() {
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        // Fill the jobs map to the cap with fake tokens for *other* pages.
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.start_scan(300, "gray".into(), None);
        s.handle_job_done(scan_ok(1, img)).await;
        for id in 2..4u32 {
            s.jobs.insert(id, CancellationToken::new());
        }
        s.request_text(1);
        assert!(
            !s.pages[0].text_pending,
            "request deferred when job cap reached"
        );
        assert_eq!(s.jobs.len(), 2);
    }

    #[tokio::test]
    async fn request_text_bails_when_image_is_gone() {
        // Regression: after a successful finish the session dir is deleted
        // but pages linger Ready with text=None; the lazy tick must not
        // respawn tesseract on the missing file forever.
        let (mut s, dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img)).await;
        assert_eq!(s.pages[0].status, PageStatus::Ready);
        // Simulate on_finish_done's remove_dir_all (tempdir drop).
        drop(dir);
        s.request_text(1);
        assert!(
            !s.pages[0].text_pending,
            "no OCR spawned for a vanished image"
        );
        assert!(s.jobs.is_empty());
    }

    #[tokio::test]
    async fn finish_stops_lazy_requests_and_new_session_re_arms() {
        // Post-finish the session dir is deleted while page stubs linger
        // Ready with text=None: the finished flag (set by on_finish_done)
        // must block both the actor-side spawn and (via meta) the TUI
        // tick's 4x/sec re-send. Also gates the page/finish commands whose
        // guards used to pass against the deleted dir.
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        assert!(!s.meta().finished);
        s.handle_job_done(JobDone::Finish {
            result: Ok(pdf::BuildOutcome::Searchable),
        })
        .await;
        assert!(s.finished, "success sets the flag via on_finish_done");
        assert!(!s.dir.exists(), "session dir removed by the build");
        s.request_text(1);
        assert!(
            !s.pages[0].text_pending,
            "finished flag blocks preview OCR despite the stub page"
        );
        assert!(s.jobs.is_empty());
        assert!(s.meta().finished, "flag surfaces in the TUI meta");
        assert!(!s.meta().dirty, "finished session is not dirty");
        assert!(
            s.guard(&Cmd::ScanNext {
                dpi: 300,
                mode: "gray".into()
            })
            .is_err(),
            "scan blocked against the deleted dir"
        );
        assert!(s.guard(&Cmd::Rescan(1)).is_err());
        assert!(s.guard(&Cmd::Rotate(1, true)).is_err());
        assert!(
            s.guard(&Cmd::Finish).is_err(),
            "re-finish blocked (images are gone)"
        );
        assert!(s.guard(&Cmd::Delete(1)).is_ok(), "stubs stay deletable");
        // A new session resets it (a fresh dir makes lazy OCR valid again).
        s.new_session();
        assert!(!s.meta().finished);
        assert!(s
            .guard(&Cmd::ScanNext {
                dpi: 300,
                mode: "gray".into()
            })
            .is_ok());
    }

    #[tokio::test]
    async fn new_session_after_finish_keeps_recreated_dir() {
        // Regression: a new session within the same second as a successful
        // finish reproduces the SAME dir name (1s stamp resolution; the old
        // lock fd refers to an unlinked inode, so no WouldBlock). Teardown
        // must not delete the dir it just created — otherwise .pending_out
        // is silently lost (leaking the placeholder on a later hard kill)
        // and every subsequent scan fails until restart. If a second
        // boundary falls between the finish and the new session the stamp
        // differs and a fresh dir is created instead — equally correct, so
        // only the weaker invariants (live dir + lock + marker) are asserted.
        let (mut s, _guard) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img)).await;
        // Finish + new session within the same wall-clock second.
        s.handle_job_done(JobDone::Finish {
            result: Ok(pdf::BuildOutcome::Searchable),
        })
        .await;
        s.new_session();
        assert!(
            s.dir.exists(),
            "teardown must not delete the recreated dir (same stamp)"
        );
        assert!(s.dir.join(".lock").exists(), "new dir carries a lock");
        assert!(
            s.dir.join(".pending_out").exists(),
            "reservation marker recorded in the live dir"
        );
        // Scanning into the session works again (new session cleared the
        // stubs; ids never reuse, so this new page gets id 2).
        s.start_scan(300, "gray".into(), None);
        let img2 = s.dir.join("page_002.png");
        std::fs::write(&img2, b"x").unwrap();
        s.handle_job_done(scan_ok(2, img2)).await;
        assert_eq!(s.pages[0].status, PageStatus::Ready);
        let final_dir = s.dir.clone();
        drop(s);
        assert!(!final_dir.exists(), "Drop cleans the live dir");
    }

    #[tokio::test]
    async fn failed_finish_keeps_lazy_requests_alive() {
        // A failed build does not delete the session dir, so preview OCR
        // must keep working afterwards.
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        s.handle_job_done(JobDone::Finish {
            result: Err(anyhow::anyhow!("ocrmypdf exploded")),
        })
        .await;
        assert!(!s.finished, "failed build does not set the flag");
        s.request_text(1);
        assert!(s.pages[0].text_pending, "lazy OCR still works");
        // A failed build must keep the dirty flag on: the pages still hold
        // real images and no PDF was produced.
        assert!(s.meta().dirty);
    }

    #[tokio::test]
    async fn dirty_tracks_finishability_and_failed_pages() {
        // The dirty flag mirrors the quit-confirm rule: Ready/Scanning/
        // Processing pages count (a PDF could still be built from them),
        // Failed pages do not (the dialog ignores them too), and a finished
        // session is clean no matter what the stubs look like.
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);

        // Fresh session: clean.
        assert!(!s.dirty(), "empty session is clean");

        // Scanning page: dirty.
        s.start_scan(300, "gray".into(), None);
        assert!(s.dirty(), "scan in flight is dirty");

        // Ready page with a captured image: still dirty.
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        assert!(s.dirty(), "captured (un-built) page is dirty");

        // All pages failed: quitting loses nothing the dialog cares about.
        if let Some(p) = s.pages.iter_mut().find(|p| p.id == 1) {
            p.status = PageStatus::Failed;
            p.error = Some("boom".into());
        }
        assert!(!s.dirty(), "all-failed session counts as clean");

        // Successful finish makes the session clean despite the stubs.
        if let Some(p) = s.pages.iter_mut().find(|p| p.id == 1) {
            p.status = PageStatus::Ready;
            p.error = None;
        }
        s.handle_job_done(JobDone::Finish {
            result: Ok(pdf::BuildOutcome::Searchable),
        })
        .await;
        assert!(s.finished);
        assert!(!s.dirty(), "finished session is clean despite stubs");
    }

    #[tokio::test]
    async fn persistent_ocr_failure_is_not_retried_for_same_image() {
        // Regression: a deterministic OCR failure (missing language data,
        // corrupt image) cleared text_pending, so the lazy tick re-sent
        // RequestText every 250ms — a ~4x/sec status spam loop. The failed
        // generation must block auto-retry until the image changes.
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;

        s.request_text(1);
        assert!(s.pages[0].text_pending);
        s.handle_job_done(JobDone::OcrText {
            id: 1,
            image: img.clone(),
            image_gen: 1,
            result: Err(anyhow::anyhow!("tesseract: no such language data")),
        })
        .await;
        let p = &s.pages[0];
        assert_eq!(p.status, PageStatus::Ready, "OCR failure stays cosmetic");
        assert_eq!(p.ocr_failed_gen, Some(1), "failure recorded for the gen");
        assert!(!p.text_pending);

        // The tick's re-request for the same image is now a silent no-op.
        s.request_text(1);
        assert!(s.jobs.is_empty(), "no respawn after a recorded failure");

        // Rescan bumps image_gen: the failure flag is cleared and OCR re-arms.
        if let Some(p) = s.pages.iter_mut().find(|p| p.id == 1) {
            p.image_gen += 1;
        }
        s.request_text(1);
        assert!(s.pages[0].text_pending, "new image gets a fresh attempt");
    }

    #[tokio::test]
    async fn rotate_re_arms_lazy_ocr_after_failure() {
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;

        s.request_text(1);
        s.handle_job_done(JobDone::OcrText {
            id: 1,
            image: img.clone(),
            image_gen: 1,
            result: Err(anyhow::anyhow!("boom")),
        })
        .await;
        assert_eq!(s.pages[0].ocr_failed_gen, Some(1));
        s.request_text(1);
        assert!(s.jobs.is_empty(), "still blocked");

        // Rotate succeeds: new image content clears the failure flag.
        s.start_rotate(1, true);
        s.handle_job_done(JobDone::Rotate {
            id: 1,
            reocr: false,
            result: Ok(None),
        })
        .await;
        assert_eq!(s.pages[0].status, PageStatus::Ready);
        assert_eq!(
            s.pages[0].ocr_failed_gen, None,
            "rotate re-arms the lazy retry"
        );
        s.request_text(1);
        assert!(s.pages[0].text_pending);
    }

    #[test]
    fn sweep_keeps_live_locks_and_deletes_dead_or_legacy_dirs() {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let dead = sessions.join("2026-01-01_000000");
        let legacy = sessions.join("2026-01-02_000000");
        let live = sessions.join("2026-01-03_000000");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(dead.join("page_001.png"), b"x").unwrap();
        std::fs::write(live.join("page_001.png"), b"x").unwrap();

        // `dead` gets a lock file that nobody holds (crash leftover: the
        // kernel released the flock when the owner died).
        lock_dir(&dead).unwrap();
        // `legacy` has no lock file at all (pre-flock leftover).
        // `live` is flocked by THIS process — the same condition another
        // live instance's session presents to the sweep.
        let live_lock = lock_dir(&live).unwrap();

        sweep_stale_sessions(&sessions);

        assert!(!dead.exists(), "dead-owner dir removed");
        assert!(!legacy.exists(), "legacy (lockless) dir removed");
        assert!(live.exists(), "live (held-lock) dir kept");

        // The live dir is sweepable once its owner dies (lock released).
        drop(live_lock);
        sweep_stale_sessions(&sessions);
        assert!(!live.exists(), "dir sweepable after lock release");
    }

    #[test]
    fn sweep_deletes_fresh_lockless_dir_created_then_abandoned() {
        // A dir created but never locked (crash between create and flock —
        // or legacy) is deleted regardless of age: no time threshold.
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let d = sessions.join("2099-01-01_000000");
        std::fs::create_dir_all(d.join("nested")).unwrap();

        sweep_stale_sessions(&sessions);

        assert!(!d.exists(), "lockless dir removed without age check");
    }

    #[test]
    fn sweep_leaves_root_files_and_missing_root_alone() {
        // Stray files in the sessions root don't belong to sessions; a
        // missing root (first launch) must not panic.
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let stray = sessions.join("stray.png");
        std::fs::write(&stray, b"x").unwrap();

        sweep_stale_sessions(&sessions);
        assert!(stray.exists(), "root-level files untouched");
        assert!(sessions.exists(), "sessions root itself survives");

        sweep_stale_sessions(&root.path().join("nope"));
    }

    #[tokio::test]
    async fn drop_deletes_session_dir_and_lock_with_pages() {
        let (mut s, _guard) = test_session(PreviewOcr::Lazy);
        let dir = s.dir.clone();
        let out_pdf = s.out_pdf.clone();
        s.start_scan(300, "gray".into(), None);
        let img = dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img)).await;
        assert!(dir.exists());
        assert!(out_pdf.exists(), "output placeholder reserved");

        drop(s);

        assert!(!dir.exists(), "quit deletes the session dir");
        assert!(!out_pdf.exists(), "dangling reservation released");
    }

    #[tokio::test]
    async fn drop_after_finish_ignores_missing_dir() {
        // on_finish_done already removed the dir; Drop must not error on
        // the ENOENT and must not touch the built PDF.
        let (mut s, _guard) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img.clone())).await;
        s.handle_job_done(JobDone::Finish {
            result: Ok(pdf::BuildOutcome::Searchable),
        })
        .await;
        let out_pdf = s.out_pdf.clone();
        std::fs::write(&out_pdf, b"pdf bytes").unwrap();
        assert!(!s.dir.exists());
        let dir = s.dir.clone();

        drop(s);

        assert!(!dir.exists(), "still gone (no resurrection)");
        assert!(out_pdf.exists(), "built PDF never released");
    }

    #[tokio::test]
    async fn drop_during_finish_leaves_dir_for_next_sweep() {
        // Quit while a build runs: the dir must survive (ocrmypdf is still
        // reading it); the freed lock makes it sweepable on next launch.
        // The reservation marker (written at reservation time) lets the
        // sweep release the placeholder.
        let (mut s, _guard) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img)).await;
        s.busy = Busy::Finishing; // simulate the in-flight build window
        let dir = s.dir.clone();
        let out_pdf = s.out_pdf.clone();

        drop(s);

        assert!(dir.exists(), "build in flight: dir left intact");
        assert!(
            dir.join(".pending_out").exists(),
            "reservation marker present for the sweep"
        );
        assert!(out_pdf.exists(), "placeholder not released by Drop");
        // Owner dead => the next launch sweeps it and releases the placeholder.
        sweep_stale_sessions(dir.parent().unwrap());
        assert!(!dir.exists(), "freed-lock dir swept on next launch");
        assert!(!out_pdf.exists(), "sweep releases the deferred placeholder");
    }

    #[tokio::test]
    async fn hard_kill_placeholder_recovered_by_sweep() {
        // A hard kill (SIGKILL/OOM) runs no Drop code at all, but the
        // kernel still closes every fd — the lock is freed while the
        // session dir survives with its marker. Regression for the leak
        // the marker at reservation time closes: the sweep must release
        // the placeholder the dead session left behind.
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let d = sessions.join("2026-01-05_000000");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("page_001.png"), b"x").unwrap();
        std::fs::write(
            d.join(".pending_out"),
            d.parent()
                .unwrap()
                .join("placeholder.pdf")
                .to_string_lossy()
                .as_bytes(),
        )
        .unwrap();
        let out_pdf = d.parent().unwrap().join("placeholder.pdf");
        std::fs::write(&out_pdf, b"").unwrap(); // zero-byte placeholder
                                                // Dead owner: a .lock file whose flock nobody holds (the kernel
                                                // released it when the process died).
        lock_dir(&d).unwrap();

        sweep_stale_sessions(&sessions);
        assert!(!d.exists(), "dead-owner dir swept");
        assert!(
            !out_pdf.exists(),
            "marker lets the sweep release the leaked placeholder"
        );
    }

    #[tokio::test]
    async fn hard_kill_built_pdf_kept_by_sweep() {
        // Same death mode, but the build completed first: the sweep must
        // keep the non-empty PDF.
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let d = sessions.join("2026-01-06_000000");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join(".pending_out"),
            d.parent()
                .unwrap()
                .join("built.pdf")
                .to_string_lossy()
                .as_bytes(),
        )
        .unwrap();
        let out_pdf = d.parent().unwrap().join("built.pdf");
        std::fs::write(&out_pdf, b"pdf bytes").unwrap();
        lock_dir(&d).unwrap();

        sweep_stale_sessions(&sessions);
        assert!(!d.exists(), "dead-owner dir swept");
        assert!(out_pdf.exists(), "built PDF kept");
    }

    #[tokio::test]
    async fn drop_during_finish_keeps_built_pdf_from_sweep_release() {
        // If the build actually finished between Drop and the sweep, the
        // placeholder is a real PDF: the sweep must leave it alone.
        let (mut s, _guard) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img)).await;
        s.busy = Busy::Finishing;
        let dir = s.dir.clone();
        let out_pdf = s.out_pdf.clone();

        drop(s);

        // The build won the race: the placeholder now holds real content.
        std::fs::write(&out_pdf, b"pdf bytes").unwrap();
        sweep_stale_sessions(dir.parent().unwrap());
        assert!(
            out_pdf.exists(),
            "sweep release never deletes a non-empty PDF"
        );
    }

    #[tokio::test]
    async fn new_session_io_failure_keeps_old_session_intact() {
        // Regression: a failing output reservation used to panic AFTER the
        // old dir (and its pages) were already destroyed. It must instead
        // report the error and keep everything.
        let (mut s, _dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img)).await;
        let old_dir = s.dir.clone();
        let old_out = s.out_pdf.clone();

        // Sabotage the output dir: a file where the directory should be
        // makes create_new fail with NotADirectory. This also removes the
        // old placeholder (it lives inside), which is fine: the invariant
        // under test is that the session's state is untouched.
        std::fs::remove_dir_all(&s.cfg.output).unwrap();
        std::fs::write(&s.cfg.output, b"not a dir").unwrap();

        s.new_session();

        assert_eq!(s.dir, old_dir, "session dir unchanged on failure");
        assert_eq!(s.out_pdf, old_out, "reservation path unchanged");
        assert_eq!(s.pages.len(), 1, "pages intact");
        assert!(old_dir.exists(), "old session dir survives");
        assert_eq!(s.pages[0].status, PageStatus::Ready);
        drop(s);
        // Drop still cleans the old dir on quit.
        assert!(!old_dir.exists());
    }

    #[tokio::test]
    async fn new_session_session_dir_failure_keeps_old_session_intact() {
        // Same guarantee when the sessions root becomes unwritable. The old
        // dir lives INSIDE the sessions root, so sabotaging the root also
        // removes the old dir — the invariant under test is that the
        // session's in-memory state (pages, dir path, reservation) stays
        // untouched and no new dir was created.
        let (mut s, dir) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img)).await;
        let old_dir = s.dir.clone();
        let old_out = s.out_pdf.clone();

        // Sabotage the sessions root: a file where the directory should be
        // makes create_dir_all fail.
        std::fs::remove_dir_all(&s.sessions_root).unwrap();
        std::fs::write(&s.sessions_root, b"not a dir").unwrap();

        s.new_session();

        assert_eq!(s.dir, old_dir, "session dir path unchanged on failure");
        assert_eq!(s.out_pdf, old_out, "reservation unchanged on failure");
        assert_eq!(s.pages.len(), 1, "pages intact");
        assert_eq!(s.pages[0].status, PageStatus::Ready);
        assert!(!s.finished, "finished flag untouched");
        drop(s);
        // Drop cleans up against the (now missing) dir: ENOENT is ignored.
        // The tempdir guard (dir) must outlive the session on purpose.
        drop(dir);
    }

    #[test]
    fn sweep_skips_dirs_with_unopenable_lock_files() {
        // An unopenable .lock is NOT proof of a dead owner (could be a live
        // instance behind restrictive perms): the sweep must skip, not
        // delete. A DIRECTORY named .lock makes open(2) fail with EISDIR
        // (works under root too, where mode 000 wouldn't). The dir stays
        // writable so deletion WOULD succeed if the sweep tried.
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let d = sessions.join("2026-01-04_000000");
        std::fs::create_dir_all(d.join(".lock")).unwrap();
        std::fs::write(d.join("page_001.png"), b"x").unwrap();

        sweep_stale_sessions(&sessions);

        assert!(d.exists(), "EISDIR on the lock file: dir kept");
        assert!(d.join("page_001.png").exists());
    }

    #[tokio::test]
    async fn new_session_removes_old_dir_and_holds_new_lock() {
        let (mut s, _guard) = test_session(PreviewOcr::Lazy);
        s.start_scan(300, "gray".into(), None);
        let img = s.dir.join("page_001.png");
        std::fs::write(&img, b"x").unwrap();
        s.handle_job_done(scan_ok(1, img)).await;
        let old_dir = s.dir.clone();

        s.new_session();

        assert!(!old_dir.exists(), "old session dir deleted");
        assert!(s.dir.exists() && s.dir != old_dir);
        assert!(s.dir.join(".lock").exists(), "new dir carries a lock");
        // The new lock is held: another sweep must leave the new dir alone.
        sweep_stale_sessions(s.dir.parent().unwrap());
        assert!(s.dir.exists(), "held lock protects the new dir");
        // Dropping the session cleans the new dir too.
        let new_dir = s.dir.clone();
        drop(s);
        assert!(!new_dir.exists());
    }

    #[tokio::test]
    async fn create_session_dir_ladders_past_taken_names() {
        // Same-second second instance: the first stamp candidate already
        // exists (its claim via create_dir hits EEXIST), so creation must
        // fall through to _2.
        let root = tempfile::tempdir().unwrap();
        let (d1, lock1) = create_session_dir(root.path()).expect("first dir");
        let (d2, lock2) = create_session_dir(root.path()).expect("second dir");
        assert_ne!(d1, d2, "existing dir forces a distinct dir");
        assert!(
            d2.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_2")),
            "suffix ladder engaged"
        );
        assert!(d1.exists() && d2.exists(), "both live dirs survive");
        assert!(d1.join(".lock").exists() && d2.join(".lock").exists());
        drop(lock2);
        drop(lock1);
    }

    #[tokio::test]
    async fn create_session_dir_never_adopts_dead_owners_dir() {
        // Same-second instance that launched and died: its dir still holds a
        // `.pending_out` marker (releasing the placeholder it points at is
        // the startup sweep's job). Adopting the dir would overwrite that
        // marker and orphan the zero-byte placeholder, so creation must
        // ladder past it to _2.
        let root = tempfile::tempdir().unwrap();
        let corpse = root.path().join(pdf::stamp_now());
        std::fs::create_dir_all(&corpse).unwrap();
        std::fs::write(corpse.join(".pending_out"), "/out/2026-01-01_000000.pdf").unwrap();
        std::fs::write(corpse.join("page_001.png"), b"x").unwrap();

        let (d, _lock) = create_session_dir(root.path()).expect("fresh dir");
        assert_ne!(d, corpse, "dead owner's dir must not be adopted");
        assert!(
            d.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_2")),
            "suffix ladder engaged past the corpse"
        );
        assert!(
            corpse.join(".pending_out").exists(),
            "corpse left for the sweep"
        );
        assert!(
            corpse.join("page_001.png").exists(),
            "corpse contents untouched"
        );
    }
}
