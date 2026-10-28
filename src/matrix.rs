//! Matrix via matrix-rust-sdk, end-to-end encryption included. It runs in
//! this process but speaks the WhatsApp helper's protocol (JSON commands in,
//! `WaEvent`s out), so the app handles both chat services the same way.
//!
//! Login happens in the terminal before the UI starts: OAuth in the browser
//! where the server supports it (matrix.org does), else user name and
//! password. The session, encryption keys and room state live in an
//! encrypted SQLite store whose passphrase is kept with the other secrets.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::authentication::oauth::registration::{ApplicationType, ClientMetadata, Localized, OAuthGrantType};
use matrix_sdk::authentication::oauth::{ClientId, OAuthSession, UserSession};
use matrix_sdk::config::{SyncSettings, SyncToken};
use matrix_sdk::encryption::{BackupDownloadStrategy, EncryptionSettings};
use matrix_sdk::media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings};
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::api::client::filter::{FilterDefinition, LazyLoadOptions};
use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
use matrix_sdk::ruma::api::client::sync::sync_events::v3::Filter;
use matrix_sdk::ruma::events::reaction::ReactionEventContent;
use matrix_sdk::ruma::events::receipt::ReceiptThread;
use matrix_sdk::ruma::events::relation::{Annotation, InReplyTo, Reply};
use matrix_sdk::ruma::events::room::message::{
    MessageType, Relation, ReplacementMetadata, RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
};
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent};
use matrix_sdk::ruma::media::Method;
use matrix_sdk::ruma::presence::PresenceState;
use matrix_sdk::ruma::serde::Raw;
use matrix_sdk::ruma::{uint, EventId, OwnedEventId, RoomId, UserId};
use matrix_sdk::utils::UrlOrQuery;
use matrix_sdk::{Client, Room, SessionChange};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::secret;
use crate::wa::{WaChat, WaEvent, WaMsg};

const SECRET_NAME: &str = "matrix";
/// Shown on the server's sign-in page; must be an https URL.
const CLIENT_URI: &str = "https://github.com/Kollerman/discommand";
/// Messages per history page.
const PAGE: u32 = 50;
/// Recently seen messages kept for edits, deletes, reactions and reply quotes.
const MAX_SEEN: usize = 5000;
/// Largest file loaded to draw an encrypted picture that has no thumbnail.
const MAX_INLINE_FILE: u64 = 4 << 20;

fn dir() -> Option<PathBuf> {
    secret::data_dir().map(|d| d.join("matrix"))
}

// ---- saved session ----------------------------------------------------------------

/// What's needed to restore the session without logging in again.
#[derive(Serialize, Deserialize)]
struct Saved {
    homeserver: String,
    /// Set for OAuth logins (the id of our dynamic client registration).
    client_id: Option<String>,
    user: UserSession,
    /// Encrypts the SQLite store (room state, encryption keys).
    passphrase: String,
}

impl Saved {
    fn load() -> Option<Saved> {
        serde_json::from_slice(&secret::load_secret(SECRET_NAME)?).ok()
    }

    fn save(&self) -> Result<(), String> {
        let data = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        secret::save_secret(SECRET_NAME, &data).map(|_| ()).map_err(|e| e.to_string())
    }
}

/// Deletes the saved session and the local store.
pub fn forget() {
    secret::remove_secret(SECRET_NAME);
    if let Some(d) = dir() {
        let _ = std::fs::remove_dir_all(d);
    }
}

pub fn has_session() -> bool {
    Saved::load().is_some()
}

/// Ends the saved session on the server too, then deletes everything local.
pub async fn logout() -> Result<(), String> {
    let result = match Saved::load() {
        Some(saved) => match restore(&saved).await {
            Ok(client) => client.logout().await.map_err(|e| e.to_string()),
            Err(e) => Err(e),
        },
        None => Ok(()),
    };
    forget();
    result
}

async fn build(server: &str, passphrase: &str, by_url: bool) -> Result<Client, String> {
    let store = dir().ok_or("no config directory")?.join("store");
    let builder = Client::builder()
        .sqlite_store(&store, Some(passphrase))
        .handle_refresh_tokens()
        // Old messages are decrypted with keys from the server-side backup,
        // fetched when they're needed.
        .with_encryption_settings(EncryptionSettings {
            auto_enable_cross_signing: true,
            backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
            auto_enable_backups: true,
        });
    let builder = if by_url { builder.homeserver_url(server) } else { builder.server_name_or_homeserver_url(server) };
    builder.build().await.map_err(|e| format!("Matrix: {e}"))
}

