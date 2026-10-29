//! Client-side cache of guilds, channels, read states and loaded messages.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Local, TimeZone, Utc};

use crate::model::*;
use crate::text::{sanitize, truncate};
use crate::pics::{discord_url, PicRef, PicSrc, HD_COLS, MAX_COLS};
use crate::view::{Reply, ViewMsg};

/// Messages kept per channel; older ones are dropped (and refetched on demand).
pub const MAX_MESSAGES: usize = 1000;

pub struct GuildInfo {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub roles: HashMap<String, Role>,
    /// None when we don't know our roles; then every channel is shown.
    pub my_roles: Option<Vec<String>>,
    pub channels: Vec<String>,
}

#[derive(Default)]
pub struct State {
    pub me: User,
    pub guilds: HashMap<String, GuildInfo>,
    pub guild_order: Vec<String>,
    pub channels: HashMap<String, Channel>,
    pub dms: Vec<String>,
    /// Last message id read locally, per channel.
    pub read: HashMap<String, u64>,
    /// Last message id acknowledged to the server, per channel.
    pub acked: HashMap<String, u64>,
    pub mentions: HashMap<String, u32>,
    /// Muted servers, categories, channels and DMs by id, with when the
    /// mute ends (none: until unmuted).
    pub muted: HashMap<String, Option<DateTime<Utc>>>,
    /// The ids each server's settings muted (key "" for the DMs), so an update replaces them.
    mute_scope: HashMap<String, Vec<String>>,
    pub messages: HashMap<String, Vec<Message>>,
    /// Channels whose full history is loaded.
    pub complete: HashSet<String>,
    /// Bumped whenever loaded messages change; keys the UI's line cache.
    pub version: u64,
}

