mod app;
mod overlays;
mod preview;
mod theme;
mod ui;

pub use app::{run_tui, App, Settings, TuiInit};

/// A guaranteed-safe halfblocks picker (no terminal query involved).
pub fn halfblocks_picker() -> ratatui_image::picker::Picker {
    ratatui_image::picker::Picker::halfblocks()
}
