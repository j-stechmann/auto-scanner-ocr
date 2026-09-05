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
        // Full path (not a bare filename): zenity derives the initial
        // folder only from a directory component, yad only calls
        // set_current_folder for an absolute URI, and kdialog's
        // --getsavefilename takes the complete start path. All three then
        // open in `dir` with `filename` pre-filled.
        let seed = dir.join(filename).to_string_lossy().into_owned();
        let title = title.to_string();
        match self {
            SaveTool::Zenity => vec![
                "--file-selection".into(),
                "--save".into(),
                "--confirm-overwrite".into(),
                format!("--filename={seed}"),
                format!("--title={title}"),
                format!("--file-filter={PDF_FILTER}"),
            ],
            SaveTool::Kdialog => {
                let mut args = vec!["--getsavefilename".into(), seed, KDIALOG_FILTER.into()];
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
                format!("--filename={seed}"),
                format!("--title={title}"),
                format!("--file-filter={PDF_FILTER}"),
            ],
        }
    }
}

/// Shared filter: PDF files (the session output is always a PDF; the
/// dialog is for choosing a save location, not filtering scan output).
const PDF_FILTER: &str = "PDF files (*.pdf) | *.pdf";
/// kdialog takes a raw Qt name filter: its `|`-to-newline split would turn
/// the shared zenity-style filter into two entries ("PDF files (*.pdf)" and
/// "*.pdf").
const KDIALOG_FILTER: &str = "PDF files (*.pdf)";

/// Which save dialog tool is available, in preference order (zenity is the
/// most common on GTK desktops, kdialog on KDE, yad as fallback).
pub fn available_tool() -> Option<SaveTool> {
    [SaveTool::Zenity, SaveTool::Kdialog, SaveTool::Yad]
        .into_iter()
        .find(|t| crate::backend::which(t.binary()).is_some())
}

/// Outcome of the system save dialog, distinguishing why no path was
/// chosen: `Cancelled` means the dialog ran and the user dismissed it
/// (Esc/Cancel), `Unavailable` means no dialog tool is installed (or it
/// could not run, e.g. no display). The TUI only falls back to the plain
/// confirm overlay for the latter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveChoice {
    Chosen(PathBuf),
    Cancelled,
    Unavailable,
}

/// True when the exit status looks like the dialog could not run at all
/// rather than being dismissed: a signal death (no code), or exit 1 with
/// stderr pointing at a display/GTK failure (zenity exits 1 both for
/// Cancel and for "cannot open display"; the message is the discriminator).
fn looks_unavailable(status: &std::process::ExitStatus, stderr: &str) -> bool {
    if status.code().is_none() {
        return true;
    }
    let msg = stderr.to_ascii_lowercase();
    msg.contains("cannot open display")
        || msg.contains("failed to open display")
        || msg.contains("cannot open wayland")
        || msg.contains("gtk cannot open display")
        || msg.contains("cannot connect to x server")
        // "qt.qpa.plugin" is Qt's "could not connect to display" prefix;
        // the bare "qt.qpa" prefix would also catch benign wayland
        // warnings kdialog might print while running fine.
        || msg.contains("qt.qpa.plugin")
}

/// Open a native save dialog. The pre-set directory/filename seed the
/// dialog; the user may change both.
pub async fn save_dialog(dir: &Path, filename: &str, title: &str) -> SaveChoice {
    let Some(tool) = available_tool() else {
        return SaveChoice::Unavailable;
    };
    let mut cmd = Command::new(tool.binary());
    cmd.args(tool.args(dir, filename, title))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        // Piped (not null): the exit-1 display-failure discriminator reads
        // the tool's error message.
        .stderr(std::process::Stdio::piped())
        // Killing the TUI while the dialog is open must not orphan the
        // native window (matches process.rs's kill_on_drop convention).
        .kill_on_drop(true);
    // Never inherit the terminal: the TUI is in raw mode and a dialog
    // writing to it would corrupt the screen.
    let res = cmd.output().await;
    let out = match res {
        Ok(out) => out,
        Err(e) => {
            tracing::warn!("{} failed: {e}", tool.binary());
            return SaveChoice::Unavailable;
        }
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if looks_unavailable(&out.status, &stderr) {
            tracing::warn!(
                "{} could not open a dialog ({}); falling back to plain confirm",
                tool.binary(),
                stderr.trim()
            );
            return SaveChoice::Unavailable;
        }
        // Genuine user cancellation (zenity/kdialog/yad exit 1): expected,
        // never logged as an error.
        return SaveChoice::Cancelled;
    }
    // Read stdout as raw bytes: Linux paths may legally contain non-UTF-8
    // bytes, and from_utf8_lossy would silently pick a different target.
    let chosen = std::path::PathBuf::from(os_string_from_capped(&out.stdout));
    if chosen.as_os_str().is_empty() {
        return SaveChoice::Cancelled;
    }
    SaveChoice::Chosen(chosen)
}

