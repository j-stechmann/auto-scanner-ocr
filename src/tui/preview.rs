//! Contact-sheet preview: one thumbnail per page, off-thread decode/encode.
//!
//! Design (after the multi-image feasibility review):
//! - `PreviewWorker` reconciles its thumbnail map against `App.pages` every
//!   frame (cheap; early-returns make the common case O(1) per page).
//! - Each page owns a `ThreadProtocol` and its OWN resize-request +
//!   response channel pair. Sharing one response channel is a real bug:
//!   `ResizeResponse.id` starts at 0 per protocol, so lockstep encodes
//!   (e.g. a grid resize) would let one thumb adopt another's image.
//! - Decodes/encodes run on `spawn_blocking`; the UI thread only polls and
//!   adopts results. Failed decodes are cached per (path, gen) so the
//!   per-frame reconcile does not retry-loop (e.g. DeletePending racing
//!   file removal).
//! - Replacement protocols are always swapped in via `replace_protocol`
//!   (never `empty_protocol`, which strands the cell blank forever) —
//!   stale in-flight responses are dropped by the crate's id check.
//! - Encode kicks are capped per frame (encode storm on resize); encode
//!   rects must equal render rects or the protocol re-encodes every frame
//!   (the flashing bug documented in the ratatui-image sync recipe).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use image::imageops::FilterType;
use ratatui::layout::{Rect, Size};
use ratatui_image::{
    picker::Picker,
    protocol::StatefulProtocol,
    thread::{ResizeRequest, ResizeResponse, ThreadProtocol},
    Resize, ResizeEncodeRender,
};
use tokio::sync::mpsc;

use super::app::App;
use crate::session::PageId;

/// Messages decode worker -> UI, tagged with the owning page.
enum Loaded {
    /// Fresh protocol for (page, path, generation) + decoded pixel aspect.
    Protocol(PageId, Box<StatefulProtocol>, PathBuf, u32, f32),
    Failed(PageId, PathBuf, u32, String),
}

/// Per-page thumbnail state.
struct Thumb {
    /// Path + generation the current/pending protocol corresponds to.
    path: PathBuf,
    gen: u32,
    protocol: ThreadProtocol,
    /// This thumb's encode RESULTS (per-thumb channel — sharing one
    /// response channel across thumbs would let thumb A adopt thumb B's
    /// image: ResizeResponse.id starts at 0 per protocol and collides in
    /// lockstep encodes). The worker task holds the sender half.
    response_rx: mpsc::UnboundedReceiver<Result<ResizeResponse, ratatui_image::errors::Errors>>,
    /// Decode in flight for a newer (path, gen) than the protocol holds.
    pending: bool,
}

/// Max pixel width/height we pre-decode a page to. Thumbnails can never
/// display more than their cell area in font-size pixels, so keeping the
/// full-resolution DynamicImage (33 MB at 300dpi) per page resident is waste.
const MAX_THUMB_PIXELS: u32 = 1600;

/// Max encode kicks issued per frame (stagger terminal-resize bursts).
const MAX_SYNC_KICKS_PER_FRAME: usize = 4;

pub struct PreviewWorker {
    picker: Picker,
    /// One thumb per page; keyed by unique, never-reused page id.
    thumbs: HashMap<PageId, Thumb>,
    /// Decodes that failed for a given (path, gen); never retried silently.
    failed: HashSet<(PathBuf, u32)>,
    /// Aspects (width / height) of decoded pages, for cell layout.
    aspects: HashMap<PageId, f32>,
    rx: mpsc::UnboundedReceiver<Loaded>,
    tx: mpsc::UnboundedSender<Loaded>,
    /// Page ids currently being decoded (dedupe while in flight).
    decoding: HashSet<PageId>,
}

