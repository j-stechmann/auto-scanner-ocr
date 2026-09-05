//! Named styles + a CI contrast gate for everything the TUI paints.
//!
//! All color decisions live here so the `palettes_pass_contrast` test has one
//! catalog to check: every style with a background is verified against a
//! matrix of real 16-color terminal palettes (VGA, Tango, Dracula, Nord,
//! Gruvbox Dark/Light, One Half Dark/Light, Solarized Dark/Light) using the
//! WCAG 2.1 relative-luminance contrast ratio. ANSI slots 0-15 follow the
//! terminal theme, and light themes invert their polarity (Gruvbox Light maps
//! slot 0 to a pale color), so pairs are checked against all palettes, not a
//! single "dark" assumption.

use ratatui::style::{Color, Style};

/// Truecolor constants. The navy selection highlight predates this module;
/// it is the only background that does not follow the theme palette, so it
/// gets truecolor fg/bg pairs that no theme can invert.
const NAVY: Color = Color::Rgb(40, 60, 80);
const TRUE_WHITE: Color = Color::Rgb(255, 255, 255);
/// Unfocused pane borders: bright enough to be visible on every palette in
/// the matrix (worst 2.0:1, Nord) without glaring on light themes.
const BORDER_DIM: Color = Color::Rgb(110, 110, 110);

pub const ACCENT: Style = Style::new().fg(Color::Cyan);
pub const ACCENT_BOLD: Style = Style::new().fg(Color::Cyan).bold();

/// Selection highlight and picker cursor: truecolor fg AND bg. The fg must
/// not be an ANSI slot - on light themes slot 15 can render dark (Gruvbox
/// Light maps it to a near-black), which would put dark text on this navy.
pub const HIGHLIGHT: Style = Style::new().fg(TRUE_WHITE).bg(NAVY);

pub const BADGE_OK: Style = Style::new().fg(Color::Black).bg(Color::Green);
pub const BADGE_FAIL: Style = Style::new().fg(Color::White).bg(Color::Red);
pub const BADGE_BUSY: Style = Style::new().fg(Color::Black).bg(Color::Yellow).bold();
/// Diagnostics WARN badge: same pair as BADGE_BUSY but static (not a
/// progress state), so it stays unbold like every other diagnostics badge.
pub const BADGE_WARN: Style = Style::new().fg(Color::Black).bg(Color::Yellow);
/// Info badge (N processing, diag " ..."): Black on ANSI 6. Same light-
/// polarity exception as BADGE_FAIL/BUSY (worst 1.9:1, Gruvbox Light).
pub const BADGE_INFO: Style = Style::new().fg(Color::Black).bg(Color::Cyan);
/// Muted badge (deleting, SKIP): Black on ANSI 7 passes on every palette in
/// the matrix (worst 4.3:1, Gruvbox Light); no fg passes on ANSI 8 there.
pub const BADGE_MUTED: Style = Style::new().fg(Color::Black).bg(Color::Gray);

/// Secondary text on the terminal default background. Accepted exception:
/// DarkGray follows the theme, so contrast against the unknown default bg
/// is theme-dependent - worst case fully invisible (1.0:1) on Solarized
/// Dark, whose slot 8 equals its default background, to ~7.7:1 (One Half
/// Light on dark).
pub const MUTED: Style = Style::new().fg(Color::DarkGray);

/// Key-column accent in help/diagnostics. Accepted exception like MUTED:
/// theme-dependent, verified bright on the dark palettes in the matrix
/// (worst 6.5:1 on black); on light palettes yellow keys are decorative.
pub const KEY: Style = Style::new().fg(Color::Yellow);

pub fn header() -> Style {
    // Accepted exception: worst 3.7:1 (Nord) across the matrix.
    Style::new().fg(Color::Black).bg(Color::LightBlue).bold()
}

pub fn header_filler() -> Style {
    // Same pair as the header band; the fg only applies to filler spaces
    // (invisible) but makes the band's colors explicit. Checked in the
    // catalog test alongside header().
    Style::new().fg(Color::Black).bg(Color::LightBlue)
}

