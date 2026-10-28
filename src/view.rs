//! Service-neutral message model: Discord and WhatsApp messages are
//! converted to this before rendering.

use chrono::{DateTime, Local};

use crate::pics::PicRef;

#[derive(Default)]
pub enum Reply {
    #[default]
    None,
    To { name: String, text: String },
    /// The message replied to no longer exists.
    Missing,
}

#[derive(Default)]
pub struct ViewMsg {
    pub author: String,
    /// Stable author id: groups consecutive messages and picks the name color.
    pub author_key: String,
    pub time: Option<DateTime<Local>>,
    pub body: String,
    pub reply: Reply,
    pub edited: bool,
    /// System notices render as one dim line: "→ author body".
    pub system: bool,
    /// Mentions the user; rendered highlighted.
    pub ping: bool,
    /// Deleted messages render dimmed.
    pub dim: bool,
    pub reactions: String,
    /// Pictures drawn under the body.
    pub pics: Vec<PicRef>,
}