impl PreviewWorker {
    pub fn new(picker: Picker) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            picker,
            thumbs: HashMap::new(),
            failed: HashSet::new(),
            aspects: HashMap::new(),
            rx,
            tx,
            decoding: HashSet::new(),
        }
    }

    /// Font size in pixels (width, height) — cell layout needs the pixel
    /// aspect of a terminal cell to turn page aspect into cell aspect.
    pub fn font_size(&self) -> (u16, u16) {
        let fs = self.picker.font_size();
        (fs.width, fs.height)
    }

    /// Majority aspect among decoded pages (falls back to A4 portrait).
    /// Used by the UI to shape grid cells; published at decode time.
    pub fn cell_aspect(&self) -> f32 {
        if self.aspects.is_empty() {
            return 1.0 / std::f32::consts::SQRT_2; // A4 portrait: w/h = 1/√2
        }
        // Majority vote avoids one landscape scan reshaping the whole grid.
        let mut portrait = 0usize;
        let mut landscape = 0usize;
        let mut sum = 0.0f32;
        for a in self.aspects.values() {
            sum += a;
            if *a >= 1.0 {
                landscape += 1;
            } else {
                portrait += 1;
            }
        }
        let n = self.aspects.len() as f32;
        if portrait > landscape || landscape > portrait {
            // Clear majority: average within the majority group only.
            let group_sum = if portrait > landscape {
                self.aspects.values().filter(|a| **a < 1.0).sum::<f32>()
            } else {
                self.aspects.values().filter(|a| **a >= 1.0).sum::<f32>()
            };
            group_sum / (portrait.max(landscape) as f32)
        } else {
            sum / n
        }
    }

    /// Page aspect expressed in terminal CELLS: pixels-per-cell differ per
    /// axis (chars are ~1:2), so divide pixel aspect by the cell's pixel
    /// aspect. The grid then shapes cells that match page proportions.
    pub fn cell_aspect_in_cells(&self) -> f32 {
        let (fw, fh) = self.font_size();
        let cell_px_aspect = if fh == 0 { 1.0 } else { fw as f32 / fh as f32 };
        if cell_px_aspect <= 0.0 {
            return self.cell_aspect();
        }
        self.cell_aspect() / cell_px_aspect
    }

    /// Reconcile thumbnails against the current page list. Cheap per frame:
    /// only touches pages whose (path, gen) changed, vanished, or appeared.
    pub fn on_pages_changed(&mut self, app: &App) {
        let mut seen = HashSet::with_capacity(app.pages.len());
        for page in &app.pages {
            let Some(path) = &page.image else {
                continue; // no image yet (scanning/failed-first-attempt)
            };
            seen.insert(page.id);
            if self.thumbs.contains_key(&page.id) {
                let thumb = &self.thumbs[&page.id];
                if thumb.path == *path && thumb.gen == page.image_gen && !thumb.pending {
                    continue; // already current
                }
            }
            if self.failed.contains(&(path.clone(), page.image_gen)) {
                continue; // decode failed for this exact content; don't retry-loop
            }
            if self.decoding.contains(&page.id) {
                continue; // a newer decode is already in flight
            }
            self.decode(page.id, path.clone(), page.image_gen);
        }
        // Drop state for pages that vanished (delete / new session) — both
        // adopted thumbs and in-flight decodes (their results would be
        // dropped at adoption anyway, or never arrive).
        let gone: Vec<PageId> = self
            .thumbs
            .keys()
            .chain(self.decoding.iter())
            .filter(|id| !seen.contains(id))
            .copied()
            .collect();
        for id in gone {
            self.thumbs.remove(&id);
            self.aspects.remove(&id);
            // Also forget in-flight decodes: the page is gone, its result
            // would be dropped at adoption anyway (and might never arrive).
            self.decoding.remove(&id);
        }
    }

    fn decode(&mut self, id: PageId, path: PathBuf, gen: u32) {
        self.decoding.insert(id);
        let picker = self.picker.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = (|| -> anyhow::Result<(StatefulProtocol, f32)> {
                let img = image::ImageReader::open(&path)?
                    .with_guessed_format()?
                    .decode()?;
                // Pre-downscale: thumbnails cannot display more pixels than
                // their cell, and full-res sources would pin ~33 MB/page.
                let img = downscale(img, MAX_THUMB_PIXELS);
                let aspect = img.width() as f32 / img.height() as f32;
                let proto = picker.new_resize_protocol(img);
                Ok((proto, aspect))
            })();
            match result {
                Ok((proto, aspect)) => {
                    let _ = tx.send(Loaded::Protocol(
                        id,
                        Box::new(proto),
                        path.clone(),
                        gen,
                        aspect,
                    ));
                }
                Err(e) => {
                    let _ = tx.send(Loaded::Failed(id, path.clone(), gen, format!("{e:#}")));
                }
            }
        });
    }

    /// Poll decode completions. Returns true when any thumbnail changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Loaded::Protocol(id, proto, path, gen, aspect) => {
                    self.decoding.remove(&id);
                    match self.thumbs.get_mut(&id) {
                        Some(thumb) => {
                            // replace_protocol bumps the crate-internal id,
                            // so an in-flight stale encode is dropped safely.
                            thumb.protocol.replace_protocol(*proto);
                            thumb.path = path;
                            thumb.gen = gen;
                            thumb.pending = false;
                        }
                        None => {
                            // First decode for this page: create its thumb
                            // with its OWN request + response channel pair.
                            // (Never share response channels across thumbs —
                            // see Thumb.response_rx doc.)
                            let (req_tx, req_rx) = mpsc::unbounded_channel();
                            let (resp_tx, resp_rx) = mpsc::unbounded_channel();
                            self.thumbs.insert(
                                id,
                                Thumb {
                                    path,
                                    gen,
                                    protocol: ThreadProtocol::new(req_tx, Some(*proto)),
                                    response_rx: resp_rx,
                                    pending: false,
                                },
                            );
                            // The protocol sends resize requests on req_rx;
                            // a dedicated task encodes them off the UI thread.
                            spawn_encode_worker(req_rx, resp_tx);
                        }
                    }
                    self.aspects.insert(id, aspect);
                    changed = true;
                }
                Loaded::Failed(id, path, gen, err) => {
                    self.decoding.remove(&id);
                    tracing::warn!("preview decode failed for {}: {err}", path.display());
                    self.failed.insert((path, gen));
                }
            }
        }
        changed
    }

    /// Poll per-thumb encode completions and adopt them. Returns true when
    /// any thumbnail content changed.
    pub fn poll_resizes(&mut self) -> bool {
        let mut changed = false;
        for thumb in self.thumbs.values_mut() {
            while let Ok(encoded) = thumb.response_rx.try_recv() {
                match encoded {
                    Ok(resp) => {
                        if !thumb.protocol.update_resized_protocol(resp) {
                            // Expected after replace_protocol (stale gen);
                            // otherwise indicates channel-routing trouble.
                            tracing::debug!("preview: dropped stale encode response");
                        } else {
                            changed = true;
                        }
                    }
                    Err(e) => tracing::warn!("preview encode failed: {e}"),
                }
            }
        }
        changed
    }

    /// After draw: per cell, kick an encode if the protocol needs a
    /// different size. `rect` must be the exact rect later used for render.
    pub fn sync_cells(&mut self, cells: &[(PageId, Rect)]) {
        let mut kicks = 0usize;
        for (id, rect) in cells {
            let Some(thumb) = self.thumbs.get_mut(id) else {
                continue;
            };
            if thumb.pending {
                continue; // still decoding; render shows the old image
            }
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            let size = Size::new(rect.width, rect.height);
            if let Some(rect_px) = thumb
                .protocol
                .needs_resize(&Resize::Fit(Some(FilterType::Triangle)), size)
            {
                if kicks < MAX_SYNC_KICKS_PER_FRAME {
                    // needs_resize returns the exact rect to encode for. It
                    // must be passed on verbatim (see module doc: encode
                    // rect != render rect re-encodes + flashes every frame).
                    thumb
                        .protocol
                        .resize_encode(&Resize::Fit(Some(FilterType::Triangle)), rect_px);
                    kicks += 1;
                }
            }
        }
    }

    /// Render a thumbnail into its grid cell (cheap; encodes happen in
    /// sync_cells). Zero-area cells are skipped — protocols compute
    /// `area.width - 1` internally and panic on empty rects.
    pub fn render_cell(&mut self, id: PageId, rect: Rect, buf: &mut ratatui::buffer::Buffer) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        if let Some(thumb) = self.thumbs.get_mut(&id) {
            if thumb.pending {
                return; // keep the previous frame's cells until decode lands
            }
            thumb.protocol.render(rect, buf);
        }
    }

    /// Is the given page's thumbnail loaded (decode done, not replaced by a
    /// newer pending decode)? Note: mid-re-encode the protocol still counts
    /// as "has image" — the cell just re-renders blank until the encode
    /// lands (crate design); showing "no image" there would be wrong.
    pub fn has_image_for(&self, id: PageId) -> bool {
        self.thumbs.get(&id).is_some_and(|t| !t.pending)
    }
}

