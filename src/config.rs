//! Configuration: TOML file + CLI overrides, mirroring the Python tool's semantics.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

pub const PROGRAM: &str = "auto-scanner-ocr";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const SCAN_MODES: [&str; 3] = ["gray", "color", "lineart"];
/// Values passed to `scanimage --mode=...`
pub const SCANIMAGE_MODES: [(&str, &str); 3] =
    [("gray", "Gray"), ("color", "Color"), ("lineart", "Lineart")];

pub const DPI_PRESETS: [u16; 4] = [150, 200, 300, 600];

/// Page cleanup strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Cleanup {
    /// No unpaper pass; ocrmypdf --deskew --clean does the cleanup at finish.
    /// Default: unpaper's default filters destroy flatbed page content and
    /// its deskew never fires on flatbed scans (both measured), so the pass
    /// is a no-op that only costs seconds per page.
    #[default]
    Off,
    /// unpaper with all content-altering filters disabled
    /// (--no-mask-scan --no-border-scan --no-border-align --no-blackfilter
    /// --no-grayfilter --no-blurfilter --no-noisefilter --no-deskew): a
    /// verified pixel-identical passthrough kept for `unpaper_extra_args`
    /// experimentation.
    Conservative,
    /// Legacy behavior: unpaper's full default filter stack. WARNING: this
    /// is the mode that erases page edges (the missing-table bug).
    Legacy,
}

impl Cleanup {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Cleanup::Off),
            "conservative" => Some(Cleanup::Conservative),
            "legacy" => Some(Cleanup::Legacy),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Cleanup::Off => "off",
            Cleanup::Conservative => "conservative",
            Cleanup::Legacy => "legacy",
        }
    }

    pub const ALL: [&'static str; 3] = ["off", "conservative", "legacy"];
}

/// When the per-page preview OCR (tesseract txt for the TUI text pane) runs.
/// The final PDF's text layer always comes from ocrmypdf at finish and is
/// unaffected by this setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewOcr {
    /// OCR every page right after capture (previous behavior).
    Eager,
    /// OCR on demand: only the selected page's text is extracted, when it
    /// has none yet. Default: avoids 2-9s of tesseract per page that the
    /// user may never look at, and never delays the next scan.
    #[default]
    Lazy,
    /// Never OCR for the text pane (rotate does not re-OCR either).
    Off,
}

impl PreviewOcr {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "eager" => Some(PreviewOcr::Eager),
            "lazy" => Some(PreviewOcr::Lazy),
            "off" => Some(PreviewOcr::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PreviewOcr::Eager => "eager",
            PreviewOcr::Lazy => "lazy",
            PreviewOcr::Off => "off",
        }
    }

    pub const ALL: [&'static str; 3] = ["eager", "lazy", "off"];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub dpi: u16,
    pub mode: String,
    pub langs: String,
    pub device: String,
    pub output: PathBuf,
    pub cleanup: Cleanup,
    /// When the per-page preview OCR for the TUI text pane runs.
    pub preview_ocr: PreviewOcr,
    /// Extra argv words appended to the unpaper command (before the file
    /// arguments), e.g. `["--blackfilter-intensity", "40"]`. Only used when
    /// cleanup != off.
    pub unpaper_extra_args: Vec<String>,
    pub notify: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // 300 is the OCR sweet spot and 2-3x faster per capture; use
            // 600 for dense small print (CLI -d / TUI +/-).
            dpi: 300,
            mode: "gray".into(),
            langs: "deu+Latin".into(),
            device: "auto".into(),
            output: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Documents/scans"),
            cleanup: Cleanup::default(),
            preview_ocr: PreviewOcr::default(),
            unpaper_extra_args: Vec::new(),
            notify: true,
        }
    }
}

/// Raw TOML shape. Accepts both flat keys and a `[scan]` section, like the
/// Python version's `data.get("scan", data)`.
#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    scan: Option<ScanSection>,
    #[serde(flatten)]
    flat: ScanSection,
}

#[derive(Debug, Default, Deserialize)]
struct ScanSection {
    dpi: Option<u16>,
    mode: Option<String>,
    langs: Option<String>,
    device: Option<String>,
    output: Option<String>,
    /// Legacy key: migrated to `cleanup` (see migrate_unpaper).
    unpaper: Option<bool>,
    cleanup: Option<String>,
    preview_ocr: Option<String>,
    unpaper_extra_args: Option<Vec<String>>,
    notify: Option<bool>,
}

