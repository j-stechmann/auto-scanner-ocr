# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- **Text contrast pass for the TUI**: unreadable color pairs fixed and now
  CI-enforced. The page-list selection and language-picker cursor pin an
  explicit white foreground on their dark-navy background (previously the
  terminal default foreground was inherited, which on light themes rendered
  dark-on-dark); the ` deleting `/`SKIP` badges use black-on-silver instead
  of black-on-dark-gray; the rotate marker `↻` is cyan instead of ANSI blue;
  unfocused pane borders are brighter (were nearly invisible on dark
  terminals); modal dialogs (help, diagnostics, language picker, confirm) no
  longer force a black background - they render in the terminal theme's own
  foreground/background pair, which fixes black-on-black text on light
  themes. A new `tui::theme` module centralizes all styling and ships a
  contrast test that checks every fg/bg pair against ten real terminal
  palettes (VGA, Tango, Dracula, Nord, Gruvbox Dark/Light, One Half
  Dark/Light, Solarized Dark/Light) using the WCAG 2.1 ratio

## [0.3.0] - 2026-09-05

### Added

- **Choose where to save at finish**: `f` opens the system save dialog
  (zenity, kdialog or yad — whichever is installed) for folder + filename,
  with the default timestamped path pre-filled and the dialog's native
  overwrite prompt. Cancelling the dialog aborts the finish. Without any
  dialog tool the old confirm dialog (default path) is used, so the finish
  flow never breaks

### Fixed

- **Save dialog panic can no longer wedge `f`**: the dialog task always
  reports an outcome (a panic inside it is mapped to the plain-confirm
  fallback); previously a panicking dialog task left `f` disabled for the
  rest of the run
- **Over-long chosen filenames no longer fail after the whole OCR run**:
  the per-build `.part` name truncates the file component (with a short
  hash to stay unique) so it stays within the kernel's NAME_MAX, and the
  cleanup matcher re-derives the same truncated stem
- **Filenames ending in whitespace are delivered exactly**: only the
  dialog tools' line terminator (`\n`/`\r\n`) is stripped from their
  stdout; other trailing whitespace (a trailing space is legal in a Linux
  filename) is part of the chosen path
- **Save dialog opens in the reserved output directory**: the dialog is
  now seeded with the full default path (directory + filename) for all
  three tools; previously zenity/yad opened wherever the app was launched
  from, with only kdialog seeded at all
- **Save dialog over SSH**: when the dialog tool is installed but cannot
  open a window (no display, e.g. SSH without forwarding), the finish now
  falls back to the plain confirm dialog instead of reporting "save dialog
  cancelled" forever
- **Concurrent builds to the same custom path no longer corrupt each
  other**: each build writes its own unique `<name>-<pid>-<nanos>.part`
  sibling; previously two sessions delivering to the same path shared one
  `.part` and interleaved writes could corrupt the delivered file
- **Non-UTF-8 filenames no longer mangled**: the dialog's chosen path is
  read as raw bytes and threaded through the PDF build as `OsStr` (argv,
  `.part` sibling, crash-recovery marker), so filenames that are legal on
  Linux but not valid UTF-8 are delivered exactly instead of silently
  targeting a different (lossily substituted) file

### Changed

- kdialog receives a plain Qt name filter instead of the zenity-style
  `| *.pdf` variant, which kdialog split into two entries
- Command logging: every child process now logs its rc and stdout/stderr
  tails at DEBUG level uniformly in the process runner (scans included);
  the per-call helper is gone

## [0.2.1] - 2026-09-05

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

[0.2.1]: https://github.com/j-stechmann/auto-scanner-ocr/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/j-stechmann/auto-scanner-ocr/compare/773e405...v0.2.0
[0.1.0]: https://github.com/j-stechmann/auto-scanner-ocr/tree/773e405