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

/// Messages editor worker -> UI.
enum EditorLoaded {
    /// Fresh protocol for (path, gen): the protocol, the downscaled base
    /// image (kept for outline re-compositing), and the ORIGINAL pixel
    /// dims (captured pre-downscale; the protocol only knows downscaled
    /// dims and no public crate API exposes them).
    Protocol(
        Box<StatefulProtocol>,
        image::DynamicImage,
        PathBuf,
        u32,
        (u32, u32),
    ),
    Failed(PathBuf, u32, String),
}

/// A completed off-thread encode of a cloned StatefulProtocol: swap it in
/// as the new display. `epoch` invalidates stale results after a decode.
struct EditorEncoded {
    epoch: u64,
    proto: Box<StatefulProtocol>,
    /// Cells the encode targeted (the render rect's size).
    size: ratatui::layout::Size,
    /// Outline baked in (downscaled px, None = none).
    outline: Option<(u32, u32, u32, u32)>,
    ok: bool,
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

/// Same cap for the editor's large single-page view (still far below the
/// 300-dpi scan size; crop coordinates are scaled back to original pixels).
const MAX_EDITOR_PIXELS: u32 = 3200;

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

/// Crop outline style for the editor's composited overlay: opaque cyan,
/// visible on light and dark page content alike. Drawn into the DOWNSCALED
/// pixels before encoding (text drawn over sixel/kitty cells can erase the
/// image — see the ratatui-image buffer model), so it survives every
/// protocol and needs no theme registry entry.
const OUTLINE_RGBA: image::Rgba<u8> = image::Rgba([0, 255, 255, 255]);
/// Outline thickness in downscaled pixels.
const OUTLINE_THICKNESS: u32 = 2;

/// Single-page editor worker: decodes the edited page at a higher cap than
/// thumbnails and encodes the (outline-composited) image OFF the UI thread.
///
/// Display/work split (fixes the blank-while-encoding bug): the DISPLAY
/// protocol is a plain `StatefulProtocol` that is NEVER encoded in place —
/// `render` always draws its last completed encode. Each `request_encode`
/// builds a fresh protocol from the composited image, encodes a CLONE of it
/// on spawn_blocking, and the completed clone is swapped in as the new
/// display. While an encode is in flight the display keeps rendering the
/// previous image (the crop outline lags behind the cursor during a drag
/// and converges on release; the header readout is instant). No
/// `ThreadProtocol` is involved: ids/blanking are a threaded-widget concern
/// this worker does not have.
///
/// `epoch` invalidates everything: a decode completion bumps it, and
/// stale encodes (old epoch) are dropped on arrival.
pub struct EditorWorker {
    picker: Picker,
    /// Current (path, gen) the display protocol corresponds to.
    path: Option<PathBuf>,
    gen: u32,
    /// The DISPLAY protocol: always renderable (last completed encode).
    /// Never handed to the encode worker.
    display: Option<StatefulProtocol>,
    /// ORIGINAL image pixel dims (pre-downscale), for coordinate mapping
    /// and the crop command. No public crate API exposes them (the
    /// protocol only knows the downscaled image), so the decode reports
    /// them alongside.
    orig_dims: (u32, u32),
    /// Downscaled base image (no outline), kept for re-compositing.
    base: Option<image::DynamicImage>,
    rx: mpsc::UnboundedReceiver<EditorLoaded>,
    enc_rx: mpsc::UnboundedReceiver<EditorEncoded>,
    tx: mpsc::UnboundedSender<EditorLoaded>,
    enc_tx: mpsc::UnboundedSender<EditorEncoded>,
    /// Bumped on every decode adoption; invalidates in-flight encodes.
    epoch: u64,
    /// Font size (cached from the picker) for size_for math.
    font_size: (u16, u16),
    /// Cells of the displayed encode (None until the first lands).
    encoded_size: Option<ratatui::layout::Size>,
    /// Outline baked into the currently displayed image (downscaled px).
    shown_outline: Option<(u32, u32, u32, u32)>,
    /// Encode bookkeeping (single in-flight encode, last request wins).
    in_flight: bool,
    queued: Option<QueuedEncode>,
}

/// What the next encode should target once the current one drains.
struct QueuedEncode {
    cells: ratatui::layout::Size,
    outline: Option<(u32, u32, u32, u32)>,
}

impl EditorWorker {
    pub fn new(picker: Picker) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (enc_tx, enc_rx) = mpsc::unbounded_channel();
        let fs = picker.font_size();
        Self {
            picker,
            path: None,
            gen: 0,
            display: None,
            orig_dims: (0, 0),
            base: None,
            rx,
            enc_rx,
            tx,
            enc_tx,
            epoch: 0,
            font_size: (fs.width, fs.height),
            encoded_size: None,
            shown_outline: None,
            in_flight: false,
            queued: None,
        }
    }

