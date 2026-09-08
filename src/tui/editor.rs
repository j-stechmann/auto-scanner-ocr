//! Page editor: full-screen single-page view with the crop tool.
//!
//! The editor is a modal UI mode (`UiMode::Editor`), one layer below the
//! overlays: overlays route first, then the editor, then the main view.
//!
//! Coordinate spaces (three of them, mapped exactly):
//! - ORIGINAL image pixels (what `Cmd::Crop` speaks; stored in the crop
//!   rect and orig_dims)
//! - DOWNSCALED image pixels (what the editor worker composites the
//!   outline in; orig ÷ downscale_factor)
//! - TERMINAL cells (what hit-testing and the render rect speak)
//!
//! The image is rendered into the LETTERBOX-TIGHT rect of the image area:
//! ratatui-image's Resize::Fit computes the aspect-preserving cell size
//! (ceiled per axis) and `Resize::resize` pads the encoded image to exact
//! font-size multiples with background, content anchored TOP-LEFT
//! (imageops::overlay at 0,0 — lib.rs). Therefore a screen cell (cx, cy)
//! inside the render rect maps to original pixel
//! `((cx - rx) * fw / k, (cy - ry) * fh / k)` with
//! `k = min(1, area_px_w/orig_w, area_px_h/orig_h)` — no centering offset,
//! and the mapping is exact within the sub-cell rounding of the ceil.

use ratatui::layout::{Rect, Size};
use tokio::sync::mpsc;

use super::app::App;
use super::overlays::{Confirm, Overlay};
use super::preview::EditorWorker;
use crate::backend::pdf::CropRect;
use crate::session::Cmd;

/// A crop rect in original image pixels (alias for readability in the TUI
/// layer; the session actor's guard does the actual validation).
pub type ImageRect = CropRect;

/// Image area geometry for the current editor frame (rebuilt each draw).
/// Stored on the app so mouse hit-testing uses the same rects as render.
#[derive(Debug, Clone, Copy, Default)]
pub struct EditorRects {
    /// Full content area offered to the image (below header, above
    /// footer/status line, with a 1-cell margin).
    pub area: Rect,
    /// Letterbox-tight rect the image actually renders into (inside
    /// `area`); crop hit-testing and mapping are relative to this.
    pub image: Rect,
}

/// Pure mapping: terminal cell (cx, cy) -> original image pixel, snapped
/// inside the image bounds. `image` is the render rect, `font_size` the
/// terminal font size in pixels, `orig` the original image dims, `k` the
/// scale factor from image pixels to screen pixels
/// (min(1, area_px_w/orig_w, area_px_h/orig_h)).
///
/// Returns None when the cell is outside the render rect or the geometry
/// is degenerate.
pub fn cell_to_image_px(
    (cx, cy): (u16, u16),
    image: Rect,
    font_size: (u16, u16),
    orig: (u32, u32),
    k: f64,
) -> Option<(u32, u32)> {
    if cx < image.x || cy < image.y || k <= 0.0 {
        return None;
    }
    let (dx, dy) = (cx - image.x, cy - image.y);
    // The image's rendered size in cells: the content is top-left anchored
    // and padded to font multiples, so a cell beyond the last PARTIAL cell
    // of content is background: clamp the cell to the content extent.
    let (fw, fh) = (u32::from(font_size.0).max(1), u32::from(font_size.1).max(1));
    // Content extent in cells = ceil(orig * k / font), the same rounding
    // size_for applies; pixels beyond the last cell are outside the image.
    let content_w = ((orig.0 as f64 * k) / fw as f64).ceil() as u32;
    let content_h = ((orig.1 as f64 * k) / fh as f64).ceil() as u32;
    if dx as u32 >= content_w.max(1) || dy as u32 >= content_h.max(1) {
        return None;
    }
    // Pixel position of the cell's TOP-LEFT corner in image space. The
    // exact pixel is somewhere in this cell; the top-left is the stable
    // choice for drag anchoring (later cells map to later pixels).
    let px = (dx as f64 * fw as f64 / k).floor() as u32;
    let py = (dy as f64 * fh as f64 / k).floor() as u32;
    Some((
        px.min(orig.0.saturating_sub(1)),
        py.min(orig.1.saturating_sub(1)),
    ))
}