impl State {
    pub fn load_ready(&mut self, r: Ready) {
        *self = State { version: self.version + 1, ..State::default() };
        self.me = r.user;
        let users: HashMap<String, User> = r.users.into_iter().map(|u| (u.id.clone(), u)).collect();

        for (i, g) in r.guilds.into_iter().enumerate() {
            let members = r.merged_members.get(i).cloned().unwrap_or_default();
            self.add_guild(g, members);
        }

        if let Some(settings) = r.user_settings {
            let mut order: Vec<String> = Vec::new();
            for id in settings.guild_folders.iter().flat_map(|f| f.guild_ids.iter()) {
                let id = match id {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if self.guilds.contains_key(&id) && !order.contains(&id) {
                    order.push(id);
                }
            }
            for id in &self.guild_order {
                if !order.contains(id) {
                    order.push(id.clone());
                }
            }
            self.guild_order = order;
        }

        for mut c in r.private_channels {
            if c.recipients.is_empty() {
                c.recipients = c.recipient_ids.iter().filter_map(|id| users.get(id).cloned()).collect();
            }
            self.dms.push(c.id.clone());
            self.channels.insert(c.id.clone(), c);
        }
        self.sort_dms();

        let settings = match r.user_guild_settings {
            Some(GuildSettingsList::List(v)) => v,
            Some(GuildSettingsList::Versioned { entries }) => entries,
            None => Vec::new(),
        };
        for s in settings {
            self.apply_guild_settings(s);
        }

        let entries = match r.read_state {
            Some(ReadStates::List(v)) => v,
            Some(ReadStates::Versioned { entries }) => entries,
            None => Vec::new(),
        };
        for e in entries {
            let id = value_sf(&e.last_message_id);
            self.read.insert(e.id.clone(), id);
            self.acked.insert(e.id.clone(), id);
            if e.mention_count > 0 {
                self.mentions.insert(e.id, e.mention_count);
            }
        }
    }

    pub fn add_guild(&mut self, g: Guild, mut members: Vec<Member>) {
        if g.unavailable && g.channels.is_empty() {
            return;
        }
        let props = g.properties.unwrap_or_default();
        let name = g.name.or(props.name).unwrap_or_else(|| "unknown server".into());
        let owner_id = g.owner_id.or(props.owner_id).unwrap_or_default();
        members.extend(g.members);
        let my_roles = members.into_iter().find(|m| m.id() == self.me.id).map(|m| m.roles);
        let mut info = GuildInfo {
            id: g.id.clone(),
            name,
            owner_id,
            roles: g.roles.into_iter().map(|r| (r.id.clone(), r)).collect(),
            my_roles,
            channels: Vec::new(),
        };
        if let Some(old) = self.guilds.remove(&g.id) {
            for c in &old.channels {
                self.channels.remove(c);
            }
            if info.my_roles.is_none() {
                info.my_roles = old.my_roles;
            }
        } else {
            self.guild_order.push(g.id.clone());
        }
        for mut c in g.channels {
            c.guild_id = Some(g.id.clone());
            info.channels.push(c.id.clone());
            self.channels.insert(c.id.clone(), c);
        }
        self.guilds.insert(g.id, info);
    }

    pub fn update_guild(&mut self, g: Guild) {
        if let Some(info) = self.guilds.get_mut(&g.id) {
            if let Some(name) = g.name {
                info.name = name;
            }
            if let Some(owner) = g.owner_id {
                info.owner_id = owner;
            }
            if !g.roles.is_empty() {
                info.roles = g.roles.into_iter().map(|r| (r.id.clone(), r)).collect();
            }
        }
    }

    pub fn remove_guild(&mut self, id: &str) {
        if let Some(g) = self.guilds.remove(id) {
            for c in &g.channels {
                self.channels.remove(c);
            }
        }
        self.guild_order.retain(|g| g != id);
    }

    pub fn upsert_channel(&mut self, mut c: Channel) {
        if let Some(old) = self.channels.get(&c.id) {
            if c.last_message_id.is_none() {
                c.last_message_id = old.last_message_id.clone();
            }
            if c.recipients.is_empty() {
                c.recipients = old.recipients.clone();
            }
        }
        if c.is_dm() {
            if !self.dms.contains(&c.id) {
                self.dms.push(c.id.clone());
            }
        } else if let Some(g) = c.guild_id.as_ref().and_then(|g| self.guilds.get_mut(g)) {
            if !g.channels.contains(&c.id) {
                g.channels.push(c.id.clone());
            }
        }
        let dm = c.is_dm();
        self.channels.insert(c.id.clone(), c);
        if dm {
            self.sort_dms();
        }
    }

    pub fn remove_channel(&mut self, id: &str) {
        if let Some(c) = self.channels.remove(id) {
            if let Some(g) = c.guild_id.as_ref().and_then(|g| self.guilds.get_mut(g)) {
                g.channels.retain(|x| x != id);
            }
        }
        self.dms.retain(|x| x != id);
        self.messages.remove(id);
    }

    pub fn sort_dms(&mut self) {
        let channels = &self.channels;
        let key = |id: &String| channels.get(id).map(|c| c.last_id().max(sf(&c.id))).unwrap_or(0);
        self.dms.sort_by_key(|id| std::cmp::Reverse(key(id)));
    }

    pub fn can_view(&self, g: &GuildInfo, c: &Channel) -> bool {
        let Some(my_roles) = &g.my_roles else { return true };
        if g.owner_id == self.me.id {
            return true;
        }
        let role_perms = |id: &str| g.roles.get(id).map(|r| r.permissions.parse::<u64>().unwrap_or(0)).unwrap_or(0);
        let mut p = role_perms(&g.id);
        for r in my_roles {
            p |= role_perms(r);
        }
        if p & PERM_ADMIN != 0 {
            return true;
        }
        let ows = &c.permission_overwrites;
        if let Some(o) = ows.iter().find(|o| o.id == g.id) {
            p = (p & !o.deny_bits()) | o.allow_bits();
        }
        let (mut allow, mut deny) = (0, 0);
        for o in ows.iter().filter(|o| !o.is_member() && my_roles.contains(&o.id)) {
            allow |= o.allow_bits();
            deny |= o.deny_bits();
        }
        p = (p & !deny) | allow;
        if let Some(o) = ows.iter().find(|o| o.is_member() && o.id == self.me.id) {
            p = (p & !o.deny_bits()) | o.allow_bits();
        }
        p & PERM_VIEW != 0
    }

    /// Visible text channels of a guild grouped by category, in display order.
    pub fn guild_layout<'a>(&'a self, g: &'a GuildInfo) -> Vec<(Option<&'a Channel>, Vec<&'a Channel>)> {
        let chans: Vec<&Channel> = g.channels.iter().filter_map(|id| self.channels.get(id)).collect();
        let mut cats: Vec<&Channel> = chans.iter().copied().filter(|c| c.kind == kind::CATEGORY).collect();
        cats.sort_by_key(|c| (c.position, sf(&c.id)));
        let cat_ids: HashSet<&str> = cats.iter().map(|c| c.id.as_str()).collect();
        let visible = |c: &&Channel| c.is_listed() && self.can_view(g, c);
        let sorted = |mut v: Vec<&'a Channel>| {
            v.sort_by_key(|c| (c.position, sf(&c.id)));
            v
        };

