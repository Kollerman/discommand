//! The chat services (Discord, WhatsApp, Matrix): their events, opening and
//! loading chats, marking them read, sending, editing, reacting, deleting and
//! opening links and media. Also the Discord status (away while idle).

use super::*;

impl App {
    /// Whether Discord should show the account as away once input stops:
    /// not yet away, and not set to Do Not Disturb or invisible there.
    pub(super) fn can_go_away(&self) -> bool {
        Service::Discord.built() && !self.away && matches!(self.discord_status.as_str(), "" | "online")
    }

    /// A key press or paste: back from away.
    pub(super) fn active(&mut self) {
        self.last_input = Instant::now();
        if self.away {
            self.away = false;
            self.presence("online");
        }
    }

    /// Sets this session's status in Discord ("online" or "idle", which
    /// Discord shows as away).
    pub(super) fn presence(&self, status: &str) {
        let idle = status == "idle";
        let since = if idle { chrono::Utc::now().timestamp_millis() } else { 0 };
        let op = json!({"op": 3, "d": {"status": status, "since": since, "activities": [], "afk": idle}});
        let _ = self.gw.send(op.to_string());
    }

    pub(super) fn on_wa(&mut self, s: Service, ev: WaEvent) {
        let viewing = |app: &Self, chat: &str| app.service == s && app.open_of(s).as_deref() == Some(chat);
        match ev {
            WaEvent::Linked { me, name, receipts } => {
                let st = self.chats_mut(s);
                st.me = me;
                st.name = name;
                st.receipts = receipts;
                if st.conn.is_empty() {
                    st.conn = "connecting…".into();
                }
            }
            WaEvent::Chats { chats } => {
                self.chats_mut(s).set_chats(chats);
                self.tree_dirty = true;
            }
            WaEvent::Chat { chat } => {
                if viewing(self, &chat.jid) && chat.unread > 0 {
                    wa::send(self.cmd_of(s), json!({"c": "seen", "chat": chat.jid}));
                }
                self.chats_mut(s).upsert_chat(chat);
                self.tree_dirty = true;
            }
            WaEvent::Msg { msg } => {
                let viewing = viewing(self, &msg.chat);
                let chat = self.chats(s).chats.get(&msg.chat);
                // Edits and reactions also arrive as "msg"; only new incoming DMs ring.
                let new = chat.map_or(true, |c| msg.ts >= c.last_ts) && !msg.edited && !msg.deleted;
                if new && !msg.from_me && !chat.is_some_and(|c| c.is_group) {
                    self.ring(viewing);
                }
                self.chats_mut(s).upsert_msg(msg);
                if viewing {
                    self.clamp_sel();
                }
            }
            WaEvent::History { chat, older, msgs, complete } => {
                let viewing = viewing(self, &chat);
                if !older && viewing {
                    self.done_refreshing();
                }
                let added = self.chats_mut(s).history(chat, older, msgs, complete);
                if viewing {
                    if let Some(sel) = self.msg_sel.as_mut() {
                        *sel += added;
                    }
                }
            }
            WaEvent::Status { status } => self.chats_mut(s).conn = status,
            WaEvent::Receipts { on } => {
                self.chats_mut(s).receipts = on;
                self.status = format!("{} read receipts {}", s.name(), if on { "on" } else { "off" });
            }
            WaEvent::File { path, purpose, id } if purpose == "transcribe" => {
                let key = voice::key(s.name(), &id);
                let context = self.voice.context(&key);
                self.spawn(async move {
                    let result = match std::fs::read(&path) {
                        Ok(data) => voice::transcribe(data, context).await,
                        Err(e) => Err(format!("couldn't read the voice message: {e}")),
                    };
                    Some(AppEvent::Transcript(key, result))
                });
            }
            WaEvent::File { path, purpose, id } if purpose == "play" => {
                let key = voice::key(s.name(), &id);
                self.spawn(async move {
                    let result = match std::fs::read(&path) {
                        Ok(data) => sound::play(data).await,
                        Err(e) => Err(format!("couldn't read the voice message: {e}")),
                    };
                    Some(AppEvent::Playing(key, result))
                });
            }
            WaEvent::File { path, .. } if crate::pdf::is_pdf(&path) => self.open_pdf(path.into()),
            WaEvent::File { path, .. } => match open_file(&path) {
                Ok(true) => {}
                Ok(false) => self.status = format!("saved {path}; not opened because this file type can run code"),
                Err(e) => self.status = format!("could not open {path}: {e}"),
            },
            WaEvent::Thumb { key, data } => {
                match base64::engine::general_purpose::STANDARD.decode(data) {
                    Ok(bytes) if !bytes.is_empty() => self.spawn(async move {
                        let pic = tokio::task::spawn_blocking({
                            let key = key.clone();
                            move || pics::decode_for(&key, &bytes)
                        })
                        .await
                        .ok()
                        .flatten();
                        Some(AppEvent::Pic { key, pic })
                    }),
                    _ => self.pics.finish(key, None),
                }
            }
            WaEvent::Error { msg } => {
                // A voice message waiting for its download won't get it.
                self.voice.cancel(s.name());
                self.status = format!("{}: {msg}", s.name());
            }
            WaEvent::Names => {
                let open = self.open_of(s).clone();
                if let Some(chat) = self.chats_mut(s).forget_messages(open.as_deref()) {
                    wa::send(self.cmd_of(s), json!({"c": "history", "chat": chat}));
                }
                self.clamp_sel();
                self.tree_dirty = true;
            }
            WaEvent::Fatal { msg } | WaEvent::LoggedOut { msg } => {
                self.chats_mut(s).conn = "offline".into();
                self.status = format!("{}: {msg} (restart discommand to log in again)", s.name());
            }
            WaEvent::Exited => {
                self.chats_mut(s).conn = "offline".into();
                if self.status.is_empty() {
                    self.status = format!("{} stopped", s.name());
                }
            }
            _ => {}
        }
    }

