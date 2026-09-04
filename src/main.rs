//! Entry point: --doctor headless path and TUI startup.

use anyhow::{Context, Result};
use clap::Parser;
use ratatui::crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use ratatui::crossterm::execute;

use auto_scanner_ocr::check;
use auto_scanner_ocr::cli::{final_config, Cli};
use auto_scanner_ocr::log as ocrlog;
use auto_scanner_ocr::session;
use auto_scanner_ocr::tui;

fn main() {
    let cli = Cli::parse();
    // Always-on file logging first (parity: created before config load).
    ocrlog::setup(cli.verbose);

    let exit = run(cli);
    if let Err(e) = &exit {
        eprintln!("error: {e:#}");
    }
    std::process::exit(match &exit {
        Ok(code) => *code,
        Err(_) => 1,
    });
}

fn run(cli: Cli) -> Result<i32> {
    // Hidden image-protocol probe: query the terminal in THIS short-lived
    // process so any orphaned stdin reader dies with us. The parent TUI
    // spawns this with piped stdio and parses the result.
    if cli.image_probe {
        return run_image_probe();
    }

    let cfg = final_config(&cli)?;

    if cli.doctor {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let report = rt.block_on(check::run_checks(&cfg));
        check::print_doctor(&cfg, &report);
        return Ok(if report.ok() { 0 } else { 1 });
    }

    // TUI path.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building tokio runtime")?;

    rt.block_on(async_tui(cfg))
}

/// Probe the terminal for graphics protocol + font size, print one result
/// line to STDERR and exit. STDOUT must stay untouched by us because
/// ratatui-image writes its protocol queries to `io::stdout()` — those bytes
/// must reach the terminal to get any answer. The parent captures only our
/// stderr line; the terminal consumes the query/response traffic itself.
/// Runs before any crossterm usage; raw mode is entered/restored by the
/// ratatui-image query itself.
fn run_image_probe() -> Result<i32> {
    use ratatui_image::picker::cap_parser::QueryStdioOptions;
    // 300ms silence window instead of the crate's 2s default: the crate
    // treats the timeout as an inactivity timer (it restarts on every
    // received chunk), so real terminals that answer in a burst are
    // unaffected — only silent terminals fall back to halfblocks sooner.
    // Keep ..Default::default(): enabling the extra OSC-11 / text-sizing
    // queries would add more response bytes and make short windows risky.
    let opts = QueryStdioOptions {
        timeout: std::time::Duration::from_millis(300),
        ..QueryStdioOptions::default()
    };
    let line = match ratatui_image::picker::Picker::from_query_stdio_with_options(opts) {
        Ok(picker) => {
            let proto = match picker.protocol_type() {
                ratatui_image::picker::ProtocolType::Kitty => "kitty",
                ratatui_image::picker::ProtocolType::Sixel => "sixel",
                ratatui_image::picker::ProtocolType::Iterm2 => "iterm2",
                ratatui_image::picker::ProtocolType::Halfblocks => "halfblocks",
            };
            let fs = picker.font_size();
            format!("protocol={proto} font={}x{}", fs.width, fs.height)
        }
        Err(_) => "protocol=halfblocks font=10x20".to_string(),
    };
    eprintln!("{line}");
    Ok(0)
}

async fn async_tui(cfg: auto_scanner_ocr::config::Config) -> Result<i32> {
    // Background preflight FIRST (before the probe child below): the slow
    // scanner detection (scanimage -L, up to ~30s worst case) must overlap
    // with the probe and with TUI drawing. It streams results into the
    // report inbox: the fast report (PATH lookups, ms-scale) lands first,
    // the full report once scanimage -L resolves. The TUI is painted
    // meanwhile; scanning stays gated until the device is known.
    let (report_tx, report_rx) = tokio::sync::mpsc::channel(4);
    {
        let cfg = cfg.clone();
        let report_tx = report_tx.clone();
        tokio::spawn(async move {
            // Fast half immediately (ms-scale; sets the Pending scanner row).
            let fast = check::run_checks_fast(&cfg);
            let _ = report_tx.send(fast.clone()).await;
            // Slow half (langs + scanimage -L, concurrently) once, then
            // merge the fast rows into it for the final report.
            let slow = check::run_checks_slow(&cfg).await;
            let _ = report_tx.send(check::merge_fast_slow(&fast, slow)).await;
        });
    }

    // Probe the terminal image protocol in a short-lived child process.
    // Doing the ratatui-image stdio query in-process breaks crossterm input:
    // the query's orphaned stdin reader eats keystrokes when the terminal
    // never answers, then restores cooked termios mid-session. The child
    // isolates that failure mode (its orphan thread dies with the process).
    // Budget ~300ms (+ spawn overhead); any failure falls back to
    // halfblocks. Synchronous spawn is fine here: the multi-thread runtime
    // keeps the preflight task running on other workers.
    let (picker, picker_available) = image_probe_picker();

    let terminal = ratatui::try_init().context("initializing terminal")?;
    execute!(std::io::stdout(), EnableMouseCapture)?;

    let init = tui::TuiInit {
        cfg: cfg.clone(),
        picker,
        picker_available,
        report_tx,
        report_rx,
    };

    // One session actor, always spawned up front with an empty device; the
    // background detection delivers the resolved device via Cmd::SetDevice
    // once scanimage -L resolves (its dir setup/flock/output reservation
    // are device-independent and stay at launch).
    let (cmd_tx, event_rx) = session::spawn(cfg.clone(), String::new())?;
    let result = tui::run_tui(terminal, init, event_rx, cmd_tx)
        .await
        .map(|startup_ok| if startup_ok { 0 } else { 1 });

    execute!(std::io::stdout(), DisableMouseCapture)?;
    ratatui::restore();
    result_or_interrupt(result)
}

