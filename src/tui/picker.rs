//! The operating system's own folder dialog, so choosing a drive is a click
//! rather than a typed path.
//!
//! Shells out to what the platform already ships (Finder via `osascript`,
//! zenity or kdialog, the Windows folder dialog via PowerShell) instead of
//! pulling in a dialog crate and its GTK tail. Over SSH, or when none of
//! those exist, the caller falls back to the typed field.

use std::path::PathBuf;
use std::process::Command;

pub const PROMPT: &str = "Where should the steroids corpus live?";

pub enum Pick {
    Chosen(PathBuf),
    Cancelled,
    /// No dialog could be shown here; type the path instead.
    Unavailable,
}

/// Block until the user picks a folder or cancels. The TUI is modal for the
/// duration, which is what a dialog means anyway.
pub fn pick_folder() -> Pick {
    // No display, no dialog: it would hang waiting for a window nobody sees.
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        return Pick::Unavailable;
    }
    for mut command in candidates() {
        let Ok(output) = command.output() else {
            // Not installed: try the next tool.
            continue;
        };
        if output.status.success() {
            return parse(&String::from_utf8_lossy(&output.stdout));
        }
        // Every tool exits 1 on cancel. Finder also says so in stderr, which
        // separates a cancel from osascript failing to reach the desktop.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(1)
            && (!cfg!(target_os = "macos") || stderr.contains("-128"))
        {
            return Pick::Cancelled;
        }
    }
    Pick::Unavailable
}

/// Dialog output is a path with a trailing newline, and Finder appends a `/`.
/// A drive root (`/`, `E:\`) keeps its separator: without it `E:` is a
/// relative path on Windows and `/` is nothing at all.
fn parse(stdout: &str) -> Pick {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Pick::Cancelled;
    }
    let path = PathBuf::from(trimmed);
    let stripped = trimmed.trim_end_matches(['/', '\\']);
    if path.parent().is_some() && PathBuf::from(stripped).parent().is_some() {
        Pick::Chosen(PathBuf::from(stripped))
    } else {
        Pick::Chosen(path)
    }
}

#[cfg(target_os = "macos")]
fn candidates() -> Vec<Command> {
    // `with invisibles`: the default corpus lives in the hidden `~/.steroids`,
    // and a dialog that cannot show it cannot point back at it.
    let mut osascript = Command::new("osascript");
    osascript.args([
        "-e",
        &format!(
            "POSIX path of (choose folder with prompt \"{PROMPT}\" \
             default location (path to home folder) with invisibles)"
        ),
    ]);
    vec![osascript]
}

#[cfg(target_os = "windows")]
fn candidates() -> Vec<Command> {
    // WinForms dialogs need a single-threaded apartment; without -STA the
    // call returns Cancel immediately.
    let mut powershell = Command::new("powershell.exe");
    powershell.args([
        "-NoProfile",
        "-STA",
        "-Command",
        &format!(
            "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; \
             Add-Type -AssemblyName System.Windows.Forms; \
             $d = New-Object System.Windows.Forms.FolderBrowserDialog; \
             $d.Description = \"{PROMPT}\"; \
             $d.ShowNewFolderButton = $true; \
             if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) \
             {{ [Console]::Out.Write($d.SelectedPath) }} else {{ exit 1 }}"
        ),
    ]);
    vec![powershell]
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn candidates() -> Vec<Command> {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Vec::new();
    }
    let mut zenity = Command::new("zenity");
    zenity.args([
        "--file-selection",
        "--directory",
        &format!("--title={PROMPT}"),
    ]);
    let mut kdialog = Command::new("kdialog");
    kdialog.args(["--getexistingdirectory", ".", "--title", PROMPT]);
    vec![zenity, kdialog]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dialog_output() {
        assert!(matches!(
            parse("/Volumes/SSD/steroids/\n"),
            Pick::Chosen(p) if p == std::path::Path::new("/Volumes/SSD/steroids")
        ));
        assert!(matches!(parse("\n"), Pick::Cancelled));
        assert!(matches!(parse(""), Pick::Cancelled));
        // A whole drive is the headline case and must survive the trim.
        assert!(matches!(parse("/\n"), Pick::Chosen(p) if p == std::path::Path::new("/")));
        #[cfg(windows)]
        assert!(matches!(parse("E:\\\r\n"), Pick::Chosen(p) if p == std::path::Path::new("E:\\")));
    }

    /// The prompt is spliced into a script; a quote in it would break out.
    #[test]
    fn prompt_is_script_safe() {
        assert!(!PROMPT.contains(['"', '\\', '\n', '$', '`']));
    }

    /// A dialog must never be attempted where nobody can see it.
    #[test]
    fn ssh_session_is_unavailable() {
        // SAFETY: tests in this module are the only readers of this variable,
        // and the process-wide env is restored before returning.
        unsafe { std::env::set_var("SSH_CONNECTION", "1.2.3.4 1 5.6.7.8 22") };
        let pick = pick_folder();
        unsafe { std::env::remove_var("SSH_CONNECTION") };
        assert!(matches!(pick, Pick::Unavailable));
    }
}
