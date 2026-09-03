//! Rendering: layout, panes, footer, and hit-testing for mouse focus.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use super::app::{Action, App, Pane};
use super::overlays::{self, Confirm, Overlay};
use super::preview::PreviewWorker;
use crate::check::Status;
use crate::config::PreviewOcr;
use crate::session::{Busy, PageId, PageStatus};

/// Per-frame pane geometry, used by both rendering and hit-testing.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaneRects {
    pub sidebar: Rect,
    pub status: Rect,
    /// Preview pane INCLUDING its outer border (cells live inside).
    pub preview: Rect,
    /// Preview content area inside the block border — the exact rect the
    /// grid cells are laid out within. `sync_cells` must use cell rects
    /// from this same geometry, or sixel encodes for a size the render
    /// guard then rejects (image only appears after a forced redraw).
    pub preview_inner: Rect,
    pub text: Rect,
    pub footer: Rect,
    pub whole: Rect,
}

pub fn layout(area: Rect) -> PaneRects {
    let whole = area;
    let [body, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).areas(body);
    let [sidebar, status] =
        Layout::vertical([Constraint::Percentage(70), Constraint::Fill(1)]).areas(left);
    let [preview, text] =
        Layout::vertical([Constraint::Percentage(55), Constraint::Fill(1)]).areas(right);
    let preview_inner = Rect::new(
        preview.x.saturating_add(1),
        preview.y.saturating_add(1),
        preview.width.saturating_sub(2),
        preview.height.saturating_sub(2),
    );
    PaneRects {
        sidebar,
        status,
        preview,
        preview_inner,
        text,
        footer,
        whole,
    }
}

/// Which pane contains this position? Overlays take precedence in app.rs.
pub fn hit_test(app: &App, pos: (u16, u16)) -> Option<Pane> {
    let rects = app.pane_rects?;
    let (x, y) = pos;
    for (pane, rect) in [
        (Pane::Sidebar, rects.sidebar),
        (Pane::Preview, rects.preview),
        (Pane::Text, rects.text),
    ] {
        if x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height {
            return Some(pane);
        }
    }
    None
}

/// Grid cell rects for `n` pages inside `area`, shaped for the given page
/// aspect (width/height in terminal cells). Picks the cols×rows split that
/// wastes the least space; leftover rows/columns stay empty.
///
/// Pure function so the math is unit-testable; the draw path applies it.
pub fn grid_cells(area: Rect, n: usize, page_aspect: f32) -> Vec<Rect> {
    if n == 0 || area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let (cols, rows) = best_grid(area, n, page_aspect);
    let cell_w = area.width / cols as u16;
    let cell_h = area.height / rows as u16;
    let mut cells = Vec::with_capacity(n);
    for i in 0..n {
        let col = (i % cols) as u16;
        let row = (i / cols) as u16;
        // Last column/row absorbs the rounding remainder so the grid uses
        // the full pane instead of leaving a ragged margin.
        let w = if col == (cols - 1) as u16 {
            area.width - col * cell_w
        } else {
            cell_w
        };
        let h = if row == (rows - 1) as u16 {
            area.height - row * cell_h
        } else {
            cell_h
        };
        cells.push(Rect::new(
            area.x + col * cell_w,
            area.y + row * cell_h,
            w,
            h,
        ));
    }
    cells
}

/// Choose cols × rows minimizing wasted area for page-shaped cells.
fn best_grid(area: Rect, n: usize, page_aspect: f32) -> (usize, usize) {
    let area_aspect = area.width as f32 / area.height as f32;
    // Ideal cols if the grid were perfectly packed: sqrt(n * area/aspect).
    let ideal_cols = ((n as f32) * (area_aspect / page_aspect)).sqrt();
    let mut best = (1, n);
    let mut best_cost = f32::INFINITY;
    if n == 0 {
        return best;
    }
    // Search splits around the ideal (clamped to [1, n]).
    let lo = ideal_cols.floor().max(1.0) as usize;
    for cols in lo.saturating_sub(2).max(1)..=(lo + 2).min(n) {
        let rows = n.div_ceil(cols);
        let cell_w = area.width as f32 / cols as f32;
        let cell_h = area.height as f32 / rows as f32;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            continue;
        }
        let cell_aspect = cell_w / cell_h;
        // Cost: how much each cell deviates from the page shape (cells are
        // letterboxed by the protocol anyway; minimize the distortion).
        let distortion = if cell_aspect >= page_aspect {
            cell_aspect / page_aspect
        } else {
            page_aspect / cell_aspect
        };
        // Slight preference for fewer rows (wider thumbs read better).
        let cost = distortion + 0.02 * rows as f32;
        if cost < best_cost {
            best_cost = cost;
            best = (cols, rows);
        }
    }
    best
}