    pub(super) fn on_dispatch(&mut self, t: &str, d: &str) {
        match t {
            "READY" => match serde_json::from_str::<Ready>(d) {
                Ok(r) => {
                    self.discord_status = r.user_settings.as_ref().map(|s| s.status.clone()).unwrap_or_default();
                    self.state.load_ready(r);
                    self.subscribed.clear();
                    self.loading.clear();
                    if let Some(c) = self.current.clone() {
                        if self.state.channels.contains_key(&c) {
                            self.subscribe(&c);
                            self.fetch(c, None);
                        } else {
                            self.current = None;
                        }
                    }
                }
                Err(e) => self.status = format!("could not parse READY: {e}"),
            },
            "GUILD_CREATE" => {
                if let Ok(g) = serde_json::from_str::<Guild>(d) {
                    self.state.add_guild(g, Vec::new());
                }
            }
            "GUILD_UPDATE" => {
                if let Ok(g) = serde_json::from_str::<Guild>(d) {
                    self.state.update_guild(g);
                }
            }
            "GUILD_DELETE" => {
                if let Ok(g) = serde_json::from_str::<GuildDelete>(d) {
                    if !g.unavailable {
                        self.state.remove_guild(&g.id);
                    }
                }
            }
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" => {
                if let Ok(c) = serde_json::from_str::<Channel>(d) {
                    self.state.upsert_channel(c);
                }
            }
            "CHANNEL_DELETE" => {
                if let Ok(c) = serde_json::from_str::<Channel>(d) {
                    self.state.remove_channel(&c.id);
                    if self.current.as_deref() == Some(c.id.as_str()) {
                        self.current = None;
                    }
                }
            }
            "MESSAGE_CREATE" => {
                if let Ok(m) = serde_json::from_str::<Message>(d) {
                    self.on_message(m);
                }
            }
            "MESSAGE_UPDATE" => {
                if let Ok(m) = serde_json::from_str::<Message>(d) {
                    self.state.update_message(m);
                }
            }
            "MESSAGE_DELETE" => {
                if let Ok(m) = serde_json::from_str::<MessageDelete>(d) {
                    if let Some(idx) = self.state.delete_message(&m.channel_id, &m.id) {
                        if self.current.as_deref() == Some(m.channel_id.as_str()) {
                            if let Some(sel) = self.msg_sel.as_mut() {
                                if *sel > idx {
                                    *sel -= 1;
                                }
                            }
                            self.clamp_sel();
                        }
                    }
                }
            }
            "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" => {
                if let Ok(r) = serde_json::from_str::<ReactionEvent>(d) {
                    if let Some(e) = &r.emoji {
                        let mine = r.user_id == self.state.me.id;
                        self.state.reaction_changed(&r.channel_id, &r.message_id, e, mine, t == "MESSAGE_REACTION_ADD");
                    }
                }
            }
            "MESSAGE_REACTION_REMOVE_ALL" | "MESSAGE_REACTION_REMOVE_EMOJI" => {
                if let Ok(r) = serde_json::from_str::<ReactionEvent>(d) {
                    self.state.reactions_cleared(&r.channel_id, &r.message_id, r.emoji.as_ref());
                }
            }
            "MESSAGE_ACK" => {
                if let Ok(a) = serde_json::from_str::<MessageAck>(d) {
                    let id = value_sf(&a.message_id);
                    let read = self.state.read.entry(a.channel_id.clone()).or_default();
                    *read = (*read).max(id);
                    let acked = self.state.acked.entry(a.channel_id.clone()).or_default();
                    *acked = (*acked).max(id);
                    self.state.mentions.remove(&a.channel_id);
                }
            }
            "USER_GUILD_SETTINGS_UPDATE" => {
                if let Ok(s) = serde_json::from_str::<GuildSettings>(d) {
                    self.state.apply_guild_settings(s);
                }
            }
            _ => return,
        }
        self.tree_dirty = true;
    }

    fn on_message(&mut self, m: Message) {
        let ch = m.channel_id.clone();
        let id = sf(&m.id);
        let mine = m.author.id == self.state.me.id;
        let viewing = self.service == Service::Discord && self.current.as_deref() == Some(ch.as_str());
        let ping = self.state.mentions_me(&m);
        self.state.push_message(m);
        if mine || viewing {
            let read = self.state.read.entry(ch.clone()).or_default();
            *read = (*read).max(id);
        }
        if mine {
            // Sending a message marks the channel read server-side.
            self.state.acked.insert(ch.clone(), id);
        } else if ping && !viewing {
            *self.state.mentions.entry(ch.clone()).or_default() += 1;
        }
        if !mine && (ping || self.state.channels.get(&ch).is_some_and(|c| c.is_dm())) {
            self.ring(viewing);
        }
        if self.state.channels.get(&ch).is_some_and(|c| c.is_dm()) {
            self.state.sort_dms();
        }
        if viewing {
            self.clamp_sel();
        }
    }

    pub(super) fn on_messages(&mut self, channel: String, older: bool, result: Result<Vec<Message>, String>) {
        self.loading.remove(&channel);
        self.state.version += 1;
        let mut list = match result {
            Ok(l) => l,
            Err(e) => {
                self.status = format!("failed to load messages: {e}");
                return;
            }
        };
        list.reverse();
        if list.len() < 50 {
            self.state.complete.insert(channel.clone());
        }
        let viewing = self.current.as_deref() == Some(channel.as_str());
        if older {
            let existing = self.state.messages.entry(channel.clone()).or_default();
            let first = existing.first().map(|m| sf(&m.id)).unwrap_or(u64::MAX);
            list.retain(|m| sf(&m.id) < first);
            let added = list.len();
            list.append(existing);
            *existing = list;
            if viewing {
                if let Some(sel) = self.msg_sel.as_mut() {
                    *sel += added;
                }
            }
        } else {
            // Keep anything the gateway delivered while the request was in flight.
            let last = list.last().map(|m| sf(&m.id)).unwrap_or(0);
            for m in self.state.messages.remove(&channel).unwrap_or_default() {
                if sf(&m.id) > last {
                    list.push(m);
                }
            }
            self.state.messages.insert(channel.clone(), list);
            if viewing {
                self.clamp_sel();
                self.done_refreshing();
            }
            if viewing {
                self.mark_read(&channel);
            }
        }
        self.tree_dirty = true;
    }

    pub(super) fn fetch(&mut self, channel: String, before: Option<String>) {
        if !self.loading.insert(channel.clone()) {
            return;
        }
        self.state.version += 1;
        let api = self.api.clone();
        self.spawn(async move {
            let result = api.messages(&channel, before.as_deref()).await;
            Some(AppEvent::Messages { channel, older: before.is_some(), result })
        });
    }

    pub(super) fn fetch_older(&mut self) {
        if matches!(self.service, Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle | Service::GroupWise) {
            return;
        }
        if self.service.is_wa_like() {
            let s = self.service;
            let Some(chat) = self.open_of(s).clone() else { return };
            if self.chats(s).complete.contains(&chat) || self.chats(s).loading.contains(&chat) {
                return;
            }
            let Some(first) = self.wa_messages().first() else { return };
            let cmd = json!({"c": "history", "chat": chat, "before_ts": first.ts, "before_id": first.id});
            self.chats_mut(s).loading.insert(chat);
            self.chats_mut(s).version += 1;
            wa::send(self.cmd_of(s), cmd);
            return;
        }
        let Some(ch) = self.current.clone() else { return };
        if self.state.complete.contains(&ch) {
            return;
        }
        let before = self.current_messages().first().map(|m| m.id.clone());
        if before.is_some() {
            self.fetch(ch, before);
        }
    }

    fn subscribe(&mut self, channel: &str) {
        let Some(g) = self.state.channels.get(channel).and_then(|c| c.guild_id.clone()) else { return };
        if self.subscribed.insert(g.clone()) {
            // Large guilds only stream messages to user clients that subscribe.
            let op = json!({"op": 14, "d": {"guild_id": g, "typing": false, "activities": false, "threads": false}});
            let _ = self.gw.send(op.to_string());
        }
    }

    /// Marks every chat of these messaging services read, on the service
    /// too (WhatsApp sends read receipts only if they're on; Matrix sends
    /// private ones unless public ones are on).
    pub(super) fn read_all(&mut self, services: &[Service]) {
        let mut n = 0;
        for &s in services.iter().filter(|s| s.built()) {
            match s {
                Service::Discord => {
                    let st = &self.state;
                    let unread: Vec<(String, u64)> = st
                        .channels
                        .values()
                        .filter(|c| st.is_unread(c) || st.mention_count(&c.id) > 0)
                        .map(|c| (c.id.clone(), c.last_id()))
                        .filter(|(_, last)| *last > 0)
                        .collect();
                    for (id, last) in &unread {
                        self.state.read.insert(id.clone(), *last);
                        self.state.acked.insert(id.clone(), *last);
                    }
                    self.state.mentions.clear();
                    n += unread.len();
                    if !unread.is_empty() {
                        let read: Vec<(String, String)> = unread.into_iter().map(|(c, last)| (c, last.to_string())).collect();
                        let api = self.api.clone();
                        self.spawn(async move {
                            api.ack_bulk(&read).await.err().map(|e| AppEvent::Error(format!("Discord: marking read failed: {e}")))
                        });
                    }
                }
                Service::WhatsApp | Service::Matrix => {
                    let chats: Vec<String> = self.chats(s).chats.values().filter(|c| c.unread > 0).map(|c| c.jid.clone()).collect();
                    for jid in &chats {
                        wa::send(self.cmd_of(s), json!({"c": "seen", "chat": jid}));
                        if let Some(c) = self.chats_mut(s).chats.get_mut(jid) {
                            c.unread = 0;
                        }
                    }
                    n += chats.len();
                }
                _ => {}
            }
        }
        self.status = match n {
            0 => "nothing unread".into(),
            1 => "marked 1 chat read".into(),
            n => format!("marked {n} chats read"),
        };
        self.tree_dirty = true;
    }

    pub(super) fn mark_read(&mut self, channel: &str) {
        let Some(c) = self.state.channels.get(channel) else { return };
        let loaded = self.state.messages.get(channel).and_then(|l| l.last()).map(|m| sf(&m.id)).unwrap_or(0);
        let last = c.last_id().max(loaded);
        self.state.mentions.remove(channel);
        if last == 0 {
            return;
        }
        let read = self.state.read.entry(channel.to_string()).or_default();
        *read = (*read).max(last);
        self.ack(channel);
    }

    fn ack(&mut self, channel: &str) {
        let read = self.state.read.get(channel).copied().unwrap_or(0);
        if read == 0 || read <= self.state.acked.get(channel).copied().unwrap_or(0) {
            return;
        }
        self.state.acked.insert(channel.to_string(), read);
        let api = self.api.clone();
        let ch = channel.to_string();
        self.spawn(async move { api.ack(&ch, &read.to_string()).await.err().map(AppEvent::Error) });
    }

    /// Acks the open channel; awaited on exit so read state isn't lost.
    pub async fn flush_ack(&mut self) {
        let Some(ch) = self.current.clone() else { return };
        let read = self.state.read.get(&ch).copied().unwrap_or(0);
        if read > self.state.acked.get(&ch).copied().unwrap_or(0) {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), self.api.ack(&ch, &read.to_string())).await;
        }
    }

    pub fn open_channel(&mut self, id: String) {
        if self.current.as_deref() == Some(id.as_str()) {
            self.enter_insert();
            return;
        }
        if let Some(prev) = self.current.take() {
            self.ack(&prev);
        }
        self.msg_sel = None;
        self.msg_scroll = 0;
        if self.mode != Mode::Normal {
            self.mode = Mode::Normal;
            self.input.clear();
        }
        self.subscribe(&id);
        self.current = Some(id.clone());
        if self.state.messages.contains_key(&id) {
            self.mark_read(&id);
        } else {
            self.fetch(id, None);
        }
        self.tree_dirty = true;
        self.enter_insert();
    }

    /// Opens a chat of the WhatsApp-style service shown.
    pub fn open_wa_chat(&mut self, jid: String) {
        let s = self.service;
        if self.open_of(s).as_deref() == Some(jid.as_str()) {
            self.enter_insert();
            return;
        }
        self.msg_sel = None;
        self.msg_scroll = 0;
        if self.mode != Mode::Normal {
            self.mode = Mode::Normal;
            self.input.clear();
        }
        let st = self.chats_mut(s);
        if !st.messages.contains_key(&jid) && st.loading.insert(jid.clone()) {
            st.version += 1;
            wa::send(self.cmd_of(s), json!({"c": "history", "chat": jid}));
        }
        if self.chats(s).chats.get(&jid).is_some_and(|c| c.unread > 0) {
            wa::send(self.cmd_of(s), json!({"c": "seen", "chat": jid}));
        }
        *self.open_of_mut(s) = Some(jid);
        self.tree_dirty = true;
        self.enter_insert();
    }

    /// Opens a chat of the active service and selects it in the sidebar.
    pub(super) fn open_chat(&mut self, id: String) {
        self.reveal_in_tree(&id);
        match self.service {
            Service::Discord => self.open_channel(id),
            Service::WhatsApp | Service::Matrix => self.open_wa_chat(id),
            Service::Calendar => {
                self.cal_select(&id);
                self.focus = Focus::Tree;
            }
            Service::Tasks | Service::Twos | Service::Moodle | Service::GroupWise => self.focus = Focus::Tree,
        }
    }

    /// Picks a file (`path` typed, else the file dialog) to send with the next
    /// message in the open chat; the input then takes its caption.
    pub fn attach(&mut self, path: Option<&str>) {
        if !self.service.is_messaging() {
            self.status = "files can be sent in Discord, WhatsApp and Matrix chats".into();
            return;
        }
        let Some(chat) = self.current_chat().map(String::from) else {
            self.status = "open a chat first".into();
            return;
        };
        let path = match path.map(|p| p.trim().trim_matches('"')).filter(|p| !p.is_empty()) {
            Some(p) => std::path::PathBuf::from(p),
            None => {
                let picked = crate::system::pick::file("Send a file");
                // The dialog was on top of the terminal: draw everything again.
                self.dirty = true;
                match picked {
                    Ok(Some(p)) => p,
                    Ok(None) => return,
                    Err(e) => {
                        self.status = e;
                        return;
                    }
                }
            }
        };
        if !path.is_file() {
            self.status = format!("{} isn't a file", path.display());
            return;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.attachment = Some((self.service, chat, path));
        self.status = format!("📎 {name}: type a caption (optional), Enter sends, Esc removes it");
        self.enter_insert();
    }

    /// The file waiting to be sent in the chat shown, if any.
    pub fn pending_file(&self) -> Option<&std::path::Path> {
        let (s, chat, path) = self.attachment.as_ref()?;
        (*s == self.service && self.current_chat() == Some(chat.as_str())).then_some(path.as_path())
    }

    pub(super) fn submit(&mut self) {
        let text = self.input.text();
        let content = text.trim_end().to_string();
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        if let (Some(path), false) = (self.pending_file().map(|p| p.to_path_buf()), matches!(mode, Mode::Edit(_))) {
            self.attachment = None;
            self.input.clear();
            self.msg_sel = None;
            return self.send_file(path, content, mode);
        }
        if content.trim().is_empty() {
            self.input.clear();
            return;
        }
        // ":q" typed in insert mode is meant for vim, not for the chat.
        if matches!(content.trim(), ":q" | ":q!" | ":qa" | ":qa!" | ":wq" | ":x" | ":quit") {
            self.input.clear();
            self.quit = true;
            return;
        }
        if self.service.is_tasks() {
            self.input.clear();
            self.leave_insert();
            let s = self.service;
            let Some(api) = self.tab(s).api.clone() else { return };
            let target = match mode {
                Mode::Edit(key) => key.split_once('\n').map(|(l, id)| (l.to_string(), Some(id.to_string()))),
                _ => self.target_list().map(|l| (l, None)),
            };
            let Some((list, id)) = target else {
                self.status = "no task list".into();
                return;
            };
            self.status = "saving…".into();
            self.spawn(async move {
                let result = match &id {
                    Some(id) => api.update(&list, id, &content).await,
                    None => api.create(&list, &content).await,
                };
                Some(AppEvent::Todo(s, match result {
                    Ok(()) => TodoMsg::Changed(if id.is_some() { "task updated".into() } else { "task added".into() }, true),
                    Err(e) => TodoMsg::Error(e),
                }))
            });
            return;
        }
        if self.service == Service::Calendar {
            self.input.clear();
            self.leave_insert();
            let Some(g) = self.google.clone() else { return };
            let target = match mode {
                Mode::Edit(key) => key.split_once('\n').map(|(c, id)| (c.to_string(), Some(id.to_string()))),
                _ => self.cal.default_calendar().map(|c| (c.to_string(), None)),
            };
            let Some((cal, id)) = target else {
                self.status = "no writable calendar".into();
                return;
            };
            self.status = "saving…".into();
            self.spawn(async move {
                let result = match &id {
                    Some(id) => g.update(&cal, id, &content).await,
                    None => g.create(&cal, &content).await,
                };
                Some(AppEvent::Cal(match result {
                    Ok(()) => CalMsg::Changed(if id.is_some() { "event updated".into() } else { "event created".into() }),
                    Err(e) => CalMsg::Error(e),
                }))
            });
            return;
        }
        if self.service.is_wa_like() {
            let Some(chat) = self.open_of(self.service).clone() else {
                self.status = "open a chat first".into();
                self.mode = mode;
                return;
            };
            self.input.clear();
            self.msg_sel = None;
            let cmd = match mode {
                Mode::Edit(id) => json!({"c": "edit", "chat": chat, "id": id, "text": content}),
                Mode::Reply { id, .. } => json!({"c": "send", "chat": chat, "text": content, "reply_to": id}),
                Mode::Normal => json!({"c": "send", "chat": chat, "text": content}),
            };
            wa::send(self.cmd_of(self.service), cmd);
            return;
        }
        let Some(ch) = self.current.clone() else {
            self.status = "open a channel first".into();
            self.mode = mode;
            return;
        };
        self.input.clear();
        self.msg_sel = None;
        let api = self.api.clone();
        self.spawn(async move {
            let result = match &mode {
                Mode::Edit(id) => api.edit(&ch, id, &content).await,
                Mode::Reply { id, ping } => api.send(&ch, &content, Some((id.as_str(), *ping))).await,
                Mode::Normal => api.send(&ch, &content, None).await,
            };
            Some(match result {
                Ok(m) => AppEvent::Sent(m),
                Err(e) => AppEvent::Error(format!("send failed: {e}")),
            })
        });
    }

    /// Sends a file to the open chat, `caption` as its text.
    fn send_file(&mut self, path: std::path::PathBuf, caption: String, mode: Mode) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let reply = match &mode {
            Mode::Reply { id, ping } => Some((id.clone(), *ping)),
            _ => None,
        };
        if self.service.is_wa_like() {
            let Some(chat) = self.open_of(self.service).clone() else { return };
            let mut cmd = json!({"c": "file", "chat": chat, "path": path.to_string_lossy(), "text": caption});
            if let Some((id, _)) = reply {
                cmd["reply_to"] = json!(id);
            }
            wa::send(self.cmd_of(self.service), cmd);
            self.status = format!("sending {name}…");
            return;
        }
        let Some(ch) = self.current.clone() else { return };
        let api = self.api.clone();
        self.status = format!("sending {name}…");
        self.spawn(async move {
            let reply = reply.as_ref().map(|(id, ping)| (id.as_str(), *ping));
            Some(match api.send_file(&ch, path, &caption, reply).await {
                Ok(m) => AppEvent::Sent(m),
                Err(e) => AppEvent::Error(format!("sending {name} failed: {e}")),
            })
        });
    }

    pub(super) fn start_edit(&mut self, idx: usize) {
        let found = match self.service {
            Service::Discord => self
                .current_messages()
                .get(idx)
                .map(|m| (m.author.id == self.state.me.id, m.id.clone(), m.content.clone())),
            Service::WhatsApp | Service::Matrix => self
                .wa_messages()
                .get(idx)
                .map(|m| (m.from_me && m.kind == "text" && !m.deleted, m.id.clone(), m.text.clone())),
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle | Service::GroupWise => None,
        };
        let Some((own, id, content)) = found else { return };
        if !own {
            self.status = "you can only edit your own text messages".into();
            return;
        }
        self.msg_sel = Some(idx);
        self.mode = Mode::Edit(id);
        self.input.set(&content);
        self.enter_insert();
    }

    fn clamp_sel(&mut self) {
        let len = self.current_len();
        if let Some(s) = self.msg_sel {
            self.msg_sel = if len == 0 { None } else { Some(s.min(len - 1)) };
        }
    }

    /// Toggles our reaction with `emoji` on the selected message.
    pub(super) fn react(&mut self, emoji: &str) {
        match self.service {
            Service::Discord => {
                let Some(m) = self.selected_message() else { return };
                let e = ReactionEmoji::unicode(emoji);
                let add = !m.reactions.iter().any(|r| r.me && r.emoji.same(&e));
                let (ch, id) = (m.channel_id.clone(), m.id.clone());
                self.state.reaction_changed(&ch, &id, &e, true, add);
                let (api, emoji) = (self.api.clone(), emoji.to_string());
                self.spawn(async move {
                    api.react(&ch, &id, &emoji, add).await.err().map(|e| AppEvent::Error(format!("reaction failed: {e}")))
                });
            }
            Service::WhatsApp | Service::Matrix => {
                let Some(m) = self.msg_sel.and_then(|i| self.wa_messages().get(i)) else { return };
                if m.deleted {
                    return;
                }
                let cmd = json!({"c": "react", "chat": m.chat, "id": m.id, "text": emoji});
                wa::send(self.cmd_of(self.service), cmd);
            }
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle | Service::GroupWise => {}
        }
    }

    pub(super) fn delete_selected(&mut self) {
        match self.service {
            Service::Discord => {
                let Some(m) = self.selected_message() else { return };
                let (ch, id) = (m.channel_id.clone(), m.id.clone());
                let api = self.api.clone();
                self.spawn(async move { api.delete(&ch, &id).await.err().map(|e| AppEvent::Error(format!("delete failed: {e}"))) });
            }
            Service::WhatsApp | Service::Matrix => {
                let Some(m) = self.msg_sel.and_then(|i| self.wa_messages().get(i)) else { return };
                let cmd = json!({"c": "delete", "chat": m.chat, "id": m.id});
                wa::send(self.cmd_of(self.service), cmd);
            }
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle | Service::GroupWise => {}
        }
    }

    pub(super) fn open_selected_url(&mut self) {
        if self.service == Service::Calendar {
            if let Some(link) = self.selected_event().map(|e| e.link.clone()) {
                if let Err(e) = open_url(&link) {
                    self.status = format!("could not open browser: {e}");
                }
            }
            return;
        }
        if self.service.is_wa_like() {
            let Some(m) = self.msg_sel.and_then(|i| self.wa_messages().get(i)) else {
                self.status = "select a message first (k)".into();
                return;
            };
            if m.media && !m.deleted {
                let cmd = json!({"c": "download", "chat": m.chat, "id": m.id});
                wa::send(self.cmd_of(self.service), cmd);
            } else if let Some(url) = find_url(&m.text).map(String::from) {
                if let Err(e) = open_url(&url) {
                    self.status = format!("could not open browser: {e}");
                }
            } else {
                self.status = "no link or media in this message".into();
            }
            return;
        }
        let Some(m) = self.selected_message() else {
            self.status = "select a message first (k)".into();
            return;
        };
        // A PDF opens in the viewer.
        if let Some(a) = m.attachments.iter().find(|a| crate::pdf::is_pdf(&a.filename)).filter(|_| crate::pdf::BUILT) {
            let (url, name) = (a.url.clone(), a.filename.clone());
            self.open_pdf_link(url, name);
            return;
        }
        // A picture opens in full quality; then files, then links.
        let url = crate::discord::state::full_picture(m)
            .or_else(|| m.attachments.first().map(|a| a.url.clone()))
            .or_else(|| find_url(&m.content).map(String::from))
            .or_else(|| m.embeds.iter().find_map(|e| e.url.clone()));
        match url {
            Some(u) => {
                if let Err(e) = open_url(&u) {
                    self.status = format!("could not open browser: {e}");
                }
            }
            None => self.status = "no link in this message".into(),
        }
    }
}
