//! WhatsApp support: talks to the `discommand-wa` helper (whatsmeow) over
//! JSON lines on its stdin/stdout.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;

use chrono::{Local, TimeZone};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::pics::{PicRef, PicSrc};
use crate::text::sanitize;
use crate::view::{Reply, ViewMsg};

/// Messages kept in memory per chat; older ones are reloaded from the helper's database.
const MAX_MESSAGES: usize = 1000;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WaChat {
    pub jid: String,
    pub name: String,
    pub is_group: bool,
    pub last_ts: i64,
    pub unread: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WaMsg {
    pub id: String,
    pub chat: String,
    pub sender: String,
    pub sender_name: String,
    pub from_me: bool,
    pub ts: i64,
    pub kind: String,
    pub text: String,
    pub file: String,
    pub reply_id: String,
    pub reply_name: String,
    pub reply_text: String,
    pub edited: bool,
    pub deleted: bool,
    pub media: bool,
    pub reactions: String,
    pub img_w: u32,
    pub img_h: u32,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum WaEvent {
    Login,
    Qr { code: String },
    PairCode { code: String },
    Paired,
    Linked {
        me: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        receipts: bool,
    },
    Chats { chats: Vec<WaChat> },
    Chat { chat: WaChat },
    Msg { msg: WaMsg },
    History { chat: String, older: bool, msgs: Vec<WaMsg>, complete: bool },
    Status { status: String },
    Receipts { on: bool },
    File { path: String },
    /// Picture bytes for a `thumb` request (base64; empty when there is none).
    Thumb { key: String, #[serde(default)] data: String },
    Error { msg: String },
    /// Contact names changed; loaded messages carry stale names.
    Names,
    Fatal { msg: String },
    LoggedOut { msg: String },
    /// The helper process ended.
    #[serde(skip)]
    Exited,
    #[serde(other)]
    Unknown,
}

#[derive(Default)]
pub struct WaState {
    pub me: String,
    pub name: String,
    pub receipts: bool,
    pub conn: String,
    pub chats: HashMap<String, WaChat>,
    /// Chat jids, most recently active first.
    pub order: Vec<String>,
    pub messages: HashMap<String, Vec<WaMsg>>,
    pub complete: HashSet<String>,
    pub loading: HashSet<String>,
    /// Bumped whenever loaded messages change; keys the UI's line cache.
    pub version: u64,
}

impl WaState {
    pub fn set_chats(&mut self, chats: Vec<WaChat>) {
        self.chats = chats.into_iter().map(|c| (c.jid.clone(), c)).collect();
        self.sort();
    }

    pub fn upsert_chat(&mut self, chat: WaChat) {
        self.chats.insert(chat.jid.clone(), chat);
        self.sort();
    }

    fn sort(&mut self) {
        let mut chats: Vec<&WaChat> = self.chats.values().filter(|c| c.last_ts > 0).collect();
        chats.sort_by(|a, b| b.last_ts.cmp(&a.last_ts).then_with(|| a.name.cmp(&b.name)));
        self.order = chats.into_iter().map(|c| c.jid.clone()).collect();
    }

    pub fn upsert_msg(&mut self, m: WaMsg) {
        let Some(list) = self.messages.get_mut(&m.chat) else { return };
        self.version += 1;
        if let Some(existing) = list.iter_mut().find(|x| x.id == m.id) {
            *existing = m;
            return;
        }
        let pos = list.iter().rposition(|x| (x.ts, &x.id) <= (m.ts, &m.id)).map_or(0, |i| i + 1);
        list.insert(pos, m);
        if list.len() > MAX_MESSAGES {
            let chat = list[0].chat.clone();
            let excess = list.len() - MAX_MESSAGES;
            list.drain(..excess);
            self.complete.remove(&chat);
        }
    }

    /// Stores a history page; returns how many messages were prepended.
    pub fn history(&mut self, chat: String, older: bool, msgs: Vec<WaMsg>, complete: bool) -> usize {
        self.loading.remove(&chat);
        self.version += 1;
        if complete {
            self.complete.insert(chat.clone());
        }
        if older {
            let existing = self.messages.entry(chat).or_default();
            let first = existing.first().map(|m| (m.ts, m.id.clone()));
            let mut list: Vec<WaMsg> = msgs
                .into_iter()
                .filter(|m| first.as_ref().map_or(true, |(ts, id)| (m.ts, &m.id) < (*ts, id)))
                .collect();
            let added = list.len();
            list.append(existing);
            *existing = list;
            added
        } else {
            // Keep live messages that arrived while the page was loading.
            let mut list = msgs;
            for m in self.messages.remove(&chat).unwrap_or_default() {
                if !list.iter().any(|x| x.id == m.id) {
                    list.push(m);
                }
            }
            list.sort_by(|a, b| (a.ts, &a.id).cmp(&(b.ts, &b.id)));
            self.messages.insert(chat, list);
            0
        }
    }

    /// Drops loaded messages so they are refetched with current names.
    /// Returns the chat that should be reloaded right away.
    pub fn forget_messages(&mut self, keep: Option<&str>) -> Option<String> {
        self.messages.clear();
        self.complete.clear();
        self.version += 1;
        let chat = keep?.to_string();
        self.loading.insert(chat.clone()).then_some(chat)
    }

    pub fn label(&self, jid: &str) -> String {
        match self.chats.get(jid) {
            Some(c) if !c.name.is_empty() => c.name.clone(),
            _ => {
                let user = jid.split('@').next().unwrap_or(jid);
                if jid.ends_with("@s.whatsapp.net") {
                    format!("+{user}")
                } else {
                    user.to_string()
                }
            }
        }
    }

    pub fn title(&self, jid: &str) -> String {
        let group = self.chats.get(jid).is_some_and(|c| c.is_group);
        format!("{}{}", if group { "#" } else { "@" }, self.label(jid))
    }

    pub fn unread_chats(&self) -> usize {
        self.chats.values().filter(|c| c.unread > 0).count()
    }

    pub fn view_messages(&self, chat: &str) -> Option<Vec<ViewMsg>> {
        Some(self.messages.get(chat)?.iter().map(view).collect())
    }
}

fn view(m: &WaMsg) -> ViewMsg {
    let body = if m.deleted {
        "message deleted".to_string()
    } else {
        let tag = match m.kind.as_str() {
            "text" => String::new(),
            "document" if !m.file.is_empty() => format!("[document] {}", m.file),
            "voice" | "audio" if !m.text.is_empty() => format!("[{} {}]", m.kind, m.text),
            kind => format!("[{kind}]"),
        };
        let text = if matches!(m.kind.as_str(), "voice" | "audio") { "" } else { m.text.as_str() };
        match (tag.is_empty(), text.is_empty()) {
            (true, _) => text.to_string(),
            (false, true) => tag,
            (false, false) => format!("{tag} {text}"),
        }
    };
    let author = if m.sender_name.is_empty() {
        if m.from_me { "you".to_string() } else { m.sender.split('@').next().unwrap_or("").to_string() }
    } else {
        m.sender_name.clone()
    };
    ViewMsg {
        author,
        author_key: m.sender.clone(),
        time: Local.timestamp_opt(m.ts, 0).single(),
        body: sanitize(&body),
        reply: if m.reply_id.is_empty() {
            Reply::None
        } else {
            Reply::To {
                name: if m.reply_name.is_empty() { "message".into() } else { m.reply_name.clone() },
                text: sanitize(&m.reply_text),
            }
        },
        edited: m.edited,
        system: false,
        ping: false,
        dim: m.deleted,
        reactions: m.reactions.clone(),
        pics: if m.media && !m.deleted && matches!(m.kind.as_str(), "image" | "video" | "gif" | "sticker") {
            vec![PicRef {
                key: pic_key(&m.chat, &m.id),
                src: PicSrc::Wa { chat: m.chat.clone(), id: m.id.clone(), full: false },
                w: m.img_w,
                h: m.img_h,
                hd_src: PicSrc::Wa { chat: m.chat.clone(), id: m.id.clone(), full: true },
            }]
        } else {
            Vec::new()
        },
    }
}

fn pic_key(chat: &str, id: &str) -> String {
    format!("wa:{chat}:{id}")
}

/// The running helper process.
pub struct Sidecar {
    pub events: UnboundedReceiver<WaEvent>,
    pub cmds: UnboundedSender<String>,
    pub child: Child,
}

pub fn send(cmds: &UnboundedSender<String>, cmd: Value) {
    let _ = cmds.send(cmd.to_string());
}

fn find_helper() -> Result<PathBuf, String> {
    let name = format!("discommand-wa{}", std::env::consts::EXE_SUFFIX);
    if let Ok(exe) = std::env::current_exe() {
        let p = exe.with_file_name(&name);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let p = dir.join(&name);
            if p.is_file() {
                return Ok(p);
            }
        }
    }
    Err(format!("{name} not found next to discommand or on PATH; build it with the install script"))
}

pub fn spawn() -> Result<Sidecar, String> {
    let exe = find_helper()?;
    let data = crate::secret::data_dir().ok_or("no config directory")?;
    let cache = crate::secret::cache_dir().join("whatsapp");
    let mut cmd = Command::new(exe);
    cmd.arg("-db").arg(data.join("whatsapp.db")).arg("-cache").arg(cache);
    if let Some(log) = std::env::var_os("DISCOMMAND_WA_LOG") {
        cmd.arg("-log").arg(log);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("could not start WhatsApp helper: {e}"))?;
    let stdout = child.stdout.take().ok_or("helper has no stdout")?;
    let mut stdin = child.stdin.take().ok_or("helper has no stdin")?;

    let (event_tx, events) = unbounded_channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(ev) = serde_json::from_str::<WaEvent>(&line) {
                if event_tx.send(ev).is_err() {
                    return;
                }
            }
        }
        let _ = event_tx.send(WaEvent::Exited);
    });

    let (cmds, mut cmd_rx) = unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(mut line) = cmd_rx.recv().await {
            line.push('\n');
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.flush().await.is_err() {
                return;
            }
        }
        // Dropping stdin tells the helper to disconnect and exit.
    });

    Ok(Sidecar { events, cmds, child })
}

