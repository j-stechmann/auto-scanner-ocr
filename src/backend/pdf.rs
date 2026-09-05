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
///
/// Atomicity: unpaper writes to a temp name which is re-encoded to PNG and
/// renamed over the final `_clean` path only on success. Rescans reuse the
/// fixed `page_NNN.rescan.png` name, so writing the output directly (or
/// deleting it on failure) could destroy the page's live image — the old
/// `_clean` file must survive a failed cleanup pass untouched.
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
    let tmp = page_png.with_extension("png.unpaper.tmp");
    let src = page_png.to_string_lossy().into_owned();
    let dst = tmp.to_string_lossy().into_owned();
    let mut cmd: Vec<String> = vec!["unpaper".into()];
    cmd.extend(unpaper_args(cleanup, extra_args));
    cmd.push(src.clone());
    cmd.push(dst.clone());
    let refs: Vec<&str> = cmd.iter().map(String::as_str).collect();
    match process::run(&refs, Some(UNPAPER_TIMEOUT)).await {
        Ok(out) if out.success && tokio::fs::try_exists(&tmp).await.unwrap_or(false) => {
            // unpaper always writes Netpbm data (even into `.png` names);
            // re-encode to a real PNG so downstream consumers don't rely on
            // format sniffing.
            if let Err(e) = rewrite_as_png(&tmp).await {
                tracing::warn!("PNG re-encode of unpaper output failed ({e}); keeping raw file");
                let _ = tokio::fs::remove_file(&tmp).await;
                return (page_png.to_path_buf(), false);
            }
            // Rename over the final name BEFORE removing the raw: consumers
            // only ever see a complete file, a pre-existing `_clean` image
            // (previous rescan) survives until the new output is ready, and
            // the raw capture stays as the failure fallback.
            if let Err(e) = tokio::fs::rename(&tmp, &cleaned).await {
                tracing::warn!("renaming unpaper output failed ({e}); keeping raw file");
                let _ = tokio::fs::remove_file(&tmp).await;
                return (page_png.to_path_buf(), false);
            }
            let _ = tokio::fs::remove_file(page_png).await;
            (cleaned, cleanup == Cleanup::Legacy)
        }
        Ok(_) => {
            tracing::warn!("unpaper failed; using raw scan");
            let _ = tokio::fs::remove_file(&tmp).await;
            (page_png.to_path_buf(), false)
        }
        Err(e) => {
            tracing::warn!("unpaper failed ({e}); using raw scan");
            let _ = tokio::fs::remove_file(&tmp).await;
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
    // Final delivery is atomic: ocrmypdf and the fallback copy write to a
    // `.part` sibling that is renamed into place only on success. A build
    // killed mid-write therefore leaves the zero-byte reservation untouched
    // (plus a garbage `.part` that release/sweep discard) instead of a
    // truncated file the sweep would mistake for a finished PDF.
    let mut part_os = plan.out_pdf.as_os_str().to_os_string();
    part_os.push(".part");
    let part = PathBuf::from(part_os);

    ocr_args.push(raw_pdf.to_string_lossy().into_owned());
    ocr_args.push(part.to_string_lossy().into_owned());

    let refs: Vec<&str> = ocr_args.iter().map(String::as_str).collect();
    let out = process::run(&refs, Some(OCRYPDF_TIMEOUT))
        .await
        .map_err(|e| process::fail_with_log_err("OCR (ocrmypdf)", &refs, e))?;
    if out.success {
        tokio::fs::rename(&part, &plan.out_pdf).await?;
        return Ok(BuildOutcome::Searchable);
    }

    // Non-fatal fallback (parity): save the raw PDF without a text layer.
    tracing::error!("ocrmypdf failed; saving PDF without text layer");
    tokio::fs::copy(&raw_pdf, &part).await?;
    tokio::fs::rename(&part, &plan.out_pdf).await?;
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
/// suffix on collision. The chosen path is RESERVED by creating the file
/// (O_EXCL) — unlike an exists()-check, a reservation cannot be won twice by
/// concurrent instances, so two same-stamp launches can never silently pick
/// the same output and overwrite each other's PDF.
///
/// Callers delete the zero-byte placeholder via `release_reservation` when
/// the session never builds (quit, new session). On a hard open error
/// (unwritable output dir etc.) nothing is created and the error propagates,
/// so a failed start cannot leave junk behind.
pub fn reserve_output_path(dir: &Path, stamp: String) -> Result<PathBuf> {
    let mut base = dir.join(format!("{stamp}.pdf"));
    let mut n = 2;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&base)
        {
            Ok(_) => return Ok(base),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                base = dir.join(format!("{stamp}_{n}.pdf"));
                n += 1;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Reserve an exact, user-chosen output path for `FinishTo` (no collision
/// ladder — the ladder is for auto-generated stamps). An existing DIRECTORY
/// is always rejected here: O_EXCL on a directory also surfaces as
/// AlreadyExists, and adopting one would only be discovered at the final
/// rename (EISDIR) after the whole OCR run. An existing zero-byte file is
/// always adopted (placeholder semantics). An existing non-empty file is
/// adopted as-is only when `allow_existing` (overwrite intent): the build
/// never opens the target — img2pdf/ocrmypdf write elsewhere and the final
/// `.part` rename replaces it — so the previous file stays byte-identical
/// until a successful build. Other open errors (missing dir => NotFound,
/// unwritable dir => PermissionDenied) propagate; the caller must have
/// created the target dir already.
pub fn reserve_target(path: &Path, allow_existing: bool) -> Result<PathBuf> {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.is_dir() {
            anyhow::bail!("{} is a directory", path.display());
        }
        if meta.is_file() && meta.len() > 0 && !allow_existing {
            anyhow::bail!(
                "{} already exists (choose a different name or allow overwrite)",
                path.display()
            );
        }
        // Adopted as-is (zero-byte placeholder, or overwrite intent). The
        // path must still be a regular file; metadata above proved it.
        return Ok(path.to_path_buf());
    }
    // Missing (or raced): create the zero-byte placeholder exclusively, so
    // two concurrent sessions cannot claim the same user path.
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    Ok(path.to_path_buf())
}

/// Remove a reservation made by `reserve_output_path`. Only deletes the
/// zero-byte placeholder, so a real PDF (written by a finished build) is
/// never touched. Also discards a leftover `<path>.part` (an interrupted
/// build's partial output): a non-empty placeholder can only mean a race
/// between a live build and the release, in which case the `.part` garbage
/// is dead weight anyway. Best effort.
pub fn release_reservation(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.is_file() && meta.len() == 0 {
            let _ = std::fs::remove_file(path);
        }
    }
    let mut part_os = path.as_os_str().to_os_string();
    part_os.push(".part");
    let _ = std::fs::remove_file(PathBuf::from(part_os));
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
    fn reserve_output_path_collisions_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let stamp = "2026-09-02_143005".to_string();
        let p1 = reserve_output_path(d, stamp.clone()).unwrap();
        assert_eq!(p1, d.join("2026-09-02_143005.pdf"));
        assert!(p1.exists(), "reservation creates the placeholder");
        // Second reserve for the same stamp must not reuse the live path.
        let p2 = reserve_output_path(d, stamp.clone()).unwrap();
        assert_eq!(p2, d.join("2026-09-02_143005_2.pdf"));
        let p3 = reserve_output_path(d, stamp.clone()).unwrap();
        assert_eq!(p3, d.join("2026-09-02_143005_3.pdf"));
        // Release only deletes empty placeholders; a written PDF survives.
        // A leftover `.part` from an interrupted build is discarded either
        // way.
        let p3_part = d.join("2026-09-02_143005_3.pdf.part");
        std::fs::write(&p3_part, b"half written").unwrap();
        release_reservation(&p2);
        assert!(!p2.exists());
        release_reservation(&p1);
        assert!(!p1.exists());
        std::fs::write(&p3, b"pdf bytes").unwrap();
        release_reservation(&p3);
        assert!(p3.exists(), "built PDF is never released");
        assert!(!p3_part.exists(), "partial output discarded");
        // Zero-byte placeholder: both the placeholder and its .part go.
        // (p2 was released above, so the same-stamp ladder reuses `_2`.)
        let p4 = reserve_output_path(d, stamp).unwrap();
        let p4_part = p4.with_extension("pdf.part");
        std::fs::write(&p4_part, b"garbage").unwrap();
        release_reservation(&p4);
        assert!(!p4.exists());
        assert!(!p4_part.exists());
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

    #[test]
    fn reserve_target_fresh_zero_byte_and_rejections() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Free path: reserved, zero-byte placeholder exists.
        let fresh = d.join("custom.pdf");
        assert_eq!(reserve_target(&fresh, false).unwrap(), fresh);
        assert!(fresh.exists());
        assert_eq!(std::fs::metadata(&fresh).unwrap().len(), 0);

        // A second reserve of the same free path (still zero-byte) is
        // adopted, not an error: the placeholder is ours.
        assert_eq!(reserve_target(&fresh, false).unwrap(), fresh);

        // Non-empty target without overwrite intent: rejected, file
        // untouched.
        let taken = d.join("taken.pdf");
        std::fs::write(&taken, b"real pdf bytes").unwrap();
        assert!(reserve_target(&taken, false).is_err());
        assert_eq!(std::fs::read(&taken).unwrap(), b"real pdf bytes");

        // Non-empty target with overwrite intent: adopted as-is (no
        // truncate — the file is only replaced by the build's final
        // rename).
        assert_eq!(reserve_target(&taken, true).unwrap(), taken);
        assert_eq!(std::fs::read(&taken).unwrap(), b"real pdf bytes");

        // Zero-byte targets are adoptable even without overwrite intent
        // (placeholder semantics; release_reservation will clean them up).
        let empty = d.join("empty.pdf");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(reserve_target(&empty, false).unwrap(), empty);

        // A directory named foo.pdf is rejected up front, never adopted.
        let as_dir = d.join("dir.pdf");
        std::fs::create_dir(&as_dir).unwrap();
        assert!(reserve_target(&as_dir, true).is_err());
        assert!(as_dir.is_dir());

        // Missing target dir: NotFound propagates, nothing created.
        let missing_dir = d.join("no/such/dir");
        assert!(reserve_target(&missing_dir.join("x.pdf"), false).is_err());
        assert!(!missing_dir.exists());
    }

    #[test]
    fn release_keeps_adopted_target_and_discards_part() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Adopted non-empty file survives release; a sibling .part from an
        // interrupted overwrite build is discarded.
        let target = d.join("adopted.pdf");
        std::fs::write(&target, b"previous content").unwrap();
        std::fs::write(target.with_extension("pdf.part"), b"garbage").unwrap();
        release_reservation(&target);
        assert_eq!(std::fs::read(&target).unwrap(), b"previous content");
        assert!(!target.with_extension("pdf.part").exists());

        // A created placeholder is released entirely.
        let placeholder = d.join("placeholder.pdf");
        reserve_target(&placeholder, false).unwrap();
        release_reservation(&placeholder);
        assert!(!placeholder.exists());
    }
}