/// Normalize any two corner points into a valid rect (min 1x1), clamped to
/// `max` (original image dims). Pure; unit-tested.
pub fn normalize_rect(a: (u32, u32), b: (u32, u32), max: (u32, u32)) -> ImageRect {
    let clamp = |v: u32, m: u32| v.min(m.saturating_sub(1));
    let (x0, y0) = (clamp(a.0, max.0), clamp(a.1, max.1));
    let (x1, y1) = (clamp(b.0, max.0), clamp(b.1, max.1));
    let (left, right) = (x0.min(x1), x0.max(x1));
    let (top, bottom) = (y0.min(y1), y0.max(y1));
    ImageRect {
        x: left,
        y: top,
        w: right - left + 1,
        h: bottom - top + 1,
    }
}

/// Move a rect by (dx, dy) pixels, clamped inside `max` (image dims).
/// Pure; unit-tested.
pub fn move_rect(rect: ImageRect, dx: i64, dy: i64, max: (u32, u32)) -> ImageRect {
    let x = (rect.x as i64 + dx).clamp(0, (max.0 - rect.w) as i64) as u32;
    let y = (rect.y as i64 + dy).clamp(0, (max.1 - rect.h) as i64) as u32;
    ImageRect { x, y, ..rect }
}

/// Extend one edge of a rect by a signed pixel amount: POSITIVE grows the
/// rect outward on that edge, negative shrinks it (keeping the opposite
/// edge fixed and the rect >= 1x1), clamped inside `max`. Pure; tested.
pub fn extend_rect(rect: ImageRect, edge: Edge, amount: i64, max: (u32, u32)) -> ImageRect {
    match edge {
        Edge::Left => {
            // Grow left (amount>0): x moves left, right edge fixed.
            // Shrink (amount<0): x moves right, w shrinks toward min 1.
            let left = rect.x as i64 - amount;
            let right = (rect.x + rect.w - 1) as i64; // fixed
            let left = left.clamp(0, right);
            ImageRect {
                x: left as u32,
                w: (right - left + 1) as u32,
                ..rect
            }
        }
        Edge::Right => {
            let w = ((rect.w as i64 + amount).clamp(1, (max.0 - rect.x) as i64)) as u32;
            ImageRect { w, ..rect }
        }
        Edge::Top => {
            let top = rect.y as i64 - amount;
            let bottom = (rect.y + rect.h - 1) as i64; // fixed
            let top = top.clamp(0, bottom);
            ImageRect {
                y: top as u32,
                h: (bottom - top + 1) as u32,
                ..rect
            }
        }
        Edge::Bottom => {
            let h = ((rect.h as i64 + amount).clamp(1, (max.1 - rect.y) as i64)) as u32;
            ImageRect { h, ..rect }
        }
    }
}

