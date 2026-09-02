# Flatbed scan → searchable OCR PDF, in a terminal UI.

**auto-scanner-ocr** is a Rust TUI for Linux that drives a flatbed SANE
scanner (e.g. HP Deskjet 1050a) and turns your scans into **searchable
PDFs** — the page image plus an invisible OCR text layer, so you can select,
copy and Ctrl+F the text in any PDF viewer.

No daemons, no web servers: you run it in a terminal when you want to scan.

## Features

- **Live TUI**: page list with per-page status (scanning → cleaning → OCR →
  ready), image preview (kitty/sixel when your terminal supports it,
  halfblock fallback everywhere), extracted OCR text pane
- **Multi-page by default**: keep scanning while earlier pages clean/OCR in
  the background — the scanner is free as soon as a capture ends, so you
  never wait on the previous page; live per-page and session timers show
  what's happening at all times; delete, reorder (`J/K` or `←/→`), rotate
  (`R`), and rescan individual pages before building
- **Searchable PDFs** via [OCRmyPDF]/[Tesseract], multiple languages
  (default: English + German); PDF/A output with automatic page-rotation fix
- **Page cleanup**: deskew + border cleaning via unpaper (grayscale/lineart),
  with OCRmyPDF `--deskew --clean` fallback when unpaper didn't run
- **Diagnostics screen** (`!`) with install hints when something is missing —
  opens automatically if a preflight check fails; headless `--doctor` kept
- **Desktop notifications** (optional), always-on log file for debugging
- Keyboard-first (vim keys **and** arrows), mouse support (click to focus,
  scroll panes)

## Requirements

- Rust 1.88+ (to build; a prebuilt binary just needs the tools below)
- Linux with SANE (`sane`), HPLIP for HP devices (`hplip`)
- `ocrmypdf`, `tesseract` (+ language data), `img2pdf`
- `unpaper` (optional, recommended for grayscale scans)
- `libnotify` for notifications (optional)
- A SANE-compatible scanner (USB-only models like the Deskjet 1050a work fine)

## Install

Build from source:

```sh
git clone https://github.com/j-stechmann/auto-scanner-ocr
cd auto-scanner-ocr
cargo build --release
```

Put the binary on your PATH (optional):

```sh
ln -s "$PWD/target/release/auto-scanner-ocr" ~/.local/bin/auto-scanner-ocr
```

Arch dependencies (note: `ocrmypdf` is in the AUR, not the official repos):

```sh
sudo pacman -S --needed sane hplip tesseract tesseract-data-eng tesseract-data-deu img2pdf unpaper libnotify
yay -S ocrmypdf            # AUR; alternative without an AUR helper: uv tool install ocrmypdf
```

Debian/Ubuntu:

```sh
sudo apt install sane hplip ocrmypdf tesseract-ocr tesseract-ocr-eng tesseract-ocr-deu img2pdf unpaper libnotify
```

### Set up the scanner (once)

```sh
scanimage -L          # should list your device
# if not:
sudo hp-setup -i      # HP-specific setup, then try scanimage -L again
```

## Usage

```sh
auto-scanner-ocr              # start the TUI
auto-scanner-ocr --doctor     # check dependencies and scanner, then exit
auto-scanner-ocr --help       # all options
```

Inside the TUI:

