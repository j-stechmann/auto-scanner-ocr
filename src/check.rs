//! Dependency and environment checks (parity with the Python --doctor/preflight).

use std::fmt;
use std::time::Duration;

use crate::backend::scan::{self, Device};
use crate::config::Config;

/// Timeout for notify-send / unpaper presence checks is not needed (PATH only).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Ok => write!(f, " OK"),
            Status::Warn => write!(f, "WARN"),
            Status::Fail => write!(f, "FAIL"),
            Status::Skip => write!(f, "SKIP"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckItem {
    pub what: String,
    pub status: Status,
    /// Detail line (e.g. device name, output dir).
    pub detail: String,
    /// Install hint when failing.
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub items: Vec<CheckItem>,
    pub device: Option<Device>,
}

impl Report {
    pub fn errors(&self) -> Vec<&CheckItem> {
        self.items
            .iter()
            .filter(|i| i.status == Status::Fail)
            .collect()
    }

    pub fn warnings(&self) -> Vec<&CheckItem> {
        self.items
            .iter()
            .filter(|i| i.status == Status::Warn)
            .collect()
    }

    pub fn ok(&self) -> bool {
        self.errors().is_empty()
    }
}

const HINTS: &[(&str, &str, &str)] = &[
    (
        "scanimage",
        "SANE (scanner access)",
        "pacman: sudo pacman -S sane hplip / apt: sudo apt install sane hplip",
    ),
    (
        "ocrmypdf",
        "OCRmyPDF (searchable PDFs)",
        "Arch: yay -S ocrmypdf (AUR) or: uv tool install ocrmypdf / apt: sudo apt install ocrmypdf",
    ),
    (
        "tesseract",
        "Tesseract OCR engine",
        "pacman: sudo pacman -S tesseract / apt: sudo apt install tesseract",
    ),
    (
        "img2pdf",
        "img2pdf (lossless image-to-PDF)",
        "pacman: sudo pacman -S img2pdf / apt: sudo apt install img2pdf",
    ),
    (
        "unpaper",
        "unpaper (deskew/clean, optional)",
        "pacman: sudo pacman -S unpaper / apt: sudo apt install unpaper",
    ),
    (
        "notify-send",
        "libnotify (desktop notifications, optional)",
        "pacman: sudo pacman -S libnotify / apt: sudo apt install libnotify",
    ),
];

fn hint_for(bin: &str) -> Option<&'static str> {
    HINTS.iter().find(|(n, _, _)| *n == bin).map(|(_, _, h)| *h)
}

/// Run all checks. `check_scanner` and tesseract langs are the slow parts
/// (scanimage -L, tesseract --list-langs); everything else is a PATH lookup.
pub async fn run_checks(cfg: &Config) -> Report {
    let mut items = Vec::new();

    // --- required/optional binaries
    for (bin, what, _) in HINTS {
        let found = crate::backend::which(bin).is_some();
        let item = match *bin {
            "unpaper" => {
                if !cfg.unpaper {
                    CheckItem {
                        what: format!("{bin} ({what})"),
                        status: Status::Skip,
                        detail: "disabled in config".into(),
                        hint: None,
                    }
                } else if found {
                    CheckItem {
                        what: format!("{bin} ({what})"),
                        status: Status::Ok,
                        detail: String::new(),
                        hint: None,
                    }
                } else {
                    CheckItem {
                        what: format!("{bin} ({what})"),
                        status: Status::Fail,
                        detail: String::new(),
                        hint: hint_for(bin).map(str::to_string),
                    }
                }
            }
            "notify-send" => CheckItem {
                what: format!("{bin} ({what})"),
                status: if found { Status::Ok } else { Status::Warn },
                detail: if found {
                    String::new()
                } else {
                    "notifications disabled".into()
                },
                hint: if found {
                    None
                } else {
                    hint_for(bin).map(str::to_string)
                },
            },
            _ => CheckItem {
                what: format!("{bin} ({what})"),
                status: if found { Status::Ok } else { Status::Fail },
                detail: String::new(),
                hint: if found {
                    None
                } else {
                    hint_for(bin).map(str::to_string)
                },
            },
        };
        items.push(item);
    }

    // --- tesseract language data (only if the binary exists; parity fix)
    let tesseract_ok = crate::backend::which("tesseract").is_some();
    if tesseract_ok {
        let have = match tokio::time::timeout(Duration::from_secs(20), available_langs()).await {
            Ok(list) => list.unwrap_or_default(),
            Err(_) => Vec::new(),
        };
        for lang in cfg.langs.split('+').filter(|l| !l.is_empty()) {
            let ok = have.iter().any(|l| l == lang);
            // Script models (e.g. Latin) ship outside distro language packs;
            // they are single traineddata files downloaded from tessdata repos.
            let script = lang.chars().next().is_some_and(|c| c.is_ascii_uppercase());
            items.push(CheckItem {
                what: format!("tesseract language '{lang}'"),
                status: if ok { Status::Ok } else { Status::Fail },
                detail: String::new(),
                hint: (!ok).then(|| {
                    if script {
                        format!(
                            "download a script model (no distro package exists), e.g.:\n  curl -sL https://github.com/tesseract-ocr/tessdata_fast/raw/main/script/{lang}.traineddata | sudo tee /usr/share/tessdata/{lang}.traineddata >/dev/null"
                        )
                    } else {
                        format!(
                            "pacman: sudo pacman -S tesseract-data-{lang} / apt: sudo apt install tesseract-ocr-{lang}"
                        )
                    }
                }),
            });
        }
    }

    // --- scanner detection (single scanimage -L call)
    let devices = scan::list_devices().await.unwrap_or_default();
    let device = scan::select_device(&devices, &cfg.device).cloned();
    items.push(match &device {
        Some(d) => CheckItem {
            what: "scanner".into(),
            status: Status::Ok,
            detail: d.name.clone(),
            hint: None,
        },
        None => CheckItem {
            what: "scanner".into(),
            status: Status::Fail,
            detail: "no device found (scanimage -L)".into(),
            hint: Some(
                "is the scanner plugged in and powered on?\n\
                 HP devices: run `sudo hp-setup -i` once, then `scanimage -L`\n\
                 test manually: scanimage -L"
                    .into(),
            ),
        },
    });

    // --- output directory (create-if-missing, parity)
    let out = &cfg.output;
    let out_status = ensure_output_dir(out);
    items.push(CheckItem {
        what: "output directory".into(),
        status: out_status.0,
        detail: format!("{}{}", out.display(), out_status.1),
        hint: None,
    });

    Report { items, device }
}

