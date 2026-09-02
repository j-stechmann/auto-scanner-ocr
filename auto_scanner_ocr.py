#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
#
# auto-scanner-ocr: flatbed scan -> cleaned, searchable PDF with OCR text layer.
# Triggered manually from the terminal; no daemons or hotkeys.
#
# Copyright (C) 2026 Jonathan
#
# This program is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program. If not, see <https://www.gnu.org/licenses/>.

from __future__ import annotations

import argparse
import logging
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
from pathlib import Path

VERSION = "0.1.0"
PROGRAM = "auto-scanner-ocr"

SCAN_MODES = {"gray": "Gray", "color": "Color", "lineart": "Lineart"}

BIN_HINTS = {
    "scanimage": ("SANE (scanner access)", {
        "pacman": "sudo pacman -S sane hplip",
        "apt": "sudo apt install sane hplip",
    }),
    "ocrmypdf": ("OCRmyPDF (searchable PDFs)", {
        "pacman": "yay -S ocrmypdf   # AUR, not in official repos (or: uv tool install ocrmypdf)",
        "apt": "sudo apt install ocrmypdf",
    }),
    "tesseract": ("Tesseract OCR engine", {
        "pacman": "sudo pacman -S tesseract",
        "apt": "sudo apt install tesseract",
    }),
    "img2pdf": ("img2pdf (lossless image-to-PDF, ships with ocrmypdf on most distros)", {
        "pacman": "sudo pacman -S img2pdf",
        "apt": "sudo apt install img2pdf",
    }),
    "unpaper": ("unpaper (deskew/clean, optional)", {
        "pacman": "sudo pacman -S unpaper",
        "apt": "sudo apt install unpaper",
    }),
    "notify-send": ("libnotify (desktop notifications, optional)", {
        "pacman": "sudo pacman -S libnotify",
        "apt": "sudo apt install libnotify",
    }),
}

DEFAULTS = {
    "dpi": 300,
    "mode": "gray",
    "langs": "eng+deu",
    "device": "auto",
    "output": "~/Documents/scans",
    "unpaper": True,
    "notify": True,
}

log = logging.getLogger(PROGRAM)


# ---------------------------------------------------------------- utilities

class UserError(Exception):
    """Fatal, user-facing error with a clean message (no traceback)."""


def expand(path: str) -> Path:
    return Path(os.path.expanduser(os.path.expandvars(path)))


def find_config(explicit: str | None) -> Path | None:
    if explicit:
        p = expand(explicit)
        if not p.is_file():
            raise UserError(f"Config file not found: {p}")
        return p
    candidates = [
        Path.cwd() / "config.toml",
        Path.home() / ".config" / PROGRAM / "config.toml",
    ]
    for c in candidates:
        if c.is_file():
            return c
    return None


def load_config(explicit: str | None) -> dict:
    cfg = dict(DEFAULTS)
    path = find_config(explicit)
    if path is None:
        return cfg
    try:
        with open(path, "rb") as fh:
            data = tomllib.load(fh)
    except tomllib.TOMLDecodeError as exc:
        raise UserError(f"Invalid TOML in {path}: {exc}") from exc
    section = data.get("scan", data)
    for key in DEFAULTS:
        if key in section:
            cfg[key] = section[key]
    log.info("Loaded config from %s", path)
    return cfg


