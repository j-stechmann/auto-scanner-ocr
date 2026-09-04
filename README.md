# Flatbed scan → searchable OCR PDF, in a terminal UI.

**auto-scanner-ocr** is a Rust TUI for Linux that drives a flatbed SANE
scanner (e.g. HP Deskjet 1050a) and turns your scans into **searchable
PDFs** — the page image plus an invisible OCR text layer, so you can select,
copy and Ctrl+F the text in any PDF viewer.

No daemons, no web servers: you run it in a terminal when you want to scan.

## Features

- **Live TUI**: page list with per-page status (scanning → cleaning → OCR →
  ready), image preview (kitty/sixel when your terminal supports it,
  halfblock fallback everywhere), extracted OCR text pane (`preview_ocr`:
  on-demand per page, per capture, or off — the PDF's text layer is
  unaffected)
- **Multi-page by default**: keep scanning while earlier pages clean in
  the background — the scanner is free as soon as a capture ends, so you
  never wait on the previous page; live per-page and session timers show
  what's happening at all times; delete, reorder (`J/K` or `←/→`), rotate
  (`R`), and rescan individual pages before building
- **Searchable PDFs** via [OCRmyPDF]/[Tesseract], multiple languages
  (default: English + German); PDF/A output with automatic page-rotation fix
- **Page cleanup**: ocrmypdf `--deskew --clean` at finish (default
  `cleanup = "off"`); optional unpaper modes — a content-safe conservative
  passthrough, or the legacy filter stack (which can erase page edges on
  flatbed scans)
- **Diagnostics screen** (`!`) with install hints when something is missing —
  opens automatically if a preflight check fails; headless `--doctor` kept
- **Desktop notifications** (optional), always-on log file for debugging
- Keyboard-first (vim keys **and** arrows), mouse support (click to focus,
  scroll panes)

## Requirements

- Rust 1.88+ (to build; a prebuilt binary just needs the tools below)
- Linux with SANE (`sane`), HPLIP for HP devices (`hplip`)
- `ocrmypdf`, `tesseract` (+ language data), `img2pdf`
- `poppler-utils` for `pdfunite` (only needed when one session mixes DPIs)
- `unpaper` (optional; only for the unpaper cleanup modes)
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
sudo pacman -S --needed sane hplip tesseract tesseract-data-eng tesseract-data-deu img2pdf poppler unpaper libnotify
yay -S ocrmypdf            # AUR; alternative without an AUR helper: uv tool install ocrmypdf
```

Debian/Ubuntu:

```sh
sudo apt install sane hplip ocrmypdf tesseract-ocr tesseract-ocr-eng tesseract-ocr-deu img2pdf poppler-utils unpaper libnotify
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
page 2 on the glass, press `s` again — the scanner is free as soon as page
1's capture ends (its text-pane OCR and cleanup continue in the background,
and under the default `preview_ocr = "lazy"` text is only extracted when
you view the page). Press `f` when done; the PDF lands in
`~/Documents/scans/` as `YYYY-MM-DD_HHMMSS.pdf`.

**Small print**: the default is 300 dpi — the OCR sweet spot, 2–3× faster
per capture and fine for normal 10–12pt text. For dense small print use
600 dpi (`-d 600` or the `+` key). Higher than 600 is pointless on
flatbeds: 1200 dpi is interpolated and can stall cheap USB scanners
mid-pass.

### Options