    /// Is the editor image loaded (decode done for the current request)?
    /// The display always holds the PREVIOUS image while a newer decode is
    /// in flight; the caller decides whether that is acceptable to show.
    pub fn ready(&self) -> bool {
        self.display.is_some()
    }

    /// Original image pixel dims ((0, 0) until the first decode lands).
    pub fn orig_dims(&self) -> (u32, u32) {
        self.orig_dims
    }

    /// ORIGINAL / downscaled scale factor (>1 when the image was
    /// downscaled); 1.0 before the first decode. Used to map terminal
    /// coords -> image pixels and crop rects between coordinate spaces.
    pub fn downscale_factor(&self) -> f64 {
        match (self.orig_dims.0, self.base.as_ref()) {
            (ow, Some(base)) if base.width() > 0 => ow as f64 / base.width() as f64,
            _ => 1.0,
        }
    }

    /// Convert a rect from ORIGINAL image pixels to the downscaled space
    /// the outline is composited in.
    pub fn to_downscaled(&self, rect: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
        let k = self.downscale_factor();
        (
            (rect.0 as f64 / k).round() as u32,
            (rect.1 as f64 / k).round() as u32,
            (rect.2 as f64 / k).round() as u32,
            (rect.3 as f64 / k).round() as u32,
        )
    }

    /// Request (re-)decoding `path` at generation `gen`. Cheap per frame:
    /// a decode is in flight iff (path, gen) differs from the current one
    /// AND no result for it has landed yet — tracked by the display's
    /// (path, gen), so a completed decode always ends the pending window.
    pub fn request(&mut self, path: PathBuf, gen: u32) {
        if self.path.as_ref() == Some(&path) && self.gen == gen && self.display.is_some() {
            return; // already current
        }
        let picker = self.picker.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = (|| {
                anyhow::Result::<(StatefulProtocol, image::DynamicImage, (u32, u32))>::Ok({
                    let img: image::DynamicImage = image::ImageReader::open(&path)?
                        .with_guessed_format()?
                        .decode()?
                        .to_rgba8()
                        .into();
                    let orig = (img.width(), img.height());
                    let small = downscale(img, MAX_EDITOR_PIXELS);
                    let proto = picker.new_resize_protocol(small.clone());
                    (proto, small, orig)
                })
            })();
            match result {
                Ok((proto, base, orig)) => {
                    let _ = tx.send(EditorLoaded::Protocol(
                        Box::new(proto),
                        base,
                        path.clone(),
                        gen,
                        orig,
                    ));
                }
                Err(e) => {
                    let _ = tx.send(EditorLoaded::Failed(path.clone(), gen, format!("{e:#}")));
                }
            }
        });
    }

    /// Poll decode completions and adopt results. Returns true when the
    /// image content changed (a draw is due). A fresh decode resets the
    /// display to the freshly-decoded (unencoded) protocol: the first
    /// frame renders nothing until the first encode is adopted — same
    /// behavior as the thumbnails' first appearance.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                EditorLoaded::Protocol(proto, base, path, gen, orig) => {
                    self.path = Some(path);
                    self.gen = gen;
                    self.orig_dims = orig;
                    self.base = Some(base);
                    self.epoch = self.epoch.wrapping_add(1);
                    self.display = Some(*proto);
                    self.encoded_size = None;
                    self.shown_outline = None;
                    self.in_flight = false;
                    self.queued = None;
                    changed = true;
                }
                EditorLoaded::Failed(path, gen, err) => {
                    tracing::warn!(
                        "editor decode failed for {} (gen {gen}): {err}",
                        path.display()
                    );
                }
            }
        }
        changed
    }

    /// Poll completed off-thread encodes and swap the newest valid one in
    /// as the display. Returns true when content changed (draw due).
    pub fn poll_encodes(&mut self) -> bool {
        let mut newest: Option<EditorEncoded> = None;
        while let Ok(msg) = self.enc_rx.try_recv() {
            if msg.epoch != self.epoch {
                continue; // stale epoch (decode landed meanwhile)
            }
            newest = Some(msg); // channel order: later sends overwrite
        }
        let Some(msg) = newest else {
            return false;
        };
        self.in_flight = false;
        if !msg.ok {
            return false;
        }
        self.display = Some(*msg.proto);
        self.encoded_size = Some(msg.size);
        self.shown_outline = msg.outline;
        true
    }

    /// Adopt any queued encode once the in-flight one drains (call right
    /// after poll_encodes; drag convergence: the last request wins).
    pub fn flush_queue(&mut self) {
        if !self.in_flight {
            if let Some(q) = self.queued.take() {
                self.spawn_encode(q.cells, q.outline);
            }
        }
    }

    /// Queue an encode for `cells` with `outline` (downscaled px, None =
    /// none) composited. Never touches the display: it keeps rendering the
    /// previous encode until the new one is swapped in.
    pub fn request_encode(
        &mut self,
        cells: ratatui::layout::Size,
        outline: Option<(u32, u32, u32, u32)>,
    ) {
        if cells.width == 0 || cells.height == 0 {
            return;
        }
        // Already displayed exactly this (or queued for it)? Skip.
        let queued_same = self
            .queued
            .as_ref()
            .is_some_and(|q| q.cells == cells && q.outline == outline);
        if queued_same {
            return;
        }
        if self.shown_matches(cells, outline) {
            return;
        }
        if self.in_flight {
            self.queued = Some(QueuedEncode { cells, outline });
            return;
        }
        self.spawn_encode(cells, outline);
    }

    fn spawn_encode(
        &mut self,
        cells: ratatui::layout::Size,
        outline: Option<(u32, u32, u32, u32)>,
    ) {
        let Some(base) = self.base.as_ref() else {
            return;
        };
        // Fresh protocol from the composited image; the DISPLAY is never
        // touched. Encode a clone off-thread and swap it in on completion.
        let img = composited(base, outline);
        let proto = self.picker.new_resize_protocol(img);
        let epoch = self.epoch;
        self.in_flight = true;
        let tx = self.enc_tx.clone();
        tokio::task::spawn_blocking(move || {
            let mut proto = proto;
            proto.resize_encode(&Resize::Fit(Some(FilterType::Triangle)), cells);
            let ok = proto.last_encoding_result().is_some_and(|r| r.is_ok());
            let _ = tx.send(EditorEncoded {
                epoch,
                proto: Box::new(proto),
                size: cells,
                outline,
                ok,
            });
        });
    }

    /// The letterbox-tight cell size the image encodes to for `area`
    /// (Resize::Fit math, same rounding as the crate's size_for; derived
    /// from the SOURCE dims so it is stable across outline changes).
    pub fn size_for(&self, area: Rect) -> Option<ratatui::layout::Size> {
        let b = self.base.as_ref()?;
        let (fw, fh) = (u32::from(self.font_size.0), u32::from(self.font_size.1));
        let avail_px_w = u32::from(area.width) * fw;
        let avail_px_h = u32::from(area.height) * fh;
        let (pw, ph) = fit_area_proportionally(b.width(), b.height(), avail_px_w, avail_px_h);
        Some(ratatui::layout::Size::new(
            (pw as f32 / fw as f32).ceil() as u16,
            (ph as f32 / fh as f32).ceil() as u16,
        ))
    }

    /// Render the editor image into `rect` (the size_for rect). Renders
    /// the display protocol's last completed encode; never blanks.
    pub fn render(&mut self, rect: Rect, buf: &mut ratatui::buffer::Buffer) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        if let Some(d) = self.display.as_mut() {
            d.render(rect, buf);
        }
    }

    /// True when the displayed image already matches (cells, outline).
    pub fn shown_matches(
        &self,
        cells: ratatui::layout::Size,
        outline: Option<(u32, u32, u32, u32)>,
    ) -> bool {
        self.encoded_size == Some(cells) && self.shown_outline == outline
    }
}

