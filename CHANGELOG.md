# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

- **Rescan self-deletion**: rescans reuse the fixed `page_NNN.rescan.png`
  name, so the second rescan of a page deleted the image it had just
  captured — the preview froze on the old thumbnail and the page's image
  file was gone (breaking OCR and the PDF build). The old-image cleanup now
  skips every file equal to the new final image path.
- **unpaper atomic output**: the cleanup pass now writes to a temp file and
  renames over the final `_clean` name only on success. Previously a failed
  unpaper run on a rescan could delete the page's live `_clean` image
  ("keeps old until success" was broken), and the direct overwrite could
  race the preview decode

### Changed

- **Rescan follows current settings**: `r` rescans with the currently
  selected dpi/mode (like `m`/`+`) instead of the page's original settings;
  the page adopts them on success and keeps its old values on failure

## [0.2.0] - 2026-09-04

### Added

- **Near-instant startup**: the TUI paints in well under a second while
  scanner detection (`scanimage -L`) and dependency checks run in the
  background; scanning unlocks the moment the device is found, and an early
  `s` press buffers the scan intent until detection completes
- **Diagnostics screen** (`!`) inside the TUI with install hints for anything
  missing; opens automatically when a preflight check fails, non-blocking `r`
  re-run; headless `--doctor` kept
- **Exit verdicts**: exit code `0` on a healthy session (and when quitting
  while background detection was still running), `1` only on failed startup
  checks or no scanner found
- **Stale-device reporting**: a detected device that disappears before the
  first scan is re-reported instead of silently failing
- **Image preview protocols**: kitty graphics and sixel render natively;
  protocol detection runs in an isolated ~600 ms probe process so it can
  never eat keystrokes; sixel works inside tmux (raw passthrough query);
  halfblock fallback everywhere else
- **TUI polish**: per-page and session timers, contact-sheet preview pane,
  mouse click-to-focus and pane scrolling
- CI: fmt, clippy (`-D warnings`), tests against a real OCR toolchain,
  MSRV 1.90, rustdoc (`-D warnings`), weekly `cargo audit`
- Release pipeline: cross-built binaries for x86_64, aarch64, armv7hl and
  riscv64 (glibc 2.31 floor + static musl), each as tarball, `.deb` and
  `.rpm`; tag/version guard; SHA256SUMS
- Dependabot (cargo + GitHub Actions, weekly)

### Changed

- **Faster scan cycle**: the scanner is freed as soon as a capture ends —
  cleanup and text-pane OCR for page N run while page N+1 scans
- **`preview_ocr` default `lazy`**: the extracted-text pane OCRs only the
  page you are viewing, on demand; `eager` and `off` remain available
- **Default 300 dpi** (the OCR sweet spot); 600 dpi available via `+` or
  `-d 600`
- **Default `langs = "deu+Latin"`**: the tesseract `Latin` *script* model
  fixes the `§` → `&` misread without hurting umlauts; mixing `eng` into
  German OCR is documented as garbling umlauts (`für` → `fiir`)
- **README accuracy**: config is read from the current working directory
  (or `~/.config/auto-scanner-ocr/config.toml`), not next to the binary;
  Rust 1.90+ to build

### Fixed

- **unpaper content loss**: the default unpaper filter stack erases page
  edges on flatbed scans (a whole table was wiped); `cleanup = "off"` is now
  the default and ocrmypdf's `--deskew --clean` does the real work at finish;
  `conservative` is a verified pixel-identical passthrough; `legacy` kept
  for book scans
- **Inconsistent page sizes** when one session mixes DPIs (merged with
  `pdfunite`; pages keep their own scanner window size)
- **`§` misread as `&`/`8&`/`$`** in German legal citations — model
  limitation, fixed by the `Latin` script model (see README for install)
- **False "unsaved changes" dialog** after successfully building the PDF
- **Atomic PDF delivery**: ocrmypdf writes to a `.part` sibling renamed into
  the reserved output path on success, so an interrupted build can never
  leave a truncated file the startup sweep mistakes for a finished PDF
- **Orphaned stdin reader** from ratatui-image's in-process terminal query
  eating keystrokes — the probe now runs in an isolated child process
- **Lazy-OCR retry loop** stopping on persistent failure instead of
  proceeding
- Rescan/rotate correctly blocked while a PDF build is in flight
- Session-directory sweep uses `flock` ownership: crash/signal leftovers are
  removed on next startup, a live instance's directory is never touched
  (even when suspended)

## [0.1.0] - 2026-09-02

### Added

- Initial release: Python 3.11+ CLI that drives a SANE flatbed scanner
  (e.g. HP Deskjet 1050a) and produces PDF/A documents with an OCR text
  layer (OCRmyPDF + Tesseract, default eng+deu)
- Single-page and background-processed multi-page modes
- unpaper cleanup with ocrmypdf deskew/clean fallback
- Startup dependency check on every run, `--doctor` for troubleshooting
- TOML config file with per-run CLI overrides
- Desktop notifications via libnotify, logs in the XDG state dir

[0.2.0]: https://github.com/j-stechmann/auto-scanner-ocr/compare/773e405...v0.2.0
[0.1.0]: https://github.com/j-stechmann/auto-scanner-ocr/tree/773e405