pub fn border_focused() -> Style {
    ACCENT
}

pub fn border_unfocused() -> Style {
    Style::new().fg(BORDER_DIM)
}

/// Rotate marker: the only persistent per-page rotation indicator. Cyan is
/// a large win on dark palettes (blue was ~1.2-2.2:1 there); accepted
/// exception like MUTED on light ones (~1.9-2.9:1, slightly below the old
/// blue on Gruvbox/Solarized Light) for a decorative glyph. It renders
/// White on selected rows.
pub const ROTATED: Style = ACCENT;

#[cfg(test)]
mod tests {
    use super::*;

    /// 16 ANSI slots as RGB, from real theme files (iTerm2-Color-Schemes;
    /// Solarized per iTerm2 defaults, which use the canonical 16 everywhere).
    /// Slots are the exact SGR indexes crossterm emits (38;5;N / 48;5;N).
    type Palette = [Rgb; 16];
    type Rgb = [u8; 3];

    #[rustfmt::skip]
    const VGA: Palette = [
        [0, 0, 0], [170, 0, 0], [0, 170, 0], [170, 85, 0], [0, 0, 170],
        [170, 0, 170], [0, 170, 170], [170, 170, 170], [85, 85, 85],
        [255, 85, 85], [85, 255, 85], [255, 255, 85], [85, 85, 255],
        [255, 85, 255], [85, 255, 255], [255, 255, 255],
    ];
    #[rustfmt::skip]
    const DRACULA: Palette = [
        [33, 34, 44], [255, 85, 85], [80, 250, 123], [241, 250, 140],
        [189, 147, 249], [255, 121, 198], [139, 233, 253], [248, 248, 242],
        [98, 114, 164], [255, 110, 110], [105, 255, 148], [255, 255, 165],
        [214, 172, 255], [255, 146, 223], [164, 255, 255], [255, 255, 255],
    ];
    #[rustfmt::skip]
    const GRUVBOX_DARK: Palette = [
        [40, 40, 40], [204, 36, 29], [152, 151, 26], [215, 153, 33],
        [69, 133, 136], [177, 98, 134], [142, 192, 124], [168, 153, 132],
        [60, 56, 54], [251, 73, 52], [184, 187, 38], [250, 189, 47],
        [131, 165, 152], [211, 134, 155], [214, 147, 87], [235, 219, 178],
    ];
    #[rustfmt::skip]
    const GRUVBOX_LIGHT: Palette = [
        [251, 241, 199], [204, 36, 29], [152, 151, 26], [215, 153, 33],
        [69, 133, 136], [177, 98, 134], [142, 192, 124], [124, 111, 100],
        [60, 56, 54], [157, 0, 6], [121, 116, 14], [181, 118, 20],
        [7, 102, 120], [143, 63, 113], [66, 123, 88], [60, 56, 54],
    ];
    #[rustfmt::skip]
    const NORD: Palette = [
        [59, 66, 82], [191, 97, 106], [163, 190, 140], [235, 203, 139],
        [129, 161, 193], [180, 142, 173], [143, 188, 187], [229, 233, 240],
        [76, 86, 106], [191, 97, 106], [163, 190, 140], [235, 203, 139],
        [129, 161, 193], [180, 142, 173], [143, 188, 187], [236, 239, 244],
    ];
    #[rustfmt::skip]
    const ONE_HALF_DARK: Palette = [
        [40, 44, 52], [224, 108, 117], [152, 195, 121], [229, 192, 123],
        [97, 175, 239], [198, 120, 221], [86, 182, 194], [220, 223, 228],
        [93, 103, 122], [224, 108, 117], [152, 195, 121], [229, 192, 123],
        [97, 175, 239], [198, 120, 221], [86, 182, 194], [220, 223, 228],
    ];
    #[rustfmt::skip]
    const ONE_HALF_LIGHT: Palette = [
        [56, 58, 66], [224, 108, 117], [152, 195, 121], [229, 192, 123],
        [97, 175, 239], [198, 120, 221], [86, 182, 194], [250, 250, 250],
        [79, 82, 94], [224, 108, 117], [152, 195, 121], [229, 192, 123],
        [97, 175, 239], [198, 120, 221], [86, 182, 194], [255, 255, 255],
    ];
    #[rustfmt::skip]
    const SOLARIZED_DARK: Palette = [
        [7, 54, 66], [220, 50, 47], [133, 153, 0], [181, 137, 0],
        [38, 139, 210], [211, 54, 130], [42, 161, 152], [238, 232, 213],
        [0, 43, 54], [203, 75, 22], [88, 110, 117], [147, 161, 161],
        [147, 161, 161], [203, 75, 22], [108, 113, 196], [253, 246, 227],
    ];
    #[rustfmt::skip]
    const SOLARIZED_LIGHT: Palette = [
        // Same canonical 16 as Solarized Dark (iTerm2 defaults swap only
        // fg/bg/selection, not the slots); kept as a named row so the
        // matrix output reads naturally, not for extra slot coverage.
        [7, 54, 66], [220, 50, 47], [133, 153, 0], [181, 137, 0],
        [38, 139, 210], [211, 54, 130], [42, 161, 152], [238, 232, 213],
        [0, 43, 54], [203, 75, 22], [88, 110, 117], [147, 161, 161],
        [147, 161, 161], [203, 75, 22], [108, 113, 196], [253, 246, 227],
    ];
    #[rustfmt::skip]
    const TANGO_DARK: Palette = [
        [0, 0, 0], [204, 0, 0], [78, 154, 6], [196, 160, 0], [52, 101, 164],
        [117, 80, 123], [6, 152, 154], [211, 215, 207], [85, 87, 83],
        [239, 41, 41], [138, 226, 52], [252, 233, 79], [114, 159, 207],
        [173, 127, 168], [52, 226, 226], [238, 238, 236],
    ];

