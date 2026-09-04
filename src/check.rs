//! Dependency and environment checks (parity with the Python --doctor/preflight).

use std::fmt;
use std::time::Duration;

use crate::backend::scan::{self};
pub use crate::backend::scan::Device;
use crate::config::Config;

/// Timeout for notify-send / unpaper presence checks is not needed (PATH only).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skip,
    /// Scanner detection still running (background preflight only). Not a
    /// failure: `errors()` intentionally does not count it.
    Pending,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Ok => write!(f, " OK"),
            Status::Warn => write!(f, "WARN"),
            Status::Fail => write!(f, "FAIL"),
            Status::Skip => write!(f, "SKIP"),
            Status::Pending => write!(f, " ..."),
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
    /// Detail shown while a check is still Pending (e.g. "detecting...").
    pub pending_detail: Option<String>,
}

/// Where a report came from. The TUI branches on this: only a manual
/// re-run clears `checks_in_flight`, and only the startup final report
/// may set the exit-code flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReportSource {
    /// TUI startup: fast preflight half (scanner item still Pending).
    #[default]
    StartupFast,
    /// TUI startup: final preflight report (scanner question answered).
    StartupFinal,
    /// Manual diagnostics re-run from the overlay.
    ReRun,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub items: Vec<CheckItem>,
    pub device: Option<Device>,
    pub source: ReportSource,
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

    /// True when no item is still being detected (background fast report).
    pub fn settled(&self) -> bool {
        !self.items.iter().any(|i| i.status == Status::Pending)
    }

    pub fn ok(&self) -> bool {
        self.errors().is_empty()
    }
}

