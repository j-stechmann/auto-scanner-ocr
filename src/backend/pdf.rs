//! Page cleanup (unpaper), rotation, and searchable-PDF assembly
//! (img2pdf -> ocrmypdf). Parity with the Python build_pdf/maybe_unpaper.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::backend::process;

/// unpaper timeout (parity: 120s).
const UNPAPER_TIMEOUT: Duration = Duration::from_secs(120);
/// img2pdf timeout (parity: 300s).
const IMG2PDF_TIMEOUT: Duration = Duration::from_secs(300);
/// ocrmypdf timeout (parity: 1800s).
const OCRYPDF_TIMEOUT: Duration = Duration::from_secs(1800);

/// Rotate a PNG 90 degrees clockwise using the image crate (pure Rust, no
/// external tool). Works before unpaper so deskew sees an upright page.
pub async fn rotate_png(path: &Path, cw: bool) -> Result<()> {
    let bytes = tokio::fs::read(path).await?;
    let img = tokio::task::spawn_blocking(move || -> Result<image::DynamicImage> {
        let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
        let img = reader.decode()?;
        Ok(if cw { img.rotate90() } else { img.rotate270() })
    })
    .await??;

    let part = path.with_extension("png.part");
    let encoded = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png)?;
        Ok(out.into_inner())
    })
    .await??;
    tokio::fs::write(&part, encoded).await?;
    tokio::fs::rename(&part, path).await?;
    Ok(())
}

/// Deskew/clean with unpaper. Returns the path to use downstream:
/// on success the `_clean.png` file (raw removed, parity), on failure the
/// original. Skipped for color mode (unpaper is grayscale-only) or when
/// disabled / binary missing.
pub async fn maybe_unpaper(page_png: &Path, enabled: bool, mode: &str) -> (PathBuf, bool) {
    if !enabled || mode == "color" {
        return (page_png.to_path_buf(), false);
    }
    if crate::backend::which("unpaper").is_none() {
        return (page_png.to_path_buf(), false);
    }
    let cleaned = clean_variant(page_png);
    let src = page_png.to_string_lossy().into_owned();
    let dst = cleaned.to_string_lossy().into_owned();
    let cmd = [
        "unpaper",
        "--layout",
        "single",
        "--deskew-scan-direction",
        "left,right",
        &src,
        &dst,
    ];
    match process::run(&cmd, Some(UNPAPER_TIMEOUT)).await {
        Ok(out) if out.success && tokio::fs::try_exists(&cleaned).await.unwrap_or(false) => {
            let _ = tokio::fs::remove_file(page_png).await;
            (cleaned, true)
        }
        Ok(_) => {
            tracing::warn!("unpaper failed; using raw scan");
            let _ = tokio::fs::remove_file(&cleaned).await;
            (page_png.to_path_buf(), false)
        }
        Err(e) => {
            tracing::warn!("unpaper failed ({e}); using raw scan");
            let _ = tokio::fs::remove_file(&cleaned).await;
            (page_png.to_path_buf(), false)
        }
    }
}

/// Mirror of the Python `_clean.png` naming.
fn clean_variant(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    match (stem, ext) {
        (Some(stem), Some(ext)) => path.with_file_name(format!("{stem}_clean.{ext}")),
        (Some(stem), None) => path.with_file_name(format!("{stem}_clean")),
        _ => path.to_path_buf(),
    }
}