        let mut out = Vec::new();
        let top: Vec<&Channel> = chans
            .iter()
            .copied()
            .filter(visible)
            .filter(|c| c.parent_id.as_deref().map_or(true, |p| !cat_ids.contains(p)))
            .collect();
        if !top.is_empty() {
            out.push((None, sorted(top)));
        }
        for cat in cats {
            let children: Vec<&Channel> = chans
                .iter()
                .copied()
                .filter(visible)
                .filter(|c| c.parent_id.as_deref() == Some(cat.id.as_str()))
                .collect();
            if !children.is_empty() {
                out.push((Some(cat), sorted(children)));
            }
        }
        out
    }

    pub fn is_unread(&self, c: &Channel) -> bool {
        // Channels without a read state were never opened; treat them as read.
        self.read.get(&c.id).is_some_and(|&r| c.last_id() > r)
    }

    /// Unread and worth a dot: not muted (mentions show anyway, as in Discord).
    pub fn shows_unread(&self, c: &Channel) -> bool {
        self.is_unread(c) && !self.channel_muted(c)
    }

    /// Replaces the mutes of one server (or of the DMs) with these settings.
    pub fn apply_guild_settings(&mut self, s: GuildSettings) {
        let scope = if s.guild_id == "@me" { String::new() } else { s.guild_id.clone() };
        for id in self.mute_scope.remove(&scope).unwrap_or_default() {
            self.muted.remove(&id);
        }
        let until = |cfg: &Option<MuteConfig>| {
            cfg.as_ref().and_then(|c| c.end_time.as_deref()).and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc))
        };
        let mut ids = Vec::new();
        if s.muted && !scope.is_empty() {
            self.muted.insert(scope.clone(), until(&s.mute_config));
            ids.push(scope.clone());
        }
        for o in s.overrides().into_iter().filter(|o| o.muted && !o.channel_id.is_empty()) {
            self.muted.insert(o.channel_id.clone(), until(&o.mute_config));
            ids.push(o.channel_id);
        }
        self.mute_scope.insert(scope, ids);
    }

    /// A server, category, channel or DM muted by its own setting (and not expired).
    pub fn is_muted(&self, id: &str) -> bool {
        match self.muted.get(id) {
            Some(Some(until)) => Utc::now() < *until,
            Some(None) => true,
            None => false,
        }
    }

    /// Muted itself, or by its category or server.
    pub fn channel_muted(&self, c: &Channel) -> bool {
        self.is_muted(&c.id)
            || c.parent_id.as_deref().is_some_and(|p| self.is_muted(p))
            || c.guild_id.as_deref().is_some_and(|g| self.is_muted(g))
    }

    pub fn mention_count(&self, id: &str) -> u32 {
        self.mentions.get(id).copied().unwrap_or(0)
    }

    pub fn guild_unread(&self, g: &GuildInfo) -> (bool, u32) {
        let mut unread = false;
        let mut mentions = 0;
        for c in g.channels.iter().filter_map(|id| self.channels.get(id)) {
            if !c.is_listed() {
                continue;
            }
            mentions += self.mention_count(&c.id);
            if !unread && self.shows_unread(c) && self.can_view(g, c) {
                unread = true;
            }
        }
        (unread, mentions)
    }

    pub fn dm_label(&self, c: &Channel) -> String {
        if let Some(n) = c.name.as_deref().filter(|n| !n.is_empty()) {
            return n.to_string();
        }
        if c.recipients.is_empty() {
            return "unknown".into();
        }
        c.recipients.iter().map(|u| u.name()).collect::<Vec<_>>().join(", ")
    }

    pub fn channel_title(&self, id: &str) -> String {
        let Some(c) = self.channels.get(id) else { return String::new() };
        if c.is_dm() {
            return format!("@{}", self.dm_label(c));
        }
        let guild = c.guild_id.as_ref().and_then(|g| self.guilds.get(g)).map(|g| g.name.as_str()).unwrap_or("");
        format!("#{} · {}", c.name.as_deref().unwrap_or("?"), guild)
    }

    /// Inserts or replaces a message in a loaded channel, keeping id order.
    pub fn push_message(&mut self, m: Message) {
        let id = sf(&m.id);
        if let Some(c) = self.channels.get_mut(&m.channel_id) {
            if id > c.last_id() {
                c.last_message_id = Some(m.id.clone());
            }
        }
        let ch = m.channel_id.clone();
        if let Some(list) = self.messages.get_mut(&ch) {
            // Only loaded channels invalidate the render cache.
            self.version += 1;
            if let Some(existing) = list.iter_mut().rev().find(|x| x.id == m.id) {
                *existing = m;
                return;
            }
            let pos = list.iter().rposition(|x| sf(&x.id) < id).map_or(0, |i| i + 1);
            list.insert(pos, m);
            if list.len() > MAX_MESSAGES {
                let excess = list.len() - MAX_MESSAGES;
                list.drain(..excess);
                self.complete.remove(&ch);
            }
        }
    }

    pub fn update_message(&mut self, m: Message) {
        self.version += 1;
        let Some(list) = self.messages.get_mut(&m.channel_id) else { return };
        let Some(existing) = list.iter_mut().find(|x| x.id == m.id) else { return };
        if m.author.id.is_empty() {
            // Partial update (e.g. link preview arrived).
            if !m.embeds.is_empty() {
                existing.embeds = m.embeds;
            }
            if m.edited_timestamp.is_some() {
                existing.content = m.content;
                existing.edited_timestamp = m.edited_timestamp;
            }
        } else {
            // Updates don't always carry these; keep what we know.
            let reply = existing.referenced_message.take();
            let reactions = std::mem::take(&mut existing.reactions);
            *existing = m;
            if existing.referenced_message.is_none() {
                existing.referenced_message = reply;
            }
            if existing.reactions.is_empty() {
                existing.reactions = reactions;
            }
        }
    }

    /// Applies one reaction added or removed. Our own reactions are applied
    /// right away and again when the gateway echoes them, so they are
    /// idempotent through the `me` flag.
    pub fn reaction_changed(&mut self, channel: &str, id: &str, emoji: &ReactionEmoji, mine: bool, add: bool) {
        let Some(m) = self.messages.get_mut(channel).and_then(|l| l.iter_mut().find(|m| m.id == id)) else { return };
        let pos = m.reactions.iter().position(|r| r.emoji.same(emoji));
        match (pos, add) {
            (Some(i), true) => {
                let r = &mut m.reactions[i];
                if mine && r.me {
                    return;
                }
                r.count += 1;
                r.me |= mine;
            }
            (None, true) => m.reactions.push(Reaction { count: 1, me: mine, emoji: emoji.clone() }),
            (Some(i), false) => {
                let r = &mut m.reactions[i];
                if mine && !r.me {
                    return;
                }
                r.count = r.count.saturating_sub(1);
                if mine {
                    r.me = false;
                }
                if r.count == 0 {
                    m.reactions.remove(i);
                }
            }
            (None, false) => return,
        }
        self.version += 1;
    }

    /// All reactions removed (by a moderator), or all of one emoji.
    pub fn reactions_cleared(&mut self, channel: &str, id: &str, emoji: Option<&ReactionEmoji>) {
        let Some(m) = self.messages.get_mut(channel).and_then(|l| l.iter_mut().find(|m| m.id == id)) else { return };
        match emoji {
            Some(e) => m.reactions.retain(|r| !r.emoji.same(e)),
            None => m.reactions.clear(),
        }
        self.version += 1;
    }

    pub fn delete_message(&mut self, channel: &str, id: &str) -> Option<usize> {
        self.version += 1;
        let list = self.messages.get_mut(channel)?;
        let idx = list.iter().position(|m| m.id == id)?;
        list.remove(idx);
        Some(idx)
    }

    /// Whether a message pings us (direct mention, @everyone, our role, or a DM).
    pub fn mentions_me(&self, m: &Message) -> bool {
        if m.author.id == self.me.id {
            return false;
        }
        if m.mention_everyone || m.mentions.iter().any(|u| u.id == self.me.id) {
            return true;
        }
        if let Some(c) = self.channels.get(&m.channel_id) {
            if c.is_dm() {
                return true;
            }
            if let Some(roles) = c.guild_id.as_ref().and_then(|g| self.guilds.get(g)).and_then(|g| g.my_roles.as_ref()) {
                return m.mention_roles.iter().any(|r| roles.contains(r));
            }
        }
        false
    }

    /// Message content with mentions, channels, emoji and timestamps resolved.
    pub fn render_content(&self, m: &Message) -> String {
        let s = &m.content;
        let mut out = String::with_capacity(s.len());
        let mut rest = s.as_str();
        while let Some(i) = rest.find('<') {
            out.push_str(&rest[..i]);
            let after = &rest[i..];
            if let Some(j) = after.find('>') {
                if let Some(rep) = self.resolve_tag(&after[1..j], m) {
                    out.push_str(&rep);
                    rest = &after[j + 1..];
                    continue;
                }
            }
            out.push('<');
            rest = &after[1..];
        }
        out.push_str(rest);
        out
    }

    fn resolve_tag(&self, inner: &str, m: &Message) -> Option<String> {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if let Some(id) = inner.strip_prefix("@&") {
            if !digits(id) {
                return None;
            }
            let guild = m.guild_id.clone().or_else(|| self.channels.get(&m.channel_id)?.guild_id.clone());
            let name = guild.and_then(|g| self.guilds.get(&g)).and_then(|g| g.roles.get(id)).map(|r| r.name.clone());
            return Some(format!("@{}", name.unwrap_or_else(|| "role".into())));
        }
        if let Some(id) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')) {
            if !digits(id) {
                return None;
            }
            let name = m
                .mentions
                .iter()
                .find(|u| u.id == id)
                .map(|u| u.name().to_string())
                .or_else(|| (self.me.id == id).then(|| self.me.name().to_string()));
            return Some(format!("@{}", name.unwrap_or_else(|| "user".into())));
        }
        if let Some(id) = inner.strip_prefix('#') {
            if !digits(id) {
                return None;
            }
            let name = self.channels.get(id).and_then(|c| c.name.clone());
            return Some(format!("#{}", name.unwrap_or_else(|| "channel".into())));
        }
        if let Some(rest) = inner.strip_prefix("t:") {
            let ts: i64 = rest.split(':').next()?.parse().ok()?;
            let t = Local.timestamp_opt(ts, 0).single()?;
            return Some(t.format("%Y-%m-%d %H:%M").to_string());
        }
        if inner.starts_with("http://") || inner.starts_with("https://") {
            return Some(inner.to_string());
        }
        if let Some(cmd) = inner.strip_prefix('/') {
            let (name, id) = cmd.rsplit_once(':')?;
            return digits(id).then(|| format!("/{name}"));
        }
        let emoji = inner.strip_prefix("a:").or_else(|| inner.strip_prefix(':'))?;
        let (name, id) = emoji.rsplit_once(':')?;
        digits(id).then(|| format!(":{name}:"))
    }

    /// Full message body: content plus attachments, stickers and embeds.
    pub fn render_body(&self, m: &Message) -> String {
        let mut s = sanitize(&self.render_content(m));
        let mut push = |line: String| {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&line);
        };
        for a in &m.attachments {
            if a.is_image() {
                push(format!("[image] {}", a.filename));
            } else {
                push(format!("[file] {} {}", a.filename, a.url));
            }
        }
        for st in &m.sticker_items {
            push(format!("[sticker] {}", st.name));
        }
        for e in &m.embeds {
            let mut parts = Vec::new();
            if let Some(t) = e.title.as_deref().filter(|t| !t.is_empty()) {
                parts.push(sanitize(t));
            }
            if let Some(d) = e.description.as_deref().filter(|d| !d.is_empty()) {
                parts.push(truncate(&sanitize(d).replace('\n', " "), 200));
            }
            if parts.is_empty() {
                continue;
            }
            push(format!("[embed] {}", parts.join(" — ")));
        }
        s
    }
}

