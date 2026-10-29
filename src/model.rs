//! Discord API objects. Every field is defaulted so partial or unexpected
//! payloads never fail to parse.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// Treats an explicit `null` the same as a missing field.
pub fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Snowflake -> u64 (0 when invalid). Snowflakes sort chronologically.
pub fn sf(id: &str) -> u64 {
    id.parse().unwrap_or(0)
}

pub fn value_sf(v: &Value) -> u64 {
    match v {
        Value::String(s) => sf(s),
        Value::Number(n) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

pub const PERM_ADMIN: u64 = 1 << 3;
pub const PERM_VIEW: u64 = 1 << 10;

pub mod kind {
    pub const TEXT: u8 = 0;
    pub const DM: u8 = 1;
    pub const GROUP_DM: u8 = 3;
    pub const CATEGORY: u8 = 4;
    pub const NEWS: u8 = 5;
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct User {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
}

impl User {
    pub fn name(&self) -> &str {
        match self.global_name.as_deref() {
            Some(n) if !n.is_empty() => n,
            _ => &self.username,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Overwrite {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: Value,
    #[serde(default, deserialize_with = "nullable")]
    pub allow: String,
    #[serde(default, deserialize_with = "nullable")]
    pub deny: String,
}

impl Overwrite {
    pub fn is_member(&self) -> bool {
        self.kind.as_u64() == Some(1) || self.kind.as_str() == Some("member")
    }
    pub fn allow_bits(&self) -> u64 {
        self.allow.parse().unwrap_or(0)
    }
    pub fn deny_bits(&self) -> u64 {
        self.deny.parse().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Channel {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(rename = "type", default, deserialize_with = "nullable")]
    pub kind: u8,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub position: i64,
    #[serde(default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub last_message_id: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub recipients: Vec<User>,
    #[serde(default, deserialize_with = "nullable")]
    pub recipient_ids: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub permission_overwrites: Vec<Overwrite>,
}

impl Channel {
    pub fn is_dm(&self) -> bool {
        matches!(self.kind, kind::DM | kind::GROUP_DM)
    }
    /// Guild text channels we show (voice/forum/stage are skipped).
    pub fn is_listed(&self) -> bool {
        matches!(self.kind, kind::TEXT | kind::NEWS)
    }
    pub fn last_id(&self) -> u64 {
        self.last_message_id.as_deref().map(sf).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Role {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
    #[serde(default, deserialize_with = "nullable")]
    pub permissions: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Member {
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub roles: Vec<String>,
}

impl Member {
    pub fn id(&self) -> &str {
        self.user_id
            .as_deref()
            .or(self.user.as_ref().map(|u| u.id.as_str()))
            .unwrap_or("")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GuildProps {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Guild {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub properties: Option<GuildProps>,
    #[serde(default, deserialize_with = "nullable")]
    pub channels: Vec<Channel>,
    #[serde(default, deserialize_with = "nullable")]
    pub roles: Vec<Role>,
    #[serde(default, deserialize_with = "nullable")]
    pub members: Vec<Member>,
    #[serde(default, deserialize_with = "nullable")]
    pub unavailable: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Attachment {
    #[serde(default, deserialize_with = "nullable")]
    pub filename: String,
    #[serde(default, deserialize_with = "nullable")]
    pub url: String,
    #[serde(default, deserialize_with = "nullable")]
    pub proxy_url: String,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        match self.content_type.as_deref() {
            Some(t) => t.starts_with("image/"),
            None => self.width.is_some(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EmbedMedia {
    #[serde(default, deserialize_with = "nullable")]
    pub proxy_url: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Embed {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub thumbnail: Option<EmbedMedia>,
    #[serde(default)]
    pub image: Option<EmbedMedia>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ReactionEmoji {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

impl ReactionEmoji {
    pub fn unicode(s: &str) -> Self {
        Self { id: None, name: Some(s.to_string()) }
    }

    pub fn same(&self, other: &Self) -> bool {
        match (&self.id, &other.id) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.name == other.name,
            _ => false,
        }
    }

    /// Unicode emoji as is; custom server emoji as :name:.
    pub fn label(&self) -> String {
        let name = self.name.clone().unwrap_or_default();
        if self.id.is_some() {
            format!(":{name}:")
        } else {
            name
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Reaction {
    #[serde(default, deserialize_with = "nullable")]
    pub count: u32,
    /// Whether we reacted with it.
    #[serde(default, deserialize_with = "nullable")]
    pub me: bool,
    #[serde(default, deserialize_with = "nullable")]
    pub emoji: ReactionEmoji,
}

/// MESSAGE_REACTION_ADD / _REMOVE / _REMOVE_ALL / _REMOVE_EMOJI.
#[derive(Debug, Default, Deserialize)]
pub struct ReactionEvent {
    #[serde(default, deserialize_with = "nullable")]
    pub user_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub channel_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub message_id: String,
    #[serde(default)]
    pub emoji: Option<ReactionEmoji>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Sticker {
    #[serde(default, deserialize_with = "nullable")]
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Message {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub channel_id: String,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub author: User,
    #[serde(default, deserialize_with = "nullable")]
    pub content: String,
    #[serde(default, deserialize_with = "nullable")]
    pub timestamp: String,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub attachments: Vec<Attachment>,
    #[serde(default, deserialize_with = "nullable")]
    pub embeds: Vec<Embed>,
    #[serde(default, deserialize_with = "nullable")]
    pub sticker_items: Vec<Sticker>,
    #[serde(default, deserialize_with = "nullable")]
    pub mentions: Vec<User>,
    #[serde(default, deserialize_with = "nullable")]
    pub mention_roles: Vec<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub mention_everyone: bool,
    #[serde(default)]
    pub referenced_message: Option<Box<Message>>,
    #[serde(default, deserialize_with = "nullable")]
    pub reactions: Vec<Reaction>,
    #[serde(rename = "type", default, deserialize_with = "nullable")]
    pub kind: u8,
}

impl Message {
    pub fn is_system(&self) -> bool {
        !matches!(self.kind, 0 | 19 | 20 | 23)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct MessageDelete {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub channel_id: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct MessageAck {
    #[serde(default, deserialize_with = "nullable")]
    pub channel_id: String,
    #[serde(default)]
    pub message_id: Value,
}

#[derive(Debug, Default, Deserialize)]
pub struct GuildDelete {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub unavailable: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct ReadState {
    #[serde(default, deserialize_with = "nullable")]
    pub id: String,
    #[serde(default)]
    pub last_message_id: Value,
    #[serde(default, deserialize_with = "nullable")]
    pub mention_count: u32,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum ReadStates {
    List(Vec<ReadState>),
    Versioned { entries: Vec<ReadState> },
}

#[derive(Debug, Default, Deserialize)]
pub struct MuteConfig {
    /// Until when (ISO 8601); none: until unmuted.
    #[serde(default, deserialize_with = "nullable")]
    pub end_time: Option<String>,
}

/// One server's notification settings (`guild_id` empty: the DMs').
#[derive(Debug, Default, Deserialize)]
pub struct GuildSettings {
    #[serde(default, deserialize_with = "nullable")]
    pub guild_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub muted: bool,
    #[serde(default)]
    pub mute_config: Option<MuteConfig>,
    /// A list of per-channel settings, or a map by channel id.
    #[serde(default)]
    pub channel_overrides: Value,
}

#[derive(Debug, Default, Deserialize)]
pub struct ChannelOverride {
    #[serde(default, deserialize_with = "nullable")]
    pub channel_id: String,
    #[serde(default, deserialize_with = "nullable")]
    pub muted: bool,
    #[serde(default)]
    pub mute_config: Option<MuteConfig>,
}

impl GuildSettings {
    pub fn overrides(&self) -> Vec<ChannelOverride> {
        match &self.channel_overrides {
            Value::Array(v) => v.iter().filter_map(|o| serde_json::from_value(o.clone()).ok()).collect(),
            Value::Object(m) => m
                .iter()
                .filter_map(|(id, o)| {
                    let mut o: ChannelOverride = serde_json::from_value(o.clone()).ok()?;
                    if o.channel_id.is_empty() {
                        o.channel_id = id.clone();
                    }
                    Some(o)
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum GuildSettingsList {
    List(Vec<GuildSettings>),
    Versioned { entries: Vec<GuildSettings> },
}

#[derive(Debug, Default, Deserialize)]
pub struct Folder {
    #[serde(default, deserialize_with = "nullable")]
    pub guild_ids: Vec<Value>,
}

#[derive(Debug, Default, Deserialize)]
pub struct UserSettings {
    #[serde(default, deserialize_with = "nullable")]
    pub guild_folders: Vec<Folder>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Ready {
    #[serde(default, deserialize_with = "nullable")]
    pub user: User,
    #[serde(default, deserialize_with = "nullable")]
    pub guilds: Vec<Guild>,
    #[serde(default, deserialize_with = "nullable")]
    pub private_channels: Vec<Channel>,
    #[serde(default)]
    pub read_state: Option<ReadStates>,
    #[serde(default)]
    pub user_settings: Option<UserSettings>,
    #[serde(default)]
    pub user_guild_settings: Option<GuildSettingsList>,
    #[serde(default, deserialize_with = "nullable")]
    pub merged_members: Vec<Vec<Member>>,
    #[serde(default, deserialize_with = "nullable")]
    pub users: Vec<User>,
}