    const PALETTES: &[(&str, Palette)] = &[
        ("VGA", VGA),
        ("Dracula", DRACULA),
        ("Gruvbox Dark", GRUVBOX_DARK),
        ("Gruvbox Light", GRUVBOX_LIGHT),
        ("Nord", NORD),
        ("One Half Dark", ONE_HALF_DARK),
        ("One Half Light", ONE_HALF_LIGHT),
        ("Solarized Dark", SOLARIZED_DARK),
        ("Solarized Light", SOLARIZED_LIGHT),
        ("Tango Dark", TANGO_DARK),
    ];

    fn channel(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    fn luminance(rgb: Rgb) -> f64 {
        0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
    }

    /// WCAG 2.1 contrast ratio; 1.0 (identical) to 21.0 (black/white).
    fn contrast(a: Rgb, b: Rgb) -> f64 {
        let (hi, lo) = if luminance(a) >= luminance(b) {
            (a, b)
        } else {
            (b, a)
        };
        (luminance(hi) + 0.05) / (luminance(lo) + 0.05)
    }

    /// Resolve a ratatui Color against a palette. Only the slot variants the
    /// catalog actually uses are handled; truecolor passes through.
    fn resolve(color: Color, palette: &Palette) -> Rgb {
        match color {
            Color::Black => palette[0],
            Color::Red => palette[1],
            Color::Green => palette[2],
            Color::Yellow => palette[3],
            Color::Cyan => palette[6],
            Color::Gray => palette[7],
            Color::DarkGray => palette[8],
            Color::LightBlue => palette[12],
            Color::White => palette[15],
            Color::Rgb(r, g, b) => [r, g, b],
            other => panic!("unexpected color in catalog: {other:?}"),
        }
    }

    /// Single registry of every style defined in this module. The f64 is
    /// the worst-case contrast threshold a bg-carrying style must beat on
    /// ANY palette; `None` marks fg-only styles, which must not paint a
    /// background. Keeping one list means a new style cannot escape both
    /// checks.
    fn registry() -> Vec<(&'static str, Style, Option<f64>)> {
        vec![
            ("HIGHLIGHT", HIGHLIGHT, Some(4.5)),
            ("BADGE_OK", BADGE_OK, Some(2.5)),
            ("BADGE_FAIL", BADGE_FAIL, Some(2.0)),
            ("BADGE_BUSY", BADGE_BUSY, Some(2.0)),
            ("BADGE_WARN", BADGE_WARN, Some(2.0)),
            ("BADGE_INFO", BADGE_INFO, Some(1.5)),
            ("BADGE_MUTED", BADGE_MUTED, Some(3.0)),
            ("header()", header(), Some(3.0)),
            ("header_filler()", header_filler(), Some(3.0)),
            ("KEY", KEY, None),
            ("ACCENT", ACCENT, None),
            ("ACCENT_BOLD", ACCENT_BOLD, None),
            ("MUTED", MUTED, None),
            ("ROTATED", ROTATED, None),
            ("border_focused()", border_focused(), None),
            ("border_unfocused()", border_unfocused(), None),
        ]
    }

    /// Every background-carrying style in the registry, with the worst
    /// ratio it must beat on ANY palette in the matrix. HIGHLIGHT is body
    /// text on a truecolor pair (4.5 = WCAG AA); BADGE_MUTED and the header
    /// band pass WCAG large-text AA (3.0). The remaining badges are
    /// pure-ANSI pairs whose worst cases on light-polarity palettes
    /// (Gruvbox Light: 1.9-2.7:1) cannot be fixed within the 16-color
    /// palette - no slot pair does better there - so their thresholds
    /// encode that documented exception.
    fn catalog() -> Vec<(&'static str, Style, f64)> {
        registry()
            .into_iter()
            .filter_map(|(name, style, thr)| thr.map(|t| (name, style, t)))
            .collect()
    }

    #[test]
    fn palettes_pass_contrast() {
        for (name, style, threshold) in catalog() {
            for (palette_name, palette) in PALETTES {
                let (Some(fg), Some(bg)) = (style.fg, style.bg) else {
                    panic!("{name} has no explicit fg/bg pair");
                };
                let ratio = contrast(resolve(fg, palette), resolve(bg, palette));
                assert!(
                    ratio >= threshold,
                    "{name} on {palette_name}: {ratio:.1}:1 (threshold {threshold})"
                );
            }
        }
    }

    /// Structural invariant: a style that paints a background must also pin
    /// the foreground, or it inherits the terminal's default fg - which is
    /// arbitrary across themes (the original 1.2:1 bug). Every style in the
    /// registry must be a complete pair if it has a bg, and must NOT paint
    /// a bg if it is fg-only (`None` threshold).
    #[test]
    fn backgrounds_pin_their_foreground() {
        for (name, style, threshold) in registry() {
            if threshold.is_some() {
                assert!(
                    style.bg.is_some() && style.fg.is_some(),
                    "{name} carries a bg threshold but does not set BOTH fg and bg \
                     (bg alone inherits default fg)"
                );
            } else {
                assert!(
                    style.bg.is_none(),
                    "{name} unexpectedly paints a background; give it a pair and a \
                     threshold in the registry"
                );
            }
        }
    }

    /// Source-scan invariant: every color decision must live in this module,
    /// or an inline `Color::`/`.bg(` in a consumer escapes both tests above.
    #[test]
    fn consumers_do_not_paint_their_own_colors() {
        for (file, src) in [
            ("mod.rs", include_str!("mod.rs")),
            ("app.rs", include_str!("app.rs")),
            ("ui.rs", include_str!("ui.rs")),
            ("overlays.rs", include_str!("overlays.rs")),
            ("preview.rs", include_str!("preview.rs")),
        ] {
            for (line_no, line) in src.lines().enumerate() {
                // Doc comments describing theme.rs behavior are allowed.
                let code = line.split("//").next().unwrap_or(line);
                assert!(
                    !code.contains("Color::") && !code.contains(".bg("),
                    "{file}:{} paints its own colors ({code:?}); add a named style in \
                     theme.rs instead so the contrast gate covers it",
                    line_no + 1
                );
            }
        }
    }
}
