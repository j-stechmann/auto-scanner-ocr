//! Page cleanup (unpaper), rotation, and searchable-PDF assembly
//! (img2pdf -> ocrmypdf).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::backend::process;
use crate::config::Cleanup;

/// unpaper timeout (parity: 120s).
const UNPAPER_TIMEOUT: Duration = Duration::from_secs(120);
/// img2pdf timeout (parity: 300s).
const IMG2PDF_TIMEOUT: Duration = Duration::from_secs(300);
/// ocrmypdf timeout (parity: 1800s).
const OCRYPDF_TIMEOUT: Duration = Duration::from_secs(1800);
/// pdfunite timeout (merge of per-page PDFs; fast even for large sessions).
const PDFUNITE_TIMEOUT: Duration = Duration::from_secs(300);

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

/// Build the unpaper argv for a cleanup mode (pure; unit-tested).
/// File arguments (`src`, `dst`) are NOT included; callers append them last
/// (unpaper's CLI is `unpaper [options] <in> <out>`).
pub fn unpaper_args(cleanup: Cleanup, extra_args: &[String]) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--layout".into(),
        "single".into(),
        "--deskew-scan-direction".into(),
        "left,right".into(),
    ];
    match cleanup {
        Cleanup::Legacy => {}
        Cleanup::Conservative => {
            // Disable every content-altering stage: measured pixel-identical
            // passthrough on flatbed scans (deskew never fires — it
            // expects the dark book-edge surroundings it doesn't find here).
            // Kept as a hook for unpaper_extra_args experiments.
            for flag in [
                "--no-mask-scan",
                "--no-border-scan",
                "--no-border-align",
                "--no-blackfilter",
                "--no-grayfilter",
                "--no-blurfilter",
                "--no-noisefilter",
                "--no-deskew",
            ] {
                args.push(flag.into());
            }
        }
        Cleanup::Off => {}
    }
    args.extend(extra_args.iter().cloned());
    args
}

/// Re-encode a Netpbm/unpaper output file to a real PNG at `path` (unpaper
/// always writes PGM/PPM data regardless of file extension). On any failure
/// the original file is left untouched.
async fn rewrite_as_png(path: &Path) -> Result<()> {
    let bytes = tokio::fs::read(path).await?;
    // If the source already IS a PNG, keep the original bytes untouched.
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Ok(());
    }
    let path_owned = path.to_path_buf();
    let encoded = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let reader = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format()?;
        let img = reader.decode()?;
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png)?;
        Ok(out.into_inner())
    })
    .await??;
    let part = path_owned.with_extension("png.part");
    tokio::fs::write(&part, &encoded).await?;
    tokio::fs::rename(&part, &path_owned).await?;
    Ok(())
}