async fn available_langs() -> anyhow::Result<Vec<String>> {
    scan::available_langs().await
}

fn ensure_output_dir(dir: &std::path::Path) -> (Status, String) {
    match std::fs::create_dir_all(dir) {
        Ok(()) => {
            let writable = dir_writable(dir);
            if writable {
                (Status::Ok, String::new())
            } else {
                (Status::Fail, " (not writable)".into())
            }
        }
        Err(e) => (Status::Fail, format!(" ({e})")),
    }
}

fn dir_writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(format!(".auto-scanner-ocr-write-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Preflight for the TUI: subset of checks that must pass before scanning.
/// Returns (report, device); device is None when no scanner was found.
pub async fn preflight(cfg: &Config) -> (Report, Option<Device>) {
    let report = run_checks(cfg).await;
    let device = report.device.clone();
    (report, device)
}

/// Headless `--doctor` output (parity formatting).
pub fn print_doctor(cfg: &Config, report: &Report) {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {} - dependency and environment check",
        crate::config::PROGRAM,
        crate::config::VERSION
    );
    let _ = writeln!(out, "\nRust: {}", rustc_version());
    let _ = writeln!(out, "\nTools:");
    for i in &report.items {
        match i.status {
            Status::Ok | Status::Warn | Status::Skip => {
                let _ = writeln!(out, "  [{}] {} {}", i.status, i.what, i.detail);
            }
            Status::Fail => {
                let _ = writeln!(out, "  [{}] {}", i.status, i.what);
                if let Some(h) = &i.hint {
                    for line in h.lines() {
                        let _ = writeln!(out, "    install: {line}");
                    }
                }
            }
        }
    }
    if let Some(d) = &report.device {
        let _ = writeln!(out, "\nScanner: {}", d.name);
    }
    if !cfg.output.as_os_str().is_empty() {
        let _ = writeln!(out, "Output: {}", cfg.output.display());
    }
    println!("{out}");
}

fn rustc_version() -> String {
    // Compile-time version of the toolchain that built this binary.
    option_env!("RUSTC_VERSION")
        .unwrap_or(env!("CARGO_PKG_RUST_VERSION"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_display() {
        assert_eq!(Status::Ok.to_string(), " OK");
        assert_eq!(Status::Warn.to_string(), "WARN");
        assert_eq!(Status::Fail.to_string(), "FAIL");
        assert_eq!(Status::Skip.to_string(), "SKIP");
    }

    #[test]
    fn report_error_classification() {
        let mut r = Report::default();
        r.items.push(CheckItem {
            what: "a".into(),
            status: Status::Ok,
            detail: String::new(),
            hint: None,
        });
        r.items.push(CheckItem {
            what: "b".into(),
            status: Status::Warn,
            detail: String::new(),
            hint: None,
        });
        r.items.push(CheckItem {
            what: "c".into(),
            status: Status::Fail,
            detail: String::new(),
            hint: None,
        });
        assert_eq!(r.errors().len(), 1);
        assert_eq!(r.warnings().len(), 1);
        assert!(!r.ok());
    }
}