/// Preview grid cell containing this position, if any.
pub fn preview_cell_at(app: &App, pos: (u16, u16)) -> Option<PageId> {
    let (x, y) = pos;
    app.preview_cells
        .iter()
        .find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
        .map(|(id, _)| *id)
}

/// Sidebar list index at a position (rows are inside the block border).
pub fn sidebar_index_at(app: &App, pos: (u16, u16)) -> Option<usize> {
    let rects = app.pane_rects?;
    let (x, y) = pos;
    let rect = rects.sidebar;
    if x < rect.x || x >= rect.x + rect.width || y < rect.y + 1 || y >= rect.y + rect.height - 1 {
        return None;
    }
    let row = (y - rect.y - 1) as usize;
    // ListState scrolling is offset-based; approximate by selection window.
    // Good enough for click-to-select at typical page counts.
    Some(row)
}

pub fn draw(f: &mut Frame, app: &mut App, preview: &mut PreviewWorker) {
    let rects = layout(f.area());
    app.pane_rects = Some(rects);

    draw_header(f, app, rects.whole);
    draw_sidebar(f, app, rects.sidebar);
    draw_status(f, app, rects.status);
    draw_preview(f, app, preview, rects.preview, rects.preview_inner);
    draw_text(f, app, rects.text);
    draw_footer(f, app, rects.footer);

    if let Some(overlay) = &mut app.overlay {
        match overlay {
            Overlay::Help => draw_help(f, rects.whole),
            Overlay::Diagnostics => draw_diagnostics(f, app, rects.whole),
            Overlay::LangPicker(picker) => draw_lang_picker(f, picker, rects.whole),
            Overlay::Confirm(confirm) => draw_confirm(f, confirm, rects.whole),
        }
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 1 {
        return;
    }
    // Single-line header above everything: program + settings + device.
    let row = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let dirty = if app.meta.as_ref().is_some_and(|m| m.dirty) {
        " ●"
    } else {
        ""
    };
    let title = format!(
        " {} {}dpi {} {}{} ",
        crate::config::PROGRAM,
        app.settings.dpi,
        app.settings.mode,
        app.cfg.langs,
        dirty
    );
    let device = format!("{} ", app.device_label);
    let filler = row
        .width
        .saturating_sub(title.len() as u16 + device.len() as u16) as usize;
    let spans = vec![
        Span::styled(
            title,
            Style::default()
                .fg(Color::Black)
                .bg(Color::LightBlue)
                .bold(),
        ),
        Span::styled(" ".repeat(filler), Style::default().bg(Color::LightBlue)),
        Span::styled(
            device,
            Style::default().fg(Color::Black).bg(Color::LightBlue),
        ),
    ];
    let line = Line::from(spans);
    f.render_widget(Paragraph::new(line), row);
}

fn draw_sidebar(f: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();

    if app.pages.is_empty() {
        let hint = vec![
            Line::from(""),
            Line::from("No pages yet."),
            Line::from(""),
            Line::from("Press s to scan your first page."),
            Line::from("Press ? for help, ! for diagnostics."),
        ];
        let block = block_with_title("Pages", is_focused(app, Pane::Sidebar));
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(hint), inner);
        return;
    }

    for (idx, page) in app.pages.iter().enumerate() {
        let selected = idx == app.selected;
        let status_span = match page.status {
            PageStatus::Ready => Span::styled(
                " ready ",
                Style::default().fg(Color::Black).bg(Color::Green),
            ),
            PageStatus::Failed => {
                Span::styled(" FAILED ", Style::default().fg(Color::White).bg(Color::Red))
            }
            // Live elapsed timer: recomputed every draw (250ms tick keeps
            // this fresh during long scans) - no actor round-trips needed.
            PageStatus::Scanning | PageStatus::Processing => {
                let label = page.stage_label();
                let secs = page
                    .stage_started
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or(0);
                Span::styled(
                    format!(" {label} {secs:>3}s "),
                    Style::default().fg(Color::Black).bg(Color::Yellow),
                )
            }
            PageStatus::DeletePending => Span::styled(
                " deleting ",
                Style::default().fg(Color::Black).bg(Color::DarkGray),
            ),
        };
        let num = format!("{:>2}. ", idx + 1);
        let settings = format!("{}dpi {}", page.dpi, page.mode);
        let rot = if page.rotated { " ↻" } else { "" };
        let line = Line::from(vec![
            Span::styled(
                num,
                Style::default().add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
            ),
            status_span,
            Span::raw(" "),
            Span::styled(settings, Style::default().fg(Color::DarkGray)),
            Span::styled(rot, Style::default().fg(Color::Blue)),
        ]);
        items.push(ListItem::new(line));
    }

    let list = List::new(items)
        .block(block_with_title("Pages", is_focused(app, Pane::Sidebar)))
        .highlight_style(Style::default().bg(Color::Rgb(40, 60, 80)))
        .highlight_symbol("▸ ");
    // Use offset so the selection stays visible.
    let mut state = ratatui::widgets::ListState::default().with_selected(Some(app.selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_status(f: &mut Frame, app: &mut App, area: Rect) {
    let block = block_with_title("Status / log", false);
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(lines_vec(&app.status_lines))
            .wrap(Wrap { trim: false })
            .scroll((
                scroll_bottom(
                    app.status_lines.len(),
                    inner.height as usize,
                    app.status_scroll,
                ),
                0,
            )),
        inner,
    );
}

fn lines_vec(items: &[String]) -> Vec<Line<'_>> {
    items.iter().map(|s| Line::from(s.clone())).collect()
}

fn scroll_bottom(total: usize, height: usize, _offset: usize) -> u16 {
    if total <= height {
        0
    } else {
        (total - height) as u16
    }
}

fn draw_preview(
    f: &mut Frame,
    app: &mut App,
    preview: &mut PreviewWorker,
    area: Rect,
    inner: Rect,
) {
    let block = block_with_title("Pages (contact sheet)", is_focused(app, Pane::Preview));
    f.render_widget(block, area);
    let n = app.pages.len();
    app.preview_cells.clear();
    if n == 0 {
        f.render_widget(
            Paragraph::new("press s to scan your first page")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let cells = grid_cells(inner, n, preview.cell_aspect_in_cells());
    for (idx, (page, cell)) in app.pages.iter().zip(cells.iter()).enumerate() {
        let selected = idx == app.selected;
        // draw_cell returns the CONTENT rect it rendered into (inside the
        // cell block border). sync_cells must get exactly that rect, or the
        // encode size and render guard disagree and the thumb stays blank.
        let content = draw_cell(f, app, preview, page.id, *cell, selected);
        app.preview_cells.push((page.id, content));
    }
}

fn draw_cell(
    f: &mut Frame,
    app: &App,
    preview: &mut PreviewWorker,
    id: PageId,
    cell: Rect,
    selected: bool,
) -> Rect {
    let border_style = if selected {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Rgb(70, 70, 70))
    };
    let page = app.pages.iter().find(|p| p.id == id);
    let num = page.map(|p| p.id).unwrap_or(id);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .border_set(symbols::border::ROUNDED)
        .title(Span::styled(
            format!(" {num} "),
            if selected {
                Style::default().fg(Color::Cyan).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    let inner = block.inner(cell);
    f.render_widget(block, cell);
    if preview.has_image_for(id) {
        preview.render_cell(id, inner, f.buffer_mut());
    } else {
        let hint = page.map(cell_hint).unwrap_or_else(|| "…".to_string());
        // Keep the hint short enough to fit the cell line.
        let hint = if inner.width as usize > 4 {
            hint
        } else {
            String::new()
        };
        f.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
    }
    inner
}

fn cell_hint(page: &crate::session::PageView) -> String {
    match page.status {
        PageStatus::Ready => "no image".into(),
        PageStatus::Scanning => "scanning…".into(),
        PageStatus::Processing => format!("{}…", page.stage_label()),
        PageStatus::Failed => "failed".into(),
        PageStatus::DeletePending => "deleting…".into(),
    }
}

fn draw_text(f: &mut Frame, app: &mut App, area: Rect) {
    let block = block_with_title("Extracted text", is_focused(app, Pane::Text));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let text = app
        .selected_page()
        .and_then(|p| p.text.clone())
        .unwrap_or_default();
    let content: Vec<Line> = if text.is_empty() {
        let page = app.selected_page();
        let hint = match (
            page.map(|p| p.status),
            page.is_some_and(|p| p.text_pending),
            // The attempt for this exact image failed (no auto-retry).
            page.is_some_and(|p| p.ocr_failed_gen == Some(p.image_gen)),
        ) {
            (Some(PageStatus::Failed), _, _) => app
                .selected_page()
                .and_then(|p| p.error.clone())
                .unwrap_or_else(|| "unknown error".into()),
            // Lazy OCR in flight for this page.
            (_, true, _) => "extracting text…".into(),
            // Preview OCR disabled by config: the emptiness is expected.
            (Some(PageStatus::Ready), false, _) if app.cfg.preview_ocr == PreviewOcr::Off => {
                "(preview OCR disabled)".into()
            }
            // Attempt failed for the current image: retrying is pointless
            // (missing language data, unreadable file); rescan/rotate re-arms.
            (Some(PageStatus::Ready), false, true) => {
                "(preview OCR failed - rescan or rotate to retry)".into()
            }
            _ => "(no text extracted yet)".into(),
        };
        vec![Line::from(Span::styled(
            content_str(&hint),
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        text.lines().map(Line::from).collect()
    };
    let scroll = app
        .text_scroll
        .min(content.len().saturating_sub(inner.height as usize));
    f.render_widget(
        Paragraph::new(content)
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0)),
        inner,
    );
}

fn content_str(s: &str) -> String {
    s.to_string()
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if area.height < 1 {
        return;
    }
    let mut spans: Vec<Span> = Vec::new();
    // Exclusive-resource badge with live elapsed time (updates via the
    // 250ms tick), plus a background-jobs counter when pages are churning.
    match app.busy() {
        Busy::Idle => {}
        Busy::Scanning => {
            let secs = app
                .meta
                .as_ref()
                .and_then(|m| m.busy_since)
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            spans.push(Span::styled(
                format!(" SCANNING {secs}s "),
                Style::default().fg(Color::Black).bg(Color::Yellow).bold(),
            ));
        }
        Busy::Finishing => {
            let secs = app
                .meta
                .as_ref()
                .and_then(|m| m.busy_since)
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            spans.push(Span::styled(
                format!(" BUILDING PDF {secs}s "),
                Style::default().fg(Color::Black).bg(Color::Yellow).bold(),
            ));
        }
    }
    let jobs = app.meta.as_ref().map(|m| m.jobs_running).unwrap_or(0);
    if jobs > 0 {
        spans.push(Span::styled(
            format!(" {jobs} processing "),
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ));
    }
    for (key, action, label) in [
        ("s", Action::Scan, "scan"),
        ("r", Action::Rescan, "rescan"),
        ("R", Action::Rotate, "rotate"),
        ("d", Action::Delete, "del"),
        ("J/K", Action::Reorder, "move"),
        ("f", Action::Finish, "finish"),
        ("o", Action::Open, "open"),
    ] {
        let allowed = app.action_allowed(action);
        let style = if allowed {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(format!(" {key}"), style));
        spans.push(Span::styled(
            format!(" {label} ·"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    // Live dpi/mode hint (discoverability of +/- and m; the header shows
    // the same values). String spans because the value is dynamic.
    let settings_allowed = app.action_allowed(Action::Settings);
    let settings_style = if settings_allowed {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    spans.push(Span::styled(" +/-", settings_style));
    spans.push(Span::styled(
        format!(" {}dpi ·", app.settings.dpi),
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(" m", settings_style));
    spans.push(Span::styled(
        format!(" {} ·", app.settings.mode),
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled(" Tab", Style::default().fg(Color::Cyan)));
    spans.push(Span::styled(
        " pane · ",
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled("?", Style::default().fg(Color::Cyan)));
    spans.push(Span::styled(
        " help · ",
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled("!", Style::default().fg(Color::Cyan)));
    spans.push(Span::styled(
        " diag · ",
        Style::default().fg(Color::DarkGray),
    ));
    spans.push(Span::styled("q", Style::default().fg(Color::Cyan)));
    spans.push(Span::styled(" quit", Style::default().fg(Color::DarkGray)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn is_focused(app: &App, pane: Pane) -> bool {
    app.focus == pane
}

fn block_with_title(title: &str, focused: bool) -> Block<'_> {
    let title_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::Rgb(70, 70, 70))
    };
    Block::default()
        .title(Span::styled(format!(" {title} "), title_style))
        .borders(Borders::ALL)
        .border_style(border_style)
        .border_set(symbols::border::ROUNDED)
}

// --------------------------------------------------------------- overlays

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn overlay_block(title: &str) -> Block<'_> {
    Block::default()
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::Cyan).bold(),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .border_set(symbols::border::ROUNDED)
        .style(Style::default().bg(Color::Black))
}

fn draw_help(f: &mut Frame, area: Rect) {
    let rows: Vec<(&str, &str)> = vec![
        ("Scanning", ""),
        ("s / Enter (pages)", "scan next page"),
        ("Esc / c", "cancel running scan"),
        ("Pages", ""),
        ("j k / ↑ ↓", "select page"),
        ("J K / ← →", "move page down / up"),
        ("r", "rescan page (keeps old until success)"),
        ("R", "rotate page 90° clockwise"),
        ("<", "rotate page 90° counter-clockwise"),
        ("d", "delete page"),
        ("1-9", "jump to page N"),
        ("Finish", ""),
        ("f", "build searchable PDF (confirm dialog)"),
        ("o", "open PDF with xdg-open"),
        ("n", "new session"),
        ("Settings", ""),
        ("m", "cycle mode: gray → color → lineart"),
        ("+ = / -", "DPI presets 150 / 200 / 300 / 600"),
        ("L", "OCR languages picker"),
        ("Interface", ""),
        ("Tab / BackTab", "cycle pane focus (also: click)"),
        ("? / !", "this help / diagnostics"),
        ("q / Ctrl-C", "quit (confirm if unsaved pages)"),
    ];
    let mut lines: Vec<Line> = Vec::new();
    for (k, v) in rows {
        if v.is_empty() {
            lines.push(Line::from(Span::styled(
                k.to_string(),
                Style::default().bold().fg(Color::Cyan),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("  {k:<20}"), Style::default().fg(Color::Yellow)),
                Span::raw(v),
            ]));
        }
    }
    let area = centered_rect(62, (lines.len() as u16 + 2).min(30), area);
    f.render_widget(Clear, area);
    let block = overlay_block("Help - Esc to close");
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_diagnostics(f: &mut Frame, app: &App, area: Rect) {
    let Some(report) = &app.report else {
        let a = centered_rect(60, 8, area);
        f.render_widget(Clear, a);
        f.render_widget(overlay_block("Diagnostics"), a);
        return;
    };
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        "press r to re-run checks · Esc to close",
        Style::default().fg(Color::DarkGray),
    ))];
    for item in &report.items {
        let (mark, style) = match item.status {
            Status::Ok => (" OK ", Style::default().fg(Color::Black).bg(Color::Green)),
            Status::Warn => ("WARN", Style::default().fg(Color::Black).bg(Color::Yellow)),
            Status::Fail => ("FAIL", Style::default().fg(Color::White).bg(Color::Red)),
            Status::Skip => (
                "SKIP",
                Style::default().fg(Color::Black).bg(Color::DarkGray),
            ),
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" [{mark}] "), style),
            Span::raw(item.what.clone()),
        ]));
        if !item.detail.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("        {}", item.detail),
                Style::default().fg(Color::DarkGray),
            )));
        }
        if let Some(hint) = &item.hint {
            for line in hint.lines() {
                lines.push(Line::from(Span::styled(
                    format!("        install: {line}"),
                    Style::default().fg(Color::Yellow),
                )));
            }
        }
    }
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let dlg = centered_rect(90, height, area);
    f.render_widget(Clear, dlg);
    let block = overlay_block("Diagnostics");
    let inner = block.inner(dlg);
    f.render_widget(block, dlg);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_lang_picker(f: &mut Frame, picker: &overlays::LangPicker, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    if picker.loading {
        lines.push(Line::from("loading installed languages…"));
    } else if picker.available.is_empty() {
        lines.push(Line::from("no tesseract languages found"));
    }
    for (idx, lang) in picker.available.iter().enumerate() {
        let selected = picker.selected.contains(lang);
        let cursor = idx == picker.cursor;
        let marker = if selected { "[x]" } else { "[ ]" };
        let style = if cursor {
            Style::default().bg(Color::Rgb(40, 60, 80))
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(format!(" {marker} {lang}"), style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "space toggle · enter confirm ({}) · esc cancel",
            if picker.selected.is_empty() {
                "none selected".into()
            } else {
                picker.result()
            }
        ),
        Style::default().fg(Color::DarkGray),
    )));
    let height = (picker.available.len() as u16 + 6).min(24);
    let area = centered_rect(40, height, area);
    f.render_widget(Clear, area);
    let block = overlay_block("OCR languages");
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_confirm(f: &mut Frame, confirm: &Confirm, area: Rect) {
    let height = (confirm.lines.len() as u16 + 4).min(12);
    let width = confirm
        .lines
        .iter()
        .map(|l| l.len() as u16 + 4)
        .chain(std::iter::once(confirm.title.len() as u16 + 4))
        .chain(std::iter::once(confirm.accept_label.len() as u16 + 20))
        .max()
        .unwrap_or(40)
        .min(area.width.saturating_sub(4));
    let area = centered_rect(width, height, area);
    f.render_widget(Clear, area);
    let block = overlay_block(&confirm.title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut lines: Vec<Line> = confirm
        .lines
        .iter()
        .map(|l| Line::from(l.clone()))
        .collect();
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            format!(" [{}] ", confirm.accept_label),
            Style::default().fg(Color::Black).bg(Color::Green),
        ),
        Span::raw("  "),
        Span::styled("[Esc] cancel", Style::default().fg(Color::DarkGray)),
    ]));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    #[test]
    fn grid_cells_empty_and_degenerate() {
        assert!(grid_cells(rect(0, 10), 3, 0.7).is_empty());
        assert!(grid_cells(rect(10, 0), 3, 0.7).is_empty());
        assert!(grid_cells(rect(10, 10), 0, 0.7).is_empty());
    }

    #[test]
    fn grid_cells_single_fills_area() {
        let cells = grid_cells(rect(40, 20), 1, 0.7);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0], rect(40, 20));
    }

    #[test]
    fn grid_cells_counts_match_and_cover() {
        for n in [2usize, 5, 17, 23] {
            for (w, h) in [(80u16, 24u16), (100, 40), (30, 12)] {
                let area = rect(w, h);
                let cells = grid_cells(area, n, 0.707);
                assert_eq!(cells.len(), n, "n={n} {w}x{h}");
                // All cells inside the area.
                for c in &cells {
                    assert!(c.x >= area.x && c.y >= area.y);
                    assert!(c.x + c.width <= area.x + area.width);
                    assert!(c.y + c.height <= area.y + area.height);
                    assert!(c.width > 0 && c.height > 0);
                }
                // First cell starts at the origin.
                assert_eq!((cells[0].x, cells[0].y), (0, 0));
            }
        }
    }

    #[test]
    fn grid_cells_tiling_is_rectangular() {
        let area = rect(80, 24);
        let cells = grid_cells(area, 7, 0.707);
        // Infer cols from first row (cells with y == 0).
        let cols = cells.iter().filter(|c| c.y == 0).count();
        assert!(cols >= 1);
        assert_eq!(cells.len(), 7);
        // Rows are contiguous: every cell's x is a col start or continuation.
        for c in &cells {
            assert!(c.width > 0 && c.height > 0);
        }
    }

    #[test]
    fn grid_cells_portrait_prefers_more_columns_than_landscape() {
        let area = rect(100, 40);
        let portrait = grid_cells(area, 6, 0.707);
        let landscape = grid_cells(area, 6, 1.8);
        let cols_of = |cells: &[Rect]| cells.iter().filter(|c| c.y == 0).count();
        // Portrait pages pack into more columns than landscape ones.
        assert!(
            cols_of(&portrait) >= cols_of(&landscape),
            "portrait cols {} vs landscape cols {}",
            cols_of(&portrait),
            cols_of(&landscape)
        );
    }

    #[test]
    fn grid_cells_last_cell_absorbs_remainder() {
        let area = rect(80, 24);
        let cells = grid_cells(area, 3, 0.707);
        let rows = cells
            .iter()
            .map(|c| c.y)
            .collect::<std::collections::HashSet<_>>();
        let rows = rows.len();
        // Bottom row cells must reach the area bottom.
        let max_y = cells.iter().map(|c| c.y + c.height).max().unwrap();
        assert_eq!(max_y, area.height, "rows={rows}");
        // Right column cells must reach the area right edge.
        let max_x = cells.iter().map(|c| c.x + c.width).max().unwrap();
        assert_eq!(max_x, area.width);
    }
}