/// The first picture of a message in full quality: an image attachment's
/// original, or an image/GIF link's full-size copy from Discord's proxy
/// (without the size parameters used for the inline preview).
pub fn full_picture(m: &Message) -> Option<String> {
    if let Some(a) = m.attachments.iter().find(|a| a.is_image()) {
        return Some(a.url.clone());
    }
    m.embeds
        .iter()
        .filter(|e| matches!(e.kind.as_deref(), Some("image" | "gifv")))
        .find_map(|e| e.image.as_ref().or(e.thumbnail.as_ref()))
        .map(|t| t.proxy_url.clone())
        .filter(|u| !u.is_empty())
}

/// Image attachments plus previews of image/GIF links.
fn message_pics(m: &Message) -> Vec<PicRef> {
    let mut out = Vec::new();
    let mut push = |proxy: &str, direct: &str, w: Option<u32>, h: Option<u32>| {
        let src = if proxy.is_empty() { direct } else { proxy };
        if src.is_empty() {
            return;
        }
        let (w, h) = (w.unwrap_or(0), h.unwrap_or(0));
        // 2 px per cell column keeps downscaling crisp.
        let url = discord_url(src, w, h, MAX_COLS * 2);
        let hd = discord_url(src, w, h, HD_COLS * 2);
        out.push(PicRef { key: url.clone(), src: PicSrc::Url(url), w, h, hd_src: PicSrc::Url(hd) });
    };
    for a in m.attachments.iter().filter(|a| a.is_image()) {
        push(&a.proxy_url, &a.url, a.width, a.height);
    }
    for e in &m.embeds {
        if matches!(e.kind.as_deref(), Some("image" | "gifv")) {
            if let Some(t) = e.thumbnail.as_ref().or(e.image.as_ref()) {
                // Never fetch third-party URLs directly: that would reveal your IP.
                push(&t.proxy_url, "", t.width, t.height);
            }
        }
    }
    out
}

