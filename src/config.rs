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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub dpi: u16,
    pub mode: String,
    pub langs: String,
    pub device: String,
    pub output: PathBuf,
    pub unpaper: bool,
    pub notify: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dpi: 600,
            mode: "gray".into(),
            langs: "deu+Latin".into(),
            device: "auto".into(),
            output: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Documents/scans"),
            unpaper: true,
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
    unpaper: Option<bool>,
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
    let mut cfg = Config::default();
    let Some(path) = find_config(explicit)? else {
        return Ok(cfg);
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
        cfg.unpaper = v;
    }
    if let Some(v) = section.notify {
        cfg.notify = v;
    }
    validate(cfg)
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
        assert_eq!(cfg.dpi, 600);
        assert_eq!(cfg.mode, "gray");
        assert_eq!(cfg.langs, "deu+Latin");
        assert_eq!(cfg.device, "auto");
        assert!(cfg.unpaper && cfg.notify);
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