/// Decode raw dialog stdout into an `OsString`, trimming the trailing
/// newline without UTF-8 loss (bytes outside the filename are trimmed as
/// ASCII whitespace; the path bytes themselves are preserved verbatim).
fn os_string_from_capped(bytes: &[u8]) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let end = bytes
            .iter()
            .rposition(|b| !b.is_ascii_whitespace())
            .map_or(0, |i| i + 1);
        std::ffi::OsString::from_vec(bytes[..end].to_vec())
    }
    #[cfg(not(unix))]
    {
        String::from_utf8_lossy(bytes).trim_end().to_string().into()
    }
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
        // Full path seed: bare names leave the dialog at the process CWD
        // (zenity/yad only derive the initial folder from a dir component).
        assert!(zenity.contains(&"--filename=/tmp/out/a.pdf".to_string()));

        let kd = SaveTool::Kdialog.args(dir, "a.pdf", "Title");
        assert!(kd
            .windows(2)
            .any(|w| w == ["--getsavefilename", "/tmp/out/a.pdf"]));
        // Raw Qt name filter: the zenity-style `| *.pdf` variant would be
        // split into two entries by kdialog's pipe-to-newline conversion.
        assert!(kd.contains(&"PDF files (*.pdf)".to_string()));
        assert!(!kd.iter().any(|a| a.contains('|')));

        let yad = SaveTool::Yad.args(dir, "a.pdf", "Title");
        assert!(yad.contains(&"--save".to_string()));
        assert!(yad.contains(&"--filename=/tmp/out/a.pdf".to_string()));
    }

    #[test]
    fn stdout_decoding_trims_and_keeps_bytes() {
        assert_eq!(os_string_from_capped(b"/tmp/x.pdf\n"), "/tmp/x.pdf");
        assert_eq!(os_string_from_capped(b"/tmp/x.pdf\n\n"), "/tmp/x.pdf");
        // Non-UTF-8 filename byte (0xFF) survives verbatim on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let raw = b"/tmp/x\xffy.pdf\n".to_vec();
            assert_eq!(
                os_string_from_capped(&raw),
                std::ffi::OsString::from_vec(b"/tmp/x\xffy.pdf".to_vec())
            );
        }
        // Whitespace-only output: empty (treated as cancelled).
        assert!(os_string_from_capped(b" \n").is_empty());
    }

    #[test]
    fn display_failure_maps_to_unavailable() {
        // Signal death (no exit code): cannot have been a user Cancel.
        let killed = {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                std::process::ExitStatus::from_raw(0x000F)
            }
            #[cfg(not(unix))]
            {
                std::process::ExitStatus::from_raw(1)
            }
        };
        assert!(looks_unavailable(&killed, ""));
        // zenity's "cannot open display" exits 1 too; the message decides.
        let exit1 = {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                std::process::ExitStatus::from_raw(0x0100)
            }
            #[cfg(not(unix))]
            {
                std::process::ExitStatus::from_raw(1)
            }
        };
        assert!(looks_unavailable(
            &exit1,
            "Gtk-WARNING **: cannot open display: "
        ));
        assert!(looks_unavailable(
            &exit1,
            "qt.qpa.plugin: could not connect to display"
        ));
        assert!(!looks_unavailable(&exit1, ""));
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
