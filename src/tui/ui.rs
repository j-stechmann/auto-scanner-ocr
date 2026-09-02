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
use crate::session::{Busy, PageStatus};

/// Per-frame pane geometry, used by both rendering and hit-testing.
#[derive(Debug, Clone, Copy, Default)]
pub struct PaneRects {
    pub sidebar: Rect,
    pub status: Rect,
    /// Preview pane INCLUDING borders (use `preview_inner` for content).
    pub preview: Rect,
    /// Preview content area inside the block border — the exact rect that
    /// `preview.render()` and `sync_area()` must both use. A mismatch here
    /// makes sixel encode for a size the render guard then rejects
    /// (image only ever appearing after a resize forces a full redraw).
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
    let block = block_with_title("Preview", is_focused(app, Pane::Preview));
    debug_assert_eq!(block.inner(area), inner, "preview inner rect mismatch");
    f.render_widget(block, area);
    if preview.has_image() {
        // Render the cached encoding; re-encode requests are issued by
        // sync_area after the frame. Placeholder text is drawn underneath so
        // protocols that cannot render here still leave the hint visible.
        preview.render(inner, f.buffer_mut());
    } else {
        let empty = selected_empty_hint(app);
        f.render_widget(
            Paragraph::new(empty).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
    }
}

fn selected_empty_hint(app: &App) -> String {
    if app.pages.is_empty() {
        "press s to scan your first page".into()
    } else {
        match app.selected_page().and_then(|p| p.image.clone()) {
            Some(_) => "loading preview…".into(),
            None => match app.selected_page().map(|p| p.status) {
                Some(PageStatus::Scanning) => "scanning…".into(),
                Some(PageStatus::Processing) => "processing…".into(),
                Some(PageStatus::Failed) => {
                    "scan failed - select another page or rescan (r)".into()
                }
                _ => "no image".into(),
            },
        }
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
        let hint = match app.selected_page().map(|p| p.status) {
            Some(PageStatus::Failed) => app
                .selected_page()
                .and_then(|p| p.error.clone())
                .unwrap_or_else(|| "unknown error".into()),
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