impl State {
    pub fn view_messages(&self, ch: &str) -> Option<Vec<ViewMsg>> {
        let msgs = self.messages.get(ch)?;
        let dm = self.channels.get(ch).is_some_and(|c| c.is_dm());
        Some(
            msgs.iter()
                .map(|m| {
                    let base = ViewMsg {
                        author: m.author.name().to_string(),
                        author_key: m.author.id.clone(),
                        time: parse_time(&m.timestamp),
                        ..ViewMsg::default()
                    };
                    if m.is_system() {
                        return ViewMsg { body: system_text(m), system: true, ..base };
                    }
                    let reply = match m.referenced_message.as_deref() {
                        Some(r) => Reply::To {
                            name: r.author.name().to_string(),
                            text: self.render_content(r).replace('\n', " "),
                        },
                        None if m.kind == 19 => Reply::Missing,
                        None => Reply::None,
                    };
                    ViewMsg {
                        body: self.render_body(m),
                        reply,
                        edited: m.edited_timestamp.is_some(),
                        ping: !dm && self.mentions_me(m),
                        pics: message_pics(m),
                        reactions: m
                            .reactions
                            .iter()
                            .map(|r| if r.count > 1 { format!("{} {}", r.emoji.label(), r.count) } else { r.emoji.label() })
                            .collect::<Vec<_>>()
                            .join("  "),
                        ..base
                    }
                })
                .collect(),
        )
    }
}

