//! Integration: the final PDF delivery is atomic. ocrmypdf (and the
//! no-text-layer fallback) write to a `.part` sibling that is renamed into
//! the reserved output path only on success, so a build killed mid-write
//! leaves the zero-byte reservation untouched instead of a truncated file
//! the startup sweep would mistake for a finished PDF. Skips gracefully
//! when `ocrmypdf`/`img2pdf` are not installed (CI / minimal systems).

use auto_scanner_ocr::backend::pdf::{build_pdf, release_reservation, BuildOutcome, BuildPlan};
use std::path::PathBuf;

fn write_page(path: &std::path::Path) {
    // A tiny synthetic page with enough structure for img2pdf; OCR content
    // is irrelevant here (the build's outcome type is not under test).
    let mut img = image::GrayImage::from_pixel(200, 280, image::Luma([245u8]));
    for y in 40..80 {
        for x in 30..170 {
            img.put_pixel(x, y, image::Luma([20u8]));
        }
    }
    img.save(path).expect("write page");
}

fn plan_for(dir: &std::path::Path, out_pdf: PathBuf) -> BuildPlan {
    let page = dir.join("page_001.png");
    write_page(&page);
    BuildPlan {
        pages: vec![(page, 300)],
        any_page_needing_cleanup: false,
        manually_rotated: false,
        langs: "eng".into(),
        out_pdf,
    }
}

#[tokio::test]
async fn successful_build_renames_part_into_reserved_path() {
    if auto_scanner_ocr::backend::which("ocrmypdf").is_none()
        || auto_scanner_ocr::backend::which("img2pdf").is_none()
    {
        eprintln!("skipping: ocrmypdf/img2pdf not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("2026-01-01_000000.pdf");
    std::fs::write(&out, b"").unwrap(); // the reservation placeholder
    let plan = plan_for(dir.path(), out.clone());

    let outcome = build_pdf(&plan).await.expect("build succeeds");
    assert_eq!(outcome, BuildOutcome::Searchable);
    let meta = std::fs::metadata(&out).unwrap();
    assert!(meta.len() > 0, "reserved path now holds the real PDF");
    assert!(
        !out.with_extension("pdf.part").exists(),
        "no .part leftover after success"
    );
    // A finished build's output is never released.
    release_reservation(&out);
    assert!(out.exists(), "release_reservation keeps the built PDF");
}

#[tokio::test]
async fn failed_ocrmypdf_fallback_also_delivers_atomically() {
    if auto_scanner_ocr::backend::which("img2pdf").is_none() {
        eprintln!("skipping: img2pdf not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("2026-01-02_000000.pdf");
    std::fs::write(&out, b"").unwrap();
    // Force the ocrmypdf step to fail: a language tesseract cannot know.
    let mut plan = plan_for(dir.path(), out.clone());
    plan.langs = "definitely-not-a-tesseract-lang".into();

    let outcome = build_pdf(&plan).await.expect("raw PDF is still built");
    assert_eq!(outcome, BuildOutcome::WithoutTextLayer);
    let meta = std::fs::metadata(&out).unwrap();
    assert!(meta.len() > 0, "fallback output landed atomically");
    assert!(
        !out.with_extension("pdf.part").exists(),
        "no .part leftover after fallback"
    );
}

#[tokio::test]
async fn interrupted_part_is_discarded_by_release_reservation() {
    // Simulates the quit-during-build aftermath: the reservation is still a
    // zero-byte placeholder, the killed build left garbage in `.part`.
    // Release must clear both and never leave the `.part` behind for the
    // sweep to trip over.
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("2026-01-03_000000.pdf");
    std::fs::write(&out, b"").unwrap();
    let part = out.with_extension("pdf.part");
    std::fs::write(&part, b"truncated pdf bytes").unwrap();

    release_reservation(&out);

    assert!(!out.exists(), "placeholder released");
    assert!(!part.exists(), "partial output discarded");
}