def notify(args, summary: str, body: str = "", urgency: str = "normal") -> None:
    if args.no_notify or not shutil.which("notify-send"):
        return
    try:
        subprocess.run(
            ["notify-send", "-a", PROGRAM, "-u", urgency, summary, body],
            timeout=10,
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except subprocess.TimeoutExpired:
        pass


def run(cmd: list[str], timeout: int | None = None, quiet: bool = False) -> subprocess.CompletedProcess:
    log.debug("Running: %s", " ".join(cmd))
    try:
        proc = subprocess.run(
            cmd,
            capture_output=True,
            timeout=timeout,
        )
    except FileNotFoundError as exc:
        raise UserError(f"Required command not found: {cmd[0]}") from exc
    except subprocess.TimeoutExpired as exc:
        raise UserError(f"Command timed out after {timeout}s: {cmd[0]}") from exc
    if proc.stdout and not quiet:
        log.debug("stdout: %s", proc.stdout[-2000:])
    if proc.stderr:
        log.debug("stderr: %s", proc.stderr[-2000:])
    return proc


def fail_with_log(context: str, proc: subprocess.CompletedProcess | None = None) -> UserError:
    tail = ""
    if proc is not None and proc.stderr:
        tail = proc.stderr.decode(errors="replace").strip().splitlines()
        tail = "\n".join(tail[-5:])
    log_path = default_logfile()
    msg = f"{context} failed"
    if tail:
        msg += f":\n{tail}"
    msg += f"\nFull log: {log_path}"
    return UserError(msg)


def state_dir() -> Path:
    d = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local" / "state")) / PROGRAM
    d.mkdir(parents=True, exist_ok=True)
    return d


def default_logfile() -> Path:
    return state_dir() / f"{PROGRAM}.log"


def setup_logging(verbose: bool) -> None:
    logfile = default_logfile()
    handlers: list[logging.Handler] = [logging.FileHandler(logfile)]
    if verbose:
        handlers.append(logging.StreamHandler(sys.stderr))
    logging.basicConfig(
        level=logging.DEBUG,
        format="%(asctime)s %(levelname)s %(message)s",
        handlers=handlers,
    )
    log.info("=== %s %s ===", PROGRAM, VERSION)


# ---------------------------------------------------------------- preflight

def check_binaries(cfg: dict) -> tuple[list[str], list[str]]:
    """Return (errors, warnings) about missing binaries."""
    errors: list[str] = []
    warnings: list[str] = []
    for name, (what, hints) in BIN_HINTS.items():
        if name == "unpaper" and (not cfg["unpaper"] or shutil.which(name)):
            continue
        if name == "notify-send":
            if not shutil.which(name):
                warnings.append(f"notify-send not found - desktop notifications disabled "
                                f"(install with: {hints['pacman']})")
            continue
        if not shutil.which(name):
            hint = " / ".join(hints.values())
            errors.append(f"missing required tool '{name}' ({what})\n  install: {hint}")
    return errors, warnings


def tesseract_available_langs() -> set[str]:
    proc = run(["tesseract", "--list-langs"], timeout=15, quiet=True)
    langs = set()
    for line in proc.stdout.decode(errors="replace").splitlines():
        line = line.strip()
        if line and line.lower() != "list of available languages":
            langs.add(line)
    return langs


def check_ocr_languages(cfg: dict) -> list[str]:
    errors = []
    wanted = [l for l in cfg["langs"].split("+") if l]
    have = tesseract_available_langs()
    missing = [l for l in wanted if l not in have]
    if missing:
        pkg = " ".join(f"tesseract-data-{m}" for m in missing)
        errors.append(
            f"tesseract language data missing: {', '.join(missing)}\n"
            f"  install: sudo pacman -S {pkg}   (Debian: tesseract-ocr-{missing[0]})"
        )
    return errors


def find_scanner(cfg: dict) -> str | None:
    try:
        proc = run(["scanimage", "-L"], timeout=30, quiet=True)
    except UserError:
        return None
    out = proc.stdout.decode(errors="replace")
    devices = re.findall(r"device `([^']+)'", out)
    if proc.returncode != 0 and not devices:
        return None
    if not devices:
        return None
    wanted = cfg["device"]
    if wanted != "auto":
        for dev in devices:
            if wanted in dev:
                return dev
        return None
    return devices[0]


def check_scanner(cfg: dict) -> list[str]:
    errors = []
    device = find_scanner(cfg)
    if device is None:
        errors.append(
            "no scanner detected (scanimage -L found nothing)\n"
            "  - is the scanner plugged in and powered on (USB only on supported models)?\n"
            "  - is hplip installed and set up? try: sudo hp-setup -i\n"
            "  - test manually: scanimage -L"
        )
    else:
        log.info("Scanner: %s", device)
    return errors


def check_output_dir(cfg: dict) -> list[str]:
    errors = []
    out = expand(cfg["output"])
    try:
        out.mkdir(parents=True, exist_ok=True)
        if not os.access(out, os.W_OK):
            raise PermissionError(out)
    except OSError as exc:
        errors.append(f"output directory not writable: {out}\n  {exc}")
    return errors


def doctor(cfg: dict) -> int:
    print(f"{PROGRAM} {VERSION} - dependency and environment check\n")
    print("Python:", sys.version.split()[0], "(requires 3.11+)")
    if sys.version_info < (3, 11):
        print("  [FAIL] Python 3.11+ required (tomllib)")
        return 1

    all_errors: list[str] = []

    print("\nTools:")
    errors, warnings = check_binaries(cfg)
    for name in ("scanimage", "ocrmypdf", "tesseract", "img2pdf", "unpaper", "notify-send"):
        if name == "unpaper" and not cfg["unpaper"]:
            print(f"  [SKIP] {name} (disabled in config)")
            continue
        if name == "notify-send":
            print(f"  [{' OK' if shutil.which(name) else 'WARN'}] {name}"
                  + ("" if shutil.which(name) else " (notifications disabled)"))
            continue
        status = " OK" if shutil.which(name) else "FAIL"
        print(f"  [{status}] {name}")

    print("\nTesseract languages:")
    errors.extend(check_ocr_languages(cfg))
    wanted = [l for l in cfg["langs"].split("+") if l]
    have = tesseract_available_langs()
    for lang in wanted:
        print(f"  [{' OK' if lang in have else 'FAIL'}] {lang}")

    print("\nScanner:")
    errors.extend(check_scanner(cfg))
    device = find_scanner(cfg)
    print(f"  [{' OK' if device else 'FAIL'}] {device or 'no device found'}")

    print("\nOutput directory:")
    errors.extend(check_output_dir(cfg))
    out = expand(cfg["output"])
    print(f"  [{' OK' if out.is_dir() and os.access(out, os.W_OK) else 'FAIL'}] {out}")

    if warnings:
        print("\nWarnings:")
        for w in warnings:
            print(f"  - {w}")
    if errors:
        print("\nProblems:")
        for e in errors:
            print(f"  - {e}")
        print("\nFix the problems above, then re-run with --doctor.")
        return 1
    print("\nAll checks passed. Ready to scan.")
    return 0


def preflight(cfg: dict) -> str:
    """Run all checks for a normal scan; returns detected scanner device string."""
    errors, warnings = check_binaries(cfg)
    errors.extend(check_ocr_languages(cfg))
    if errors:
        raise UserError(
            "Startup check failed:\n  - " + "\n  - ".join(errors)
            + "\nRun 'auto_scanner_ocr.py --doctor' after fixing."
        )
    for w in warnings:
        log.warning("%s", w)

    errors = check_output_dir(cfg)
    if errors:
        raise UserError("Startup check failed:\n  - " + "\n  - ".join(errors))

    device = find_scanner(cfg)
    if device is None:
        raise UserError(
            "No scanner detected.\n"
            "  - is the scanner plugged in and powered on?\n"
            "  - try: sudo hp-setup -i   then: scanimage -L"
        )
    return device


# ---------------------------------------------------------------- pipeline

def scan_page(device: str, cfg: dict, out_png: Path) -> None:
    """Capture one page from the scanner into out_png."""
    args = [
        "scanimage", "-d", device,
        "--format=png",
        f"--resolution={cfg['dpi']}",
        f"--mode={SCAN_MODES[cfg['mode']]}",
    ]
    candidates = [args, args[:5], args[:3]]  # progressively drop mode/resolution
    last = None
    for attempt in candidates:
        proc = run(attempt, timeout=None)
        if proc.returncode == 0 and proc.stdout:
            out_png.write_bytes(proc.stdout)
            return
        last = proc
        log.warning("scanimage attempt failed (rc=%s), retrying with fewer options",
                    proc.returncode)
    raise fail_with_log("Scanning", last)


def maybe_unpaper(page_png: Path, cfg: dict) -> Path:
    """Deskew/clean with unpaper when enabled; returns path to use downstream."""
    if not cfg["unpaper"] or not shutil.which("unpaper"):
        return page_png
    if cfg["mode"] == "color":
        # unpaper works in grayscale and would destroy color scans
        log.info("unpaper skipped for color mode")
        return page_png
    cleaned = page_png.with_name(page_png.stem + "_clean.png")
    proc = run(
        ["unpaper", "--layout", "single",
         "--deskew-scan-direction", "left,right",
         str(page_png), str(cleaned)],
        timeout=120,
    )
    if proc.returncode == 0 and cleaned.exists():
        page_png.unlink(missing_ok=True)
        return cleaned
    log.warning("unpaper failed (rc=%s); using raw scan", proc.returncode)
    cleaned.unlink(missing_ok=True)
    return page_png


def build_pdf(images: list[Path], out_pdf: Path, cfg: dict, args) -> None:
    """img2pdf (lossless) -> ocrmypdf (text layer, deskew/clean fallbacks)."""
    with tempfile.TemporaryDirectory(prefix=f"{PROGRAM}-") as tmp:
        raw_pdf = Path(tmp) / "raw.pdf"
        proc = run(["img2pdf", *[str(i) for i in images], "-o", str(raw_pdf)], timeout=300)
        if proc.returncode != 0:
            raise fail_with_log("PDF assembly (img2pdf)", proc)

        ocr_args = [
            "ocrmypdf",
            "--language", cfg["langs"],
            "--output-type", "pdfa",
            "--optimize", "0",
        ]
        if not cfg["unpaper"] or cfg["mode"] == "color":
            # no unpaper pass happened -> let ocrmypdf deskew/clean
            ocr_args += ["--deskew", "--clean"]
        ocr_args += [str(raw_pdf), str(out_pdf)]

        proc = run(ocr_args, timeout=1800)
        if proc.returncode != 0:
            log.error("ocrmypdf failed (rc=%s); saving PDF without text layer", proc.returncode)
            shutil.copyfile(raw_pdf, out_pdf)
            notify(args, "OCR failed", f"Saved without text layer:\n{out_pdf}", urgency="critical")


def unique_path(base: Path) -> Path:
    if not base.exists():
        return base
    n = 2
    while True:
        cand = base.with_name(f"{base.stem}_{n}{base.suffix}")
        if not cand.exists():
            return cand
        n += 1


# ---------------------------------------------------------------- sessions

def scan_session(args, cfg: dict, device: str) -> int:
    out_dir = expand(cfg["output"])
    stamp = datetime.now().strftime("%Y-%m-%d_%H%M%S")
    final_pdf = unique_path(out_dir / f"{stamp}.pdf")

    with tempfile.TemporaryDirectory(prefix=f"{PROGRAM}-") as tmpname:
        tmp = Path(tmpname)
        pages: list[tuple[int, Path]] = []
        errors: list[str] = []
        pool = ThreadPoolExecutor(max_workers=1)
        futures = []

        def process(idx: int, raw: Path) -> None:
            try:
                pages.append((idx, maybe_unpaper(raw, cfg)))
            except Exception as exc:  # noqa: BLE001 - report, don't crash the session
                log.exception("page %d processing failed", idx)
                errors.append(f"page {idx}: {exc}")

        def submit(idx: int, raw: Path) -> None:
            futures.append(pool.submit(process, idx, raw))

        try:
            if args.multi:
                print("Multi-page mode: place page 1 on the scanner glass.")
                idx = 1
                while True:
                    answer = input(f"[page {idx}] press Enter to scan, 'q'+Enter to finish: ").strip().lower()
                    if answer == "q":
                        break
                    notify(args, f"Scanning page {idx}…")
                    raw = tmp / f"page_{idx:03d}.png"
                    try:
                        scan_page(device, cfg, raw)
                    except UserError as exc:
                        print(f"\nScan failed: {exc}", file=sys.stderr)
                        notify(args, "Scan failed", str(exc), urgency="critical")
                        continue
                    print(f"  page {idx} captured - processing in background…")
                    submit(idx, raw)
                    idx += 1
                    print(f"Place page {idx} on the glass (or 'q' to finish).")
            else:
                print("Place the page on the scanner glass.")
                input("press Enter to start scanning…")
                notify(args, "Scanning…")
                raw = tmp / "page_001.png"
                scan_page(device, cfg, raw)
                print("  page captured - processing…")
                submit(1, raw)

            pool.shutdown(wait=True)
            if errors:
                for e in errors:
                    print(f"error: {e}", file=sys.stderr)
                raise UserError("One or more pages failed to process; nothing saved.")

            if not pages:
                print("No pages scanned - nothing to do.")
                return 0

            pages.sort(key=lambda p: p[0])
            images = [p[1] for p in pages]
            print(f"Building searchable PDF ({len(images)} page(s), OCR langs: {cfg['langs']})…")
            notify(args, "Processing", "Running OCR, this can take a moment…")
            build_pdf(images, final_pdf, cfg, args)
        finally:
            pool.shutdown(wait=False, cancel_futures=True)

    size_kb = final_pdf.stat().st_size // 1024
    print(f"Done: {final_pdf} ({size_kb} KB)")
    notify(args, "Scan complete", str(final_pdf))
    return 0


# ---------------------------------------------------------------- CLI

def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog=PROGRAM,
        description="Scan with a flatbed SANE scanner and produce a searchable OCR PDF.",
    )
    p.add_argument("-m", "--multi", action="store_true",
                   help="multi-page session: scan several pages, merge into one PDF")
    p.add_argument("--dpi", type=int, metavar="N", help="scan resolution (default 300)")
    p.add_argument("--mode", choices=sorted(SCAN_MODES), help="scan mode (default gray)")
    p.add_argument("--langs", metavar="A+B", help="OCR languages, plus-separated (default eng+deu)")
    p.add_argument("--output", metavar="DIR", help="output directory (default ~/Documents/scans)")
    p.add_argument("--device", metavar="NAME", help="SANE device name or substring (default: first found)")
    p.add_argument("--config", metavar="FILE", help="config file to use (default: ./config.toml)")
    p.add_argument("--no-unpaper", action="store_true", help="skip unpaper cleanup step")
    p.add_argument("--no-notify", action="store_true", help="disable desktop notifications")
    p.add_argument("--doctor", action="store_true", help="check dependencies and environment, then exit")
    p.add_argument("--verbose", action="store_true", help="also print log output to the terminal")
    p.add_argument("--version", action="version", version=f"{PROGRAM} {VERSION}")
    return p.parse_args(argv)