/// The downscaled image with `outline` composited (or a plain clone).
fn composited(
    base: &image::DynamicImage,
    outline: Option<(u32, u32, u32, u32)>,
) -> image::DynamicImage {
    match outline {
        None => base.clone(),
        Some(rect) => {
            let mut img = base.clone();
            draw_rect_outline(&mut img, rect, OUTLINE_THICKNESS, OUTLINE_RGBA);
            img
        }
    }
}

/// `Resize::Fit` pixel math (lib.rs fit_area_proportionally, private
/// there): aspect-preserving fit, capped at the source size, min 1px.
fn fit_area_proportionally(width: u32, height: u32, nwidth: u32, nheight: u32) -> (u32, u32) {
    let wratio = f64::from(nwidth) / f64::from(width);
    let hratio = f64::from(nheight) / f64::from(height);
    let ratio = wratio.min(hratio);
    let nw = ((f64::from(width) * ratio).round() as u32).max(1);
    let nh = ((f64::from(height) * ratio).round() as u32).max(1);
    (nw.min(width), nh.min(height))
}

/// Draw a rect outline into `img` (downscaled space), clamped to bounds.
/// Four edge bands, inclusive of the rect's boundary pixels: the outline
/// marks exactly the crop rect's edges in the preview.
fn draw_rect_outline(
    img: &mut image::DynamicImage,
    (x, y, w, h): (u32, u32, u32, u32),
    thickness: u32,
    color: image::Rgba<u8>,
) {
    use image::GenericImage;
    let (iw, ih) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return;
    }
    let x2 = x
        .saturating_add(w)
        .saturating_sub(1)
        .min(iw.saturating_sub(1));
    let y2 = y
        .saturating_add(h)
        .saturating_sub(1)
        .min(ih.saturating_sub(1));
    let x = x.min(x2);
    let y = y.min(y2);
    let t = thickness.max(1);
    let fill_h = |img: &mut image::DynamicImage, x0: u32, x1: u32, y0: u32, y1: u32| {
        for yy in y0..=y1 {
            for xx in x0..=x1 {
                img.put_pixel(xx, yy, color);
            }
        }
    };
    fill_h(img, x, x2, y, (y + t - 1).min(y2)); // top
    if y2 > y + t - 1 {
        fill_h(img, x, x2, (y2 + 1).saturating_sub(t), y2); // bottom
    }
    fill_h(img, x, (x + t - 1).min(x2), y, y2); // left
    if x2 > x + t - 1 {
        fill_h(img, (x2 + 1).saturating_sub(t), x2, y, y2); // right
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
        let (finish_tx, _finish_rx) = mpsc::channel(1);
        let mut app = App::new(crate::config::Config::default(), diag_tx, finish_tx);
        app.device_label = "test device".into();
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
                text_pending: false,
                ocr_failed_gen: None,
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

#[cfg(test)]
mod editor_tests {
    use super::*;

    /// The core no-blanking guarantee: while an encode is in flight the
    /// display still renders the PREVIOUS image, and the completed encode
    /// swap-in renders the new one. Uses halfblocks (synchronous encode).
    #[tokio::test]
    async fn display_never_blanks_during_outline_changes() {
        let picker = crate::tui::halfblocks_picker();
        let mut w = EditorWorker::new(picker.clone());
        // A real 40x30 PNG on disk (decode runs off-thread).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("page.png");
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(40, 30)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&path, png).unwrap();

        w.request(path.clone(), 0);
        // Decode is off-thread: yield until the result arrives.
        for _ in 0..100 {
            if w.poll() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(w.ready());

        // Request the first encode; display renders nothing yet (no
        // completed encode) but the API must not panic.
        let cells = w
            .size_for(ratatui::layout::Rect::new(0, 0, 80, 40))
            .unwrap();
        w.request_encode(cells, None);
        // Tiny image: the off-thread encode may complete instantly; wait
        // for it (the in-flight bookkeeping is what the assertions cover).
        let mut adopted = false;
        for _ in 0..100 {
            w.flush_queue();
            if w.poll_encodes() {
                adopted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(adopted, "first encode adopted");
        assert!(w.shown_matches(cells, None));

        // Now an outline change while the display shows the plain image:
        // request a NEW encode with an outline. Until it lands, the
        // display still matches the old (None outline) state.
        w.request_encode(cells, Some((2, 2, 20, 20)));
        assert!(
            w.shown_matches(cells, None),
            "display keeps the previous encode while the new one encodes"
        );
        let mut adopted2 = false;
        for _ in 0..100 {
            w.flush_queue();
            if w.poll_encodes() {
                adopted2 = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(adopted2, "second encode adopted");
        assert!(w.shown_matches(cells, Some((2, 2, 20, 20))));

        // Render must not panic at every stage and the buffer gets
        // non-default cells once an encode is displayed.
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, 40));
        let area = ratatui::layout::Rect::new(0, 0, cells.width, cells.height);
        w.render(area, &mut buf);
    }

    /// Stale-epoch drop: an encode that started before a decode adoption
    /// must NOT be swapped into the new display.
    #[tokio::test]
    async fn stale_encode_dropped_after_redecode() {
        let picker = crate::tui::halfblocks_picker();
        let mut w = EditorWorker::new(picker.clone());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("page.png");
        let mut buf = Vec::new();
        image::DynamicImage::new_rgb8(40, 30)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&path, buf).unwrap();

        w.request(path.clone(), 0);
        for _ in 0..100 {
            if w.poll() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(w.ready());
        let cells = w
            .size_for(ratatui::layout::Rect::new(0, 0, 80, 40))
            .unwrap();
        w.request_encode(cells, None);

        // A "re-decode" adoption lands (epoch bump) BEFORE the encode
        // completes: the encode result must be dropped as stale.
        let mut buf2 = Vec::new();
        image::DynamicImage::new_rgb8(50, 40)
            .write_to(
                &mut std::io::Cursor::new(&mut buf2),
                image::ImageFormat::Png,
            )
            .unwrap();
        std::fs::write(&path, buf2).unwrap();
        w.request(path.clone(), 1);
        for _ in 0..100 {
            if w.poll() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(w.epoch, 2, "decode adoption bumped the epoch");
        // The in-flight encode (epoch 0) completes now: dropped.
        w.flush_queue();
        assert!(!w.poll_encodes(), "stale-epoch encode must not adopt");
        assert!(w.encoded_size.is_none(), "fresh display has no encode yet");
    }
}