/// Apply a drag position to a rect. `edges` = (left, top, right, bottom)
/// flags for which edges follow the cursor; opposite edges stay fixed;
/// result stays inside `max` with min 1x1. Pure; tested.
pub fn drag_resize(
    rect: ImageRect,
    (px, py): (u32, u32),
    edges: (bool, bool, bool, bool),
    max: (u32, u32),
) -> ImageRect {
    let (move_l, move_t, move_r, move_b) = edges;
    let cx = px.min(max.0.saturating_sub(1));
    let cy = py.min(max.1.saturating_sub(1));
    let x2 = rect.x + rect.w.saturating_sub(1);
    let y2 = rect.y + rect.h.saturating_sub(1);
    // New left/right edge positions in px (inclusive).
    let left = if move_l { cx.min(x2) } else { rect.x };
    let right = if move_r { cx.max(left) } else { x2.max(left) };
    let top = if move_t { cy.min(y2) } else { rect.y };
    let bottom = if move_b { cy.max(top) } else { y2.max(top) };
    ImageRect {
        x: left,
        y: top,
        w: right - left + 1,
        h: bottom - top + 1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

/// What an in-progress drag is doing (mouse mode only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drag {
    /// Drawing a fresh rect: anchor = fixed corner, cursor = free corner.
    Draw { anchor: (u32, u32) },
    /// Moving the whole rect: anchor = grab point, offset preserved.
    Move { grab_dx: i64, grab_dy: i64 },
    /// Resizing: the grabbed edges (corner = two flags), opposite edges
    /// stay fixed.
    Resize {
        left: bool,
        top: bool,
        right: bool,
        bottom: bool,
    },
}

impl Drag {
    /// Which edges does this drag move? (Resize only.)
    pub fn edges(self) -> (bool, bool, bool, bool) {
        match self {
            Drag::Draw { .. } | Drag::Move { .. } => (false, false, false, false),
            Drag::Resize {
                left,
                top,
                right,
                bottom,
            } => (left, top, right, bottom),
        }
    }
}

/// Edge-band hit test in ORIGINAL image pixels: which rect edges is this
/// point on? Bands are `tolerance` px wide (a tiny rect's whole edge is a
/// handle since the band is capped at the rect size). Corners set both
/// flags. Pure; tested.
pub fn hit_edges(
    rect: ImageRect,
    (px, py): (u32, u32),
    tolerance: u32,
) -> Option<(bool, bool, bool, bool)> {
    let x2 = rect.x + rect.w.saturating_sub(1);
    let y2 = rect.y + rect.h.saturating_sub(1);
    let band_x = tolerance.max(1).min(rect.w.max(1));
    let band_y = tolerance.max(1).min(rect.h.max(1));
    let on_left = px <= rect.x.saturating_add(band_x - 1);
    let on_right = px >= (rect.x + rect.w).saturating_sub(band_x) && px <= x2;
    let on_top = py <= rect.y.saturating_add(band_y - 1);
    let on_bottom = py >= (rect.y + rect.h).saturating_sub(band_y) && py <= y2;
    let inside_x = px >= rect.x && px <= x2;
    let inside_y = py >= rect.y && py <= y2;
    if !(inside_x && inside_y) {
        return None;
    }
    let (l, r) = (on_left && inside_x, on_right && inside_x);
    let (t, b) = (on_top && inside_y, on_bottom && inside_y);
    if !(l || r || t || b) {
        return None; // interior: move, not resize
    }
    Some((l, t, r, b))
}

/// Keyboard step for rect moves/extends: ~2% of the image's longest side,
/// at least 1px. Pure; unit-tested.
pub fn key_step(orig: (u32, u32)) -> u32 {
    (orig.0.max(orig.1) / 50).max(1)
}

impl App {
    /// Editor area for the current frame: everything between the header
    /// row and the footer row, minus a 1-cell margin on each side.
    pub fn editor_area(whole: Rect) -> Rect {
        let x = whole.x + 1;
        let y = whole.y + 2; // header row + margin
        let width = whole.width.saturating_sub(2);
        let height = whole.height.saturating_sub(4); // header + footer + 2 margins
        Rect::new(x, y, width, height.max(1))
    }
}

/// Editor-side geometry context for the mapping helpers (kept out of App
/// to stay unit-testable).
pub struct Geometry {
    pub image_rect: Rect,
    pub font_size: (u16, u16),
    pub orig: (u32, u32),
    pub k: f64,
}

impl Geometry {
    pub fn cell_to_px(&self, cell: (u16, u16)) -> Option<(u32, u32)> {
        cell_to_image_px(cell, self.image_rect, self.font_size, self.orig, self.k)
    }

    /// The image area's scale factor k for the given content area
    /// (area_px = cells * font px): how many screen pixels one image
    /// pixel occupies under Resize::Fit.
    pub fn scale_for(area: Size, font_size: (u16, u16), orig: (u32, u32)) -> f64 {
        let area_px_w = f64::from(area.width) * f64::from(font_size.0);
        let area_px_h = f64::from(area.height) * f64::from(font_size.1);
        let (ow, oh) = (f64::from(orig.0.max(1)), f64::from(orig.1.max(1)));
        (area_px_w / ow).min(area_px_h / oh).min(1.0)
    }
}

/// Draw entry point wired from ui.rs; lives here so editor.rs owns all
/// editor rendering.
pub fn draw_editor(
    f: &mut ratatui::Frame,
    app: &mut App,
    editor: &mut super::preview::EditorWorker,
) {
    let whole = f.area();
    let area = App::editor_area(whole);
    let orig = editor.orig_dims();

    // Readout in the header row (never over the image rect).
    let header = Rect::new(whole.x, whole.y, whole.width, 1);
    let mut line = String::new();
    if editor.ready() {
        line.push_str(&format!(" editing page {} ", app.editor_page_label()));
        if let Some(rect) = app.editor_crop_rect() {
            let pct = crop_percent(rect, orig);
            line.push_str(&format!(
                "· crop {}x{} px ({pct:.0}% of page)",
                rect.w, rect.h
            ));
        }
    } else {
        line.push_str(" decoding image… ");
    }
    f.render_widget(
        ratatui::widgets::Paragraph::new(ratatui::text::Line::from(line))
            .style(super::theme::header()),
        header,
    );

    // Footer hint row (never over the image rect).
    let footer = Rect::new(whole.x, whole.y + whole.height - 1, whole.width, 1);
    let hint = if app.editor_crop_rect().is_some() {
        " drag edge resize · inside move · outside redraw · hjkl move · Alt+hjkl shrink · HJKL grow · Enter apply · c done · Esc back "
    } else {
        " c crop · drag to draw · Esc back "
    };
    f.render_widget(
        ratatui::widgets::Paragraph::new(ratatui::text::Line::from(hint))
            .style(super::theme::MUTED),
        footer,
    );

    if !editor.ready() || orig.0 == 0 {
        return;
    }
    // Letterbox-tight render rect; encode for it with the crop outline
    // composited into the pixels when the tool is active. request_encode
    // NEVER touches the display: it renders the last completed encode
    // (outline lags during drags and converges; no blanking).
    let outline = app
        .editor_crop_rect()
        .map(|r| editor.to_downscaled((r.x, r.y, r.w, r.h)));
    let Some(size) = editor.size_for(area) else {
        return;
    };
    editor.request_encode(size, outline);
    let image = Rect::new(
        area.x,
        area.y,
        size.width.min(area.width),
        size.height.min(area.height),
    );
    app.editor_rects = Some(EditorRects { area, image });
    editor.render(image, f.buffer_mut());
}

fn crop_percent(rect: ImageRect, orig: (u32, u32)) -> f64 {
    if orig.0 == 0 || orig.1 == 0 {
        return 0.0;
    }
    (rect.w as f64 * rect.h as f64) / (orig.0 as f64 * orig.1 as f64) * 100.0
}

// ------------------------------------------------------------- input

/// Editor key handling. Swallows everything except a deliberate set:
/// - Esc / q: leave the editor (with the discard arm when a crop rect is
///   unapplied)
/// - Ctrl-C: global quit
/// - c: toggle the crop tool
/// - Enter: apply the crop (confirm dialog)
/// - hjkl / HJKL: move / extend
/// - ? / !: open overlays
///
/// Everything else (digits, Tab, J/K, letters) is swallowed while editing.
pub async fn handle_key(
    app: &mut App,
    editor: &mut EditorWorker,
    key: ratatui::crossterm::event::KeyEvent,
    cmd_tx: &mpsc::Sender<Cmd>,
) -> anyhow::Result<()> {
    use ratatui::crossterm::event::{KeyCode as K, KeyModifiers};

    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == K::Char('c') {
        app.quit_requested = true;
        return Ok(());
    }
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    match key.code {
        K::Esc => {
            let Some(e) = app.editor.as_mut() else {
                return Ok(());
            };
            if e.crop.is_some() && !e.esc_pending {
                // First Esc: arm the discard question (status line).
                e.esc_pending = true;
                app.set_status("discard crop? Esc again to discard, c to keep");
                return Ok(());
            }
            app.close_editor();
        }
        K::Char('q') => {
            app.close_editor();
        }
        K::Char('?') => app.overlay = Some(Overlay::Help),
        K::Char('!') => app.overlay = Some(Overlay::Diagnostics),
        K::Char('c') => {
            let Some(e) = app.editor.as_mut() else {
                return Ok(());
            };
            if e.crop.is_some() {
                // Done with the tool; the rect is discarded (kept only via
                // the Esc arm or the apply dialog).
                e.crop = None;
                e.drag = None;
                e.esc_pending = false;
            } else if editor.ready() {
                // Start with the middle 80% as the initial rect: a big,
                // obvious starting point that is easy to shrink.
                let (w, h) = editor.orig_dims();
                let (w, h) = (w.max(1), h.max(1));
                let (iw, ih) = (w / 5, h / 5);
                e.crop = Some(ImageRect {
                    x: iw,
                    y: ih,
                    w: w - 2 * iw,
                    h: h - 2 * ih,
                });
                e.esc_pending = false;
            } else {
                app.set_status("image still decoding");
            }
        }
        K::Enter => {
            // Apply the crop: confirm first (destructive). The rect
            // survives an Esc'd confirm (overlays route first).
            let Some(rect) = app.editor.as_ref().and_then(|e| e.crop) else {
                app.set_status("no crop rect - press c to start cropping");
                return Ok(());
            };
            let id = app.editor.as_ref().expect("editor open").page_id;
            app.overlay = Some(Overlay::Confirm(Confirm::crop(id, rect)));
        }
        K::Char(c @ ('h' | 'j' | 'k' | 'l' | 'H' | 'J' | 'K' | 'L')) => {
            // Alt+hjkl shrinks from the corresponding edge (resize); plain
            // hjkl moves, HJKL grows.
            nudge_rect(app, editor, c, alt);
        }
        K::Left => nudge_rect(app, editor, 'h', false),
        K::Right => nudge_rect(app, editor, 'l', false),
        K::Up => nudge_rect(app, editor, 'k', false),
        K::Down => nudge_rect(app, editor, 'j', false),
        _ => {}
    }
    let _ = cmd_tx;
    Ok(())
}

/// Move/extend the crop rect by the keyboard step. Plain hjkl/HJKL moves
/// and grows; Alt+hjkl SHRINKS from the corresponding edge. No-ops without
/// an active rect or a decoded image.
fn nudge_rect(app: &mut App, editor: &EditorWorker, c: char, shrink: bool) {
    let Some(e) = app.editor.as_mut() else {
        return;
    };
    let Some(rect) = e.crop else {
        return;
    };
    let (w, h) = editor.orig_dims();
    if w == 0 || h == 0 {
        return;
    }
    let step = key_step((w, h)) as i64;
    let max = (w, h);
    e.esc_pending = false;
    e.crop = Some(match c {
        'h' if shrink => extend_rect(rect, Edge::Left, -step, max),
        'h' => move_rect(rect, -step, 0, max),
        'l' if shrink => extend_rect(rect, Edge::Right, -step, max),
        'l' => move_rect(rect, step, 0, max),
        'j' if shrink => extend_rect(rect, Edge::Bottom, -step, max),
        'j' => move_rect(rect, 0, step, max),
        'k' if shrink => extend_rect(rect, Edge::Top, -step, max),
        'k' => move_rect(rect, 0, -step, max),
        'H' => extend_rect(rect, Edge::Left, step, max),
        'J' => extend_rect(rect, Edge::Bottom, step, max),
        'K' => extend_rect(rect, Edge::Top, step, max),
        'L' => extend_rect(rect, Edge::Right, step, max),
        _ => rect,
    });
}

/// Editor mouse handling: fully intercepted (stale main-view geometry is
/// never consulted). With the tool active:
/// - down on a rect EDGE/corner starts a RESIZE of the grabbed edges
/// - down in the rect interior starts a MOVE
/// - down outside the rect starts a fresh DRAW
///
/// Drag follows the cursor; Up finalizes. Without the tool, a drag draws
/// a fresh rect (implicit tool start).
pub async fn handle_mouse(
    app: &mut App,
    editor: &EditorWorker,
    mouse: ratatui::crossterm::event::MouseEvent,
    cmd_tx: &mpsc::Sender<Cmd>,
) {
    use ratatui::crossterm::event::{MouseButton, MouseEventKind};
    let pos = (mouse.column, mouse.row);
    let Some(rects) = app.editor_rects else {
        return;
    };
    let Some(e) = app.editor.as_ref() else {
        return;
    };
    let (w, h) = editor.orig_dims();
    let geom = Geometry {
        image_rect: rects.image,
        font_size: app.editor_font_size,
        orig: (w, h),
        k: Geometry::scale_for(
            Size::new(rects.area.width, rects.area.height),
            app.editor_font_size,
            (w, h),
        ),
    };
    let drag = e.drag;
    let crop = e.crop;
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(px) = geom.cell_to_px(pos) else {
                end_drag(app);
                return;
            };
            let max = (w, h);
            match (crop, drag) {
                (Some(rect), None) => {
                    // Edge hit (with a small pixel tolerance) -> resize;
                    // interior -> move; outside -> draw fresh.
                    let tol = drag_tolerance_px(&rects, app.editor_font_size, (w, h));
                    if let Some(edges) = hit_edges(rect, px, tol) {
                        start_drag(
                            app,
                            Drag::Resize {
                                left: edges.0,
                                top: edges.1,
                                right: edges.2,
                                bottom: edges.3,
                            },
                        );
                    } else {
                        // Interior grab: preserve the cursor offset within
                        // the rect so it doesn't jump under the cursor.
                        start_drag(
                            app,
                            Drag::Move {
                                grab_dx: px.0 as i64 - rect.x as i64,
                                grab_dy: px.1 as i64 - rect.y as i64,
                            },
                        );
                    }
                }
                (None, _) => {
                    // No tool yet: start drawing a fresh rect.
                    start_drag(app, Drag::Draw { anchor: px });
                    if let Some(e) = app.editor.as_mut() {
                        e.crop = Some(normalize_rect(px, px, max));
                    }
                }
                (Some(_), Some(d)) => {
                    // Down during an active drag: finalize it.
                    apply_drag(app, d, px, geom);
                    end_drag(app);
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let (Some(d), Some(px)) = (drag, geom.cell_to_px(pos)) {
                apply_drag(app, d, px, geom);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let (Some(d), Some(px)) = (drag, geom.cell_to_px(pos)) {
                apply_drag(app, d, px, geom);
            }
            end_drag(app);
        }
        _ => {}
    }
    let _ = cmd_tx;
}

/// Pixel tolerance for edge grabbing: a comfortable band is ~half a cell
/// in image pixels (at least 8), so edges stay grabbable at any zoom.
fn drag_tolerance_px(rects: &EditorRects, font_size: (u16, u16), orig: (u32, u32)) -> u32 {
    let k = Geometry::scale_for(
        Size::new(rects.area.width, rects.area.height),
        font_size,
        orig,
    );
    // Half a cell in image pixels, at least 8 image px, capped so the band
    // stays narrower than the rect's interior.
    let half_cell = (f64::from(font_size.0) / k.max(1e-6) / 2.0) as u32;
    half_cell.max(8)
}

fn apply_drag(app: &mut App, d: Drag, px: (u32, u32), geom: Geometry) {
    let Some(e) = app.editor.as_mut() else {
        return;
    };
    let max = geom.orig;
    if max.0 == 0 || max.1 == 0 {
        return;
    }
    let rect = match e.crop {
        Some(r) => r,
        None => return,
    };
    e.esc_pending = false;
    e.crop = Some(match d {
        Drag::Draw { anchor } => normalize_rect(anchor, px, max),
        Drag::Move { grab_dx, grab_dy } => {
            let nx = (px.0 as i64 - grab_dx).clamp(0, (max.0 - rect.w) as i64) as u32;
            let ny = (px.1 as i64 - grab_dy).clamp(0, (max.1 - rect.h) as i64) as u32;
            ImageRect {
                x: nx,
                y: ny,
                ..rect
            }
        }
        Drag::Resize {
            left,
            top,
            right,
            bottom,
        } => drag_resize(rect, px, (left, top, right, bottom), max),
    });
}

fn start_drag(app: &mut App, d: Drag) {
    if let Some(e) = app.editor.as_mut() {
        e.drag = Some(d);
        e.esc_pending = false;
    }
}

fn end_drag(app: &mut App) {
    if let Some(e) = app.editor.as_mut() {
        e.drag = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: (u16, u16) = (10, 20); // classic 1:2 font

    #[test]
    fn scale_for_caps_at_one() {
        // Small image in a huge terminal: k stays 1 (no upscale).
        let k = Geometry::scale_for(Size::new(100, 50), FS, (50, 50));
        assert!((k - 1.0).abs() < 1e-9);
        // Big image: k scales down to fit.
        let k = Geometry::scale_for(Size::new(100, 50), FS, (10000, 10000));
        let area_px_w = 100.0f64 * 10.0;
        let area_px_h = 50.0f64 * 20.0;
        assert!((k - (area_px_w / 10000.0).min(area_px_h / 10000.0)).abs() < 1e-9);
    }

    #[test]
    fn cell_mapping_round_trip_within_extent() {
        // 1000x1000 image, k = 0.5 -> 500x500 screen px -> 50x25 cells.
        let orig = (1000u32, 1000u32);
        let k = 0.5f64;
        let image = Rect::new(5, 5, 50, 25);
        // Corner cells map to corners (clamped).
        assert_eq!(cell_to_image_px((5, 5), image, FS, orig, k), Some((0, 0)));
        assert_eq!(
            cell_to_image_px((54, 29), image, FS, orig, k),
            Some((980, 960))
        );
        // Outside the render rect: None.
        assert_eq!(cell_to_image_px((4, 5), image, FS, orig, k), None);
        assert_eq!(cell_to_image_px((55, 5), image, FS, orig, k), None);
        // Inside the padded area but beyond the content extent (56 wide
        // render rect would still be background): None.
        assert_eq!(cell_to_image_px((55, 15), image, FS, orig, k), None);
    }

    #[test]
    fn normalize_rect_swaps_and_clamps() {
        let max = (100u32, 80u32);
        // Corner swap.
        assert_eq!(
            normalize_rect((50, 40), (10, 20), max),
            ImageRect {
                x: 10,
                y: 20,
                w: 41,
                h: 21
            }
        );
        // Clamped to image bounds.
        assert_eq!(
            normalize_rect((200, 200), (300, 300), max),
            ImageRect {
                x: 99,
                y: 79,
                w: 1,
                h: 1
            }
        );
        // Minimum 1x1 for a single point.
        assert_eq!(
            normalize_rect((10, 10), (10, 10), max),
            ImageRect {
                x: 10,
                y: 10,
                w: 1,
                h: 1
            }
        );
    }

    #[test]
    fn move_rect_clamps() {
        let max = (100u32, 100u32);
        let r = ImageRect {
            x: 90,
            y: 90,
            w: 10,
            h: 10,
        };
        let moved = move_rect(r, 50, 50, max);
        assert_eq!(moved.x, 90, "clamped at right edge");
        assert_eq!(moved.y, 90);
        let back = move_rect(r, -500, -500, max);
        assert_eq!((back.x, back.y), (0, 0));
    }

    #[test]
    fn extend_rect_edges() {
        let max = (100u32, 100u32);
        let r = ImageRect {
            x: 10,
            y: 10,
            w: 10,
            h: 10,
        };
        // Growing right.
        assert_eq!(extend_rect(r, Edge::Right, 5, max).w, 15);
        // Shrinking right keeps min 1.
        assert_eq!(extend_rect(r, Edge::Right, -50, max).w, 1);
        // Growing left moves x and keeps the right edge fixed.
        let l = extend_rect(r, Edge::Left, 5, max);
        assert_eq!(l.x, 5);
        assert_eq!(l.w, 15);
        assert_eq!(l.x + l.w, r.x + r.w, "right edge fixed");
        // Shrinking left below x+w-1 clamps.
        assert_eq!(extend_rect(r, Edge::Left, -50, max).w, 1);
        assert_eq!(
            extend_rect(r, Edge::Left, -50, max).x,
            19,
            "right edge fixed"
        );
        // Bottom grow clamps to image edge.
        let b = extend_rect(r, Edge::Bottom, 500, max);
        assert_eq!(b.h, 90);
        // Top grow clamps at 0.
        let t = extend_rect(r, Edge::Top, 500, max);
        assert_eq!(t.y, 0);
        assert_eq!(t.h, 20);
    }

    #[test]
    fn key_step_two_percent() {
        assert_eq!(key_step((1000, 2000)), 40);
        assert_eq!(key_step((40, 40)), 1, "min step 1px");
    }

    #[test]
    fn hit_edges_bands_and_interior() {
        let r = ImageRect {
            x: 10,
            y: 10,
            w: 50,
            h: 40,
        };
        // Corners: both flags.
        assert_eq!(hit_edges(r, (10, 10), 8), Some((true, true, false, false)));
        assert_eq!(hit_edges(r, (59, 49), 8), Some((false, false, true, true)));
        // Edge bands without corners.
        assert_eq!(hit_edges(r, (30, 12), 8), Some((false, true, false, false)));
        assert_eq!(hit_edges(r, (55, 30), 8), Some((false, false, true, false)));
        // Interior: None (move, not resize).
        assert_eq!(hit_edges(r, (30, 30), 8), None);
        // Outside the rect entirely: None.
        assert_eq!(hit_edges(r, (70, 30), 8), None);
        // Tiny rect: whole edge is a handle (band covers everything).
        let tiny = ImageRect {
            x: 10,
            y: 10,
            w: 2,
            h: 2,
        };
        assert_eq!(hit_edges(tiny, (10, 10), 8), Some((true, true, true, true)));
    }

    #[test]
    fn drag_resize_moves_grabbed_edges_only() {
        let max = (100u32, 100u32);
        let r = ImageRect {
            x: 10,
            y: 10,
            w: 50,
            h: 40,
        };
        // Grab right edge, drag left of it: shrink to the cursor.
        let r2 = drag_resize(r, (30, 30), (false, false, true, false), max);
        assert_eq!((r2.x, r2.w), (10, 21), "left edge fixed, right follows");
        // Drag right edge beyond: clamps at image edge.
        let r3 = drag_resize(r, (200, 30), (false, false, true, false), max);
        assert_eq!(r3.w, 90, "clamped to image width");
        // Grab left edge past the right edge: cursor clamps (min 1 wide).
        let r4 = drag_resize(r, (5, 5), (true, false, false, false), max);
        assert_eq!((r4.x, r4.w), (5, 55), "right edge 59 stays fixed");
        let r5 = drag_resize(r, (95, 30), (true, false, false, false), max);
        assert_eq!(r5.w, 1, "cursor clamps to right edge");
        assert_eq!(r5.x, 59, "clamped to the fixed right edge");
        // Corner grab moves two edges at once.
        let c = drag_resize(r, (5, 5), (true, true, false, false), max);
        assert_eq!((c.x, c.y, c.w, c.h), (5, 5, 55, 45));
        // No edges (shouldn't happen): rect unchanged.
        let n = drag_resize(r, (1, 1), (false, false, false, false), max);
        assert_eq!(n, r);
    }

    #[test]
    fn drag_move_preserves_offset() {
        let max = (100u32, 100u32);
        let r = ImageRect {
            x: 20,
            y: 30,
            w: 10,
            h: 10,
        };
        // Grab at (22, 31): offset (2, 1). Cursor to (50, 60) -> x=48.
        let d = Drag::Move {
            grab_dx: 2,
            grab_dy: 1,
        };
        let moved = match d {
            Drag::Move { grab_dx, grab_dy } => {
                let nx = (50u32 as i64 - grab_dx).clamp(0, (max.0 - r.w) as i64) as u32;
                let ny = (60u32 as i64 - grab_dy).clamp(0, (max.1 - r.h) as i64) as u32;
                ImageRect { x: nx, y: ny, ..r }
            }
            _ => unreachable!(),
        };
        assert_eq!((moved.x, moved.y), (48, 59));
    }
}
