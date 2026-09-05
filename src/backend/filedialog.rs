//! System file-save dialogs (zenity / kdialog / yad): the terminal TUI
//! delegates path choice to the desktop's native picker instead of
//! reimplementing a file browser. All three tools offer directory
//! navigation, a filename field and a built-in overwrite prompt.
//!
//! Fallback: when none is installed the caller shows the plain confirm
//! dialog (default timestamped path), so the finish flow never breaks.

use std::path::{Path, PathBuf};

use tokio::process::Command;

/// How the dialog tool was invoked (exposed for tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveTool {
    Zenity,
    Kdialog,
    Yad,
}

impl SaveTool {
    pub fn binary(self) -> &'static str {
        match self {
            SaveTool::Zenity => "zenity",
            SaveTool::Kdialog => "kdialog",
            SaveTool::Yad => "yad",
        }
    }

    fn args(self, dir: &Path, filename: &str, title: &str) -> Vec<String> {
        let dir = dir.to_string_lossy().into_owned();
        let title = title.to_string();
        match self {
            SaveTool::Zenity => vec![
                "--file-selection".into(),
                "--save".into(),
                "--confirm-overwrite".into(),
                format!("--filename={filename}"),
                format!("--title={title}"),
                format!("--file-filter={ALL_FILTER}"),
            ],
            SaveTool::Kdialog => {
                let mut args = vec!["--getsavefilename".into(), dir, ALL_FILTER.to_string()];
                if !title.is_empty() {
                    args.push("--title".into());
                    args.push(title);
                }
                args
            }
            SaveTool::Yad => vec![
                "--file-selection".into(),
                "--save".into(),
                "--confirm-overwrite".into(),
                format!("--filename={filename}"),
                format!("--title={title}"),
                format!("--file-filter={ALL_FILTER}"),
            ],
        }
    }
}

/// Shared filter: everything (the dialog is for choosing a save location,
/// not filtering scan output).
const ALL_FILTER: &str = "PDF files (*.pdf) | *.pdf";

/// Which save dialog tool is available, in preference order (zenity is the
/// most common on GTK desktops, kdialog on KDE, yad as fallback).
pub fn available_tool() -> Option<SaveTool> {
    [SaveTool::Zenity, SaveTool::Kdialog, SaveTool::Yad]
        .into_iter()
        .find(|t| crate::backend::which(t.binary()).is_some())
}

/// Dialog exit codes: 0 = chosen, 1 = cancelled. zenity also uses 1 for
/// "no display" style errors — treat both as cancellation (the caller then
/// keeps the default path; nothing destructive happens either way).
const CANCELLED: i32 = 1;

/// Open a native save dialog. Returns None on cancel (or when the dialog
/// could not run, e.g. no display). The pre-set directory/filename seed the
/// dialog; the user may change both.
pub async fn save_dialog(dir: &Path, filename: &str, title: &str) -> Option<PathBuf> {
    let tool = available_tool()?;
    let mut cmd = Command::new(tool.binary());
    cmd.args(tool.args(dir, filename, title))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // Never inherit the terminal: the TUI is in raw mode and a dialog
    // writing to it would corrupt the screen.
    let res = cmd.output().await;
    let out = match res {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!("{} failed: {e}", tool.binary());
            return None;
        }
    };
    if !out.status.success() {
        let code = out.status.code().unwrap_or(CANCELLED);
        if code != CANCELLED {
            tracing::warn!("{} exited with {code}", tool.binary());
        }
        return None;
    }
    let chosen = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if chosen.is_empty() {
        return None;
    }
    Some(PathBuf::from(chosen))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_shape_per_tool() {
        let dir = Path::new("/tmp/out");
        let zenity = SaveTool::Zenity.args(dir, "a.pdf", "Title");
        assert!(zenity.contains(&"--save".to_string()));
        assert!(zenity.contains(&"--confirm-overwrite".to_string()));
        assert!(zenity.contains(&"--filename=a.pdf".to_string()));

        let kd = SaveTool::Kdialog.args(dir, "a.pdf", "Title");
        assert!(kd
            .windows(2)
            .any(|w| w == ["--getsavefilename", "/tmp/out"]));

        let yad = SaveTool::Yad.args(dir, "a.pdf", "Title");
        assert!(yad.contains(&"--save".to_string()));
    }

    #[test]
    fn available_tool_prefers_zenity_when_present() {
        // Purely observational (machine-dependent): never fails, only
        // checks the ordering is stable.
        let order = [SaveTool::Zenity, SaveTool::Kdialog, SaveTool::Yad];
        for (i, t) in order.iter().enumerate() {
            if crate::backend::which(t.binary()).is_some() {
                assert_eq!(available_tool(), Some(*t));
                let _ = i;
                break;
            }
        }
    }
}
