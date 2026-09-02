//! Always-on file logging (parity: `~/.local/state/auto-scanner-ocr/auto-scanner-ocr.log`,
//! DEBUG level, created before config load). `--verbose` also mirrors to stderr.

use std::path::PathBuf;
use std::sync::OnceLock;

use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer as _;

use crate::config::PROGRAM;

static LOGFILE: OnceLock<PathBuf> = OnceLock::new();

/// Log file location (parity with Python's state_dir()/default_logfile()).
pub fn logfile() -> &'static PathBuf {
    LOGFILE.get_or_init(|| {
        let base = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join(PROGRAM).join(format!("{PROGRAM}.log"))
    })
}

/// Initialize tracing: DEBUG level to the log file (always), INFO+ to stderr
/// when `verbose`. File logging is best-effort: if the directory can't be
/// created we fall back to stderr-only logging rather than failing.
pub fn setup(verbose: bool) {
    let path = logfile();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    let file_layer = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok();

    // Per-layer filters: the file always logs at DEBUG (parity), the stderr
    // mirror follows RUST_LOG (default INFO). A single global EnvFilter would
    // clamp everything to the env default (ERROR when unset), hiding file
    // events - hence per-layer `with_filter`.
    let file_filter = tracing_subscriber::filter::Targets::new()
        .with_target("auto_scanner_ocr", tracing::Level::DEBUG);
    let stderr_filter = tracing_subscriber::filter::EnvFilter::from_default_env();

    let stderr_layer = verbose.then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_timer(chrono_local())
            .with_filter(stderr_filter)
    });

    let registry = tracing_subscriber::registry().with(stderr_layer);
    match file_layer {
        Some(file) => {
            let layer = tracing_subscriber::fmt::layer()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_timer(chrono_local())
                .with_filter(file_filter);
            registry.with(layer).init();
        }
        None => {
            registry.init();
            eprintln!(
                "warning: could not open log file for writing: {}",
                path.display()
            );
        }
    }

    tracing::info!("=== {} {} ===", PROGRAM, crate::config::VERSION);
}

fn chrono_local() -> ChronoLocal {
    ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f".to_string())
}

/// Record a completed command with output tails (parity with Python's run()).
pub fn log_command(cmd: &[&str], output: &crate::backend::process::Output) {
    tracing::debug!("{} rc-success={}", cmd.join(" "), output.success);
    if !output.stdout.is_empty() {
        tracing::debug!("stdout: {}", tail_bytes(&output.stdout));
    }
    if !output.stderr.is_empty() {
        tracing::debug!("stderr: {}", tail_bytes(&output.stderr));
    }
}

/// Record a failed command (spawn error / timeout).
pub fn log_failure(cmd: &[&str], err: impl std::fmt::Display) {
    tracing::debug!("{} failed: {err}", cmd.join(" "));
}

fn tail_bytes(bytes: &[u8]) -> String {
    const MAX: usize = 2000;
    let start = bytes.len().saturating_sub(MAX);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logfile_location() {
        let path = logfile();
        assert!(path.to_string_lossy().contains(PROGRAM));
        assert!(path.to_string_lossy().ends_with(&format!("{PROGRAM}.log")));
    }
}