pub fn system_text(m: &Message) -> String {
    let base = match m.kind {
        1 => "added someone to the group",
        2 => "removed someone from the group",
        3 => "started a call",
        4 => "changed the channel name",
        5 => "changed the channel icon",
        6 => "pinned a message",
        7 => "joined the server",
        8 => "boosted the server",
        9..=11 => "boosted the server to a new level",
        12 => "followed a channel",
        18 => "started a thread",
        21 => "started a thread here",
        24 => "triggered automod",
        _ => "sent a system message",
    };
    if m.content.is_empty() || m.kind == 7 {
        base.to_string()
    } else {
        format!("{base}: {}", sanitize(&m.content).replace('\n', " "))
    }
}

pub fn parse_time(ts: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(ts).ok().map(|d| d.with_timezone(&Local))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutes_hide_unread_dots() {
        let mut s = State::default();
        let settings = |v: serde_json::Value| serde_json::from_value::<GuildSettings>(v).unwrap();
        // READY's list form, with a category mute and an expired channel mute.
        s.apply_guild_settings(settings(serde_json::json!({
            "guild_id": "g", "muted": false, "mute_config": null,
            "channel_overrides": [
                { "channel_id": "cat", "muted": true, "mute_config": null },
                { "channel_id": "old", "muted": true, "mute_config": { "end_time": "2020-01-01T00:00:00.000Z" } },
                { "channel_id": "loud", "muted": false }
            ]
        })));
        let chan = |id: &str, parent: Option<&str>| Channel {
            id: id.into(),
            guild_id: Some("g".into()),
            parent_id: parent.map(String::from),
            last_message_id: Some("9".into()),
            ..Default::default()
        };
        s.read.insert("a".into(), 1);
        s.read.insert("old".into(), 1);
        assert!(!s.shows_unread(&chan("a", Some("cat"))), "muted by its category");
        assert!(s.is_unread(&chan("a", Some("cat"))));
        assert!(s.shows_unread(&chan("old", None)), "the mute has ended");
        assert!(!s.is_muted("loud"));

        // An update (the map form from PATCH) replaces the server's mutes.
        s.apply_guild_settings(settings(serde_json::json!({ "guild_id": "g", "muted": true, "channel_overrides": {} })));
        assert!(!s.is_muted("cat") && s.is_muted("g"));
        assert!(!s.shows_unread(&chan("old", None)), "muted by its server");
        // DMs' settings come with no guild id.
        s.apply_guild_settings(settings(serde_json::json!({ "guild_id": null, "channel_overrides": { "dm": { "muted": true } } })));
        assert!(s.is_muted("dm") && s.is_muted("g"));
    }

    #[test]
    fn own_reactions_count_once() {
        let mut s = State::default();
        let m = Message { id: "m".into(), channel_id: "c".into(), ..Default::default() };
        s.messages.insert("c".into(), vec![m]);
        let heart = ReactionEmoji::unicode("❤️");
        let count = |s: &State| s.messages["c"][0].reactions.first().map(|r| (r.count, r.me));

        s.reaction_changed("c", "m", &heart, true, true); // optimistic
        s.reaction_changed("c", "m", &heart, true, true); // gateway echo
        assert_eq!(count(&s), Some((1, true)));
        s.reaction_changed("c", "m", &heart, false, true); // someone else
        assert_eq!(count(&s), Some((2, true)));
        s.reaction_changed("c", "m", &heart, true, false);
        s.reaction_changed("c", "m", &heart, true, false); // echo of the removal
        assert_eq!(count(&s), Some((1, false)));
        s.reactions_cleared("c", "m", None);
        assert_eq!(count(&s), None);
    }
}
