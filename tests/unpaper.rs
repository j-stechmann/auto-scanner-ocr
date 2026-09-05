//! Integration: unpaper cleanup modes against real page images.
//! Skips gracefully when `unpaper` is not installed (CI / minimal systems).

use auto_scanner_ocr::backend::pdf::{maybe_unpaper, unpaper_args};

/// Fixture width of `write_flatbed_page`.
const FIXTURE_W: usize = 400;

fn gray_from_png(path: &std::path::Path) -> (u32, u32, Vec<u8>) {
    let img = image::ImageReader::open(path)
        .expect("open")
        .with_guessed_format()
        .expect("format")
        .decode()
        .expect("decode");
    let gray = img.to_luma8();
    let (w, h) = (gray.width(), gray.height());
    (w, h, gray.into_raw())
}

/// A synthetic "flatbed page": white page content on a dark scanner
/// background, with text lines near BOTH edges — the default unpaper mask
/// scan erases exactly these on real flatbed scans (the missing-table bug).
fn write_flatbed_page(path: &std::path::Path) {
    let (w, h) = (FIXTURE_W as u32, 560u32);
    let mut img = image::GrayImage::from_pixel(w, h, image::Luma([40u8])); // dark lid
                                                                           // White page covering most of the sheet.
    for y in 20..540 {
        for x in 10..390 {
            img.put_pixel(x, y, image::Luma([245u8]));
        }
    }
    // Text lines: left margin, middle, right margin.
    for line in 0..10 {
        let y0 = 60 + line * 45;
        for x in [30..80, 180..230, 320..370] {
            for xx in x {
                for dy in 0..6 {
                    img.put_pixel(xx, y0 + dy, image::Luma([20u8]));
                }
            }
        }
    }
    img.save(path).expect("write page");
}

#[tokio::test]
async fn conservative_unpaper_preserves_page_content() {
    if auto_scanner_ocr::backend::which("unpaper").is_none() {
        eprintln!("skipping: unpaper not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("page_001.png");
    write_flatbed_page(&raw);
    let (w0, h0, px0) = gray_from_png(&raw);

    let (cleaned, deskewed) = maybe_unpaper(
        &raw,
        auto_scanner_ocr::config::Cleanup::Conservative,
        &[],
        "gray",
    )
    .await;
    // Conservative is a passthrough: nothing claims deskew credit.
    assert!(!deskewed);

    let (_, _, px1) = gray_from_png(&cleaned);
    assert!(
        std::fs::metadata(&cleaned).unwrap().len() > 0,
        "output exists"
    );
    // Dimensions preserved (no rescale/crop) — the varying-size bug class.
    let img1 = image::open(&cleaned).unwrap();
    assert_eq!((w0, h0), (img1.width(), img1.height()));
    // Byte-identical pixels: conservative disables every filter, so unpaper
    // must not alter a single pixel (the missing-table AND de-speckle bug
    // classes). This enforces the "pixel-identical passthrough" the docs
    // advertise.
    assert_eq!(px0, px1, "conservative altered pixel data");
}

#[tokio::test]
async fn off_mode_skips_unpaper_entirely() {
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("page_001.png");
    write_flatbed_page(&raw);
    let before = std::fs::read(&raw).unwrap();

    let (cleaned, deskewed) =
        maybe_unpaper(&raw, auto_scanner_ocr::config::Cleanup::Off, &[], "gray").await;
    assert!(!deskewed);
    assert_eq!(cleaned, raw);
    let after = std::fs::read(&raw).unwrap();
    assert_eq!(before, after, "off mode must not touch the file");
}

#[tokio::test]
async fn color_mode_is_never_unpapered() {
    if auto_scanner_ocr::backend::which("unpaper").is_none() {
        eprintln!("skipping: unpaper not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("page_001.png");
    write_flatbed_page(&raw);
    let before = std::fs::read(&raw).unwrap();

    let (cleaned, deskewed) = maybe_unpaper(
        &raw,
        auto_scanner_ocr::config::Cleanup::Legacy,
        &[],
        "color",
    )
    .await;
    assert!(!deskewed);
    assert_eq!(cleaned, raw);
    assert_eq!(before, std::fs::read(&raw).unwrap());
}

#[tokio::test]
async fn failed_unpaper_keeps_previous_clean_image() {
    if auto_scanner_ocr::backend::which("unpaper").is_none() {
        eprintln!("skipping: unpaper not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let raw = dir.path().join("page_001.png");

    // First pass succeeds: this is the page's live `_clean` image (as after
    // a rescan, which reuses the fixed raw name).
    write_flatbed_page(&raw);
    let (cleaned, _) =
        maybe_unpaper(&raw, auto_scanner_ocr::config::Cleanup::Legacy, &[], "gray").await;
    assert_eq!(cleaned, raw.with_file_name("page_001_clean.png"));
    let clean_before = std::fs::read(&cleaned).unwrap();
    assert!(!clean_before.is_empty());

    // Second capture (rescan) is garbage, so unpaper must fail. The previous
    // `_clean` image must survive byte-for-byte and the raw capture must
    // stay as the fallback.
    std::fs::write(&raw, b"corrupt capture, not an image").unwrap();
    let (cleaned2, deskewed) =
        maybe_unpaper(&raw, auto_scanner_ocr::config::Cleanup::Legacy, &[], "gray").await;
    assert!(!deskewed);
    assert_eq!(cleaned2, raw, "failure must fall back to the raw path");
    assert_eq!(
        clean_before,
        std::fs::read(&cleaned).unwrap(),
        "failed unpaper run damaged the live _clean image"
    );
    assert!(raw.exists(), "raw capture must survive as the fallback");
}

#[test]
fn unpaper_args_file_args_come_last() {
    // Callers append `src dst` after the builder output; ensure the builder
    // never produces path-looking tokens (would be parsed as filenames).
    for mode in [
        auto_scanner_ocr::config::Cleanup::Off,
        auto_scanner_ocr::config::Cleanup::Conservative,
        auto_scanner_ocr::config::Cleanup::Legacy,
    ] {
        for arg in unpaper_args(mode, &["--layout".into(), "single".into()]) {
            assert!(!arg.contains('/'), "path-like token in options: {arg}");
        }
    }
}
