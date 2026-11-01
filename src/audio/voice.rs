//! Transcripts of voice messages (`t` on one), kept in `transcripts.json`
//! so a message is only ever transcribed once. The transcribing itself is
//! in `transcribe` and only built with the `transcribe` feature.

use std::collections::HashMap;
use std::path::PathBuf;

/// Whether this build can transcribe.
pub const BUILT: bool = cfg!(feature = "transcribe");

#[derive(Default)]
pub struct Transcripts {
    done: HashMap<String, String>,
    /// Being transcribed now, with the chat's context for each.
    pending: HashMap<String, String>,
    /// Bumped whenever a transcript appears or starts: redraws the messages.
    pub version: u64,
}

/// The key of a message: its service and id.
pub fn key(service: &str, id: &str) -> String {
    format!("{service}:{id}")
}

fn path() -> Option<PathBuf> {
    crate::system::secret::data_dir().map(|d| d.join("transcripts.json"))
}

impl Transcripts {
    pub fn load() -> Self {
        let done = path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { done, ..Self::default() }
    }

    /// The transcript of a message, or "transcribing…" while it's made.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.done.get(key).map(String::as_str).or_else(|| self.pending.contains_key(key).then_some("transcribing…"))
    }

    pub fn busy(&self, key: &str) -> bool {
        self.pending.contains_key(key)
    }

    /// The context given when a transcription started (once its file is there).
    pub fn context(&self, key: &str) -> String {
        self.pending.get(key).cloned().unwrap_or_default()
    }

    pub fn start(&mut self, key: String, context: String) {
        self.pending.insert(key, context);
        self.version += 1;
    }

    /// The downloads for a service's transcriptions failed: none is waiting any more.
    pub fn cancel(&mut self, service: &str) {
        let before = self.pending.len();
        self.pending.retain(|k, _| !k.starts_with(&key(service, "")));
        if self.pending.len() != before {
            self.version += 1;
        }
    }

    /// A transcription finished: stores the text (and saves it).
    pub fn finish(&mut self, key: &str, result: &Result<String, String>) {
        self.pending.remove(key);
        self.version += 1;
        if let Ok(text) = result {
            let text = if text.is_empty() { "(nothing understood)" } else { text };
            self.done.insert(key.to_string(), text.to_string());
            self.save();
        }
    }

    fn save(&self) {
        let Some(path) = path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec(&self.done) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// Deletes the saved transcripts (`--logout-a`). Whether there were any.
pub fn forget() -> bool {
    path().is_some_and(|p| std::fs::remove_file(p).is_ok())
}

/// Transcribes a voice message (its file's bytes): downloads the speech
/// model first if needed, then runs it off the UI thread. One at a time,
/// so several `t` presses don't fight over the CPU.
#[cfg(feature = "transcribe")]
pub async fn transcribe(data: Vec<u8>, context: String) -> Result<String, String> {
    static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _turn = ONE.lock().await;
    if !crate::audio::transcribe::ready() {
        crate::audio::transcribe::fetch().await?;
    }
    tokio::task::spawn_blocking(move || crate::audio::transcribe::transcribe(&data, &context)).await.map_err(|e| e.to_string())?
}

#[cfg(not(feature = "transcribe"))]
pub async fn transcribe(_data: Vec<u8>, _context: String) -> Result<String, String> {
    Err(NOT_BUILT.into())
}

/// Shown on `t` when the feature isn't built in.
pub const NOT_BUILT: &str = "voice transcription isn't built in: run the install script again and answer y for \"Voice message transcription\"";

/// Whether the speech model still has to be downloaded (259 MB, once).
pub fn needs_model() -> bool {
    #[cfg(feature = "transcribe")]
    return !crate::audio::transcribe::ready();
    #[cfg(not(feature = "transcribe"))]
    false
}
