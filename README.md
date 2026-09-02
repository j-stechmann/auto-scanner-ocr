# Flatbed scan → searchable OCR PDF, in one command.

**auto-scanner-ocr** is a small Python CLI for Linux that drives a flatbed
SANE scanner (e.g. HP Deskjet 1050a) and turns each scan into a
**searchable PDF** — the page image plus an invisible OCR text layer, so you
can select, copy and Ctrl+F the text in any PDF viewer.

No daemons, no web servers, no hotkeys: you run it in a terminal when you
want to scan.

## Features

- Single-page mode and multi-page mode (pages are processed in the background
  while you place the next one; merged into one PDF)
- OCR text layer via [OCRmyPDF]/[Tesseract], multiple languages (default: English + German)
- Page cleanup: deskew + border cleaning via unpaper (with OCRmyPDF fallback)
- Startup dependency check on every run, plus a `--doctor` command for troubleshooting
- Desktop notifications (optional), log file for debugging
- Single file, Python standard library only (Python 3.11+)

## Requirements

- Linux with SANE (`sane`), HPLIP for HP devices (`hplip`)
- `ocrmypdf`, `tesseract` (+ language data), `img2pdf`, `unpaper`
- `libnotify` for notifications (optional)
- A SANE-compatible scanner (USB-only models like the Deskjet 1050a work fine)

## Install

Arch:

```sh
sudo pacman -S --needed sane hplip ocrmypdf tesseract tesseract-data-eng tesseract-data-deu img2pdf unpaper libnotify
```

Debian/Ubuntu:

```sh
sudo apt install sane hplip ocrmypdf tesseract-ocr tesseract-ocr-eng tesseract-ocr-deu img2pdf unpaper libnotify
```

Then:

```sh
git clone <your-repo-url>
cd auto-scanner-ocr
chmod +x auto_scanner_ocr.py
```

Optional: put the script (or a symlink) on your PATH:

```sh
ln -s "$PWD/auto_scanner_ocr.py" ~/.local/bin/scan
```

### Set up the scanner (once)

```sh
scanimage -L          # should list your device
# if not:
sudo hp-setup -i      # HP-specific setup, then try scanimage -L again
```

## Usage

```sh
./auto_scanner_ocr.py                # single page → searchable PDF
./auto_scanner_ocr.py -m             # multi-page: Enter scans next page, q finishes
./auto_scanner_ocr.py --doctor       # check dependencies and scanner, then exit
./auto_scanner_ocr.py --help         # all options
```

Typical session (`-m`):

```
$ ./auto_scanner_ocr.py -m
Multi-page mode: place page 1 on the scanner glass.
[page 1] press Enter to scan, 'q'+Enter to finish:
  page 1 captured - processing in background…
Place page 2 on the glass (or 'q' to finish).
[page 2] press Enter to scan, 'q'+Enter to finish: q
Building searchable PDF (2 page(s), OCR langs: eng+deu)…
Done: /home/you/Documents/scans/2026-09-02_143005.pdf (812 KB)
```

### Options

| Flag | Meaning | Default |
|---|---|---|
| `-m, --multi` | multi-page session, merged into one PDF | off |
| `--dpi N` | scan resolution | 300 |
| `--mode` | `gray`, `color` or `lineart` | gray |
| `--langs A+B` | OCR languages, plus-separated | eng+deu |
| `--output DIR` | where PDFs are written | ~/Documents/scans |
| `--device NAME` | SANE device name or substring | first found |
| `--no-unpaper` | skip the unpaper cleanup step | — |
| `--no-notify` | disable desktop notifications | — |
| `--config FILE` | use a specific config file | ./config.toml or ~/.config/auto-scanner-ocr/config.toml |

### Configuration

Defaults live in `config.toml` next to the script (or
`~/.config/auto-scanner-ocr/config.toml`); every value can be overridden on
the command line.

## Troubleshooting

- Run `./auto_scanner_ocr.py --doctor` — it checks every dependency, tesseract
  language data, scanner detection and the output directory, and prints install
  hints for anything missing.
- The tool always runs a startup check before scanning; if something is
  missing you'll see exactly what (and how to install it) instead of a
  mid-scan failure.
- Logs are written to `~/.local/state/auto-scanner-ocr/auto-scanner-ocr.log`;
  add `--verbose` to see them in the terminal.
- Scanner not found? Check USB, power, and `scanimage -L`. For HP devices run
  `sudo hp-setup -i` once.

## The result

Each run produces `YYYY-MM-DD_HHMMSS.pdf` in your output directory: a PDF/A
document whose visible layer is your scan (deskewed and cleaned) and whose
invisible text layer contains the OCR result — searchable and copy-pastable
in any PDF viewer.

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).

[OCRmyPDF]: https://github.com/ocrmypdf/OCRmyPDF
[Tesseract]: https://github.com/tesseract-ocr/tesseract