/// Config search order (parity with the Python tool):
/// explicit `--config` (must exist) -> `./config.toml` -> `~/.config/<prog>/config.toml`.
/// Note: XDG_CONFIG_HOME is deliberately ignored for parity; `~` is hardcoded.
pub fn find_config(explicit: Option<&PathBuf>) -> Result<Option<PathBuf>> {
    if let Some(p) = explicit {
        let p = expand_path(p.to_string_lossy().as_ref())?;
        if !p.is_file() {
            anyhow::bail!("Config file not found: {}", p.display());
        }
        return Ok(Some(p));
    }
    let mut candidates = vec![PathBuf::from("config.toml")];
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".config").join(PROGRAM).join("config.toml"));
    }
    for c in candidates {
        if c.is_file() {
            return Ok(Some(c));
        }
    }
    Ok(None)
}

/// Expand `~` and `$VARS` (parity with Python's expand()).
pub fn expand_path(s: &str) -> Result<PathBuf> {
    let expanded = shellexpand::full(s)
        .with_context(|| format!("invalid path expression: {s:?}"))?
        .into_owned();
    Ok(PathBuf::from(expanded))
}

pub fn load_config(explicit: Option<&PathBuf>) -> Result<Config> {
    // Seed from Config::default() so the two can never drift apart; only the
    // load-specific bookkeeping fields are set here.
    let d = Config::default();
    let mut cfg = LoadedConfig {
        dpi: d.dpi,
        mode: d.mode,
        langs: d.langs,
        device: d.device,
        output: d.output,
        cleanup: d.cleanup,
        cleanup_explicit: false,
        unpaper_legacy: None,
        preview_ocr: d.preview_ocr,
        unpaper_extra_args: d.unpaper_extra_args,
        notify: d.notify,
    };
    let Some(path) = find_config(explicit)? else {
        return Ok(migrate_unpaper(cfg));
    };
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read config file {}", path.display()))?;
    let data: RawConfig =
        toml::from_str(&raw).with_context(|| format!("invalid TOML in {}", path.display()))?;
    let section = data.scan.unwrap_or(data.flat);
    if let Some(v) = section.dpi {
        cfg.dpi = v;
    }
    if let Some(v) = section.mode {
        cfg.mode = v;
    }
    if let Some(v) = section.langs {
        cfg.langs = v;
    }
    if let Some(v) = section.device {
        cfg.device = v;
    }
    if let Some(v) = section.output {
        cfg.output = expand_path(&v)?;
    }
    if let Some(v) = section.unpaper {
        cfg.unpaper_legacy = Some(v);
    }
    if let Some(v) = &section.cleanup {
        match Cleanup::parse(v) {
            Some(c) => {
                cfg.cleanup = c;
                cfg.cleanup_explicit = true;
            }
            None => anyhow::bail!("invalid cleanup '{}' (use: {})", v, Cleanup::ALL.join(", ")),
        }
    }
    if let Some(v) = &section.preview_ocr {
        match PreviewOcr::parse(v) {
            Some(p) => cfg.preview_ocr = p,
            None => anyhow::bail!(
                "invalid preview_ocr '{}' (use: {})",
                v,
                PreviewOcr::ALL.join(", ")
            ),
        }
    }
    if let Some(v) = section.unpaper_extra_args {
        cfg.unpaper_extra_args = v;
    }
    if let Some(v) = section.notify {
        cfg.notify = v;
    }
    let cfg = migrate_unpaper(cfg);
    validate(cfg)
}

/// Legacy-key migration. Rules (user-approved):
/// - explicit `cleanup` wins; a coexisting `unpaper` key is ignored with a
///   warning.
/// - `unpaper = false` -> `cleanup = "off"`.
/// - `unpaper = true` -> `cleanup = "conservative"` + deprecation warning
///   (the old default destroyed flatbed page content; `cleanup = "legacy"`
///   restores the exact old behavior).
/// - neither key -> `cleanup = "off"` (the struct default).
fn migrate_unpaper(mut cfg: LoadedConfig) -> Config {
    let warn = |msg: &str| tracing::warn!("{msg}");
    if let Some(legacy) = cfg.unpaper_legacy {
        if cfg.cleanup_explicit {
            warn(
                "config has both 'unpaper' and 'cleanup'; \
                 ignoring 'unpaper' ('cleanup' wins)",
            );
        } else {
            let new_mode = if legacy {
                warn(
                    "config key 'unpaper' is deprecated; \
                     use cleanup = \"conservative\" (or \"off\" / \"legacy\")",
                );
                Cleanup::Conservative
            } else {
                Cleanup::Off
            };
            cfg.cleanup = new_mode;
        }
    }
    Config {
        dpi: cfg.dpi,
        mode: cfg.mode,
        langs: cfg.langs,
        device: cfg.device,
        output: cfg.output,
        cleanup: cfg.cleanup,
        preview_ocr: cfg.preview_ocr,
        unpaper_extra_args: cfg.unpaper_extra_args,
        notify: cfg.notify,
    }
}

