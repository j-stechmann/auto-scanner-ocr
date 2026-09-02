//! Command line interface, flag-for-flag compatible with the Python tool
//! (minus `-m/--multi`, which the TUI makes obsolete).

use std::path::PathBuf;

use clap::Parser;

use crate::config::{self, Config};

#[derive(Debug, Parser)]
#[command(
    name = config::PROGRAM,
    version = config::VERSION,
    about = "Scan with a flatbed SANE scanner and produce a searchable OCR PDF - now with a terminal UI."
)]
pub struct Cli {
    /// Scan resolution in DPI (default 300)
    #[arg(short = 'd', long)]
    pub dpi: Option<u16>,

    /// Scan mode: gray, color or lineart (default gray)
    #[arg(short = 'M', long)]
    pub mode: Option<String>,

    /// OCR languages, plus-separated (default eng+deu)
    #[arg(short = 'l', long)]
    pub langs: Option<String>,

    /// Output directory for finished PDFs (default ~/Documents/scans)
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    /// SANE device name or substring (default: first found)
    #[arg(short = 'e', long)]
    pub device: Option<String>,

    /// Config file to use (default: ./config.toml then ~/.config/auto-scanner-ocr/config.toml)
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Skip the unpaper cleanup step
    #[arg(long)]
    pub no_unpaper: bool,

    /// Disable desktop notifications
    #[arg(long)]
    pub no_notify: bool,

    /// Check dependencies and environment, then exit
    #[arg(long)]
    pub doctor: bool,

    /// Also print log output to stderr
    #[arg(short = 'v', long)]
    pub verbose: bool,

    /// Hidden: probe terminal image protocol (kitty/sixel) in isolation.
    /// Prints "protocol=<type> font=<w>x<h>" and exits. Used by the TUI
    /// because ratatui-image's in-process stdio query can leave an orphaned
    /// stdin reader that eats keystrokes when the terminal never answers.
    #[arg(long, hide = true)]
    pub image_probe: bool,
}

impl Cli {
    /// Apply CLI overrides on top of the file config (parity with Python's
    /// apply_overrides, including the same validations).
    pub fn apply_overrides(&self, mut cfg: Config) -> anyhow::Result<Config> {
        if let Some(v) = self.dpi {
            cfg.dpi = v;
        }
        if let Some(v) = &self.mode {
            cfg.mode = v.clone();
        }
        if let Some(v) = &self.langs {
            cfg.langs = v.clone();
        }
        if let Some(v) = &self.output {
            cfg.output = v.clone();
        }
        if let Some(v) = &self.device {
            cfg.device = v.clone();
        }
        if self.no_unpaper {
            cfg.unpaper = false;
        }
        if self.no_notify {
            cfg.notify = false;
        }
        config::validate(cfg)
    }
}

/// Compose final config: file -> CLI overrides.
pub fn final_config(cli: &Cli) -> anyhow::Result<Config> {
    let cfg = config::load_config(cli.config.as_ref())?;
    cli.apply_overrides(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("auto-scanner-ocr").chain(args.iter().copied()))
    }

    #[test]
    fn parses_short_flags() {
        let c = cli(&["-d", "600", "-M", "color", "-l", "eng"]);
        assert_eq!(c.dpi, Some(600));
        assert_eq!(c.mode.as_deref(), Some("color"));
        assert_eq!(c.langs.as_deref(), Some("eng"));
    }

    #[test]
    fn overrides_apply() {
        let c = cli(&["--dpi", "150", "--no-unpaper", "--no-notify"]);
        let cfg = c.apply_overrides(Config::default()).unwrap();
        assert_eq!(cfg.dpi, 150);
        assert!(!cfg.unpaper);
        assert!(!cfg.notify);
    }

    #[test]
    fn rejects_bad_dpi() {
        let c = cli(&["--dpi", "100"]);
        assert!(c.apply_overrides(Config::default()).is_err());
    }

    #[test]
    fn rejects_bad_mode() {
        let c = cli(&["--mode", "nope"]);
        assert!(c.apply_overrides(Config::default()).is_err());
    }
}