| Flag | Meaning | Default |
|---|---|---|
| `-d, --dpi N` | scan resolution | 300 |
| `-M, --mode` | `gray`, `color` or `lineart` | gray |
| `-l, --langs A+B` | OCR languages, plus-separated | deu+Latin |
| `-o, --output DIR` | where PDFs are written | ~/Documents/scans |
| `-e, --device NAME` | SANE device name or substring | first found |
| `--no-unpaper` | alias for `--cleanup off` | — |
| `--cleanup MODE` | page cleanup: `off`, `conservative` or `legacy` | off |
| `--preview-ocr MODE` | text-pane OCR: `eager` (after capture), `lazy` (on demand) or `off` | lazy |
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
dpi = 300              # OCR sweet spot; 600 for dense small print
mode = "gray"          # gray | color | lineart
langs = "deu+Latin"
device = "auto"
output = "~/Documents/scans"
cleanup = "off"        # off | conservative | legacy
preview_ocr = "lazy"   # lazy (on demand) | eager (after capture) | off
unpaper_extra_args = []  # extra unpaper argv when cleanup != off
notify = true
```

**About `cleanup`**: the default `off` skips unpaper entirely — ocrmypdf's
`--deskew --clean` (which runs unpaper internally with conservative
settings) does the real work once at finish. unpaper's own default filter
stack (`legacy`) is tuned for book scans: on flatbed scans its mask/border
detection misfires and can erase page edges (measured: a whole table wiped).
`conservative` disables every content-altering filter (a verified pixel-identical
passthrough, kept as a hook for `unpaper_extra_args`). Old configs with
`unpaper = true` map to `conservative` (with a deprecation warning);
`unpaper = false` maps to `off`.

**About `preview_ocr`**: this controls only the per-page OCR that fills the
"Extracted text" pane in the TUI — the final PDF always gets its searchable
text layer from ocrmypdf at finish, whatever you pick here.
`lazy` (the default) extracts text only for the page you are currently
viewing, on demand: flipping pages never runs tesseract you didn't ask for,
and scans never wait on OCR. `eager` OCRs every page right after capture so
the pane is always filled. `off` skips the pane entirely (rotate then also
skips its re-OCR).

**About `langs`**: tesseract degrades when language models are mixed — German
umlauts turn into ligature confusions (`für` → `fiir`, `Rück` → `Riick`,
`Grüßen` → `GruRen`) when `eng+deu` is combined. For monolingual documents
stick to German plus the `Latin` script model (the default `deu+Latin`) or a
single language (`deu`, `eng`, …) and switch via the `L` picker or `--langs`
when you scan in another language. Measured on a real German letter: `deu`
alone and `deu+Latin` both produced zero broken umlauts; any `eng` mix
produced several.

**About `Latin`**: `Latin` is a tesseract *script* model, not a language —
it fixes the `§` → `&` artifact in German legal citations (`& 115 SGB`
instead of `§ 115 SGB`) without hurting umlauts, at ~0.3s extra per page.
Debian/Ubuntu ship it (`sudo apt install tesseract-ocr-script-latn`); Arch
does not (its `tesseract-data-lat` is the Latin *language* model — a
different file that degrades umlauts in `deu+lat` mixes). On Arch, or when
your distro lacks the package, download the script model once — note that
tesseract's tessdata path varies (`tesseract --list-langs -v` shows where
your build looks):

```sh
curl -fsSL https://github.com/tesseract-ocr/tessdata_fast/raw/main/script/Latin.traineddata \
  | sudo tee /usr/share/tessdata/Latin.traineddata >/dev/null
```

Without it, `deu` alone misreads `§` as `&`/`8&`/`$` regardless of scan
quality (verified up to 600 dpi) — it's a model limitation, not an input
problem. `--doctor` prints the install hint when `Latin` is configured but
missing.

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
  mixes two language models — pick the document's language alone with `L`
  (or set `langs = "deu"` in the config). Mixed-language OCR reliably garbles
  umlauts; single-language OCR reads them correctly.
- **`§` read as `&`, `8&` or `$` (`& 115 SGB` instead of `§ 115 SGB`)?** The
  `deu` model alone misreads thin `§` glyphs as a model limitation — no scan
  setting fixes it (verified up to 600 dpi). Add the `Latin` script model:
  `langs = "deu+Latin"` and install the data file (see "About `Latin`"
  above).
- Logs are written to `~/.local/state/auto-scanner-ocr/auto-scanner-ocr.log`;
  add `-v` to see them in the terminal.
- Scanner not found? Check USB, power, and `scanimage -L`. For HP devices run
  `sudo hp-setup -i` once.
- Quitting with un-built pages deletes them (the confirm dialog means it),
  unless a PDF build was in flight — its session dir is then swept on the
  next startup. Crash or signal leftovers under
  `~/.local/state/auto-scanner-ocr/sessions/` are swept automatically on
  startup too: each session dir carries a lock file whose flock the kernel
  releases when the process dies, so dead owners are deleted immediately
  while a live instance's dir is never touched (even when suspended). This
  applies to instances of the same version; a session of the older
  (pre-lock) binary still running during an upgrade is not protected.

## The result

Each finish produces `YYYY-MM-DD_HHMMSS.pdf` in your output directory
(collision-safe `_2` suffixes): a PDF/A document whose visible layer is your
scans (deskewed and cleaned) and whose invisible text layer contains the OCR
result — searchable and copy-pastable in any PDF viewer. Pages keep the
scanner window size of their own capture (mixed-DPI sessions are merged with
`pdfunite`). If ocrmypdf fails, the PDF is still saved (without a text
layer) and the TUI tells you so.

## License

GPL-2.0-or-later — see [LICENSE](LICENSE).

[OCRmyPDF]: https://github.com/ocrmypdf/OCRmyPDF
[Tesseract]: https://github.com/tesseract-ocr/tesseract