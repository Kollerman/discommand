//! The system's file dialog: picking files to send (and anything else that
//! needs a file from the user). Windows and macOS use their own dialog,
//! Linux the desktop's file chooser portal; without a desktop (SSH, Termux)
//! there is none and callers ask for a path instead.

use std::path::PathBuf;

/// Lets the user pick one file. `Ok(None)`: the dialog was cancelled.
/// `Err`: no dialog can be shown here.
pub fn file(title: &str) -> Result<Option<PathBuf>, String> {
    if !available() {
        return Err(NO_DIALOG.into());
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(rfd::FileDialog::new().set_title(title).pick_file())
    }
    #[cfg(target_os = "android")]
    {
        let _ = title;
        Err(NO_DIALOG.into())
    }
}

const NO_DIALOG: &str = "no file dialog here (no desktop): use :attach <path>";

/// Whether a dialog can appear: always on Windows and macOS, on Linux only
/// with a graphical session (not over plain SSH).
fn available() -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        return true;
    }
    if cfg!(target_os = "android") {
        return false;
    }
    ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|v| std::env::var_os(v).is_some_and(|s| !s.is_empty()))
}