fn item(what: impl Into<String>, status: Status) -> CheckItem {
    CheckItem {
        what: what.into(),
        status,
        detail: String::new(),
        hint: None,
        pending_detail: None,
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
        "pdfunite",
        "pdfunite (merges pages of mixed DPI, optional)",
        "pacman: sudo pacman -S poppler / apt: sudo apt install poppler-utils",
    ),
    (
        "unpaper",
        "unpaper (legacy cleanup; optional)",
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

/// Run all checks. Scanner detection and tesseract langs are the slow parts
/// (scanimage -L, tesseract --list-langs); everything else is a PATH lookup.
/// Fully blocking (doctor + manual re-runs); the TUI startup uses
/// [`run_checks_fast`] + [`run_checks_slow`] instead so the UI can paint
/// immediately. The report carries the resolved device (None when no
/// scanner matched): manual re-runs rely on it to re-deliver the device
/// to the actor.
pub async fn run_checks(cfg: &Config) -> Report {
    let mut items = Vec::new();

    // --- required/optional binaries
    push_bin_checks(cfg, &mut items);

    // --- tesseract language data (only if the binary exists; parity fix)
    items.extend(lang_checks(cfg).await);

    // --- scanner detection (single scanimage -L call)
    let (dev_items, device) = detect_device(cfg).await;
    items.extend(dev_items);

    // --- output directory (create-if-missing, parity)
    push_output_check(cfg, &mut items);

    Report {
        items,
        device,
        source: ReportSource::ReRun,
    }
}

/// Fast half of the checks: PATH lookups + output directory only (all
/// millisecond-scale). Returns the report without a device; the scanner item
/// is emitted as Pending so the diagnostics overlay can show it.
pub fn run_checks_fast(cfg: &Config) -> Report {
    let mut items = Vec::new();
    push_bin_checks(cfg, &mut items);
    push_output_check(cfg, &mut items);
    items.push(CheckItem {
        pending_detail: Some("detecting...".into()),
        ..item("scanner", Status::Pending)
    });
    Report {
        items,
        device: None,
        source: ReportSource::StartupFast,
    }
}

/// Slow half: tesseract langs + scanner detection, run concurrently.
/// Runs as the background preflight task; `run_checks` = fast + this.
pub async fn run_checks_slow(cfg: &Config) -> Report {
    let mut items = Vec::new();
    // Langs and scanner detection run concurrently: both spawn external
    // processes, neither depends on the other, and either can be slow.
    let (langs_res, dev_res) = tokio::join!(lang_checks(cfg), detect_device(cfg));
    items.extend(langs_res);
    items.extend(dev_res.0);
    Report {
        items,
        device: dev_res.1,
        source: ReportSource::StartupFinal,
    }
}

/// Startup final report = the fast half's items (PATH lookups, output dir)
/// merged in front of the slow half's (langs + resolved scanner), with the
/// fast report's pending scanner placeholder dropped in favor of the
/// resolved one. The merge matters: without it the final report would
/// consist of the slow half only, so fast-half failures (missing binary,
/// unwritable output dir) would vanish from the stored report, the
/// diagnostics overlay and the exit verdict once detection completes.
pub fn merge_fast_slow(fast: &Report, slow: Report) -> Report {
    let mut items: Vec<CheckItem> = fast
        .items
        .iter()
        .filter(|i| i.status != Status::Pending)
        .cloned()
        .collect();
    items.extend(slow.items);
    Report {
        items,
        device: slow.device,
        source: slow.source,
    }
}

fn push_bin_checks(cfg: &Config, items: &mut Vec<CheckItem>) {
    for (bin, what, _) in HINTS {
        let found = crate::backend::which(bin).is_some();
        let it = match *bin {
            "unpaper" => {
                // Tiered by cleanup mode: off needs no unpaper (ocrmypdf
                // cleans at finish); conservative treats it as an optional
                // passthrough; legacy wants it (its absence degrades to
                // ocrmypdf cleanup, never a hard failure).
                let status = match cfg.cleanup {
                    crate::config::Cleanup::Off => Status::Skip,
                    _ if found => Status::Ok,
                    _ => Status::Warn,
                };
                let detail = match (cfg.cleanup, found) {
                    (crate::config::Cleanup::Off, _) => {
                        format!("cleanup = {}", cfg.cleanup.as_str())
                    }
                    (_, true) => String::new(),
                    (crate::config::Cleanup::Conservative, false) => {
                        "not installed; ocrmypdf deskews/cleans at finish".into()
                    }
                    (crate::config::Cleanup::Legacy, false) => {
                        "not installed; pages fall back to ocrmypdf cleanup".into()
                    }
                };
                CheckItem {
                    what: format!("{bin} ({what})"),
                    status,
                    detail,
                    hint: (status == Status::Warn)
                        .then(|| hint_for(bin).map(str::to_string))
                        .flatten(),
                    pending_detail: None,
                }
            }
            "pdfunite" => {
                // Only needed when a session mixes DPIs; a warning is enough.
                CheckItem {
                    what: format!("{bin} ({what})"),
                    status: if found { Status::Ok } else { Status::Warn },
                    detail: if found {
                        String::new()
                    } else {
                        "mixed-DPI sessions will fail to build".into()
                    },
                    hint: if found {
                        None
                    } else {
                        hint_for(bin).map(str::to_string)
                    },
                    pending_detail: None,
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
                pending_detail: None,
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
                pending_detail: None,
            },
        };
        items.push(it);
    }
}

async fn lang_checks(cfg: &Config) -> Vec<CheckItem> {
    let mut out = Vec::new();
    let tesseract_ok = crate::backend::which("tesseract").is_some();
    if !tesseract_ok {
        return out;
    }
    let have = match tokio::time::timeout(Duration::from_secs(20), available_langs()).await {
        Ok(list) => list.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    for lang in cfg.langs.split('+').filter(|l| !l.is_empty()) {
        let ok = have.iter().any(|l| l == lang);
        out.push(CheckItem {
            what: format!("tesseract language '{lang}'"),
            status: if ok { Status::Ok } else { Status::Fail },
            detail: String::new(),
            hint: (!ok).then(|| {
                if let Some(pkg) = crate::config::script_lang_package(lang) {
                    // Script models are single traineddata files; some
                    // distros ship them, others need a manual download.
                    format!(
                        "apt: sudo apt install {pkg}\n  \
                         arch/other (tessdata dir may differ; see `tesseract --list-langs -v`):\n  \
                         curl -fsSL https://github.com/tesseract-ocr/tessdata_fast/raw/main/script/{lang}.traineddata | sudo tee /usr/share/tessdata/{lang}.traineddata >/dev/null"
                    )
                } else {
                    format!(
                        "pacman: sudo pacman -S tesseract-data-{lang} / apt: sudo apt install tesseract-ocr-{lang}"
                    )
                }
            }),
            pending_detail: None,
        });
    }
    out
}

async fn detect_device(cfg: &Config) -> (Vec<CheckItem>, Option<Device>) {
    let devices = scan::list_devices().await.unwrap_or_default();
    let device = scan::select_device(&devices, &cfg.device).cloned();
    let check = match &device {
        Some(d) => CheckItem {
            what: "scanner".into(),
            status: Status::Ok,
            detail: d.name.clone(),
            hint: None,
            pending_detail: None,
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
            pending_detail: None,
        },
    };
    (vec![check], device)
}

fn push_output_check(cfg: &Config, items: &mut Vec<CheckItem>) {
    // --- output directory (create-if-missing, parity)
    let out = &cfg.output;
    let out_status = ensure_output_dir(out);
    items.push(CheckItem {
        what: "output directory".into(),
        status: out_status.0,
        detail: format!("{}{}", out.display(), out_status.1),
        hint: None,
        pending_detail: None,
    });
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

/// Human-readable label for a device (parity with the old main.rs logic:
/// label if set, else the SANE name, else "no scanner").
pub fn device_label(d: Option<&Device>) -> String {
    match d {
        Some(d) if !d.label.is_empty() => d.label.clone(),
        Some(d) => d.name.clone(),
        None => "no scanner".to_string(),
    }
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
            Status::Ok | Status::Warn | Status::Skip | Status::Pending => {
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
        assert_eq!(Status::Pending.to_string(), " ...");
    }

    #[test]
    fn pending_is_not_an_error_and_reports_settle() {
        let mut r = Report::default();
        r.items.push(item("scanner", Status::Pending));
        assert!(r.ok(), "Pending must not fail the report");
        assert!(!r.settled(), "Pending means still detecting");
        r.items.push(item("bin", Status::Fail));
        assert!(!r.ok());
        r.items[0].status = Status::Ok;
        assert!(r.settled());
    }

    #[test]
    fn report_error_classification() {
        let mut r = Report::default();
        r.items.push(CheckItem {
            what: "a".into(),
            status: Status::Ok,
            detail: String::new(),
            hint: None,
            pending_detail: None,
        });
        r.items.push(CheckItem {
            what: "b".into(),
            status: Status::Warn,
            detail: String::new(),
            hint: None,
            pending_detail: None,
        });
        r.items.push(CheckItem {
            what: "c".into(),
            status: Status::Fail,
            detail: String::new(),
            hint: None,
            pending_detail: None,
        });
        assert_eq!(r.errors().len(), 1);
        assert_eq!(r.warnings().len(), 1);
        assert!(!r.ok());
    }

    #[test]
    fn merge_fast_slow_drops_placeholder_keeps_failures() {
        // Fast half: a failed PATH check + the Pending scanner placeholder.
        let fast = Report {
            items: vec![
                CheckItem {
                    what: "ocrmypdf (OCRmyPDF (searchable PDFs))".into(),
                    status: Status::Fail,
                    detail: String::new(),
                    hint: None,
                    pending_detail: None,
                },
                item("scanner", Status::Pending),
            ],
            device: None,
            source: ReportSource::StartupFast,
        };
        // Slow half: the resolved scanner row replaces the placeholder.
        let slow = Report {
            items: vec![CheckItem {
                what: "scanner".into(),
                status: Status::Ok,
                detail: "hpaio:/usb/x".into(),
                hint: None,
                pending_detail: None,
            }],
            device: Some(Device {
                name: "hpaio:/usb/x".into(),
                label: String::new(),
            }),
            source: ReportSource::StartupFinal,
        };

        let merged = merge_fast_slow(&fast, slow);
        assert_eq!(merged.items.len(), 2, "placeholder dropped, fail kept");
        assert_eq!(merged.items[0].status, Status::Fail, "fast fail survives");
        assert_eq!(merged.items[1].what, "scanner");
        assert!(merged.device.is_some());
        assert_eq!(merged.source, ReportSource::StartupFinal);
        // The verdict-relevant property: fast-half failures make !ok().
        assert!(!merged.ok());
    }
}