fn print_qr(code: &str) {
    crate::qr::show(
        &[
            "Link WhatsApp: on your phone open WhatsApp → Settings → Linked devices → Link a device,",
            "then scan this code. (Or start with --wa-phone +491701234567 for a pairing code.)",
        ],
        code,
    );
}

/// Runs before the TUI starts. Returns once the account is linked; the
/// events consumed on the way are returned so the app can replay them.
/// Unlinks this device from WhatsApp: Ok(false) if it wasn't linked.
pub async fn unlink() -> Result<bool, String> {
    let mut sc = spawn()?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut sent = false;
        while let Some(ev) = sc.events.recv().await {
            match ev {
                WaEvent::Login | WaEvent::Qr { .. } => return Ok(false),
                WaEvent::Linked { .. } if !sent => {
                    send(&sc.cmds, json!({"c": "logout"}));
                    sent = true;
                }
                WaEvent::LoggedOut { .. } => return Ok(true),
                WaEvent::Error { msg } if sent => return Err(msg),
                WaEvent::Fatal { msg } => return Err(msg),
                WaEvent::Exited => break,
                _ => {}
            }
        }
        Err("the WhatsApp helper exited".to_string())
    })
    .await
    .unwrap_or_else(|_| Err("no answer from WhatsApp".into()));
    let _ = sc.child.kill().await;
    result
}