/// Probe the terminal via a short-lived child process (`--image-probe`).
/// Returns (picker, native_protocol_available). Silence budget ~300ms plus
/// spawn overhead; any failure falls back to halfblocks.
///
/// tmux caveat (measured): ratatui-image wraps its queries in a tmux DCS
/// passthrough (`\ePtmux;…\e\\`) when TERM starts with "tmux", and tmux then
/// swallows the responses — the probe always ends up halfblocks. tmux itself,
/// however, answers raw queries and reports the outer terminal's capabilities
/// (e.g. sixel) when `terminal-features` includes them. So the probe child
/// runs with the tmux prefix stripped from TERM, forcing the crate's raw
/// query path, which works both inside and outside tmux.
fn image_probe_picker() -> (ratatui_image::picker::Picker, bool) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return (tui::halfblocks_picker(), false),
    };
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--image-probe")
        .stdin(std::process::Stdio::inherit()) // the probe must query OUR tty
        // stdout = our tty (inherit): ratatui-image writes its protocol
        // queries there and the terminal's responses go back via stdin.
        .stdout(std::process::Stdio::inherit())
        // The probe's RESULT line goes to stderr; that's all we capture.
        .stderr(std::process::Stdio::piped());
    // Force the crate's raw (non-DCS-wrapped) query inside tmux: both env
    // markers must be cleared — the user's shell exports TERM_PROGRAM=tmux
    // even to child processes, and the crate checks TERM *and* TERM_PROGRAM.
    let term = std::env::var("TERM").unwrap_or_default();
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    if term.starts_with("tmux") || term_program == "tmux" {
        cmd.env("TERM", "xterm-256color");
        cmd.env("TERM_PROGRAM", "xterm");
        // tmux also needs passthrough enabled to relay the queries; enabling
        // it per-pane is harmless when already set.
        let _ = std::process::Command::new("tmux")
            .args(["set", "-p", "allow-passthrough", "on"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    match cmd.output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stderr);
            parse_probe_line(&text).unwrap_or_else(|| (tui::halfblocks_picker(), false))
        }
        Err(_) => (tui::halfblocks_picker(), false),
    }
}

fn parse_probe_line(text: &str) -> Option<(ratatui_image::picker::Picker, bool)> {
    use ratatui_image::picker::{Picker, ProtocolType};
    let line = text.lines().next()?;
    let mut protocol = "halfblocks";
    let mut font = (10u16, 20u16);
    for part in line.split_whitespace() {
        if let Some(v) = part.strip_prefix("protocol=") {
            protocol = v;
        }
        if let Some(v) = part.strip_prefix("font=") {
            let mut it = v.split('x');
            if let (Some(w), Some(h)) = (it.next(), it.next()) {
                font = (w.parse().ok()?, h.parse().ok()?);
            }
        }
    }
    let picker = match protocol {
        "kitty" | "sixel" | "iterm2" => {
            // Reconstruct a picker from the probe's out-of-band detection:
            // from_fontsize is the crate-supported way to do this (fields
            // are private); the deprecation targets naive in-process use.
            #[allow(deprecated)]
            let mut p = Picker::from_fontsize(ratatui_image::FontSize::new(font.0, font.1));
            p.set_protocol_type(match protocol {
                "kitty" => ProtocolType::Kitty,
                "sixel" => ProtocolType::Sixel,
                _ => ProtocolType::Iterm2,
            });
            p
        }
        _ => Picker::halfblocks(),
    };
    let native = protocol != "halfblocks";
    Some((picker, native))
}

fn result_or_interrupt(result: Result<i32>) -> Result<i32> {
    match result {
        Ok(code) => Ok(code),
        Err(e) => {
            if e.to_string().contains("terminal event error") {
                Ok(130)
            } else {
                Err(e)
            }
        }
    }
}
