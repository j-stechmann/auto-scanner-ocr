//! Integration: unpaper cleanup modes against real page images.
//! Skips gracefully when `unpaper` is not installed (CI / minimal systems).

use auto_scanner_ocr::backend::pdf::{maybe_unpaper, unpaper_args};

/// True when the content bounding box (dark pixels below `thresh`) is the
/// same in both images. Sampled for speed at 600dpi sizes.
fn same_content_bbox(a: &[u8], b: &[u8], thresh: u8) -> bool {
    let bbox = |px: &[u8]| -> (usize, usize, usize, usize) {
        let w = 400;
        let h = px.len() / w;
        let (mut mnx, mut mxx, mut mny, mut mxy) = (w, 0, h, 0);
        for y in (0..h).step_by(2) {
            for x in (0..w).step_by(2) {
                if px[y * w + x] < thresh {
                    mnx = mnx.min(x);
                    mxx = mxx.max(x);
                    mny = mny.min(y);
                    mxy = mxy.max(y);
                }
            }
        }
        (mnx, mny, mxx, mxy)
    };
    bbox(a) == bbox(b)
}

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
    let (w, h) = (400u32, 560u32);
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

    let (_, h1, px1) = gray_from_png(&cleaned);
    assert!(
        std::fs::metadata(&cleaned).unwrap().len() > 0,
        "output exists"
    );
    // Dimensions preserved (no rescale/crop) — the varying-size bug class.
    let img1 = image::open(&cleaned).unwrap();
    assert_eq!((w0, h0), (img1.width(), img1.height()));
    // Edge text survives (the missing-table bug class).
    assert!(
        same_content_bbox(&px0, &px1, 128),
        "content bbox changed: edge content was destroyed"
    );
    let _ = h1;
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