pub async fn login(sc: &mut Sidecar, phone: Option<&str>) -> Result<Vec<WaEvent>, String> {
    let mut phone_sent = false;
    while let Some(ev) = sc.events.recv().await {
        match ev {
            WaEvent::Login => {
                if let Some(p) = phone {
                    let digits: String = p.chars().filter(char::is_ascii_digit).collect();
                    send(&sc.cmds, json!({"c": "pair", "phone": digits}));
                    phone_sent = true;
                    println!("Requesting a pairing code for +{digits}…");
                }
            }
            WaEvent::Qr { code } if !phone_sent => print_qr(&code),
            WaEvent::Qr { .. } => {}
            WaEvent::PairCode { code } => {
                println!("\nOn your phone: WhatsApp → Settings → Linked devices → Link a device →");
                println!("\"Link with phone number instead\", then enter:\n\n    {code}\n");
            }
            WaEvent::Paired => println!("Linked! Syncing chats…"),
            ev @ WaEvent::Linked { .. } => return Ok(vec![ev]),
            WaEvent::Error { msg } => eprintln!("WhatsApp: {msg}"),
            WaEvent::Fatal { msg } | WaEvent::LoggedOut { msg } => return Err(format!("WhatsApp: {msg}")),
            WaEvent::Exited => return Err("WhatsApp helper exited unexpectedly".into()),
            _ => {}
        }
    }
    Err("WhatsApp helper exited unexpectedly".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events() {
        let ev: WaEvent = serde_json::from_str(r#"{"t":"chat","chat":{"jid":"1@s.whatsapp.net","name":"A","is_group":false,"last_ts":5,"unread":2}}"#).unwrap();
        assert!(matches!(ev, WaEvent::Chat { chat } if chat.unread == 2 && chat.name == "A"));
        let ev: WaEvent = serde_json::from_str(r#"{"t":"something_new","x":1}"#).unwrap();
        assert!(matches!(ev, WaEvent::Unknown));
    }

    #[test]
    fn history_merges_and_orders() {
        let msg = |id: &str, ts| WaMsg { id: id.into(), chat: "c".into(), ts, kind: "text".into(), ..Default::default() };
        let mut s = WaState::default();
        s.history("c".into(), false, vec![msg("b", 2), msg("c", 3)], false);
        s.upsert_msg(msg("d", 4));
        assert_eq!(s.history("c".into(), true, vec![msg("a", 1), msg("b", 2)], true), 1);
        let ids: Vec<&str> = s.messages["c"].iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
        assert!(s.complete.contains("c"));
    }

    #[test]
    fn renders_media_and_deleted() {
        let m = WaMsg { kind: "image".into(), text: "look".into(), ..Default::default() };
        assert_eq!(view(&m).body, "[image] look");
        let m = WaMsg { kind: "voice".into(), text: "0:12".into(), ..Default::default() };
        assert_eq!(view(&m).body, "[voice 0:12]");
        let m = WaMsg { kind: "text".into(), deleted: true, ..Default::default() };
        assert!(view(&m).dim);
    }
}