/// The config under construction (mirrors `Config` but with the legacy
/// `unpaper` key still unresolved).
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub dpi: u16,
    pub mode: String,
    pub langs: String,
    pub device: String,
    pub output: PathBuf,
    pub cleanup: Cleanup,
    /// True when the config file set `cleanup` explicitly (drives the
    /// both-keys-present migration warning).
    pub cleanup_explicit: bool,
    pub unpaper_legacy: Option<bool>,
    pub preview_ocr: PreviewOcr,
    pub unpaper_extra_args: Vec<String>,
    pub notify: bool,
}

/// Validation shared by config load and CLI overrides (parity: dpi >= 150,
/// known mode, plus-separated langs pattern). Lang parts are lowercase codes
/// (`deu`, `chi_sim`, …) or the script model `Latin` (needed for `§` — the
/// `deu` model alone misreads it as `&`; see README troubleshooting).
pub fn validate(cfg: Config) -> Result<Config> {
    if cfg.dpi < 150 {
        anyhow::bail!(
            "dpi must be >= 150 for usable OCR results (got {})",
            cfg.dpi
        );
    }
    if !SCAN_MODES.contains(&cfg.mode.as_str()) {
        anyhow::bail!(
            "invalid mode '{}' (use: {})",
            cfg.mode,
            SCAN_MODES.join(", ")
        );
    }
    if !langs_valid(&cfg.langs) {
        anyhow::bail!(
            "invalid langs '{}' (use plus-separated codes, e.g. deu+Latin)",
            cfg.langs
        );
    }
    if cfg.unpaper_extra_args.iter().any(|a| a.trim().is_empty()) {
        anyhow::bail!("unpaper_extra_args must be non-empty strings");
    }
    Ok(cfg)
}

/// Script-model names allowed alongside lowercase language codes, paired
/// with the Debian/Ubuntu package suffix for the ones distros ship.
/// `Latin` is a tesseract script model (tessdata `script/Latin.traineddata`);
/// it recognizes `§` reliably where language models fail.
const SCRIPT_LANGS: &[(&str, &str)] = &[("Latin", "latn")];

/// True when `lang` names a tesseract script model (not a language code).
pub fn is_script_lang(lang: &str) -> bool {
    SCRIPT_LANGS.iter().any(|(name, _)| *name == lang)
}

/// Debian/Ubuntu package shipping the script model, if one exists.
pub fn script_lang_package(lang: &str) -> Option<String> {
    SCRIPT_LANGS
        .iter()
        .find(|(name, _)| *name == lang)
        .map(|(_, suffix)| format!("tesseract-ocr-script-{suffix}"))
}

