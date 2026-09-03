//! Session actor: owns the page list, runs jobs, enforces guards.
//!
//! Architecture (per the concurrency review + UX findings):
//! - The actor exclusively owns `Vec<Page>`; the TUI never touches it.
//! - TUI -> actor: `mpsc<Cmd>`. Actor -> TUI: `mpsc<Event>` (try_send; the
//!   UI is never allowed to block the actor).
//! - Long work runs as spawned JOBS (scan, per-page process, rotate, PDF
//!   build). The actor loop only selects on commands + job completions and
//!   never awaits a long operation inline, so commands (delete, cancel,
//!   new session, quit) are handled within microseconds while work runs.
//! - The scanner is the single serialized resource (`Busy::Scanning`).
//!   Per-page cleaning/OCR runs as jobs that overlap with the NEXT scan —
//!   that's the whole point of a multi-page session (parity with the
//!   Python tool's background processing).
//! - Every state change ships a `SessionMeta` snapshot with the pages so
//!   the footer/header always show current facts (busy badge, output path,
//!   dirty flag). Live elapsed timers are computed by the UI from
//!   `Instant`s — no actor round-trips for ticking.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::backend::pdf::{self, BuildOutcome};
use crate::backend::scan;
use crate::config::{Cleanup, Config};

/// Unique per-page id (never reused; reorder never renames files).
pub type PageId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageStatus {
    /// scanimage running.
    Scanning,
    /// unpaper/rotate/ocr running.
    Processing,
    /// Image ready + text extracted.
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
    /// Rotate page image 90° CW (false = CCW) and re-OCR.
    Rotate(PageId, bool),
    /// Delete page (kills job if processing; deferred if scanning).
    Delete(PageId),
    /// Move page within the list (index-based).
    Move { from: usize, to: usize },
    /// Build the final PDF; actor refuses while busy.
    Finish,
    /// Reset the session (drop all pages, new output path).
    NewSession,
    /// Query installed tesseract languages (reply via event).
    ListLangs,
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
        /// Process settings: the job chains scan -> unpaper -> OCR itself so
        /// the actor never awaits these (only completion notifications).
        cleanup: Cleanup,
        unpaper_extra_args: Vec<String>,
        langs: String,
        dir: PathBuf,
        token: CancellationToken,
    },
    Rotate {
        id: PageId,
        image: PathBuf,
        cw: bool,
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
        text: Option<String>,
        result: anyhow::Result<()>,
    },
    Rotate {
        id: PageId,
        /// Ok(text) = rotated + re-OCRed; Err(msg) = failed/cancelled.
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
    out_pdf: PathBuf,
    pages: Vec<Page>,
    next_id: PageId,
    busy: Busy,
    busy_since: Option<Instant>,
    /// Scan job cancellation (single scanner resource).
    scan_token: Option<CancellationToken>,
    /// Per-page job cancellation tokens (unpaper/OCR/rotate).
    jobs: HashMap<PageId, CancellationToken>,
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
        let out_pdf = pdf::unique_path(&cfg.output, pdf::stamp_now());
        let dir = state_dir().join("sessions").join(pdf::stamp_now());
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            cfg,
            device,
            dir,
            out_pdf,
            pages: Vec::new(),
            next_id: 1,
            busy: Busy::Idle,
            busy_since: None,
            scan_token: None,
            jobs: HashMap::new(),
            event_tx,
            job_tx,
        })
    }

    // ------------------------------------------------------------- helpers

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
        }
    }

    pub fn dirty(&self) -> bool {
        self.busy != Busy::Idle
            || !self.jobs.is_empty()
            || self
                .pages
                .iter()
                .any(|p| p.status != PageStatus::DeletePending)
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
                if self.busy == Busy::Scanning {
                    return Err("scanner busy - press Esc to cancel".into());
                }
                if self.busy == Busy::Finishing {
                    return Err("building PDF - scan once it finishes".into());
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
                if self.busy == Busy::Scanning {
                    return Err("scanner busy - press Esc to cancel".into());
                }
                match self.pages.iter().find(|p| p.id == *id).map(|p| p.status) {
                    Some(PageStatus::Ready) | Some(PageStatus::Failed) => Ok(()),
                    Some(_) => Err("page busy - rescan after it finishes".into()),
                    None => Err("no such page".into()),
                }
            }
            Cmd::Rotate(id, _) => {
                if self.jobs.contains_key(id) {
                    return Err("page busy - rotate after it finishes".into());
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
                if let Some(p) = self.pages.iter().find(|p| p.status == PageStatus::Failed) {
                    return Err(format!(
                        "page {} failed - rescan (r) or delete (d) first",
                        p.id
                    ));
                }
                if let Some(p) = self
                    .pages
                    .iter()
                    .find(|p| matches!(p.status, PageStatus::Processing))
                {
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
            Cmd::ListLangs => Ok(()),
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
            Cmd::ListLangs => {
                let langs = scan::available_langs().await.unwrap_or_default();
                self.push(Event::Langs(langs));
            }
        }
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
                text,
                result,
            } => self.on_scan_done(
                id,
                is_rescan,
                image,
                unpaper_deskewed,
                used_fallback,
                text,
                result,
            ),
            JobDone::Rotate { id, result } => self.on_rotate_done(id, result),
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
                    langs: self.cfg.langs.clone(),
                    dir: self.dir.clone(),
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
                    langs: self.cfg.langs.clone(),
                    dir: self.dir.clone(),
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
                    langs,
                    dir,
                    token,
                } => {
                    // scan -> unpaper -> OCR chained in the job; the actor
                    // only receives the final result and stays responsive.
                    let result = scan::scan_page(&device, dpi, &mode, &path, &token).await;
                    match result {
                        Ok(scan::ScanOutcome { used_fallback }) => {
                            if used_fallback {
                                tracing::warn!(
                                    "scanner rejected --resolution/--mode; fallback used (page dpi metadata may differ from request)"
                                );
                            }
                            let (image, unpaper_deskewed) =
                                pdf::maybe_unpaper(&path, cleanup, &unpaper_extra_args, &mode)
                                    .await;
                            let text = if token.is_cancelled() {
                                None
                            } else {
                                scan::ocr_text_cancellable(&image, &langs, &dir, &token)
                                    .await
                                    .ok()
                            };
                            JobDone::Scan {
                                id,
                                is_rescan,
                                image,
                                unpaper_deskewed,
                                used_fallback,
                                text,
                                result: Ok(()),
                            }
                        }
                        Err(e) => JobDone::Scan {
                            id,
                            is_rescan,
                            image: path,
                            unpaper_deskewed: false,
                            used_fallback: false,
                            text: None,
                            result: Err(e),
                        },
                    }
                }
                Job::Rotate {
                    id,
                    image,
                    cw,
                    langs,
                    dir,
                    token,
                } => {
                    let result = match pdf::rotate_png(&image, cw).await {
                        Ok(()) => {
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
                        Err(e) => Err(format!("rotate failed: {e:#}")),
                    };
                    JobDone::Rotate { id, result }
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
        text: Option<String>,
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
                if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                    p.image = Some(image);
                    p.unpaper_deskewed = unpaper_deskewed;
                    p.used_fallback = used_fallback;
                    p.text = text;
                    p.status = PageStatus::Ready;
                    p.stage = None;
                    p.stage_started = None;
                    p.image_gen += 1;
                }
                self.notify_pages();
                let note = match (used_fallback, unpaper_deskewed) {
                    (true, _) => " - scanner rejected resolution/mode; page size may differ",
                    (false, false)
                        if self.cfg.cleanup == Cleanup::Legacy
                            && self.cfg.unpaper_extra_args.is_empty() =>
                    {
                        // Normal for color pages in legacy mode (unpaper is
                        // grayscale-only); ocrmypdf deskews at finish.
                        ""
                    }
                    (false, false) => " - unpaper cleanup unavailable; ocrmypdf deskews at finish",
                    (false, true) => "",
                };
                self.status(format!("page {id} ready{note}"));
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

    fn start_rotate(&mut self, id: PageId, cw: bool) {
        let Some(page) = self.pages.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let Some(image) = page.image.clone() else {
            return;
        };
        if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
            p.status = PageStatus::Processing;
            p.stage = Some(Stage::Clean);
            p.stage_started = Some(Instant::now());
        }
        let token = CancellationToken::new();
        self.jobs.insert(id, token.clone());
        self.notify_pages();
        self.spawn_job(Job::Rotate {
            id,
            image,
            cw,
            langs: self.cfg.langs.clone(),
            dir: self.dir.clone(),
            token,
        });
    }

    fn on_rotate_done(&mut self, id: PageId, result: Result<Option<String>, String>) {
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
                    if text.is_some() {
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
            cleanup: self.cfg.cleanup,
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
            PageStatus::Processing => {
                // Cancel that page's job; its completion handler finishes
                // the deletion (and the job checks the pending flag).
                if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
                    p.status = PageStatus::DeletePending;
                }
                if let Some(t) = self.jobs.get(&id) {
                    t.cancel();
                }
                self.notify_pages();
                self.status("deleting page…");
            }
            _ => {
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
        for p in &self.pages {
            remove_page_files(p);
        }
        self.pages.clear();
        self.jobs.clear();
        self.busy = Busy::Idle;
        self.busy_since = None;
        self.out_pdf = pdf::unique_path(&self.cfg.output, pdf::stamp_now());
        self.dir = state_dir().join("sessions").join(pdf::stamp_now());
        let _ = std::fs::create_dir_all(&self.dir);
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
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>(64);
    let (event_tx, event_rx) = mpsc::channel::<Event>(256);
    let (job_tx, job_rx) = mpsc::unbounded_channel::<JobDone>();
    let session = Session::with_channels(cfg, device, event_tx, job_tx)?;
    tokio::spawn(actor_loop(session, cmd_rx, job_rx));
    Ok((cmd_tx, event_rx))
}
