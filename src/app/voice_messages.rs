//! Voice messages: playing them (`p`), transcribing them (`t`) with the
//! chat as context, and the alert for new messages.

use super::*;

impl App {
    /// The selected voice message: its id, where to get it (WhatsApp/Matrix:
    /// the helper's download command, `for` this purpose; Discord: its link)
    /// and the chat around it. Says so when there's none.
    fn selected_voice(&mut self, purpose: &str) -> Option<(String, serde_json::Value, String)> {
        let Some(i) = self.msg_sel else {
            self.status = "select a voice message first (k)".into();
            return None;
        };
        // The chat around it (who's in it, what was written just before) helps
        // Whisper spell names and words as the chat does.
        let found = if self.service.is_wa_like() {
            let msgs = self.wa_messages();
            let m = msgs.get(i)?;
            let earlier = &msgs[i.saturating_sub(CONTEXT_SCAN)..i];
            let names = earlier.iter().chain([m]).map(|m| m.sender_name.as_str());
            let texts = earlier.iter().filter(|m| m.kind == "text" && !m.deleted).map(|m| m.text.clone());
            let context = chat_context(names, texts);
            m.is_voice().then(|| (m.id.clone(), json!({"c": "download", "chat": m.chat, "id": m.id, "for": purpose}), context))
        } else {
            let msgs = self.current_messages();
            let m = msgs.get(i)?;
            let earlier = &msgs[i.saturating_sub(CONTEXT_SCAN)..i];
            let names = earlier.iter().chain([m]).map(|m| m.author.name());
            let texts = earlier.iter().filter(|m| !m.is_system()).map(|m| self.state.render_content(m));
            let context = chat_context(names, texts);
            m.attachments.iter().find(|a| a.is_audio()).map(|a| (m.id.clone(), json!(a.url), context))
        };
        if found.is_none() {
            self.status = "this isn't a voice message".into();
        }
        found
    }

    /// `p`: plays the selected voice message, or stops it if it's playing.
    pub(super) fn play_selected(&mut self) {
        if !sound::BUILT {
            self.status = sound::NOT_BUILT.into();
            return;
        }
        let Some((id, source, _)) = self.selected_voice("play") else { return };
        let key = voice::key(self.service.name(), &id);
        if self.playing.take().is_some_and(|(k, end)| k == key && end > Instant::now()) {
            sound::stop();
            self.status = "stopped".into();
            return;
        }
        self.status = "loading the voice message…".into();
        if self.service.is_wa_like() {
            // The helper downloads it and answers with a File event `for` this.
            wa::send(self.cmd_of(self.service), source);
            return;
        }
        let (http, url) = (self.pic_http.clone(), source.as_str().unwrap_or_default().to_string());
        self.spawn(async move {
            let result = match fetch_audio(&http, &url).await {
                Ok(data) => sound::play(data).await,
                Err(e) => Err(e),
            };
            Some(AppEvent::Playing(key, result))
        });
    }

    /// The alert for a new message: only while the window is in the
    /// background (where the terminal doesn't say, for chats not open), and
    /// at most every few seconds.
    pub(super) fn ring(&mut self, viewing: bool) {
        let away = self.focused.map_or(!viewing, |f| !f);
        if away && self.alerted.is_none_or(|t| t.elapsed() >= ALERT_GAP) {
            self.alerted = Some(Instant::now());
            sound::alert();
        }
    }

    /// `t`: writes out what the selected voice message says (on this
    /// computer), shown under it from then on.
    pub(super) fn transcribe_selected(&mut self) {
        if !voice::BUILT {
            self.status = voice::NOT_BUILT.into();
            return;
        }
        let s = self.service;
        let Some((id, source, context)) = self.selected_voice("transcribe") else { return };
        let key = voice::key(s.name(), &id);
        if self.voice.busy(&key) {
            return;
        }
        self.status = if voice::needs_model() {
            "transcribing… (the first time downloads the speech model, 259 MB)".into()
        } else {
            "transcribing…".into()
        };
        self.voice.start(key.clone(), context.clone());
        if s.is_wa_like() {
            // The helper downloads it and answers with a File event `for` this.
            wa::send(self.cmd_of(s), source);
            return;
        }
        let (http, url) = (self.pic_http.clone(), source.as_str().unwrap_or_default().to_string());
        self.spawn(async move {
            let result = match fetch_audio(&http, &url).await {
                Ok(data) => voice::transcribe(data, context).await,
                Err(e) => Err(e),
            };
            Some(AppEvent::Transcript(key, result))
        });
    }
}

/// How many messages before a voice message are looked at for its context.
const CONTEXT_SCAN: usize = 30;

/// The alert sounds at most this often (a burst of messages rings once).
const ALERT_GAP: std::time::Duration = std::time::Duration::from_secs(3);

/// The context for transcribing a voice message: the names in the chat,
/// then the last few texts written before it (at most ~600 characters;
/// Whisper uses the end of it).
fn chat_context<'a>(names: impl Iterator<Item = &'a str>, texts: impl DoubleEndedIterator<Item = String>) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for n in names.map(str::trim).filter(|n| !n.is_empty()) {
        if !seen.contains(&n) {
            seen.push(n);
        }
    }
    let mut recent: Vec<String> = Vec::new();
    let mut len = 0;
    for t in texts.rev().map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|t| !t.is_empty()).take(5) {
        len += t.len();
        if len > 600 && !recent.is_empty() {
            break;
        }
        recent.push(t);
    }
    recent.reverse();
    let mut out = seen.join(", ");
    for t in recent {
        if !out.is_empty() {
            out.push_str(if out.ends_with(['.', '!', '?']) { " " } else { ". " });
        }
        out.push_str(&t);
    }
    out
}

/// A Discord voice message from its CDN link (at most 25 MB).
async fn fetch_audio(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let resp = http.get(url).send().await.map_err(|e| format!("couldn't download the voice message: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("couldn't download the voice message (HTTP {})", resp.status().as_u16()));
    }
    if resp.content_length().is_some_and(|n| n > 25 << 20) {
        return Err("the voice message is too large".into());
    }
    Ok(resp.bytes().await.map_err(|e| format!("couldn't download the voice message: {e}"))?.to_vec())
}

#[cfg(test)]
mod tests {
    #[test]
    fn builds_voice_context() {
        let names = ["Anna", "", "Ben", "Anna"].into_iter();
        let texts = ["hi", "kommst du zur Übung?", "  ja,\n gleich  "].map(String::from).into_iter();
        assert_eq!(super::chat_context(names, texts), "Anna, Ben. hi. kommst du zur Übung? ja, gleich");
        // Only the last five texts.
        let texts = (1..=8).map(|n| format!("m{n}"));
        assert_eq!(super::chat_context(std::iter::empty(), texts), "m4. m5. m6. m7. m8");
    }
}
