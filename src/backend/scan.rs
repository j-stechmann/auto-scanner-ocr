//! Scanner access via SANE (`scanimage`).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::backend::process::{self, Output, RunError};

/// Timeout for `scanimage -L` device listing (parity: 30s).
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Timeout for tesseract --list-langs (parity: 15s).
pub const LIST_LANGS_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Full SANE device name, e.g. `hpaio:/usb/Deskjet_1050_J410?serial=CN...`
    pub name: String,
    /// Human-readable part, e.g. `HP Deskjet 1050 J410`
    pub label: String,
}

/// Map config mode -> scanimage --mode value (parity with SCAN_MODES).
pub fn scanimage_mode(mode: &str) -> Option<&'static str> {
    match mode {
        "gray" => Some("Gray"),
        "color" => Some("Color"),
        "lineart" => Some("Lineart"),
        _ => None,
    }
}

/// Parse `scanimage -L` output for `device `name' is <label>` lines
/// (parity regex: device `([^']+)').
pub fn parse_devices(out: &str) -> Vec<Device> {
    let re = Regex::new(r"device `([^']+)' (.*)").expect("static regex");
    out.lines()
        .filter_map(|line| {
            let caps = re.captures(line.trim())?;
            let name = caps.get(1)?.as_str().to_string();
            let rest = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            // Typical: "is a HP Deskjet 1050 J410 flatbed scanner"
            let label = rest
                .trim_start_matches("is a")
                .trim_start_matches("is an")
                .trim_start_matches("is")
                .trim()
                .trim_end_matches("flatbed scanner")
                .trim()
                .to_string();
            Some(Device { name, label })
        })
        .collect()
}

/// List SANE devices. Returns parsed devices; empty list means none found.
/// Parity: rc != 0 with devices present still succeeds.
pub async fn list_devices() -> Result<Vec<Device>, RunError> {
    let output = process::run(&["scanimage", "-L"], Some(LIST_TIMEOUT)).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let devices = parse_devices(&text);
    if !output.success && devices.is_empty() {
        return Err(RunError::Failed(-1));
    }
    Ok(devices)
}

/// Select the device per config: "auto" -> first found; otherwise substring
/// match. Parity: no fallback to first device on failed substring match.
pub fn select_device<'a>(devices: &'a [Device], wanted: &str) -> Option<&'a Device> {
    if wanted == "auto" {
        return devices.first();
    }
    devices.iter().find(|d| d.name.contains(wanted))
}

/// Per-page scan result details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanOutcome {
    /// True when the scanner rejected the requested settings and a fallback
    /// attempt (dropping --mode and/or --resolution) succeeded instead; the
    /// actual image scale may then differ from the requested dpi.
    pub used_fallback: bool,
}

/// Capture one page from the scanner into `out_path` (PNG).
///
/// Retry ladder (parity with Python scan_page, one strict improvement):
/// 1. full: -d dev --format=png --resolution=N --mode=M
/// 2. drop --mode
/// 3. bare: -d dev --format=png  (Python dropped --format here too, producing
///    PNM bytes in a .png file; we keep PNG in every attempt)
///
/// Success requires rc 0 AND non-empty stdout (parity).
/// Cancellation kills scanimage (process group) between or during attempts.
/// Returns which attempt level succeeded so callers can warn about silent
/// fallbacks (they change page dimensions and OCR scale).
pub async fn scan_page(
    device: &str,
    dpi: u16,
    mode: &str,
    out_path: &Path,
    token: &CancellationToken,
) -> Result<ScanOutcome> {
    let base: Vec<String> = vec![
        "scanimage".into(),
        "-d".into(),
        device.to_string(),
        "--format=png".into(),
    ];
    let resolution = format!("--resolution={dpi}");
    let si_mode = scanimage_mode(mode).unwrap_or("Gray");

    // Attempt 1: full; attempt 2: drop --mode; attempt 3: bare (keep PNG format).
    let attempts: Vec<Vec<String>> = vec![
        {
            let mut v = base.clone();
            v.push(resolution.clone());
            v.push(format!("--mode={si_mode}"));
            v
        },
        {
            let mut v = base.clone();
            v.push(resolution);
            v
        },
        base,
    ];

    let mut last_output: Option<Output> = None;
    let mut last_err: Option<RunError> = None;

    for (attempt_idx, attempt) in attempts.iter().enumerate() {
        if token.is_cancelled() {
            return Err(process::RunError::Cancelled.into());
        }
        let refs: Vec<&str> = attempt.iter().map(String::as_str).collect();
        match process::run_cancellable(&refs, None, token).await {
            Ok(output) => {
                if output.success && !output.stdout.is_empty() {
                    // Write via .part then rename (never leave partial files).
                    let part = out_path.with_extension("png.part");
                    tokio::fs::write(&part, &output.stdout)
                        .await
                        .with_context(|| format!("writing {}", part.display()))?;
                    tokio::fs::rename(&part, out_path)
                        .await
                        .with_context(|| format!("renaming into {}", out_path.display()))?;
                    if attempt_idx > 0 {
                        tracing::warn!(
                            "scan attempt {}/{} succeeded; requested settings were rejected",
                            attempt_idx + 1,
                            attempts.len()
                        );
                    }
                    return Ok(ScanOutcome {
                        used_fallback: attempt_idx > 0,
                    });
                }
                last_output = Some(output);
            }
            Err(e) => {
                if matches!(e, RunError::Cancelled) {
                    return Err(e.into());
                }
                last_err = Some(e);
            }
        }
    }

    match (last_output, last_err) {
        (Some(out), _) => Err(process::fail_with_log("Scanning", &out)),
        (None, Some(err)) => Err(process::fail_with_log_err("Scanning", &["scanimage"], err)),
        (None, None) => unreachable!("attempts is non-empty"),
    }
}