| Key | Action |
|---|---|
| `s` / `Enter` | Scan next page (while the Pages pane is focused for Enter) |
| `Esc` / `c` | Cancel a running scan |
| `j`/`k` or `↑`/`↓` | Select page (Pages) · scroll (Text) |
| `J`/`K` or `←`/`→` | Move page down/up (Pages pane) |
| `r` | Rescan page (keeps the old image until the new scan succeeds) |
| `R` / `<` | Rotate page 90° clockwise / counter-clockwise |
| `d` | Delete page (kills the job if it's still processing) |
| `1`–`9` | Jump to page N |
| `f` | Build the searchable PDF (confirm dialog shows the output path) |
| `o` | Open the finished PDF with `xdg-open` |
| `n` | New session |
| `m` | Cycle scan mode: gray → color → lineart |
| `+`/`=` / `-` | DPI presets 150 / 200 / 300 / 600 |
| `L` | OCR language picker (lists installed tesseract data) |
| `Tab` / click | Cycle pane focus (Pages → Preview → Text) |
| `?` / `!` | Help / diagnostics (press `r` inside to re-run checks) |
| `q` / `Ctrl-C` | Quit (confirm if pages aren't saved) |

Typical session: put page 1 on the glass, press `s`, wait for the scan, put
page 2 on the glass, press `s` again — page 1 is already being cleaned and
OCRed in the background. Press `f` when done; the PDF lands in
`~/Documents/scans/` as `YYYY-MM-DD_HHMMSS.pdf`.

### Options

| Flag | Meaning | Default |
|---|---|---|
| `-d, --dpi N` | scan resolution | 300 |
| `-M, --mode` | `gray`, `color` or `lineart` | gray |
| `-l, --langs A+B` | OCR languages, plus-separated | eng+deu |
| `-o, --output DIR` | where PDFs are written | ~/Documents/scans |
| `-e, --device NAME` | SANE device name or substring | first found |
| `--no-unpaper` | skip the unpaper cleanup step | — |
| `--no-notify` | disable desktop notifications | — |
| `--config FILE` | use a specific config file | ./config.toml or ~/.config/auto-scanner-ocr/config.toml |
| `--doctor` | check dependencies and scanner, then exit | — |
| `-v, --verbose` | also print log output to the terminal | — |

### Configuration

Defaults live in `config.toml` (keep it next to the binary, or at
`~/.config/auto-scanner-ocr/config.toml`); every value can be overridden on
the command line. Same format as before:

```toml
[scan]
dpi = 300
mode = "gray"          # gray | color | lineart
langs = "deu"
device = "auto"
output = "~/Documents/scans"
unpaper = true
notify = true
```

**About `langs`**: tesseract degrades when languages are mixed — German
umlauts turn into ligature confusions (`für` → `fiir`, `Rück` → `Riick`,
`Grüßen` → `GruRen`) when `eng+deu` is combined. For monolingual documents
use a single language (`deu`, `eng`, …) and switch via the `L` picker or
`--langs` when you scan in another language. Measured on a real German
letter: `deu` alone produced zero broken umlauts; any mix produced several.

### Image preview protocols

The preview auto-detects the terminal's graphics protocol via a short-lived
probe process: kitty graphics and sixel render native images; everywhere
else it falls back to unicode halfblocks. Inside tmux the probe queries
raw (tmux swallows answers to its own passthrough-wrapped queries), so sixel
works in tmux too when the outer terminal (e.g. foot) supports it.
Detection never interferes with keyboard input — the probe runs in an
isolated child process.

## Troubleshooting

- Run `auto-scanner-ocr --doctor` — it checks every dependency, tesseract
  language data, scanner detection and the output directory, and prints
  install hints for anything missing. Inside the TUI, `!` shows the same
  checks with `r` to re-run them.
- The TUI runs the same checks at startup; if something is missing you'll see
  the diagnostics screen immediately instead of a mid-scan failure.
- **Wrong umlauts in the OCR text (`fiir` instead of `für`)?** Your `langs`
  mixes two languages — pick the document's language alone with `L` (or set
  `langs = "deu"` in the config). Mixed-language OCR reliably garbles
  umlauts; single-language OCR reads them correctly.
- Logs are written to `~/.local/state/auto-scanner-ocr/auto-scanner-ocr.log`;
  add `-v` to see them in the terminal.
- Scanner not found? Check USB, power, and `scanimage -L`. For HP devices run
  `sudo hp-setup -i` once.
- Scans left over from a crashed session live under
  `~/.local/state/auto-scanner-ocr/sessions/` and are safe to delete when no
  scan is running.

## The result

Each finish produces `YYYY-MM-DD_HHMMSS.pdf` in your output directory
(collision-safe `_2` suffixes): a PDF/A document whose visible layer is your
scans (deskewed and cleaned) and whose invisible text layer contains the OCR
result — searchable and copy-pastable in any PDF viewer. If ocrmypdf fails,
the PDF is still saved (without a text layer) and the TUI tells you so.

## License

GPL-2.0-or-later — see [LICENSE](LICENSE).

[OCRmyPDF]: https://github.com/ocrmypdf/OCRmyPDF
[Tesseract]: https://github.com/tesseract-ocr/tesseract