/// The client of the saved session, or a new login (interactive, in the terminal).
pub async fn login() -> Result<Client, String> {
    if let Some(saved) = Saved::load() {
        match restore(&saved).await {
            Ok(c) => return Ok(c),
            Err(e) => println!("\nThe Matrix session couldn't be restored ({e}). Please log in again."),
        }
    }
    forget();
    println!("\nLog in to Matrix.");
    let user = loop {
        let input = prompt("Matrix ID (e.g. @alice:matrix.org): ")?;
        let input = if input.starts_with('@') { input } else { format!("@{input}") };
        match UserId::parse(input.as_str()) {
            Ok(u) => break u,
            Err(_) => println!("That isn't a Matrix ID; it looks like @name:server.org"),
        }
    };
    let passphrase = random_string(32);
    let client = build(user.server_name().as_str(), &passphrase, false).await?;

    let client_id = if client.oauth().server_metadata().await.is_ok() {
        Some(oauth_login(&client, &user).await?)
    } else {
        let password = rpassword::prompt_password("Password (input hidden): ").map_err(|e| e.to_string())?;
        client
            .matrix_auth()
            .login_username(&user, &password)
            .initial_device_display_name("discommand")
            .await
            .map_err(|e| format!("Matrix login failed: {e}"))?;
        None
    };
    let (Some(meta), Some(tokens)) = (client.session_meta().cloned(), client.session_tokens()) else {
        return Err("Matrix login returned no session".into());
    };
    let saved = Saved { homeserver: client.homeserver().to_string(), client_id, user: UserSession { meta, tokens }, passphrase };
    if let Err(e) = saved.save() {
        eprintln!("warning: could not save the Matrix session: {e}");
    }
    println!("Logged in to Matrix as {user}.");
    unlock_history(&client).await;
    Ok(client)
}

async fn restore(saved: &Saved) -> Result<Client, String> {
    let client = build(&saved.homeserver, &saved.passphrase, true).await?;
    let user = saved.user.clone();
    let result = match &saved.client_id {
        Some(id) => client.restore_session(OAuthSession { client_id: ClientId::new(id.clone()), user }).await,
        None => client.restore_session(MatrixSession { meta: user.meta, tokens: user.tokens }).await,
    };
    result.map_err(|e| e.to_string())?;
    Ok(client)
}

/// OAuth in the browser, redirected back to a loopback port. Registers
/// this app with the server on the fly; returns that client id.
async fn oauth_login(client: &Client, user: &UserId) -> Result<String, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = reqwest::Url::parse(&format!("http://127.0.0.1:{port}/")).map_err(|e| e.to_string())?;
    let mut metadata = ClientMetadata::new(
        ApplicationType::Native,
        vec![OAuthGrantType::AuthorizationCode { redirect_uris: vec![redirect.clone()] }],
        Localized::new(reqwest::Url::parse(CLIENT_URI).unwrap(), []),
    );
    metadata.client_name = Some(Localized::new("discommand".to_string(), []));
    let metadata = Raw::new(&metadata).map_err(|e| e.to_string())?;
    let auth = client
        .oauth()
        .login(redirect, None, Some(metadata.into()), None)
        .user_id_hint(user)
        .build()
        .await
        .map_err(|e| format!("Matrix sign-in: {e}"))?;
    println!("Sign in in your browser. If it didn't open, visit:\n\n  {}\n", auth.url);
    let _ = crate::app::open_url(auth.url.as_str());
    let query = crate::cal::await_redirect(&listener, |q| q.contains_key("code") || q.contains_key("error")).await?;
    client.oauth().finish_login(UrlOrQuery::Query(query)).await.map_err(|e| format!("Matrix sign-in failed: {e}"))?;
    client.oauth().client_id().map(|id| id.as_str().to_string()).ok_or_else(|| "no OAuth client id".to_string())
}

/// A new login can't read old encrypted messages until it has the account's
/// keys: entering the recovery key fetches them and verifies this device.
async fn unlock_history(client: &Client) {
    let enc = client.encryption();
    enc.wait_for_e2ee_initialization_tasks().await;
    // The recovery setup is account data, which arrives with the first sync.
    if let Err(e) = client.sync_once(sync_settings().timeout(Duration::ZERO)).await {
        println!("Matrix: first sync failed ({e}); encrypted history may stay locked.");
        return;
    }
    println!("\nTo read your encrypted messages, enter your Matrix recovery key (also called");
    println!("security key; Element shows it under Settings → Encryption). Press Enter to skip;");
    println!("then only new messages can be decrypted.");
    loop {
        let key = match rpassword::prompt_password("Recovery key (input hidden): ") {
            Ok(k) => k.trim().to_string(),
            Err(_) => return,
        };
        if key.is_empty() {
            println!("Skipped. Encrypted history stays locked on this device.");
            return;
        }
        match enc.recovery().recover(&key).await {
            Ok(()) => {
                if let Ok(Some(device)) = enc.get_own_device().await {
                    if !device.is_cross_signed_by_owner() {
                        let _ = device.verify().await;
                    }
                }
                println!("Unlocked: this device is verified and can read your history.");
                return;
            }
            Err(e) => println!("That didn't work ({e}). Try again, or press Enter to skip."),
        }
    }
}

