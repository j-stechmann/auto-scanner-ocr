//! Image preview: off-thread decode + resize/encode via ratatui-image.
//!
//! Flow (per the UX review's recipe):
//! - background worker decodes the page PNG and builds a `StatefulProtocol`
//!   with the shared `Picker`, then does the first `resize_encode` for the
//!   current preview area and ships the protocol back over a channel
//! - the UI thread only polls completed results and renders the cached
//!   encoding; re-encodes happen when the selection, image generation, or
//!   pane geometry changes (checked once per frame in `sync_area`)

use std::path::PathBuf;

use ratatui::layout::{Rect, Size};
use ratatui_image::{
    picker::Picker,
    protocol::StatefulProtocol,
    thread::{ResizeRequest, ThreadProtocol},
    Resize, ResizeEncodeRender,
};
use tokio::sync::mpsc;

use super::app::App;

/// Messages worker -> UI.
enum Loaded {
    /// Fresh protocol for (path, generation) at the given area size.
    Protocol(Box<StatefulProtocol>, PathBuf, u32, Size),
    Failed(PathBuf, String),
}

pub struct PreviewWorker {
    /// Shared picker (font size + protocol). Cheap to clone.
    picker: Picker,
    /// Current renderable state (UI-owned; encode work happens in ThreadProtocol).
    protocol: Option<ThreadProtocol>,
    /// What the current protocol shows.
    loaded: Option<(PathBuf, u32)>,
    rx: mpsc::UnboundedReceiver<Loaded>,
    tx: mpsc::UnboundedSender<Loaded>,
    /// Channel ThreadProtocol uses to hand back resize requests.
    resize_rx: Option<mpsc::UnboundedReceiver<ResizeRequest>>,
    /// Completed encodings coming back from spawn_blocking workers.
    encoded_rx:
        mpsc::UnboundedReceiver<Result<ratatui_image::thread::ResizeResponse, _EncodeError>>,
    ui_tx: mpsc::UnboundedSender<Result<ratatui_image::thread::ResizeResponse, _EncodeError>>,
    /// Whether a decode is in flight.
    pending: Option<(PathBuf, u32)>,
}

/// Alias so the struct fields stay readable (Errors type from the crate).
type _EncodeError = ratatui_image::errors::Errors;

impl PreviewWorker {
    pub fn new(picker: Picker) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (ui_tx, encoded_rx) = mpsc::unbounded_channel();
        Self {
            picker,
            protocol: None,
            loaded: None,
            rx,
            tx,
            resize_rx: None,
            encoded_rx,
            ui_tx,
            pending: None,
        }
    }

    /// Called whenever the page list/selection changes.
    pub fn on_pages_changed(&mut self, app: &App) {
        let want = app
            .selected_page()
            .and_then(|p| p.image.as_ref().map(|img| (img.clone(), p.image_gen)));
        match want {
            None => {
                if self.loaded.is_some() || self.protocol.is_some() {
                    self.protocol = None;
                    self.loaded = None;
                    self.pending = None;
                }
            }
            Some((path, gen)) => {
                if self
                    .loaded
                    .as_ref()
                    .is_some_and(|(p, g)| *p == path && *g == gen)
                {
                    return; // already showing this exact image
                }
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|(p, g)| *p == path && *g == gen)
                {
                    return; // already loading it
                }
                self.request(path, gen);
            }
        }
    }

    fn request(&mut self, path: PathBuf, generation: u32) {
        self.pending = Some((path.clone(), generation));
        let picker = self.picker.clone();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = (|| -> anyhow::Result<StatefulProtocol> {
                let img = image::ImageReader::open(&path)?
                    .with_guessed_format()?
                    .decode()?;
                Ok(picker.new_resize_protocol(img))
            })();
            match result {
                Ok(proto) => {
                    let _ = tx.send(Loaded::Protocol(
                        Box::new(proto),
                        path.clone(),
                        generation,
                        Size::new(0, 0), // not yet encoded for any area
                    ));
                }
                Err(e) => {
                    let _ = tx.send(Loaded::Failed(path, format!("{e:#}")));
                }
            }
        });
    }

    /// Poll worker results. Returns true when the preview content changed and
    /// a redraw is worthwhile.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Loaded::Protocol(proto, path, gen, _size) => {
                    let (rtx, rrx) = mpsc::unbounded_channel();
                    let tp = ThreadProtocol::new(rtx, Some(*proto));
                    self.resize_rx = Some(rrx);
                    self.protocol = Some(tp);
                    self.loaded = Some((path, gen));
                    self.pending = None;
                    changed = true;
                }
                Loaded::Failed(path, err) => {
                    tracing::warn!("preview load failed for {}: {err}", path.display());
                    self.pending = None;
                }
            }
        }
        changed
    }

    /// Poll resize-encode completions from ThreadProtocol's worker channel.
    /// The heavy encode runs on `spawn_blocking`; this only adopts results.
    pub fn poll_resizes(&mut self) -> bool {
        let mut changed = false;
        let Some(rx) = &mut self.resize_rx else {
            return false;
        };
        while let Ok(req) = rx.try_recv() {
            // Move the encode OFF the UI thread: sixel-encoding a full-page
            // image takes hundreds of ms and would freeze the event loop.
            let tx = self.ui_tx.clone();
            tokio::task::spawn_blocking(move || {
                let encoded = req.resize_encode();
                // Send back for adoption on the UI thread (protocol state is
                // not Send-safe to leave in the worker; the completed
                // encoding is plain data).
                let _ = tx.send(encoded);
            });
        }
        // Adopt any completed encodings.
        while let Ok(encoded) = self.encoded_rx.try_recv() {
            if let Some(protocol) = &mut self.protocol {
                match encoded {
                    Ok(resp) => {
                        protocol.update_resized_protocol(resp);
                        changed = true;
                    }
                    Err(e) => tracing::warn!("preview encode failed: {e}"),
                }
            }
        }
        changed
    }

    /// After draw: if the protocol needs a different size for this area,
    /// kick off an encode. Called once per frame with the preview area.
    pub fn sync_area(&mut self, area: Rect) {
        let Some(protocol) = &mut self.protocol else {
            return;
        };
        if area.width == 0 || area.height == 0 {
            return;
        }
        let size = Size::new(area.width, area.height);
        // needs_resize returns the exact RECT to encode for. It must be
        // passed on verbatim: encoding with the full target area instead
        // makes last_encoding_area != size_for forever, so needs_resize
        // keeps returning Some and every frame re-encodes + re-emits the
        // sixel -> the image flashes twice per second.
        if let Some(rect) = protocol.needs_resize(&Resize::Fit(None), size) {
            protocol.resize_encode(&Resize::Fit(None), rect);
        }
    }

    /// Render the current encoding into the buffer (cheap; no encode here).
    pub fn render(&mut self, area: Rect, buf: &mut ratatui::buffer::Buffer) {
        if let Some(protocol) = &mut self.protocol {
            use ratatui_image::ResizeEncodeRender as _;
            protocol.render(area, buf);
        }
    }

    pub fn scroll(&mut self, _delta: i32) {
        // Preview always fits the pane in v1 (no image scrolling).
    }

    pub fn on_resize(&mut self) {
        // Next frame's sync_area triggers a re-encode if the area changed.
    }

    pub fn has_image(&self) -> bool {
        self.protocol.is_some()
    }

    pub fn mark_rendered(&mut self, _app: &App) {}
}