/// Dedicated encode loop for one thumbnail: drains the protocol's resize
/// requests and encodes each OFF the UI thread (sixel-encoding can take
/// hundreds of ms). Exits when the protocol side is dropped (thumb gone).
fn spawn_encode_worker(
    mut req_rx: mpsc::UnboundedReceiver<ResizeRequest>,
    resp_tx: mpsc::UnboundedSender<Result<ResizeResponse, ratatui_image::errors::Errors>>,
) {
    tokio::spawn(async move {
        while let Some(req) = req_rx.recv().await {
            let tx = resp_tx.clone();
            tokio::task::spawn_blocking(move || {
                let _ = tx.send(req.resize_encode());
            });
        }
    });
}

/// Downscale so the longer side is at most `max` pixels, preserving aspect.
fn downscale(img: image::DynamicImage, max: u32) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w <= max && h <= max {
        return img;
    }
    let scale = f64::from(max) / f64::from(w.max(h));
    let nw = ((f64::from(w) * scale).round() as u32).max(1);
    let nh = ((f64::from(h) * scale).round() as u32).max(1);
    img.resize(nw, nh, FilterType::Triangle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::Settings;

    /// Tests call `decode` (spawn_blocking), so each runs inside a tokio
    /// runtime via #[tokio::test].
    fn test_app_with_pages(n: usize) -> App {
        let (diag_tx, _diag_rx) = mpsc::channel(4);
        let mut app = App::new(
            crate::config::Config::default(),
            "test device".into(),
            diag_tx,
        );
        app.settings = Settings {
            dpi: 300,
            mode: "gray".into(),
        };
        // Fake pages with unique ids and image paths (files need not exist;
        // reconcile only compares path/gen and decodes off-thread).
        for i in 0..n {
            app.pages.push(crate::session::PageView {
                id: (i + 1) as PageId,
                status: crate::session::PageStatus::Ready,
                stage: None,
                stage_started: None,
                image: Some(PathBuf::from(format!("/tmp/fake_{i}.png"))),
                image_gen: 0,
                text: None,
                error: None,
                dpi: 300,
                mode: "gray".into(),
                rotated: false,
            });
        }
        if n > 0 {
            app.selected = 0;
        }
        app
    }

    fn worker() -> PreviewWorker {
        PreviewWorker::new(crate::tui::halfblocks_picker())
    }

    #[tokio::test]
    async fn reconcile_spawns_decodes_for_new_pages_only() {
        let mut w = worker();
        let app = test_app_with_pages(3);
        w.on_pages_changed(&app);
        assert_eq!(w.decoding.len(), 3);
        // Second call with unchanged state must not re-decode.
        w.on_pages_changed(&app);
        assert_eq!(w.decoding.len(), 3);
    }

    #[tokio::test]
    async fn pending_thumb_skips_redecode() {
        let mut w = worker();
        let mut app = test_app_with_pages(1);
        w.on_pages_changed(&app);
        assert_eq!(w.decoding.len(), 1);
        // Page image content changed (rotate/rescan) while decode in flight:
        // reconcile must not stack a second decode for the same page id.
        app.pages[0].image_gen += 1;
        w.on_pages_changed(&app);
        assert_eq!(w.decoding.len(), 1);
    }

    #[tokio::test]
    async fn vanished_pages_are_dropped() {
        let mut w = worker();
        let app = test_app_with_pages(2);
        w.on_pages_changed(&app);
        assert_eq!(w.decoding.len(), 2);
        // All pages deleted -> new session.
        let empty = test_app_with_pages(0);
        w.on_pages_changed(&empty);
        assert!(w.decoding.is_empty());
        assert!(w.thumbs.is_empty());
    }

    #[test]
    fn failed_decodes_are_cached_and_not_retried() {
        let mut w = worker();
        let app = test_app_with_pages(1);
        let path = app.pages[0].image.clone().unwrap();
        w.failed.insert((path, app.pages[0].image_gen));
        w.on_pages_changed(&app);
        assert!(w.decoding.is_empty());
    }

    #[tokio::test]
    async fn regen_change_redecodes_after_completion() {
        let mut w = worker();
        let mut app = test_app_with_pages(1);
        w.on_pages_changed(&app);
        assert_eq!(w.decoding.len(), 1);
        // Decode completes: only the bookkeeping matters here (thumb
        // adoption requires a real protocol), so simulate completion by
        // clearing the in-flight set; the test asserts the retry semantics:
        // a gen bump schedules a second decode once the first completes.
        app.pages[0].image_gen += 1;
        w.on_pages_changed(&app); // still decoding -> skipped
        assert_eq!(w.decoding.len(), 1);
        w.decoding.clear(); // emulate completion without protocol adoption
        w.on_pages_changed(&app); // now re-decode for new gen
        assert_eq!(w.decoding.len(), 1);
        assert_eq!(w.thumbs.len(), 0); // no thumb until a real protocol lands
    }

    #[test]
    fn cell_aspect_majority_and_fallback() {
        let mut w = worker();
        assert!((w.cell_aspect() - 1.0 / std::f32::consts::SQRT_2).abs() < 1e-6);
        w.aspects.insert(1, 0.707);
        w.aspects.insert(2, 0.71);
        w.aspects.insert(3, 2.0);
        let a = w.cell_aspect();
        assert!((a - (0.707 + 0.71) / 2.0).abs() < 1e-3);
    }

    #[test]
    fn downscale_caps_longer_side() {
        let img = image::DynamicImage::new_rgb8(3000, 2000);
        let small = downscale(img, 1600);
        assert_eq!(small.width(), 1600);
        assert_eq!(small.height(), 1067);
        // No upscale of small images.
        let img = image::DynamicImage::new_rgb8(800, 600);
        let same = downscale(img, 1600);
        assert_eq!(same.width(), 800);
        assert_eq!(same.height(), 600);
    }
}