pub fn langs_valid(langs: &str) -> bool {
    if langs.is_empty() {
        return false;
    }
    langs.split('+').all(|part| {
        if part.is_empty() {
            return false;
        }
        if is_script_lang(part) {
            return true;
        }
        part.chars().all(|c| c.is_ascii_lowercase() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert_eq!(cfg.dpi, 300);
        assert_eq!(cfg.mode, "gray");
        assert_eq!(cfg.langs, "deu+Latin");
        assert_eq!(cfg.device, "auto");
        assert_eq!(cfg.cleanup, Cleanup::Off);
        assert_eq!(cfg.preview_ocr, PreviewOcr::Lazy);
        assert!(cfg.unpaper_extra_args.is_empty());
        assert!(cfg.notify);
    }

    #[test]
    fn cleanup_parse_and_str() {
        assert_eq!(Cleanup::parse("off"), Some(Cleanup::Off));
        assert_eq!(Cleanup::parse("conservative"), Some(Cleanup::Conservative));
        assert_eq!(Cleanup::parse("legacy"), Some(Cleanup::Legacy));
        assert_eq!(Cleanup::parse("nope"), None);
        for mode in Cleanup::ALL {
            let c = Cleanup::parse(mode).unwrap();
            assert_eq!(c.as_str(), mode);
        }
    }

    #[test]
    fn preview_ocr_parse_and_str() {
        assert_eq!(PreviewOcr::parse("eager"), Some(PreviewOcr::Eager));
        assert_eq!(PreviewOcr::parse("lazy"), Some(PreviewOcr::Lazy));
        assert_eq!(PreviewOcr::parse("off"), Some(PreviewOcr::Off));
        assert_eq!(PreviewOcr::parse("nope"), None);
        for mode in PreviewOcr::ALL {
            let p = PreviewOcr::parse(mode).unwrap();
            assert_eq!(p.as_str(), mode);
        }
    }

    #[test]
    fn legacy_unpaper_key_migrates() {
        // unpaper = true -> conservative + deprecation warning path.
        let cfg = migrate_unpaper(LoadedConfig {
            unpaper_legacy: Some(true),
            ..test_loaded()
        });
        assert_eq!(cfg.cleanup, Cleanup::Conservative);
        // unpaper = false -> off.
        let cfg = migrate_unpaper(LoadedConfig {
            unpaper_legacy: Some(false),
            ..test_loaded()
        });
        assert_eq!(cfg.cleanup, Cleanup::Off);
        // Explicit cleanup (even "off") wins over unpaper.
        let cfg = migrate_unpaper(LoadedConfig {
            unpaper_legacy: Some(true),
            cleanup: Cleanup::Off,
            cleanup_explicit: true,
            ..test_loaded()
        });
        assert_eq!(cfg.cleanup, Cleanup::Off);
        // Neither key -> default (off).
        let cfg = migrate_unpaper(test_loaded());
        assert_eq!(cfg.cleanup, Cleanup::Off);
    }

    fn test_loaded() -> LoadedConfig {
        LoadedConfig {
            dpi: 600,
            mode: "gray".into(),
            langs: "deu+Latin".into(),
            device: "auto".into(),
            output: PathBuf::from("/tmp/scans"),
            cleanup: Cleanup::Off,
            cleanup_explicit: false,
            unpaper_legacy: None,
            preview_ocr: PreviewOcr::Lazy,
            unpaper_extra_args: Vec::new(),
            notify: true,
        }
    }

    #[test]
    fn langs_validation() {
        assert!(langs_valid("eng"));
        assert!(langs_valid("eng+deu"));
        assert!(langs_valid("chi_sim+eng"));
        assert!(langs_valid("deu+Latin"));
        assert!(langs_valid("Latin"));
        assert!(!langs_valid(""));
        assert!(!langs_valid("eng+"));
        assert!(!langs_valid("+eng"));
        assert!(!langs_valid("eng++deu"));
        assert!(!langs_valid("Eng"));
        assert!(!langs_valid("eng+Latin2"));
        assert!(!langs_valid("en-gb"));
        assert!(!langs_valid("en gb"));
    }

    #[test]
    fn validate_rejects_low_dpi() {
        let cfg = Config {
            dpi: 100,
            ..Default::default()
        };
        assert!(validate(cfg).is_err());
    }

    #[test]
    fn validate_rejects_bad_mode() {
        let cfg = Config {
            mode: "nope".into(),
            ..Default::default()
        };
        assert!(validate(cfg).is_err());
    }

    #[test]
    fn expand_path_tilde() {
        let home = dirs::home_dir().unwrap();
        let p = expand_path("~/Documents").unwrap();
        assert_eq!(p, home.join("Documents"));
        let p = expand_path("$HOME/x").unwrap();
        assert_eq!(p, home.join("x"));
    }
}
#[cfg(test)]
mod file_tests {
    use super::*;

    fn write_cfg(dir: &std::path::Path, content: &str) -> PathBuf {
        let p = dir.join("config.toml");
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn parses_scan_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cfg(dir.path(), "[scan]\ndpi = 600\nmode = \"color\"\n");
        let raw = std::fs::read_to_string(&path).unwrap();
        let data: RawConfig = toml::from_str(&raw).unwrap();
        let section = data.scan.unwrap();
        assert_eq!(section.dpi, Some(600));
        assert_eq!(section.mode.as_deref(), Some("color"));
    }

    #[test]
    fn parses_preview_ocr() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cfg(dir.path(), "[scan]\npreview_ocr = \"eager\"\n");
        let raw = std::fs::read_to_string(&path).unwrap();
        let data: RawConfig = toml::from_str(&raw).unwrap();
        let section = data.scan.unwrap();
        assert_eq!(section.preview_ocr.as_deref(), Some("eager"));
        // Flat form works too.
        let path = write_cfg(dir.path(), "preview_ocr = \"off\"\n");
        let raw = std::fs::read_to_string(&path).unwrap();
        let data: RawConfig = toml::from_str(&raw).unwrap();
        assert_eq!(data.flat.preview_ocr.as_deref(), Some("off"));
    }

    #[test]
    fn parses_flat_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cfg(dir.path(), "dpi = 200\nlangs = \"eng\"\n");
        let raw = std::fs::read_to_string(&path).unwrap();
        let data: RawConfig = toml::from_str(&raw).unwrap();
        assert_eq!(data.flat.dpi, Some(200));
        assert_eq!(data.flat.langs.as_deref(), Some("eng"));
    }

    #[test]
    fn output_expands_tilde() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_cfg(dir.path(), "[scan]\noutput = \"~/scans\"\n");
        let raw = std::fs::read_to_string(&path).unwrap();
        let data: RawConfig = toml::from_str(&raw).unwrap();
        let section = data.scan.unwrap();
        let out = expand_path(&section.output.unwrap()).unwrap();
        assert!(out.starts_with(dirs::home_dir().unwrap()));
    }
}
