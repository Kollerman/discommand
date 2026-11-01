//! Opening links and downloaded files with the system's own programs (the
//! browser, the image viewer, …), carefully: only web links, and only file
//! types that can't run code.

/// File types that are opened directly after a download. Anything else
/// (.exe, .bat, .lnk, .js, …) could run code, so it is only shown in the
/// file manager.
const SAFE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jfif", "jpe", "png", "gif", "webp", "bmp", "heic", "mp4", "mov", "webm", "mkv", "avi", "3gp", "mp3", "ogg",
    "opus", "oga", "m4a", "aac", "wav", "flac", "pdf", "txt", "csv",
];

/// Opens a downloaded file if its type is safe; otherwise reveals it.
/// Returns whether it was opened.
pub fn open_file(path: &str) -> std::io::Result<bool> {
    use std::process::{Command, Stdio};
    let p = std::path::Path::new(path);
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if SAFE_EXTENSIONS.contains(&ext.as_str()) {
        launch(path)?;
        return Ok(true);
    }
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("explorer");
        c.arg(format!("/select,{path}"));
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.args(["-R", path]);
        c
    } else {
        let dir = p.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        let mut c = Command::new("xdg-open");
        c.arg(dir);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    Ok(false)
}

/// Opens a web link. Only http(s) is allowed: other schemes (file:, UNC
/// paths, custom handlers) can launch local programs.
pub fn open_url(url: &str) -> std::io::Result<()> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(std::io::Error::other("only http(s) links are opened"));
    }
    launch(url)
}

fn launch(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "android") {
        let mut c = Command::new("termux-open-url");
        c.arg(url);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(|_| ())
}