/// Lazy-loaded members, a short timeline per room, and no "online" status.
fn sync_settings() -> SyncSettings {
    let mut filter = FilterDefinition::default();
    filter.room.state.lazy_load_options = LazyLoadOptions::Enabled { include_redundant_members: false };
    filter.room.timeline.limit = Some(uint!(20));
    SyncSettings::default().filter(Filter::FilterDefinition(filter)).set_presence(PresenceState::Offline)
}

fn prompt(label: &str) -> Result<String, String> {
    use std::io::Write;
    print!("{label}");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(line.trim().to_string())
}

fn random_string(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("system randomness");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

// ---- running ------------------------------------------------------------------------

/// Small local state: when each room was last active (the sync doesn't
/// repeat old messages after a restart) and the read-receipt setting.
#[derive(Default, Serialize, Deserialize)]
struct Recent {
    last_ts: HashMap<String, i64>,
    receipts: bool,
}

impl Recent {
    fn path() -> Option<PathBuf> {
        dir().map(|d| d.join("recent.json"))
    }

    fn load() -> Recent {
        Self::path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    fn save(&self) {
        if let (Some(p), Ok(data)) = (Self::path(), serde_json::to_vec(self)) {
            let _ = std::fs::write(p, data);
        }
    }
}

struct Media {
    source: MediaSource,
    thumb: Option<MediaSource>,
    size: u64,
    name: String,
}

struct Reaction {
    id: String,
    sender: String,
    key: String,
}

#[derive(Default)]
struct Shared {
    recent: Recent,
    recent_dirty: bool,
    /// Recently seen messages by event id.
    seen: HashMap<String, WaMsg>,
    media: HashMap<String, Media>,
    /// Reactions by the message they're on, and the message of each reaction.
    reactions: HashMap<String, Vec<Reaction>>,
    reaction_of: HashMap<String, String>,
    /// The newest event of each room (what a read receipt points at).
    newest: HashMap<String, String>,
    /// Where the next older history page starts, per room (None: no more).
    back: HashMap<String, Option<String>>,
    names: HashMap<(String, String), String>,
}

type State = Arc<Mutex<Shared>>;

struct Mx {
    client: Client,
    me: String,
    state: State,
    tx: UnboundedSender<WaEvent>,
}

/// Starts syncing; returns the event stream and the command channel.
pub fn start(client: Client) -> (UnboundedReceiver<WaEvent>, UnboundedSender<String>) {
    let (tx, events) = unbounded_channel();
    let (cmd_tx, mut cmds) = unbounded_channel::<String>();
    let me = client.user_id().map(|u| u.to_string()).unwrap_or_default();
    let state: State = Arc::new(Mutex::new(Shared { recent: Recent::load(), ..Shared::default() }));
    let mx = Arc::new(Mx { client, me, state, tx });

    tokio::spawn({
        let mx = mx.clone();
        async move { mx.sync_loop().await }
    });
    tokio::spawn({
        let mx = mx.clone();
        async move {
            while let Some(line) = cmds.recv().await {
                let Ok(cmd) = serde_json::from_str::<Value>(&line) else { continue };
                // One at a time keeps sends in order; slow ones (pictures) run beside.
                if matches!(cmd["c"].as_str(), Some("thumb" | "download" | "history")) {
                    let mx = mx.clone();
                    tokio::spawn(async move { mx.command(cmd).await });
                } else {
                    mx.command(cmd).await;
                }
            }
        }
    });
    tokio::spawn({
        let mx = mx.clone();
        async move { mx.watch_session().await }
    });
    (events, cmd_tx)
}

fn event_id(id: &str) -> Option<OwnedEventId> {
    EventId::parse(id).ok()
}

impl Mx {
    fn emit(&self, ev: WaEvent) {
        let _ = self.tx.send(ev);
    }

    fn room(&self, id: &str) -> Option<Room> {
        self.client.get_room(<&RoomId>::try_from(id).ok()?)
    }

    /// Keeps refreshed tokens saved; a revoked session ends the app's Matrix part.
    async fn watch_session(&self) {
        let mut changes = self.client.subscribe_to_session_changes();
        while let Ok(change) = changes.recv().await {
            match change {
                SessionChange::TokensRefreshed => {
                    if let (Some(mut saved), Some(tokens)) = (Saved::load(), self.client.session_tokens()) {
                        saved.user.tokens = tokens;
                        let _ = saved.save();
                    }
                }
                SessionChange::UnknownToken { .. } => {
                    forget();
                    self.emit(WaEvent::LoggedOut { msg: "the Matrix session ended (logged out elsewhere?)".into() });
                    return;
                }
            }
        }
    }

    async fn sync_loop(&self) {
        let name = self.client.account().get_display_name().await.ok().flatten().unwrap_or_default();
        let receipts = self.state.lock().unwrap().recent.receipts;
        self.emit(WaEvent::Linked { me: self.me.clone(), name, receipts });
        self.emit(WaEvent::Status { status: "connecting…".into() });

        let mut settings = sync_settings().timeout(Duration::from_secs(30));
        // Nothing known about the rooms yet (first start): sync from scratch,
        // so every room's latest messages arrive and give it its place.
        if self.state.lock().unwrap().recent.last_ts.is_empty() {
            settings = settings.token(SyncToken::NoToken);
        }
        let mut first = true;
        let mut backoff = 1;
        loop {
            match self.client.sync_once(settings.clone()).await {
                Ok(resp) => {
                    backoff = 1;
                    settings = settings.token(resp.next_batch.clone());
                    for (room_id, update) in &resp.rooms.joined {
                        let Some(room) = self.client.get_room(room_id) else { continue };
                        for ev in &update.timeline.events {
                            let Ok(ev) = ev.raw().deserialize() else { continue };
                            // Messages of the first sync are old news: no bell, no live update.
                            self.on_event(&room, ev, !first).await;
                        }
                        if !first {
                            if let Some(chat) = self.chat(&room).await {
                                self.emit(WaEvent::Chat { chat });
                            }
                        }
                    }
                    if first {
                        first = false;
                        let mut chats = Vec::new();
                        for room in self.client.joined_rooms() {
                            if let Some(chat) = self.chat(&room).await {
                                chats.push(chat);
                            }
                        }
                        self.emit(WaEvent::Chats { chats });
                        self.emit(WaEvent::Status { status: "connected".into() });
                    }
                    let mut st = self.state.lock().unwrap();
                    if std::mem::take(&mut st.recent_dirty) {
                        st.recent.save();
                    }
                }
                Err(e) => {
                    self.emit(WaEvent::Status { status: format!("reconnecting… ({e})") });
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    }

    async fn chat(&self, room: &Room) -> Option<WaChat> {
        if room.is_space() {
            return None;
        }
        let id = room.room_id().to_string();
        let name = room.display_name().await.map(|n| n.to_string()).unwrap_or_else(|_| id.clone());
        let direct = room.is_direct().await.unwrap_or(false);
        // Rooms never seen active still show, at the bottom.
        let last_ts = self.state.lock().unwrap().recent.last_ts.get(&id).copied().unwrap_or(1);
        let unread = room.unread_notification_counts().notification_count as u32;
        Some(WaChat { jid: id, name, is_group: !direct, last_ts, unread })
    }

    async fn sender_name(&self, room: &Room, user: &UserId) -> String {
        let key = (room.room_id().to_string(), user.to_string());
        if let Some(n) = self.state.lock().unwrap().names.get(&key) {
            return n.clone();
        }
        let name = match room.get_member_no_sync(user).await {
            Ok(Some(m)) => m.name().to_string(),
            _ => user.localpart().to_string(),
        };
        self.state.lock().unwrap().names.insert(key, name.clone());
        name
    }

    /// Applies one timeline event; returns the message it created or changed.
    /// Live events are also sent to the app.
    async fn on_event(&self, room: &Room, ev: AnySyncTimelineEvent, live: bool) -> Option<WaMsg> {
        let AnySyncTimelineEvent::MessageLike(ev) = ev else { return None };
        let chat = room.room_id().to_string();
        let id = ev.event_id().to_string();
        let millis = i64::from(ev.origin_server_ts().0);
        let (ts, ms) = (millis / 1000, (millis % 1000) as u16);
        {
            let mut st = self.state.lock().unwrap();
            st.newest.insert(chat.clone(), id.clone());
            if st.recent.last_ts.get(&chat).is_none_or(|t| *t < ts) {
                st.recent.last_ts.insert(chat.clone(), ts);
                st.recent_dirty = true;
            }
        }
        let changed = match ev {
            AnySyncMessageLikeEvent::RoomMessage(m) => match m.as_original() {
                Some(orig) => match &orig.content.relates_to {
                    Some(Relation::Replacement(r)) => {
                        let text = r.new_content.msgtype.body().to_string();
                        let mut st = self.state.lock().unwrap();
                        let m = st.seen.get_mut(r.event_id.as_str())?;
                        m.text = text;
                        m.edited = true;
                        Some(m.clone())
                    }
                    rel => {
                        let reply = match rel {
                            Some(Relation::Reply(r)) => Some(r.in_reply_to.event_id.to_string()),
                            _ => None,
                        };
                        let sender = orig.sender.to_string();
                        let sender_name = self.sender_name(room, &orig.sender).await;
                        let msg = self.message(&chat, &id, (ts, ms), sender, sender_name, &orig.content.msgtype, reply);
                        self.remember(msg.clone());
                        Some(msg)
                    }
                },
                None => {
                    let sender_name = self.sender_name(room, m.sender()).await;
                    let msg = WaMsg {
                        id: id.clone(),
                        chat: chat.clone(),
                        sender: m.sender().to_string(),
                        sender_name,
                        from_me: m.sender().as_str() == self.me,
                        ts,
                        ms,
                        kind: "text".into(),
                        deleted: true,
                        ..WaMsg::default()
                    };
                    self.remember(msg.clone());
                    Some(msg)
                }
            },
            AnySyncMessageLikeEvent::Sticker(s) => {
                let orig = s.as_original()?;
                let sender_name = self.sender_name(room, &orig.sender).await;
                let msg = WaMsg {
                    id: id.clone(),
                    chat: chat.clone(),
                    sender: orig.sender.to_string(),
                    sender_name,
                    from_me: orig.sender.as_str() == self.me,
                    ts,
                    ms,
                    kind: "sticker".into(),
                    text: orig.content.body.clone(),
                    media: true,
                    img_w: orig.content.info.width.map_or(0, |w| u64::from(w) as u32),
                    img_h: orig.content.info.height.map_or(0, |h| u64::from(h) as u32),
                    ..WaMsg::default()
                };
                let thumb = orig.content.info.thumbnail_source.clone();
                self.state.lock().unwrap().media.insert(
                    id.clone(),
                    Media { source: orig.content.source.clone().into(), thumb, size: 0, name: "sticker".into() },
                );
                self.remember(msg.clone());
                Some(msg)
            }
            AnySyncMessageLikeEvent::Reaction(r) => {
                let orig = r.as_original()?;
                let target = orig.content.relates_to.event_id.to_string();
                let reaction = Reaction { id: id.clone(), sender: orig.sender.to_string(), key: orig.content.relates_to.key.clone() };
                let mut st = self.state.lock().unwrap();
                st.reaction_of.insert(id.clone(), target.clone());
                let list = st.reactions.entry(target.clone()).or_default();
                if !list.iter().any(|x| x.id == reaction.id) {
                    list.push(reaction);
                }
                refresh_reactions(&mut st, &target)
            }
            AnySyncMessageLikeEvent::RoomRedaction(r) => {
                let target = r.as_original()?.redacts.as_ref()?.to_string();
                let mut st = self.state.lock().unwrap();
                if let Some(msg_id) = st.reaction_of.remove(&target) {
                    if let Some(list) = st.reactions.get_mut(&msg_id) {
                        list.retain(|x| x.id != target);
                    }
                    refresh_reactions(&mut st, &msg_id)
                } else {
                    let m = st.seen.get_mut(&target)?;
                    m.deleted = true;
                    m.text.clear();
                    m.media = false;
                    Some(m.clone())
                }
            }
            AnySyncMessageLikeEvent::RoomEncrypted(e) => {
                // Couldn't be decrypted (yet): the key may arrive later.
                let orig = e.as_original()?;
                let sender_name = self.sender_name(room, &orig.sender).await;
                let msg = WaMsg {
                    id: id.clone(),
                    chat: chat.clone(),
                    sender: orig.sender.to_string(),
                    sender_name,
                    from_me: orig.sender.as_str() == self.me,
                    ts,
                    ms,
                    kind: "text".into(),
                    text: "🔒 can't decrypt this message (the key is missing on this device)".into(),
                    ..WaMsg::default()
                };
                Some(msg)
            }
            _ => None,
        };
        if live {
            if let Some(m) = &changed {
                self.emit(WaEvent::Msg { msg: m.clone() });
            }
        }
        changed
    }

    fn message(&self, chat: &str, id: &str, (ts, ms): (i64, u16), sender: String, sender_name: String, msgtype: &MessageType, reply: Option<String>) -> WaMsg {
        let mut m = WaMsg {
            id: id.to_string(),
            chat: chat.to_string(),
            from_me: sender == self.me,
            sender,
            sender_name,
            ts,
            ms,
            kind: "text".into(),
            ..WaMsg::default()
        };
        if let Some(r) = reply {
            let st = self.state.lock().unwrap();
            if let Some(q) = st.seen.get(&r) {
                m.reply_name = q.sender_name.clone();
                m.reply_text = q.text.clone();
            }
            m.reply_id = r;
        }
        let size = |s: Option<matrix_sdk::ruma::UInt>| s.map_or(0, u64::from);
        let media = |source: &MediaSource, thumb: Option<MediaSource>, size: u64, name: &str| {
            self.state
                .lock()
                .unwrap()
                .media
                .insert(id.to_string(), Media { source: source.clone(), thumb, size, name: name.to_string() });
        };
        match msgtype {
            MessageType::Text(t) => m.text = strip_reply_fallback(&t.body, !m.reply_id.is_empty()),
            MessageType::Notice(t) => m.text = strip_reply_fallback(&t.body, !m.reply_id.is_empty()),
            MessageType::Emote(t) => m.text = format!("* {} {}", m.sender_name, t.body),
            MessageType::Image(i) => {
                m.kind = "image".into();
                m.media = true;
                m.text = caption(i.caption());
                if let Some(info) = &i.info {
                    m.img_w = info.width.map_or(0, |w| u64::from(w) as u32);
                    m.img_h = info.height.map_or(0, |h| u64::from(h) as u32);
                    media(&i.source, info.thumbnail_source.clone(), size(info.size), &i.body);
                } else {
                    media(&i.source, None, 0, &i.body);
                }
            }
            MessageType::Video(v) => {
                m.kind = "video".into();
                m.media = true;
                m.text = caption(v.caption());
                let info = v.info.as_deref();
                m.img_w = info.and_then(|i| i.width).map_or(0, |w| u64::from(w) as u32);
                m.img_h = info.and_then(|i| i.height).map_or(0, |h| u64::from(h) as u32);
                media(&v.source, info.and_then(|i| i.thumbnail_source.clone()), size(info.and_then(|i| i.size)), &v.body);
            }
            MessageType::Audio(a) => {
                m.kind = "audio".into();
                m.media = true;
                media(&a.source, None, size(a.info.as_deref().and_then(|i| i.size)), &a.body);
            }
            MessageType::File(f) => {
                m.kind = "document".into();
                m.media = true;
                m.file = f.filename.clone().unwrap_or_else(|| f.body.clone());
                m.text = f.caption().map(String::from).unwrap_or_default();
                media(&f.source, None, size(f.info.as_deref().and_then(|i| i.size)), &m.file);
            }
            MessageType::Location(l) => {
                m.kind = "location".into();
                m.text = format!("{} {}", l.body, l.geo_uri);
            }
            other => {
                m.kind = other.msgtype().to_string();
                m.text = other.body().to_string();
            }
        }
        m
    }

    fn remember(&self, m: WaMsg) {
        let mut st = self.state.lock().unwrap();
        if st.seen.len() >= MAX_SEEN {
            st.seen.clear();
        }
        st.seen.insert(m.id.clone(), m);
    }

    async fn command(&self, cmd: Value) {
        let s = |k: &str| cmd[k].as_str().unwrap_or("").to_string();
        let (chat, id) = (s("chat"), s("id"));
        let result: Result<(), String> = async {
            match cmd["c"].as_str().unwrap_or("") {
                "history" => self.history(&chat, cmd["before_ts"].as_i64().unwrap_or(0) > 0).await,
                "send" => {
                    let room = self.room(&chat).ok_or("unknown room")?;
                    let mut content = RoomMessageEventContent::text_plain(s("text"));
                    if let Some(r) = event_id(&s("reply_to")) {
                        content.relates_to = Some(Relation::Reply(Reply::new(InReplyTo::new(r))));
                    }
                    room.send(content).await.map_err(|e| e.to_string())?;
                    Ok(())
                }
                "edit" => {
                    let room = self.room(&chat).ok_or("unknown room")?;
                    let target = event_id(&id).ok_or("bad message id")?;
                    let new = RoomMessageEventContentWithoutRelation::text_plain(s("text"));
                    room.send(new.make_replacement(ReplacementMetadata::new(target, None))).await.map_err(|e| e.to_string())?;
                    Ok(())
                }
                "delete" => {
                    let room = self.room(&chat).ok_or("unknown room")?;
                    let target = event_id(&id).ok_or("bad message id")?;
                    room.redact(&target, None, None).await.map_err(|e| e.to_string())?;
                    Ok(())
                }
                "react" => self.react(&chat, &id, &s("text")).await,
                "seen" => {
                    let room = self.room(&chat).ok_or("unknown room")?;
                    let (newest, public) = {
                        let st = self.state.lock().unwrap();
                        (st.newest.get(&chat).cloned(), st.recent.receipts)
                    };
                    // Not seen since the last start: ask the server for the newest event.
                    let newest = match newest {
                        Some(id) => Some(id),
                        None => {
                            let mut opts = MessagesOptions::backward();
                            opts.limit = 1u32.into();
                            let page = room.messages(opts).await.map_err(|e| e.to_string())?;
                            page.chunk.first().and_then(|e| e.event_id()).map(|id| id.to_string())
                        }
                    };
                    if let Some(ev) = newest.as_deref().and_then(event_id) {
                        // Private receipts clear unread counts without telling anyone.
                        let kind = if public { ReceiptType::Read } else { ReceiptType::ReadPrivate };
                        room.send_single_receipt(kind, ReceiptThread::Unthreaded, ev).await.map_err(|e| e.to_string())?;
                    }
                    if let Some(mut c) = self.chat(&room).await {
                        c.unread = 0;
                        self.emit(WaEvent::Chat { chat: c });
                    }
                    Ok(())
                }
                "receipts" => {
                    let on = cmd["on"].as_bool().unwrap_or(false);
                    let mut st = self.state.lock().unwrap();
                    st.recent.receipts = on;
                    st.recent.save();
                    self.emit(WaEvent::Receipts { on });
                    Ok(())
                }
                "thumb" => {
                    let data = self.picture(&id, cmd["full"].as_bool().unwrap_or(false)).await.unwrap_or_default();
                    self.emit(WaEvent::Thumb { key: s("key"), data: base64::engine::general_purpose::STANDARD.encode(data) });
                    Ok(())
                }
                "download" => {
                    self.emit(WaEvent::Status { status: "downloading…".into() });
                    let path = self.download(&id).await;
                    self.emit(WaEvent::Status { status: "connected".into() });
                    self.emit(WaEvent::File { path: path? });
                    Ok(())
                }
                "logout" => {
                    let result = self.client.logout().await;
                    forget();
                    self.emit(WaEvent::LoggedOut { msg: "logged out of Matrix".into() });
                    result.map_err(|e| e.to_string())
                }
                _ => Ok(()),
            }
        }
        .await;
        if let Err(e) = result {
            self.emit(WaEvent::Error { msg: e });
        }
    }

    /// The newest page of a room, or the next older one.
    async fn history(&self, chat: &str, older: bool) -> Result<(), String> {
        let room = self.room(chat).ok_or("unknown room")?;
        let from = if older {
            match self.state.lock().unwrap().back.get(chat) {
                Some(Some(token)) => Some(token.clone()),
                _ => None,
            }
        } else {
            None
        };
        if older && from.is_none() {
            self.emit(WaEvent::History { chat: chat.into(), older, msgs: Vec::new(), complete: true });
            return Ok(());
        }
        let mut opts = MessagesOptions::backward();
        opts.from = from;
        opts.limit = PAGE.into();
        let page = room.messages(opts).await.map_err(|e| e.to_string())?;
        self.state.lock().unwrap().back.insert(chat.to_string(), page.end.clone());
        // Oldest first, so edits, reactions and deletes find their message.
        let mut msgs: Vec<WaMsg> = Vec::new();
        for ev in page.chunk.iter().rev() {
            let Ok(ev) = ev.raw().deserialize() else { continue };
            if let Some(m) = self.on_event(&room, ev, false).await {
                match msgs.iter_mut().find(|x| x.id == m.id) {
                    Some(x) => *x = m,
                    None => msgs.push(m),
                }
            }
        }
        if !older {
            if let Some(m) = msgs.last() {
                self.state.lock().unwrap().newest.insert(chat.to_string(), m.id.clone());
            }
        }
        self.emit(WaEvent::History { chat: chat.into(), older, msgs, complete: page.end.is_none() });
        Ok(())
    }

    /// Toggles our reaction `key` on a message.
    async fn react(&self, chat: &str, id: &str, key: &str) -> Result<(), String> {
        let room = self.room(chat).ok_or("unknown room")?;
        let own = {
            let st = self.state.lock().unwrap();
            st.reactions.get(id).and_then(|l| l.iter().find(|r| r.sender == self.me && r.key == key).map(|r| r.id.clone()))
        };
        match own.as_deref().and_then(event_id) {
            Some(reaction) => {
                room.redact(&reaction, None, None).await.map_err(|e| e.to_string())?;
            }
            None => {
                let target = event_id(id).ok_or("bad message id")?;
                room.send(ReactionEventContent::new(Annotation::new(target, key.to_string()))).await.map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// Picture bytes: a small thumbnail, or (`full`) the original.
    async fn picture(&self, id: &str, full: bool) -> Option<Vec<u8>> {
        let (source, thumb, size) = {
            let st = self.state.lock().unwrap();
            let m = st.media.get(id)?;
            (m.source.clone(), m.thumb.clone(), m.size)
        };
        let media = self.client.media();
        let file = |source: MediaSource| MediaRequestParameters { source, format: MediaFormat::File };
        let request = match (full, thumb, &source) {
            (false, Some(t), _) => file(t),
            // The server scales unencrypted pictures itself.
            (_, _, MediaSource::Plain(_)) => MediaRequestParameters {
                source: source.clone(),
                format: MediaFormat::Thumbnail(MediaThumbnailSettings::with_method(
                    Method::Scale,
                    if full { uint!(640) } else { uint!(96) },
                    if full { uint!(480) } else { uint!(96) },
                )),
            },
            // Encrypted, without a thumbnail: only small files.
            (false, None, _) if size == 0 || size > MAX_INLINE_FILE => return None,
            _ => file(source.clone()),
        };
        media.get_media_content(&request, true).await.ok()
    }

    async fn download(&self, id: &str) -> Result<String, String> {
        let (source, name) = {
            let st = self.state.lock().unwrap();
            let m = st.media.get(id).ok_or("no file in this message")?;
            (m.source.clone(), m.name.clone())
        };
        let data = self
            .client
            .media()
            .get_media_content(&MediaRequestParameters { source, format: MediaFormat::File }, false)
            .await
            .map_err(|e| format!("download failed: {e}"))?;
        let dir = secret::cache_dir().join("matrix");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let short: String = id.chars().filter(char::is_ascii_alphanumeric).take(8).collect();
        let path = dir.join(format!("{short}-{}", safe_name(&name)));
        std::fs::write(&path, data).map_err(|e| e.to_string())?;
        Ok(path.to_string_lossy().into_owned())
    }
}

/// Rewrites a message's reaction summary ("👍 2  ❤️") after a change.
fn refresh_reactions(st: &mut Shared, target: &str) -> Option<WaMsg> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for r in st.reactions.get(target).into_iter().flatten() {
        match counts.iter_mut().find(|(k, _)| *k == r.key) {
            Some((_, n)) => *n += 1,
            None => counts.push((r.key.clone(), 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    let summary: Vec<String> = counts.into_iter().map(|(k, n)| if n > 1 { format!("{k} {n}") } else { k }).collect();
    let m = st.seen.get_mut(target)?;
    m.reactions = summary.join("  ");
    Some(m.clone())
}

/// A media message's caption (without one, its body is just the file name).
fn caption(caption: Option<&str>) -> String {
    caption.unwrap_or("").to_string()
}

/// Older clients quote the replied-to message in the body ("> <@a:b> text"
/// lines and a blank line); the reply is shown separately.
fn strip_reply_fallback(body: &str, is_reply: bool) -> String {
    if !is_reply || !body.starts_with("> ") {
        return body.to_string();
    }
    let mut lines = body.lines();
    for line in lines.by_ref() {
        if !line.starts_with('>') {
            break;
        }
    }
    let rest: Vec<&str> = lines.collect();
    rest.join("\n")
}

/// A file name that is safe to create in the download folder.
fn safe_name(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or("");
    let name: String = name
        .chars()
        .map(|c| if c < ' ' || "<>:\"/\\|?*".contains(c) || ('\u{202A}'..='\u{202E}').contains(&c) || ('\u{2066}'..='\u{2069}').contains(&c) { '_' } else { c })
        .collect();
    let name = name.trim_matches(['.', ' ']);
    let name: String = name.chars().rev().take(120).collect::<Vec<_>>().into_iter().rev().collect();
    if name.is_empty() {
        "file".into()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_reply_quotes() {
        assert_eq!(strip_reply_fallback("> <@a:b> hi\n> there\n\nanswer", true), "answer");
        assert_eq!(strip_reply_fallback("> not a reply", false), "> not a reply");
        assert_eq!(strip_reply_fallback("plain", true), "plain");
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(safe_name("../../evil.exe"), "evil.exe");
        assert_eq!(safe_name("a:b\\c.txt"), "c.txt");
        assert_eq!(safe_name("..."), "file");
        assert_eq!(safe_name("x\u{202E}gpj.exe"), "x_gpj.exe");
    }
}
