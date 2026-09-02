//! Desktop notifications via notify-send (optional, honors config + --no-notify).

use std::process::Stdio;

use tokio::process::Command;
use tracing::debug;

use crate::config::PROGRAM;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        }
    }
}

/// Fire a desktop notification. Fire-and-forget: errors are logged, never fatal.
/// Parity: `notify-send -a <prog> -u <urgency> <summary> <body>` with 10s timeout.
pub async fn notify(enabled: bool, summary: &str, body: &str, urgency: Urgency) {
    if !enabled {
        return;
    }
    // Pre-check existence to stay quiet when libnotify is absent.
    if crate::backend::which("notify-send").is_none() {
        debug!("notify-send not found; skipping notification: {summary}");
        return;
    }
    let res = Command::new("notify-send")
        .arg("-a")
        .arg(PROGRAM)
        .arg("-u")
        .arg(urgency.as_str())
        .arg(summary)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await;
    if let Err(e) = res {
        debug!("notify-send failed: {e}");
    }
}