/// What the finish step needs to know about the session before building.
#[derive(Debug, Clone)]
pub struct BuildPlan {
    /// Ordered page image paths (already unpapered/rotated).
    pub images: Vec<PathBuf>,
    /// True when unpaper actually ran for at least one page.
    pub unpaper_ran: bool,
    /// True when any page was manually rotated: ocrmypdf --rotate-pages is
    /// then omitted entirely (it cannot be exempted per page).
    pub manually_rotated: bool,
    /// True when any page fell back to raw (unpaper failed): those pages get
    /// deskew/clean from ocrmypdf.
    pub any_raw_page: bool,
    pub dpi: u16,
    pub langs: String,
    pub out_pdf: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildOutcome {
    /// Searchable PDF written.
    Searchable,
    /// PDF written but without text layer (ocrmypdf failed; raw copy saved).
    WithoutTextLayer,
}

/// Build the final PDF: img2pdf (lossless) -> ocrmypdf (text layer).
/// Returns the outcome; errors from img2pdf are fatal, ocrmypdf failure is
/// NOT (parity: raw PDF is copied and a distinct warning is surfaced).
pub async fn build_pdf(plan: &BuildPlan) -> Result<BuildOutcome> {
    let tmp = tempfile::TempDir::new()?;
    let raw_pdf = tmp.path().join("raw.pdf");

    let img2pdf_args: Vec<String> = std::iter::once("img2pdf".to_string())
        .chain(["--imgsize".to_string(), format!("{}dpi", plan.dpi)])
        .chain(plan.images.iter().map(|p| p.to_string_lossy().into_owned()))
        .chain(["-o".to_string(), raw_pdf.to_string_lossy().into_owned()])
        .collect();
    let refs: Vec<&str> = img2pdf_args.iter().map(String::as_str).collect();
    let out = process::run(&refs, Some(IMG2PDF_TIMEOUT))
        .await
        .map_err(|e| process::fail_with_log_err("PDF assembly (img2pdf)", &refs, e))?;
    if !out.success {
        return Err(process::fail_with_log("PDF assembly (img2pdf)", &out));
    }

    // Let ocrmypdf deskew/clean only when unpaper didn't actually run for
    // those pages (outcome-based, improvement over config-based parity).
    let mut ocr_args: Vec<String> = vec![
        "ocrmypdf".into(),
        "--language".into(),
        plan.langs.clone(),
        "--output-type".into(),
        "pdfa".into(),
        "--optimize".into(),
        "0".into(),
    ];
    if !plan.manually_rotated {
        ocr_args.push("--rotate-pages".into());
        ocr_args.push("--rotate-pages-threshold".into());
        ocr_args.push("10".into());
    }
    if !plan.unpaper_ran || plan.any_raw_page {
        ocr_args.push("--deskew".into());
        ocr_args.push("--clean".into());
    }
    ocr_args.push(raw_pdf.to_string_lossy().into_owned());
    ocr_args.push(plan.out_pdf.to_string_lossy().into_owned());

    let refs: Vec<&str> = ocr_args.iter().map(String::as_str).collect();
    let out = process::run(&refs, Some(OCRYPDF_TIMEOUT))
        .await
        .map_err(|e| process::fail_with_log_err("OCR (ocrmypdf)", &refs, e))?;
    if out.success {
        return Ok(BuildOutcome::Searchable);
    }

    // Non-fatal fallback (parity): save the raw PDF without a text layer.
    tracing::error!("ocrmypdf failed; saving PDF without text layer");
    tokio::fs::copy(&raw_pdf, &plan.out_pdf).await?;
    Ok(BuildOutcome::WithoutTextLayer)
}

/// Timestamped unique output path: `YYYY-MM-DD_HHMMSS.pdf` with `_2`, `_3`...
/// suffix on collision (parity with unique_path; resolved once at session
/// creation).
pub fn unique_path(dir: &Path, stamp: String) -> PathBuf {
    let base = dir.join(format!("{stamp}.pdf"));
    if !base.exists() {
        return base;
    }
    let mut n = 2;
    loop {
        let cand = dir.join(format!("{stamp}_{n}.pdf"));
        if !cand.exists() {
            return cand;
        }
        n += 1;
    }
}

/// Local timestamp string `YYYY-MM-DD_HHMMSS` (parity with Python format).
pub fn stamp_now() -> String {
    let now = chrono::Local::now();
    now.format("%Y-%m-%d_%H%M%S").to_string()
}

/// Report file size in KB (parity: `stat().st_size // 1024`).
pub fn size_kb(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len() / 1024).unwrap_or(0)
}

/// Check whether ocrmypdf output indicates progress lines like
/// "Processing page 3" (used for the build progress display).
pub fn parse_ocrmypdf_progress(stderr: &str) -> Option<(u32, u32)> {
    // We don't stream ocrmypdf stderr in v1; kept for future use.
    let re = regex::Regex::new(r"Processing page (\d+)").ok()?;
    let last = re.captures_iter(stderr).last()?;
    Some((last[1].parse().ok()?, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_path_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let stamp = "2026-09-02_143005".to_string();
        let p1 = unique_path(d, stamp.clone());
        assert_eq!(p1, d.join("2026-09-02_143005.pdf"));
        std::fs::write(&p1, b"x").unwrap();
        let p2 = unique_path(d, stamp.clone());
        assert_eq!(p2, d.join("2026-09-02_143005_2.pdf"));
        std::fs::write(&p2, b"x").unwrap();
        let p3 = unique_path(d, stamp);
        assert_eq!(p3, d.join("2026-09-02_143005_3.pdf"));
    }

    #[test]
    fn stamp_format() {
        let s = stamp_now();
        assert_eq!(s.len(), 17);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "_");
        assert!(s
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == '_'));
    }

    #[test]
    fn parse_progress() {
        let s = "Processing page 1\nProcessing page 2\nProcessing page 3";
        assert_eq!(parse_ocrmypdf_progress(s), Some((3, 0)));
        assert_eq!(parse_ocrmypdf_progress("nothing"), None);
    }
}