def apply_overrides(args, cfg: dict) -> dict:
    if args.dpi:
        cfg["dpi"] = args.dpi
    if args.mode:
        cfg["mode"] = args.mode
    if args.langs:
        cfg["langs"] = args.langs
    if args.output:
        cfg["output"] = args.output
    if args.device:
        cfg["device"] = args.device
    if args.no_unpaper:
        cfg["unpaper"] = False
    if cfg["dpi"] < 150:
        raise UserError("--dpi must be >= 150 for usable OCR results")
    if cfg["mode"] not in SCAN_MODES:
        raise UserError(f"invalid mode '{cfg['mode']}' (use: {', '.join(SCAN_MODES)})")
    if not re.fullmatch(r"[a-z_]+(\+[a-z_]+)*", cfg["langs"]):
        raise UserError(f"invalid langs '{cfg['langs']}' (use plus-separated codes, e.g. eng+deu)")
    return cfg


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    setup_logging(args.verbose)

    try:
        cfg = apply_overrides(args, load_config(args.config))

        if args.doctor:
            return doctor(cfg)

        device = preflight(cfg)
        return scan_session(args, cfg, device)
    except UserError as exc:
        print(f"error: {exc}", file=sys.stderr)
        notify(args, PROGRAM, str(exc), urgency="critical")
        return 1
    except KeyboardInterrupt:
        print("\nInterrupted.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    signal.signal(signal.SIGINT, signal.default_int_handler)
    sys.exit(main(sys.argv[1:]))