/// Run the unpaper cleanup pass per the cleanup mode. Returns the path to
/// use downstream (on success the `_clean` file, raw removed; on failure the
/// original) and whether the page is fully cleaned+deskewed (only legacy
/// mode on gray/lineart input can claim that). Skipped entirely for
/// cleanup=off or color mode (unpaper is grayscale-only).
pub async fn maybe_unpaper(
    page_png: &Path,
    cleanup: Cleanup,
    extra_args: &[String],
    mode: &str,
) -> (PathBuf, bool) {
    if cleanup == Cleanup::Off || mode == "color" {
        return (page_png.to_path_buf(), false);
    }
    if crate::backend::which("unpaper").is_none() {
        tracing::warn!("unpaper not found; using raw scan (ocrmypdf deskews at finish)");
        return (page_png.to_path_buf(), false);
    }
    let cleaned = clean_variant(page_png);
    let src = page_png.to_string_lossy().into_owned();
    let dst = cleaned.to_string_lossy().into_owned();
    let mut cmd: Vec<String> = vec!["unpaper".into()];
    cmd.extend(unpaper_args(cleanup, extra_args));
    cmd.push(src.clone());
    cmd.push(dst.clone());
    let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
    match process::run(&refs, Some(UNPAPER_TIMEOUT)).await {
        Ok(out) if out.success && tokio::fs::try_exists(&cleaned).await.unwrap_or(false) => {
            // unpaper always writes Netpbm data (even into `.png` names);
            // re-encode to a real PNG so downstream consumers don't rely on
            // format sniffing.
            if let Err(e) = rewrite_as_png(&cleaned).await {
                tracing::warn!("PNG re-encode of unpaper output failed ({e}); keeping raw file");
            }
            let _ = tokio::fs::remove_file(page_png).await;
            (cleaned, cleanup == Cleanup::Legacy)
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
    /// Ordered (image path, dpi) pairs (already unpapered/rotated). DPI is
    /// per page: sessions may mix resolutions via the +/- presets.
    pub pages: Vec<(PathBuf, u16)>,
    /// True when at least one page was NOT fully cleaned+deskewed by unpaper
    /// (cleanup off/conservative, unpaper failure, color pages in legacy
    /// mode): ocrmypdf then --cleans every page at finish (--deskew runs
    /// unconditionally).
    pub any_page_needing_cleanup: bool,
    /// True when any page was manually rotated: ocrmypdf --rotate-pages is
    /// then omitted entirely (it cannot be exempted per page).
    pub manually_rotated: bool,
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

/// img2pdf argv for a set of (image, dpi) pages (pure; unit-tested).
/// Single-DPI sessions get one call; mixed-DPI sessions must be assembled
/// per page and merged with pdfunite (img2pdf applies one --imgsize to all
/// inputs). `pagesize` stays derived from the image pixels + DPI ("auto":
/// page size = pixels / DPI, preserving the scanner window — receipts stay
/// receipt-sized).
fn img2pdf_pages_args(pages: &[(PathBuf, u16)], out_pdf: &Path) -> Vec<Vec<String>> {
    let mut groups: Vec<Vec<(PathBuf, u16)>> = Vec::new();
    for (path, dpi) in pages {
        match groups.last_mut() {
            // Consecutive same-dpi pages share one img2pdf call.
            Some(g) if g.last().map(|(_, d)| *d) == Some(*dpi) => g.push((path.clone(), *dpi)),
            _ => groups.push(vec![(path.clone(), *dpi)]),
        }
    }
    groups
        .iter()
        .map(|g| {
            std::iter::once("img2pdf".to_string())
                .chain(["--imgsize".to_string(), format!("{}dpi", g[0].1)])
                .chain(g.iter().map(|(p, _)| p.to_string_lossy().into_owned()))
                .chain(["-o".to_string(), out_pdf.to_string_lossy().into_owned()])
                .collect()
        })
        .collect()
}

/// Build the final PDF: img2pdf (lossless) -> [pdfunite when DPIs are mixed]
/// -> ocrmypdf (text layer). Returns the outcome; errors from img2pdf/
/// pdfunite are fatal, ocrmypdf failure is NOT (raw PDF is copied and a
/// distinct warning is surfaced).
pub async fn build_pdf(plan: &BuildPlan) -> Result<BuildOutcome> {
    let tmp = tempfile::TempDir::new()?;
    let raw_pdf = tmp.path().join("raw.pdf");

    let calls = img2pdf_pages_args(&plan.pages, &raw_pdf);
    if calls.len() == 1 {
        run_pdf_step("PDF assembly (img2pdf)", &calls[0], IMG2PDF_TIMEOUT).await?;
    } else {
        // Mixed DPIs: one PDF per group (each with its own --imgsize), then
        // merge. pdfunite preserves per-page dimensions.
        if crate::backend::which("pdfunite").is_none() {
            anyhow::bail!(
                "session mixes page DPIs ({} groups) and pdfunite is not installed \
                 - install poppler (pacman) or poppler-utils (apt), or rescan \
                 all pages at one DPI",
                calls.len()
            );
        }
        let mut parts: Vec<String> = Vec::new();
        for (i, args) in calls.iter().enumerate() {
            let part = tmp.path().join(format!("part{i}.pdf"));
            let mut full = args.clone();
            // Swap the shared out path for this part file (last two entries).
            let n = full.len();
            full[n - 1] = part.to_string_lossy().into_owned();
            run_pdf_step("PDF assembly (img2pdf)", &full, IMG2PDF_TIMEOUT).await?;
            parts.push(part.to_string_lossy().into_owned());
        }
        let mut unite: Vec<String> = vec!["pdfunite".into()];
        unite.extend(parts);
        unite.push(raw_pdf.to_string_lossy().into_owned());
        // Fatal on failure: raw.pdf doesn't exist yet, so the ocrmypdf
        // fallback path could not rescue this anyway.
        run_pdf_step("PDF merge (pdfunite)", &unite, PDFUNITE_TIMEOUT).await?;
    }

    // ocrmypdf deskew/clean. --deskew always runs: it is content-safe and
    // idempotent on pages unpaper already deskewed, and unpaper's own deskew
    // is a no-op on flatbed scans (conservative explicitly disables it), so
    // legacy-mode pages still need it. --clean (not per-page) runs when any
    // page missed unpaper's cleanup, and only when the unpaper binary is
    // present (ocrmypdf hard-fails without it).
    let mut ocr_args: Vec<String> = vec![
        "ocrmypdf".into(),
        "--language".into(),
        plan.langs.clone(),
        "--output-type".into(),
        "pdfa".into(),
        "--optimize".into(),
        "0".into(),
        "--deskew".into(),
    ];
    if !plan.manually_rotated {
        ocr_args.push("--rotate-pages".into());
        ocr_args.push("--rotate-pages-threshold".into());
        ocr_args.push("10".into());
    }
    if crate::backend::which("unpaper").is_some() && plan.any_page_needing_cleanup {
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

/// Run one external PDF step; both failure modes are fatal with log context.
async fn run_pdf_step(what: &str, args: &[String], timeout: Duration) -> Result<()> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = process::run(&refs, Some(timeout))
        .await
        .map_err(|e| process::fail_with_log_err(what, &refs, e))?;
    if !out.success {
        return Err(process::fail_with_log(what, &out));
    }
    Ok(())
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

    #[test]
    fn unpaper_args_by_cleanup_mode() {
        use crate::config::Cleanup;
        // All modes share the base flags.
        for mode in [Cleanup::Off, Cleanup::Conservative, Cleanup::Legacy] {
            let args = unpaper_args(mode, &[]);
            assert_eq!(
                &args[..4],
                [
                    "--layout",
                    "single",
                    "--deskew-scan-direction",
                    "left,right"
                ]
            );
            assert!(!args
                .iter()
                .any(|a| a.starts_with('/') || a.ends_with(".png")));
        }
        // Conservative disables every content-altering stage (incl. deskew:
        // it never fires on flatbed input and would double-deskew if it did).
        let cons = unpaper_args(Cleanup::Conservative, &[]);
        for flag in [
            "--no-mask-scan",
            "--no-border-scan",
            "--no-border-align",
            "--no-blackfilter",
            "--no-grayfilter",
            "--no-blurfilter",
            "--no-noisefilter",
            "--no-deskew",
        ] {
            assert!(cons.iter().any(|a| a == flag), "missing {flag}");
        }
        // Legacy = the historical command, nothing extra.
        assert_eq!(unpaper_args(Cleanup::Legacy, &[]).len(), 4);
        // Extra args land after the fixed set, before file args get appended.
        let extra = unpaper_args(
            Cleanup::Legacy,
            &["--blackfilter-intensity".into(), "40".into()],
        );
        assert_eq!(&extra[4..], ["--blackfilter-intensity", "40"]);
    }

    #[test]
    fn img2pdf_groups_by_dpi() {
        let p = |n: &str| PathBuf::from(n);
        let pages = vec![
            (p("a.png"), 600),
            (p("b.png"), 600),
            (p("c.png"), 300),
            (p("d.png"), 300),
            (p("e.png"), 600),
        ];
        let calls = img2pdf_pages_args(&pages, Path::new("/tmp/out.pdf"));
        // Three DPI groups: 600x2, 300x2, 600x1 (consecutive runs).
        assert_eq!(calls.len(), 3);
        assert!(calls[0].contains(&"600dpi".to_string()));
        assert!(calls[1].contains(&"300dpi".to_string()));
        assert!(calls[2].contains(&"600dpi".to_string()));
        // Each call carries its own -o and the images of its group only.
        for call in &calls {
            assert_eq!(call[call.len() - 2], "-o");
            assert_eq!(call[call.len() - 1], "/tmp/out.pdf");
        }
        assert_eq!(calls[0].len(), 5 + 2); // img2pdf --imgsize Ndpi a b -o
        assert_eq!(calls[2].len(), 4 + 2);
    }

    #[test]
    fn img2pdf_single_dpi_single_call() {
        let pages = vec![(PathBuf::from("a.png"), 300), (PathBuf::from("b.png"), 300)];
        let calls = img2pdf_pages_args(&pages, Path::new("/tmp/out.pdf"));
        assert_eq!(calls.len(), 1);
        assert!(calls[0].windows(2).any(|w| w == ["--imgsize", "300dpi"]));
        assert!(calls[0].contains(&"a.png".to_string()) && calls[0].contains(&"b.png".to_string()));
    }
}
