//! External process execution helpers.
//!
//! Key parity points with the Python tool's `run()`:
//! - spawn errors ("command not found") become friendly errors
//! - stdout/stderr tails are captured for error messages and the log file
//! - per-binary timeouts
//!
//! Key tokio requirements:
//! - stderr MUST be drained concurrently while stdout is read, or the child
//!   can block once the stderr pipe buffer fills
//! - children are killed on drop and run in their own process group, so a
//!   cancelled scan takes down SANE helper processes too

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::log;

/// How much trailing stderr to keep for error messages (parity: last 5 lines).
const TAIL_LINES: usize = 5;
/// How many bytes of stdout/stderr to log at debug level (parity: 2000).
const LOG_TAIL_BYTES: usize = 2000;

#[derive(Debug, Default, Clone)]
pub struct Output {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub success: bool,
}

impl Output {
    /// Last `n` lines of stderr, decoded lossily (for fail_with_log-style messages).
    pub fn stderr_tail(&self, n: usize) -> String {
        let text = String::from_utf8_lossy(&self.stderr);
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }
}

#[derive(Debug)]
pub enum RunError {
    /// Binary not found (parity: "Required command not found: {cmd}").
    NotFound,
    /// Timed out after the given duration.
    Timeout(Duration),
    /// Cancelled via token.
    Cancelled,
    /// Exited with a non-zero status.
    Failed(i32),
    /// I/O failure while running (broken pipe etc).
    Io(std::io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::NotFound => write!(f, "not found"),
            RunError::Timeout(d) => write!(f, "timed out after {}s", d.as_secs()),
            RunError::Cancelled => write!(f, "cancelled"),
            RunError::Failed(rc) => write!(f, "exit status {rc}"),
            RunError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Run a command capturing stdout+stderr. `timeout=None` means no timeout
/// (parity: scans may run indefinitely). Success = rc 0.
pub async fn run(cmd: &[&str], timeout: Option<Duration>) -> Result<Output, RunError> {
    run_inner(cmd, timeout, None).await
}

/// Like [`run`], but abortable: when the token is cancelled the child (and
/// its process group) is killed and `RunError::Cancelled` is returned.
pub async fn run_cancellable(
    cmd: &[&str],
    timeout: Option<Duration>,
    token: &CancellationToken,
) -> Result<Output, RunError> {
    run_inner(cmd, timeout, Some(token)).await
}

async fn run_inner(
    cmd: &[&str],
    timeout: Option<Duration>,
    token: Option<&CancellationToken>,
) -> Result<Output, RunError> {
    debug!("run: {}", cmd.join(" "));
    let mut command = Command::new(cmd[0]);
    command
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        // Own process group: lets us kill SANE helper processes as a group.
        .process_group(0);

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::log_failure(cmd, &RunError::NotFound);
            return Err(RunError::NotFound);
        }
        Err(e) => {
            log::log_failure(
                cmd,
                RunError::Io(std::io::Error::new(e.kind(), e.to_string())),
            );
            return Err(RunError::Io(e));
        }
    };

    // Drain stderr concurrently so the child never blocks on a full pipe.
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let read_fut = async {
        let mut stdout_buf = Vec::new();
        stdout_pipe.read_to_end(&mut stdout_buf).await?;
        let stderr_buf = stderr_task.await.unwrap_or_else(|_| Vec::new());
        Ok((stdout_buf, stderr_buf))
    };

    let cancel_fut = async {
        match token {
            Some(t) => t.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };

    enum Outcome {
        Done(Result<(Vec<u8>, Vec<u8>), std::io::Error>),
        TimedOut(Duration),
    }

    let outcome = tokio::select! {
        r = async {
            match timeout {
                Some(d) => match tokio::time::timeout(d, read_fut).await {
                    Ok(r) => Outcome::Done(r),
                    Err(_) => Outcome::TimedOut(d),
                },
                None => Outcome::Done(read_fut.await),
            }
        } => r,
        _ = cancel_fut => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            log::log_failure(cmd, &RunError::Cancelled);
            return Err(RunError::Cancelled);
        }
    };

    let (stdout_buf, stderr_buf) = match outcome {
        Outcome::Done(Ok(v)) => v,
        Outcome::Done(Err(e)) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            log::log_failure(
                cmd,
                RunError::Io(std::io::Error::new(e.kind(), e.to_string())),
            );
            return Err(RunError::Io(e));
        }
        Outcome::TimedOut(d) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            log::log_failure(cmd, RunError::Timeout(d));
            return Err(RunError::Timeout(d));
        }
    };

    let status = child.wait().await;
    let (success, rc) = match &status {
        Ok(s) => (s.success(), s.code().unwrap_or(-1)),
        Err(_) => (false, -1),
    };
    if !success {
        warn!("{cmd:?} exited with {rc}");
    }

    let output = Output {
        stdout: stdout_buf,
        stderr: stderr_buf,
        success,
    };
    log::log_command(cmd, &output);
    Ok(output)
}

/// Like [`run`] but maps non-zero exit / NotFound / timeout to friendly errors.
pub async fn run_ok(cmd: &[&str], timeout: Option<Duration>) -> Result<Output> {
    let output = run(cmd, timeout).await.map_err(|e| map_err(cmd, e))?;
    if !output.success {
        return Err(anyhow!(
            "{} failed: {}",
            cmd[0],
            output.stderr_tail(TAIL_LINES)
        ));
    }
    Ok(output)
}

fn map_err(cmd: &[&str], e: RunError) -> anyhow::Error {
    match e {
        RunError::NotFound => anyhow!("Required command not found: {}", cmd[0]),
        RunError::Timeout(d) => anyhow!("Command timed out after {}s: {}", d.as_secs(), cmd[0]),
        RunError::Cancelled => anyhow!("cancelled"),
        RunError::Failed(rc) => anyhow!("{} failed (rc={rc})", cmd[0]),
        RunError::Io(err) => anyhow!("{} failed: {err}", cmd[0]),
    }
}

/// Contextual error with tail + log path (parity: fail_with_log).
pub fn fail_with_log(context: &str, output: &Output) -> anyhow::Error {
    let tail = output.stderr_tail(TAIL_LINES);
    let mut msg = format!("{context} failed");
    if !tail.is_empty() {
        msg.push_str(&format!(":\n{tail}"));
    }
    msg.push_str(&format!("\nFull log: {}", log::logfile().display()));
    anyhow!(msg)
}

/// Contextual error with tail + log path from a RunError.
pub fn fail_with_log_err(context: &str, cmd: &[&str], err: RunError) -> anyhow::Error {
    let mut msg = match err {
        RunError::NotFound => format!("{context} failed: Required command not found: {}", cmd[0]),
        RunError::Timeout(d) => format!(
            "{context} failed: {} timed out after {}s",
            cmd[0],
            d.as_secs()
        ),
        RunError::Cancelled => format!("{context} cancelled"),
        RunError::Failed(rc) => format!("{context} failed (rc={rc})"),
        RunError::Io(e) => format!("{context} failed: io error: {e}"),
    };
    msg.push_str(&format!("\nFull log: {}", log::logfile().display()));
    anyhow!(msg)
}

/// Truncate to the last N bytes for logging.
pub fn log_tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(LOG_TAIL_BYTES);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}