/// Extract text from a page image with tesseract (for the TUI text pane).
/// Uses the same `langs` string as the final OCR pass.
pub async fn ocr_text(image: &Path, langs: &str, workdir: &Path) -> Result<String> {
    let base = workdir.join(format!(
        "tess_{}_{}",
        std::process::id(),
        monotonically_naming_counter()
    ));
    let cmd = [
        "tesseract",
        image.to_str().expect("utf8 path"),
        &base.to_string_lossy(),
        "-l",
        langs,
        "txt",
    ];
    process::run_ok(&cmd, None).await?;
    let txt_path = base.with_extension("txt");
    let text = tokio::fs::read_to_string(&txt_path)
        .await
        .unwrap_or_default();
    let _ = tokio::fs::remove_file(&txt_path).await;
    Ok(text)
}

/// Like [`ocr_text`], but abortable: killing tesseract leaves no output file
/// and the function reports "cancelled" (checked by callers via the message).
pub async fn ocr_text_cancellable(
    image: &Path,
    langs: &str,
    workdir: &Path,
    token: &CancellationToken,
) -> Result<String> {
    let base = workdir.join(format!(
        "tess_{}_{}",
        std::process::id(),
        monotonically_naming_counter()
    ));
    let cmd = [
        "tesseract",
        image.to_str().expect("utf8 path"),
        &base.to_string_lossy(),
        "-l",
        langs,
        "txt",
    ];
    let out = process::run_cancellable(&cmd, None, token).await?;
    if !out.success {
        anyhow::bail!("tesseract failed: {}", out.stderr_tail(3));
    }
    if token.is_cancelled() {
        anyhow::bail!("cancelled");
    }
    let txt_path = base.with_extension("txt");
    let text = tokio::fs::read_to_string(&txt_path)
        .await
        .unwrap_or_default();
    let _ = tokio::fs::remove_file(&txt_path).await;
    Ok(text)
}

fn monotonically_naming_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// List installed tesseract languages (parity: header line skipped
/// case-insensitively). Used by the langs picker and diagnostics.
pub async fn available_langs() -> Result<Vec<String>> {
    let out = process::run_ok(&["tesseract", "--list-langs"], Some(LIST_LANGS_TIMEOUT)).await?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("list of available languages"))
        .map(String::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scanimage_l_output() {
        let sample = r#"
device `hpaio:/usb/Deskjet_1050_J410?serial=CN1' is a Hewlett-Packard Deskjet 1050 J410 flatbed scanner
device `escl:http://127.0.0.1:60000' is a HP Scanjet eSCL backend scanner
"#;
        let devices = parse_devices(sample);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].name, "hpaio:/usb/Deskjet_1050_J410?serial=CN1");
        assert!(devices[0].label.contains("Deskjet"));
    }

    #[test]
    fn parses_empty_output() {
        assert!(parse_devices("No scanners were identified.").is_empty());
    }

    #[test]
    fn selects_device_auto_and_substring() {
        let devices = vec![
            Device {
                name: "hpaio:/usb/A".into(),
                label: "A".into(),
            },
            Device {
                name: "escl:http://B".into(),
                label: "B".into(),
            },
        ];
        assert_eq!(
            select_device(&devices, "auto").unwrap().name,
            "hpaio:/usb/A"
        );
        assert_eq!(
            select_device(&devices, "escl").unwrap().name,
            "escl:http://B"
        );
        assert!(select_device(&devices, "missing").is_none());
    }

    #[test]
    fn scanimage_mode_mapping() {
        assert_eq!(scanimage_mode("gray"), Some("Gray"));
        assert_eq!(scanimage_mode("color"), Some("Color"));
        assert_eq!(scanimage_mode("lineart"), Some("Lineart"));
        assert_eq!(scanimage_mode("nope"), None);
    }
}
