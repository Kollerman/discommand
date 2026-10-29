//! Application state and vim-style key handling.

use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;

use base64::Engine;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use ratatui::widgets::ListState;
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Semaphore;

use crate::api::Api;
use crate::cal::{self, CalState, Creds, Google, Synced};
use crate::todo::{self, Backend, Task, TaskList, Todo, TodoState};
use crate::twos::{self, Twos};
use crate::gateway::GwEvent;
use crate::model::*;
use crate::pics::{self, Pic, PicRef, PicSrc, Pics};
use crate::state::State;
use crate::text::{find_url, Input};
use crate::view::ViewMsg;
use crate::wa::{self, WaEvent, WaMsg, WaState};

pub enum AppEvent {
    Gateway(GwEvent),
    Messages { channel: String, older: bool, result: Result<Vec<Message>, String> },
    Sent(Message),
    Error(String),
    /// From a chat service speaking the WhatsApp helper protocol (WhatsApp, Matrix).
    Wa(Service, WaEvent),
    Pic { key: String, pic: Option<Pic> },
    Cal(CalMsg),
    /// From the task tab of this service.
    Todo(Service, TodoMsg),
    /// A newer release (its tag) is on GitHub.
    Update(String),
    /// A server's notification settings after `:mute`/`:unmute`, and what to say.
    Muted(GuildSettings, String),
}

pub enum TodoMsg {
    Synced(Vec<TaskList>, Vec<Task>, Vec<todo::Page>),
    Error(String),
    /// A whole list was read (or couldn't be).
    Page(String, Result<todo::Page, String>),
    /// Login started: open this page and enter the code.
    DeviceCode(String, String),
    LoggedIn(todo::Creds),
    /// A task changed; the text is shown, and `true` re-syncs.
    Changed(String, bool),
}

pub enum CalMsg {
    Synced(Synced),
    Error(String),
    LoggedIn(Creds),
    /// An event was created/changed/deleted; the text is shown, then we re-sync.
    Changed(String),
}

/// Wrapped message lines of the open chat, valid for one width and version.
pub struct MsgCache {
    pub key: String,
    pub width: usize,
    pub version: u64,
    pub lines: Vec<(usize, Line<'static>)>,
    /// Pictures referenced by each message index.
    pub pics: Vec<(usize, PicRef)>,
    /// Message shown with large pictures, and their row limit.
    pub hd: Option<(usize, u32)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Service {
    Discord,
    WhatsApp,
    Matrix,
    Calendar,
    Tasks,
    Twos,
}

impl Service {
    /// The integrations built in (Cargo features), in Shift+Tab / `gt` order.
    /// A new integration gets a feature and an entry here; per-service state
    /// and switching follow from this list.
    pub const ALL: &'static [Service] = &[
        #[cfg(feature = "discord")]
        Service::Discord,
        #[cfg(feature = "whatsapp")]
        Service::WhatsApp,
        #[cfg(feature = "matrix")]
        Service::Matrix,
        #[cfg(feature = "calendar")]
        Service::Calendar,
        #[cfg(feature = "todo")]
        Service::Tasks,
        #[cfg(feature = "twos")]
        Service::Twos,
    ];
    pub const COUNT: usize = Self::ALL.len();

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    /// The service `by` steps away in the cycle (negative = backwards).
    pub fn step(self, by: isize) -> Service {
        let n = Self::COUNT as isize;
        Self::ALL[(self.index() as isize + by).rem_euclid(n) as usize]
    }

    pub fn built(self) -> bool {
        Self::ALL.contains(&self)
    }

    pub fn name(self) -> &'static str {
        match self {
            Service::Discord => "Discord",
            Service::WhatsApp => "WhatsApp",
            Service::Matrix => "Matrix",
            Service::Calendar => "Google Calendar",
            Service::Tasks => "Microsoft To Do",
            Service::Twos => "Twos",
        }
    }

    /// A chat service (with unread messages to mark read).
    pub fn is_messaging(self) -> bool {
        matches!(self, Service::Discord | Service::WhatsApp | Service::Matrix)
    }

    /// A chat service run through the WhatsApp helper protocol (`WaState`).
    pub fn is_wa_like(self) -> bool {
        matches!(self, Service::WhatsApp | Service::Matrix)
    }

    /// A task tab (one task app each, see `TaskTab`).
    pub fn is_tasks(self) -> bool {
        matches!(self, Service::Tasks | Service::Twos)
    }
}

/// Calendar views, cycled with `v`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CalView {
    Week,
    ThreeDays,
    Day,
    Agenda,
}

impl CalView {
    /// Day columns in the time grid (0 for the agenda list).
    pub fn days(self) -> usize {
        match self {
            CalView::Week => 7,
            CalView::ThreeDays => 3,
            CalView::Day => 1,
            CalView::Agenda => 0,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            CalView::Week => "week",
            CalView::ThreeDays => "3 days",
            CalView::Day => "day",
            CalView::Agenda => "agenda",
        }
    }

    fn next(self) -> Self {
        match self {
            CalView::Week => CalView::ThreeDays,
            CalView::ThreeDays => CalView::Day,
            CalView::Day => CalView::Agenda,
            CalView::Agenda => CalView::Week,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Messages,
    Input,
    Command,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Reply { id: String, ping: bool },
    Edit(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    DmHeader,
    Dm,
    Guild,
    Category,
    Channel,
    WaChat,
    CalDay,
    CalEvent,
    TodoList,
    TodoTask,
    /// A Twos line without a checkbox: a note, bullet or heading.
    TodoNote,
}

pub struct TreeItem {
    pub kind: ItemKind,
    pub id: String,
    pub depth: u16,
    pub label: String,
    pub unread: bool,
    pub mentions: u32,
    pub expanded: bool,
    /// WhatsApp group (vs. a person).
    pub group: bool,
    /// Calendar color of an event.
    pub color: Option<[u8; 3]>,
    /// Past events.
    pub dim: bool,
}

pub struct Finder {
    pub query: Input,
    pub all: Vec<(String, String)>,
    pub matches: Vec<usize>,
    pub sel: usize,
}

impl Finder {
    fn refilter(&mut self) {
        let q = self.query.text().to_lowercase();
        let terms: Vec<&str> = q.split_whitespace().collect();
        self.matches = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, (_, label))| {
                let l = label.to_lowercase();
                terms.iter().all(|t| l.contains(t))
            })
            .map(|(i, _)| i)
            .collect();
        self.sel = 0;
    }
}

/// Calendar ids of the tasks each task app shows in the calendar.
const TODO_CAL: &str = "todo";
const TWOS_CAL: &str = "twos";
/// The sidebar section holding the lists without open to-dos.
const MORE_LISTS: &str = "\u{1}more";

/// A task tab: one task app's lists and tasks.
#[derive(Default)]
pub struct TaskTab {
    pub state: TodoState,
    api: Option<Backend>,
    /// Task lists folded or unfolded in the sidebar (the opposite of how they start).
    toggled: HashSet<String>,
    /// "More lists" is unfolded.
    more_open: bool,
    positioned: bool,
    /// The list shown whole in the main pane (Twos), and its selected line.
    page: Option<String>,
    page_sel: Option<usize>,
}

impl TaskTab {
    fn new(api: Option<Backend>) -> Self {
        let state = TodoState { reminded_until: Some(chrono::Local::now()), ..TodoState::default() };
        Self { state, api, ..Self::default() }
    }
}

pub struct App {
    pub state: State,
    api: Api,
    tx: UnboundedSender<AppEvent>,
    gw: UnboundedSender<String>,

    pub service: Service,
    pub wa: WaState,
    wa_cmd: UnboundedSender<String>,
    pub wa_current: Option<String>,
    /// Matrix: the same as the three above.
    pub mx: WaState,
    mx_cmd: UnboundedSender<String>,
    mx_current: Option<String>,
    /// Sidebar selection and unsent draft of each service while not shown.
    saved_trees: [ListState; Service::COUNT],
    saved_inputs: [Input; Service::COUNT],

    pub cal: CalState,
    google: Option<std::sync::Arc<Google>>,
    /// The agenda has been scrolled to today once.
    cal_positioned: bool,
    pub cal_view: CalView,
    /// Time-grid cursor: the start of a 30-minute slot.
    pub cal_cursor: chrono::NaiveDateTime,
    /// First day shown, and first slot row shown, in the time grid.
    pub cal_start: chrono::NaiveDate,
    pub cal_top: usize,
    /// The grid has been scrolled to the cursor once.
    pub cal_scrolled: bool,
    /// Event details popup.
    pub cal_detail: bool,
    /// The selected event in the time grid (key), which may be one of several
    /// at the cursor or an all-day event.
    pub cal_sel: Option<String>,
    /// The reaction picker is open for the selected message.
    pub react_picker: bool,

    /// The task tabs.
    pub todo: TaskTab,
    pub twos: TaskTab,

    pub focus: Focus,
    /// Pane to return to when leaving insert/command mode.
    last_pane: Focus,
    pub tree: Vec<TreeItem>,
    pub tree_state: ListState,
    expanded: HashSet<String>,
    collapsed: HashSet<String>,
    dms_open: bool,
    pub tree_dirty: bool,

    pub current: Option<String>,
    pub msg_sel: Option<usize>,
    pub msg_scroll: usize,
    /// Rows in the message view, for half-page motions.
    pub msg_height: usize,
    pub msg_cache: Option<MsgCache>,

    pub pics: Pics,
    /// Pictures the last draw found on screen but not loaded yet.
    pub pics_wanted: Vec<PicRef>,
    /// CDN client without credentials, and a cap on parallel downloads.
    pic_http: reqwest::Client,
    pic_sem: Arc<Semaphore>,

    pub input: Input,
    pub cmdline: Input,
    pub mode: Mode,
    pub status: String,
    pub conn: String,
    pub finder: Option<Finder>,
    pub help: bool,

    pending: Option<char>,
    count: Option<usize>,
    loading: HashSet<String>,
    subscribed: HashSet<String>,

    pub dirty: bool,
    pub quit: bool,
    pub logout: bool,
    /// A newer release, shown in the status bar.
    pub update: Option<String>,
}

impl App {
    pub fn new(
        api: Api,
        tx: UnboundedSender<AppEvent>,
        gw: UnboundedSender<String>,
        wa_cmd: UnboundedSender<String>,
        mx_cmd: UnboundedSender<String>,
    ) -> Self {
        Self {
            state: State::default(),
            api,
            tx,
            gw,
            service: Service::ALL[0],
            wa: WaState::default(),
            wa_cmd,
            wa_current: None,
            mx: WaState { matrix: true, ..WaState::default() },
            mx_cmd,
            mx_current: None,
            saved_trees: Default::default(),
            saved_inputs: Default::default(),
            cal: {
                let mut c = CalState::default();
                c.reminded_until = Some(chrono::Local::now());
                c
            },
            // Only integrations built in connect; the others stay idle.
            google: Creds::load()
                .filter(|c| Service::Calendar.built() && !c.refresh_token.is_empty())
                .map(|c| std::sync::Arc::new(Google::new(c))),
            cal_positioned: false,
            cal_view: CalView::Week,
            cal_cursor: cal::floor_slot(chrono::Local::now().naive_local()),
            cal_start: chrono::Local::now().date_naive(),
            cal_top: 0,
            cal_scrolled: false,
            cal_detail: false,
            cal_sel: None,
            react_picker: false,
            todo: TaskTab::new(
                todo::Creds::load()
                    .filter(|c| Service::Tasks.built() && !c.refresh_token.is_empty())
                    .map(|c| Backend::Microsoft(Arc::new(Todo::new(c)))),
            ),
            twos: TaskTab::new(twos::load_key().filter(|_| Service::Twos.built()).map(|k| Backend::Twos(Arc::new(Twos::new(k))))),
            focus: Focus::Tree,
            last_pane: Focus::Tree,
            tree: Vec::new(),
            tree_state: ListState::default().with_selected(Some(0)),
            expanded: HashSet::new(),
            collapsed: HashSet::new(),
            dms_open: true,
            tree_dirty: true,
            current: None,
            msg_sel: None,
            msg_scroll: 0,
            msg_height: 20,
            msg_cache: None,
            pics: Pics::default(),
            pics_wanted: Vec::new(),
            pic_http: reqwest::Client::builder()
                .user_agent(crate::api::user_agent())
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
            pic_sem: Arc::new(Semaphore::new(4)),
            input: Input::default(),
            cmdline: Input::default(),
            mode: Mode::Normal,
            status: String::new(),
            conn: "connecting…".into(),
            finder: None,
            help: false,
            pending: None,
            count: None,
            loading: HashSet::new(),
            subscribed: HashSet::new(),
            dirty: true,
            quit: false,
            logout: false,
            update: None,
        }
    }

    // ---- service-neutral accessors (used by the UI) ---------------------

    /// The chats of a WhatsApp-style service (WhatsApp or Matrix).
    pub fn chats(&self, s: Service) -> &WaState {
        if s == Service::Matrix {
            &self.mx
        } else {
            &self.wa
        }
    }

    fn chats_mut(&mut self, s: Service) -> &mut WaState {
        if s == Service::Matrix {
            &mut self.mx
        } else {
            &mut self.wa
        }
    }

    fn cmd_of(&self, s: Service) -> &UnboundedSender<String> {
        if s == Service::Matrix {
            &self.mx_cmd
        } else {
            &self.wa_cmd
        }
    }

    /// The open chat of a WhatsApp-style service.
    fn open_of(&self, s: Service) -> &Option<String> {
        if s == Service::Matrix {
            &self.mx_current
        } else {
            &self.wa_current
        }
    }

    fn open_of_mut(&mut self, s: Service) -> &mut Option<String> {
        if s == Service::Matrix {
            &mut self.mx_current
        } else {
            &mut self.wa_current
        }
    }

    pub fn current_chat(&self) -> Option<&str> {
        match self.service {
            Service::Discord => self.current.as_deref(),
            Service::WhatsApp | Service::Matrix => self.open_of(self.service).as_deref(),
            Service::Calendar | Service::Tasks | Service::Twos => None,
        }
    }

    pub fn chat_title(&self, id: &str) -> String {
        match self.service {
            Service::Discord => self.state.channel_title(id),
            Service::WhatsApp | Service::Matrix => self.chats(self.service).title(id),
            Service::Calendar | Service::Tasks | Service::Twos => String::new(),
        }
    }

    pub fn author_of(&self, id: &str) -> Option<String> {
        match self.service {
            Service::Discord => self.current_messages().iter().find(|m| m.id == id).map(|m| m.author.name().to_string()),
            Service::WhatsApp | Service::Matrix => self.wa_messages().iter().find(|m| m.id == id).map(|m| m.sender_name.clone()),
            Service::Calendar | Service::Tasks | Service::Twos => None,
        }
    }

    pub fn cache_key(&self, id: &str) -> String {
        match self.service {
            Service::Discord => format!("d:{id}"),
            Service::WhatsApp => format!("w:{id}"),
            Service::Matrix => format!("m:{id}"),
            Service::Calendar => format!("c:{id}"),
            Service::Tasks => format!("t:{id}"),
            Service::Twos => format!("2:{id}"),
        }
    }

    /// Changes whenever loaded messages of either service change.
    pub fn msg_version(&self) -> u64 {
        self.state.version + self.wa.version + self.mx.version + self.pics.version
    }

    /// Starts loading the pictures the last draw asked for.
    pub fn load_pics(&mut self) {
        for r in std::mem::take(&mut self.pics_wanted) {
            if !self.pics.start(&r.key) {
                continue;
            }
            match r.src {
                PicSrc::Url(url) => {
                    let (http, sem, key) = (self.pic_http.clone(), self.pic_sem.clone(), r.key);
                    self.spawn(async move {
                        let _permit = sem.acquire_owned().await.ok();
                        let pic = fetch_pic(&http, &url, key.clone()).await;
                        Some(AppEvent::Pic { key, pic })
                    });
                }
                PicSrc::Wa { chat, id, full } => {
                    wa::send(&self.wa_cmd, json!({"c": "thumb", "chat": chat, "id": id, "full": full, "key": r.key}))
                }
                PicSrc::Mx { chat, id, full } => {
                    wa::send(&self.mx_cmd, json!({"c": "thumb", "chat": chat, "id": id, "full": full, "key": r.key}))
                }
            }
        }
    }

    pub fn chat_loading(&self, id: &str) -> bool {
        match self.service {
            Service::Discord => self.loading.contains(id),
            Service::WhatsApp | Service::Matrix => self.chats(self.service).loading.contains(id),
            Service::Calendar | Service::Tasks | Service::Twos => false,
        }
    }

    pub fn chat_complete(&self, id: &str) -> bool {
        match self.service {
            Service::Discord => self.state.complete.contains(id),
            Service::WhatsApp | Service::Matrix => self.chats(self.service).complete.contains(id),
            Service::Calendar | Service::Tasks | Service::Twos => false,
        }
    }

    pub fn view_messages(&self, id: &str) -> Option<Vec<ViewMsg>> {
        match self.service {
            Service::Discord => self.state.view_messages(id),
            Service::WhatsApp | Service::Matrix => self.chats(self.service).view_messages(id),
            Service::Calendar | Service::Tasks | Service::Twos => None,
        }
    }

    fn wa_messages(&self) -> &[WaMsg] {
        self.open_of(self.service)
            .as_ref()
            .and_then(|c| self.chats(self.service).messages.get(c))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn current_len(&self) -> usize {
        match self.service {
            Service::Discord => self.current_messages().len(),
            Service::WhatsApp | Service::Matrix => self.wa_messages().len(),
            Service::Calendar | Service::Tasks | Service::Twos => 0,
        }
    }

    fn last_own(&self) -> Option<usize> {
        match self.service {
            Service::Discord => {
                let me = &self.state.me.id;
                self.current_messages().iter().rposition(|m| &m.author.id == me)
            }
            Service::WhatsApp | Service::Matrix => self.wa_messages().iter().rposition(|m| m.from_me && !m.deleted),
            Service::Calendar | Service::Tasks | Service::Twos => None,
        }
    }

    fn selected_id(&self) -> Option<String> {
        match self.service {
            Service::Discord => self.selected_message().map(|m| m.id.clone()),
            Service::WhatsApp | Service::Matrix => self.wa_messages().get(self.msg_sel?).map(|m| m.id.clone()),
            Service::Calendar | Service::Tasks | Service::Twos => None,
        }
    }

    fn tree_sel(&self) -> usize {
        self.tree_state.selected().unwrap_or(0)
    }

    fn set_tree_sel(&mut self, i: usize) {
        let max = self.tree.len().saturating_sub(1);
        self.tree_state.select(Some(i.min(max)));
    }

    pub fn rebuild_tree(&mut self) {
        self.tree_dirty = false;
        match self.service {
            Service::WhatsApp | Service::Matrix => return self.rebuild_wa_tree(),
            Service::Calendar => return self.rebuild_cal_tree(),
            Service::Tasks | Service::Twos => return self.rebuild_todo_tree(),
            Service::Discord => {}
        }
        let sel_key = self.tree.get(self.tree_sel()).map(|t| (t.kind, t.id.clone()));
        let s = &self.state;
        let mut items = Vec::new();

        let mut dm_unread = false;
        let mut dm_mentions = 0;
        for c in s.dms.iter().filter_map(|id| s.channels.get(id)) {
            dm_unread |= s.shows_unread(c);
            dm_mentions += s.mention_count(&c.id);
        }
        items.push(TreeItem {
            kind: ItemKind::DmHeader,
            id: String::new(),
            depth: 0,
            label: "Direct Messages".into(),
            unread: dm_unread,
            mentions: dm_mentions,
            expanded: self.dms_open,
            group: false,
            color: None,
            dim: false,
        });
        if self.dms_open {
            for c in s.dms.iter().filter_map(|id| s.channels.get(id)) {
                items.push(TreeItem {
                    kind: ItemKind::Dm,
                    id: c.id.clone(),
                    depth: 1,
                    label: s.dm_label(c),
                    unread: s.shows_unread(c),
                    mentions: s.mention_count(&c.id),
                    expanded: false,
                    group: false,
                    color: None,
                    dim: s.channel_muted(c),
                });
            }
        }

        for g in s.guild_order.iter().filter_map(|id| s.guilds.get(id)) {
            let open = self.expanded.contains(&g.id);
            let (unread, mentions) = s.guild_unread(g);
            items.push(TreeItem {
                kind: ItemKind::Guild,
                id: g.id.clone(),
                depth: 0,
                label: g.name.clone(),
                unread,
                mentions,
                expanded: open,
                group: false,
                color: None,
                dim: s.is_muted(&g.id),
            });
            if !open {
                continue;
            }
            for (cat, chans) in s.guild_layout(g) {
                let depth = match cat {
                    Some(cat) => {
                        let shut = self.collapsed.contains(&cat.id);
                        items.push(TreeItem {
                            kind: ItemKind::Category,
                            id: cat.id.clone(),
                            depth: 1,
                            label: cat.name.clone().unwrap_or_default().to_uppercase(),
                            unread: shut && chans.iter().any(|c| s.shows_unread(c)),
                            mentions: if shut { chans.iter().map(|c| s.mention_count(&c.id)).sum() } else { 0 },
                            expanded: !shut,
                            group: false,
                            color: None,
                            dim: false,
                        });
                        if shut {
                            continue;
                        }
                        2
                    }
                    None => 1,
                };
                for c in chans {
                    items.push(TreeItem {
                        kind: ItemKind::Channel,
                        id: c.id.clone(),
                        depth,
                        label: c.name.clone().unwrap_or_default(),
                        unread: s.shows_unread(c),
                        mentions: s.mention_count(&c.id),
                        expanded: false,
                        group: false,
                        color: None,
                        dim: s.channel_muted(c),
                    });
                }
            }
        }

        self.tree = items;
        if let Some((k, id)) = sel_key {
            if let Some(i) = self.tree.iter().position(|t| t.kind == k && t.id == id) {
                self.tree_state.select(Some(i));
            }
        }
        let sel = self.tree_sel();
        self.set_tree_sel(sel);
    }

    fn rebuild_wa_tree(&mut self) {
        let sel_id = self.tree.get(self.tree_sel()).map(|t| t.id.clone());
        let chats = self.chats(self.service);
        let tree = chats
            .order
            .iter()
            .filter_map(|jid| chats.chats.get(jid))
            .map(|c| TreeItem {
                kind: ItemKind::WaChat,
                id: c.jid.clone(),
                depth: 0,
                label: chats.label(&c.jid),
                unread: c.unread > 0,
                mentions: c.unread,
                expanded: false,
                group: c.is_group,
                color: None,
                dim: false,
            })
            .collect();
        self.tree = tree;
        if let Some(id) = sel_id {
            if let Some(i) = self.tree.iter().position(|t| t.id == id) {
                self.tree_state.select(Some(i));
            }
        }
        let sel = self.tree_sel();
        self.set_tree_sel(sel);
    }

    fn rebuild_cal_tree(&mut self) {
        let sel_id = self.tree.get(self.tree_sel()).map(|t| t.id.clone());
        let now = chrono::Local::now();
        let today = now.date_naive();
        let mut items = Vec::new();
        let mut day = None;
        for e in &self.cal.events {
            let d = e.start.date_naive();
            if day != Some(d) {
                day = Some(d);
                let name = d.format("%a %d %b").to_string();
                let label = match (d - today).num_days() {
                    0 => format!("Today · {name}"),
                    1 => format!("Tomorrow · {name}"),
                    -1 => format!("Yesterday · {name}"),
                    _ => name,
                };
                items.push(TreeItem {
                    kind: ItemKind::CalDay,
                    id: format!("day:{d}"),
                    depth: 0,
                    label,
                    unread: d == today,
                    mentions: 0,
                    expanded: false,
                    group: false,
                    color: None,
                    dim: d < today,
                });
            }
            let time = if e.all_day { "all day".to_string() } else { e.start.format("%H:%M").to_string() };
            items.push(TreeItem {
                kind: ItemKind::CalEvent,
                id: e.key(),
                depth: 1,
                label: format!("{time}  {}", e.title),
                unread: false,
                mentions: 0,
                expanded: false,
                group: false,
                color: self.cal.calendars.get(&e.cal).and_then(|c| c.color),
                dim: e.end <= now,
            });
        }
        self.tree = items;
        match sel_id.and_then(|id| self.tree.iter().position(|t| t.id == id)) {
            Some(i) if self.cal_positioned => self.tree_state.select(Some(i)),
            _ if !self.cal.events.is_empty() => {
                self.cal_positioned = true;
                self.jump_to_today();
            }
            _ => {}
        }
        let sel = self.tree_sel();
        self.set_tree_sel(sel);
    }

    /// Selects the first event that hasn't ended yet.
    fn jump_to_today(&mut self) {
        let now = chrono::Local::now();
        let first = self.cal.events.iter().find(|e| e.end > now).map(|e| e.key());
        let idx = first.and_then(|k| self.tree.iter().position(|t| t.id == k)).unwrap_or(self.tree.len().saturating_sub(1));
        // Show the day header above it too.
        self.tree_state.select(Some(idx.saturating_sub(1)));
        self.tree_state.select(Some(idx));
    }

    /// The event selected in the agenda.
    pub fn cal_connected(&self) -> bool {
        self.google.is_some()
    }

    fn tab(&self, s: Service) -> &TaskTab {
        if s == Service::Twos {
            &self.twos
        } else {
            &self.todo
        }
    }

    fn tab_mut(&mut self, s: Service) -> &mut TaskTab {
        if s == Service::Twos {
            &mut self.twos
        } else {
            &mut self.todo
        }
    }

    /// The tasks of the task tab shown.
    pub fn tasks(&self) -> &TodoState {
        &self.tab(self.service).state
    }

    pub fn tasks_connected(&self) -> bool {
        self.tab(self.service).api.is_some()
    }

    /// Open tasks due today or earlier, in all task apps.
    pub fn due_tasks(&self) -> usize {
        let today = chrono::Local::now().date_naive();
        self.todo.state.due_count(today) + self.twos.state.due_count(today)
    }

    /// Shows the dated tasks of all task apps in the calendar.
    fn show_tasks_in_cal(&mut self) {
        let mut events = self.todo.state.as_events(TODO_CAL);
        events.extend(self.twos.state.as_events(TWOS_CAL));
        self.cal.set_tasks(events, &[(TODO_CAL, "Microsoft To Do", [37, 100, 207]), (TWOS_CAL, "Twos", [235, 140, 50])]);
    }

    /// The task tab a task shown in the calendar comes from.
    fn task_service(cal: &str) -> Service {
        if cal == TWOS_CAL {
            Service::Twos
        } else {
            Service::Tasks
        }
    }

    /// The event under the agenda selection or the time-grid cursor.
    pub fn selected_event(&self) -> Option<&cal::Event> {
        if self.cal_view == CalView::Agenda {
            let item = self.tree.get(self.tree_sel()).filter(|t| t.kind == ItemKind::CalEvent)?;
            return self.cal.find(&item.id);
        }
        self.cal.find(self.cal_sel.as_deref()?)
    }

    /// Events at the cursor: the day's all-day events, then timed events
    /// overlapping the cursor slot (the order `Tab` cycles through).
    fn events_at_cursor(&self) -> Vec<&cal::Event> {
        let day = self.cal_cursor.date();
        let from = cal::to_local(self.cal_cursor);
        let to = from + chrono::Duration::minutes(30);
        let (mut all_day, timed): (Vec<&cal::Event>, Vec<&cal::Event>) = self
            .cal
            .events
            .iter()
            .filter(|e| {
                if e.all_day {
                    e.start.date_naive() <= day && e.end.date_naive() > day
                } else {
                    e.start < to && e.end > from
                }
            })
            .partition(|e| e.all_day);
        all_day.extend(timed);
        all_day
    }

    /// Selects the event in front at the cursor slot (the one that started
    /// last), or nothing on a free slot.
    fn cal_autoselect(&mut self) {
        let from = cal::to_local(self.cal_cursor);
        let to = from + chrono::Duration::minutes(30);
        self.cal_sel = self
            .cal
            .events
            .iter()
            .filter(|e| !e.all_day && e.start < to && e.end > from)
            .max_by_key(|e| e.start)
            .map(|e| e.key());
    }

    /// `Tab`: the next event at the cursor, including all-day ones.
    fn cal_cycle(&mut self) {
        let here = self.events_at_cursor();
        if here.is_empty() {
            self.status = "no events here".into();
            return;
        }
        let i = self
            .cal_sel
            .as_ref()
            .and_then(|k| here.iter().position(|e| &e.key() == k))
            .map_or(0, |i| (i + 1) % here.len());
        let (key, n) = (here[i].key(), here.len());
        self.cal_sel = Some(key);
        if n > 1 {
            self.status = format!("event {} of {n} here (Tab for the next)", i + 1);
        }
    }

    fn cal_goto(&mut self, t: chrono::DateTime<chrono::Local>) {
        self.cal_cursor = cal::floor_slot(t.naive_local());
        self.cal_autoselect();
    }

    /// Moves the cursor to an event and selects exactly it.
    fn cal_select(&mut self, key: &str) {
        let Some(e) = self.cal.find(key) else { return };
        self.cal_cursor = if e.all_day {
            // Keep the time of day; just go to its date.
            e.start.date_naive().and_time(self.cal_cursor.time())
        } else {
            cal::floor_slot(e.start.naive_local())
        };
        self.cal_sel = Some(key.to_string());
    }

    /// Moves the grid cursor to the next (or previous) event start.
    /// `w` / `b`: steps through all events in order, one at a time, so
    /// events starting together and all-day events are reachable too.
    fn cal_jump(&mut self, forward: bool) {
        let evs = &self.cal.events;
        let cur = self.cal_sel.as_ref().and_then(|k| evs.iter().position(|e| &e.key() == k));
        let at = cal::to_local(self.cal_cursor);
        let idx = match (cur, forward) {
            (Some(i), true) => (i + 1 < evs.len()).then_some(i + 1),
            (Some(i), false) => i.checked_sub(1),
            (None, true) => evs.iter().position(|e| e.start >= at),
            (None, false) => evs.iter().rposition(|e| e.start < at),
        };
        match idx.map(|i| evs[i].key()) {
            Some(key) => self.cal_select(&key),
            None => self.status = "no more events".into(),
        }
    }

    fn cal_grid_key(&mut self, k: KeyEvent, pending: Option<char>, n: usize) {
        use chrono::Duration as D;
        let ctrl = is_ctrl(&k);
        let page = self.cal_view.days().max(1) as i64;
        let n = n as i64;
        let day = self.cal_cursor.date();
        let at = |h: u32, m: u32| day.and_hms_opt(h, m, 0).unwrap();
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => self.cal_cursor += D::minutes(30 * n),
            KeyCode::Char('k') | KeyCode::Up => self.cal_cursor -= D::minutes(30 * n),
            KeyCode::Char('l') | KeyCode::Right => self.cal_cursor += D::days(n),
            KeyCode::Char('h') | KeyCode::Left => self.cal_cursor -= D::days(n),
            KeyCode::Char('L') | KeyCode::PageDown => self.cal_cursor += D::days(page * n),
            KeyCode::Char('H') | KeyCode::PageUp => self.cal_cursor -= D::days(page * n),
            KeyCode::Char('d') if ctrl => self.cal_cursor += D::hours(4),
            KeyCode::Char('u') if ctrl => self.cal_cursor -= D::hours(4),
            KeyCode::Char('g') if pending == Some('g') => self.cal_cursor = at(0, 0),
            KeyCode::Char('g') => self.pending = Some('g'),
            KeyCode::Char('G') | KeyCode::End => self.cal_cursor = at(23, 30),
            KeyCode::Home => self.cal_cursor = at(0, 0),
            _ => return,
        }
        self.cal_autoselect();
    }

    // ---- calendar ---------------------------------------------------------

    pub fn cal_sync(&mut self) {
        let Some(g) = self.google.clone() else { return };
        if self.cal.syncing {
            return;
        }
        self.cal.syncing = true;
        self.cal.next_poll = None;
        if self.cal.status.is_empty() {
            self.cal.status = "syncing…".into();
        }
        self.spawn(async move {
            Some(AppEvent::Cal(match g.sync().await {
                Ok(s) => CalMsg::Synced(s),
                Err(e) => CalMsg::Error(e),
            }))
        });
    }

    fn on_cal(&mut self, msg: CalMsg) {
        match msg {
            CalMsg::Synced(s) => {
                self.cal.syncing = false;
                self.cal.next_poll = Some(std::time::Instant::now() + cal::POLL);
                self.cal.apply(s);
                self.done_refreshing();
                // Events are new objects after a sync; keep the selection if it still exists.
                if self.cal_sel.as_ref().map_or(true, |k| self.cal.find(k).is_none()) {
                    self.cal_autoselect();
                }
            }
            CalMsg::Error(e) => {
                self.cal.syncing = false;
                self.cal.next_poll = Some(std::time::Instant::now() + cal::POLL);
                self.cal.status = "sync failed".into();
                self.status = e;
            }
            CalMsg::LoggedIn(creds) => {
                if let Err(e) = creds.save() {
                    self.status = format!("could not save Google login: {e}");
                }
                self.google = Some(std::sync::Arc::new(Google::new(creds)));
                self.status = "Google Calendar connected".into();
                self.cal.status = "syncing…".into();
                self.cal_sync();
            }
            CalMsg::Changed(text) => {
                self.status = text;
                self.cal_sync();
            }
        }
        self.tree_dirty = true;
    }

    /// When the event loop should call `on_tick` next.
    pub fn next_wake(&self) -> Option<std::time::Instant> {
        use chrono::Timelike;
        let now = chrono::Local::now();
        let earliest = |a: Option<std::time::Instant>, b: Option<std::time::Instant>| match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let cal = earliest(
            self.google.as_ref().and_then(|_| self.cal.next_wake(now)),
            earliest(
                self.todo.api.as_ref().and_then(|_| self.todo.state.next_wake(now)),
                self.twos.api.as_ref().and_then(|_| self.twos.state.next_wake(now)),
            ),
        );
        // The time grid's "now" line moves every 30 minutes.
        let grid = (self.service == Service::Calendar && self.cal_view != CalView::Agenda).then(|| {
            let secs = (30 - now.minute() % 30) as u64 * 60 - now.second() as u64;
            std::time::Instant::now() + std::time::Duration::from_secs(secs)
        });
        match (cal, grid) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Timers: reminders, polling, and redraws when events start or end.
    pub fn on_tick(&mut self) {
        let now = chrono::Local::now();
        let due: Vec<String> = self
            .cal
            .due(now)
            .into_iter()
            .map(|e| {
                let mins = (e.start - now).num_minutes().max(0);
                let when = if mins == 0 { "now".to_string() } else { format!("in {mins} min") };
                format!("⏰ {} {when} ({})", e.title, e.start.format("%H:%M"))
            })
            .collect();
        if !due.is_empty() {
            self.status = due.join(" · ");
            bell();
        }
        if self.cal.next_poll.is_some_and(|t| t <= std::time::Instant::now()) {
            self.cal_sync();
        }
        for s in [Service::Tasks, Service::Twos] {
            let tasks: Vec<String> = self.tab_mut(s).state.due_reminders(now).into_iter().map(|t| format!("☐ {}", t.title)).collect();
            if !tasks.is_empty() {
                self.status = format!("⏰ {}: {}", s.name(), tasks.join(" · "));
                bell();
            }
            if self.tab(s).state.next_poll.is_some_and(|t| t <= std::time::Instant::now()) {
                self.todo_sync(s);
            }
        }
        self.tree_dirty = true;
        self.dirty = true;
    }

    // ---- task tabs (Microsoft To Do, Twos) --------------------------------------

    pub fn todo_sync(&mut self, s: Service) {
        let tab = self.tab_mut(s);
        let Some(api) = tab.api.clone() else { return };
        if tab.state.syncing {
            return;
        }
        tab.state.syncing = true;
        tab.state.next_poll = None;
        if tab.state.status.is_empty() {
            tab.state.status = "syncing…".into();
        }
        self.spawn(async move {
            Some(AppEvent::Todo(s, match api.sync().await {
                Ok((lists, tasks, pages)) => TodoMsg::Synced(lists, tasks, pages),
                Err(e) => TodoMsg::Error(e),
            }))
        });
    }

    fn on_todo(&mut self, s: Service, msg: TodoMsg) {
        match msg {
            TodoMsg::Synced(lists, tasks, pages) => {
                let st = &mut self.tab_mut(s).state;
                st.syncing = false;
                st.next_poll = Some(std::time::Instant::now() + todo::POLL);
                st.apply(lists, tasks, pages);
                self.done_refreshing();
            }
            TodoMsg::Page(list, page) => match page {
                Ok(p) => {
                    let st = &mut self.tab_mut(s).state;
                    st.loading.remove(&list);
                    st.pages.insert(list, p);
                }
                // Stays marked as loading, so it's tried again at the next sync, not on every redraw.
                Err(e) => self.status = e,
            },
            TodoMsg::Error(e) => {
                let st = &mut self.tab_mut(s).state;
                st.syncing = false;
                st.login = None;
                st.next_poll = Some(std::time::Instant::now() + todo::POLL);
                st.status = "sync failed".into();
                self.status = e;
            }
            TodoMsg::DeviceCode(uri, code) => {
                self.status = format!("Microsoft sign-in: open {uri} and enter {code}");
                let _ = open_url(&uri);
                self.todo.state.login = Some((uri, code));
            }
            TodoMsg::LoggedIn(creds) => {
                if let Err(e) = creds.save() {
                    self.status = format!("could not save Microsoft login: {e}");
                } else {
                    self.status = "Microsoft To Do connected".into();
                }
                self.todo.state.login = None;
                self.todo.api = Some(Backend::Microsoft(Arc::new(Todo::new(creds))));
                self.todo.state.status = "syncing…".into();
                self.todo_sync(Service::Tasks);
            }
            TodoMsg::Changed(text, resync) => {
                self.status = text;
                if resync {
                    self.todo_sync(s);
                }
            }
        }
        self.show_tasks_in_cal();
        self.tree_dirty = true;
    }

    fn todo_login(&mut self, client_id: String) {
        let tx = self.tx.clone();
        self.status = "contacting Microsoft…".into();
        tokio::spawn(async move {
            let send = |m| {
                let _ = tx.send(AppEvent::Todo(Service::Tasks, m));
            };
            match todo::device_code(&client_id).await {
                Ok(dc) => {
                    send(TodoMsg::DeviceCode(dc.uri.clone(), dc.code.clone()));
                    send(match todo::finish_login(client_id, dc).await {
                        Ok(c) => TodoMsg::LoggedIn(c),
                        Err(e) => TodoMsg::Error(format!("Microsoft sign-in failed: {e}")),
                    });
                }
                Err(e) => send(TodoMsg::Error(e)),
            }
        });
    }

    fn todo_command(&mut self, arg: &str) {
        let (sub, rest) = arg.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((arg, ""));
        match sub {
            "setup" => {
                let id = rest.trim().trim_matches('"');
                if id.len() < 30 || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
                    self.status = "usage: :todo setup <Application (client) ID>, e.g. 1a2b3c4d-…".into();
                    return;
                }
                let creds = todo::Creds { client_id: id.to_string(), refresh_token: String::new() };
                let _ = creds.save();
                self.todo_login(creds.client_id);
            }
            "login" => match todo::Creds::load().map(|c| c.client_id).or_else(todo::builtin_client_id) {
                Some(id) => self.todo_login(id),
                None => self.status = "run :todo setup <client id> first".into(),
            },
            "logout" => {
                todo::Creds::remove();
                self.todo = TaskTab::default();
                self.show_tasks_in_cal();
                self.tree_dirty = true;
                self.status = "Microsoft To Do disconnected".into();
            }
            "refresh" | "sync" | "" => self.todo_sync(Service::Tasks),
            _ => self.status = "usage: :todo setup <client id> · :todo login · :todo logout · :todo refresh".into(),
        }
    }

    fn twos_command(&mut self, arg: &str) {
        let (sub, rest) = arg.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((arg, ""));
        match sub {
            "login" | "key" => {
                let key = rest.trim_matches('"');
                if !twos::is_key(key) {
                    self.status = "usage: :twos login <API key> (Twos → Settings → API Keys; it starts with twos_)".into();
                    return;
                }
                if let Err(e) = twos::save_key(key) {
                    self.status = format!("could not save the Twos key: {e}");
                    return;
                }
                self.twos = TaskTab::new(Some(Backend::Twos(Arc::new(Twos::new(key.to_string())))));
                self.status = "Twos connected".into();
                self.todo_sync(Service::Twos);
                self.tree_dirty = true;
            }
            "logout" => {
                twos::remove_key();
                self.twos = TaskTab::default();
                self.show_tasks_in_cal();
                self.tree_dirty = true;
                self.status = "Twos disconnected".into();
            }
            "refresh" | "sync" | "" => self.todo_sync(Service::Twos),
            _ => self.status = "usage: :twos login <API key> · :twos logout · :twos refresh".into(),
        }
    }

    fn rebuild_todo_tree(&mut self) {
        let sel_id = self.tree.get(self.tree_sel()).map(|t| t.id.clone());
        let today = chrono::Local::now().date_naive();
        let mut items = Vec::new();
        let tab = self.tab(self.service);
        let list_item = |id: &str, depth: u16, label: String, expanded: bool| TreeItem {
            kind: ItemKind::TodoList,
            id: id.to_string(),
            depth,
            label,
            unread: false,
            mentions: 0,
            expanded,
            group: false,
            color: None,
            dim: false,
        };
        let name = |l: &TaskList| if l.emoji.is_empty() { l.name.clone() } else { format!("{} {}", l.emoji, l.name) };
        let (top, more): (Vec<&TaskList>, Vec<&TaskList>) = tab.state.lists.iter().partition(|l| !l.more);
        for l in top {
            let tasks: Vec<&Task> = tab.state.tasks.iter().filter(|t| t.list == l.id).collect();
            let open = tasks.iter().filter(|t| t.check && !t.done).count();
            let shut = l.folded != tab.toggled.contains(&l.id);
            items.push(list_item(&l.id, 0, if open > 0 { format!("{} ({open})", name(l)) } else { name(l) }, !shut));
            if shut {
                continue;
            }
            for t in tasks {
                let due = t.due.map(|d| match (d - today).num_days() {
                    0 => "today".to_string(),
                    1 => "tomorrow".to_string(),
                    _ => d.format("%a %d %b").to_string(),
                });
                let color = match t.due {
                    _ if t.done => None,
                    Some(d) if d < today => Some([230, 80, 80]),
                    Some(d) if d == today => Some([230, 190, 40]),
                    _ => None,
                };
                let mut label = format!("{}{}", t.marker, t.title);
                if let Some(d) = due {
                    label.push_str(&format!("  · {d}"));
                }
                if let Some(from) = t.from {
                    label.push_str(&format!("  ↪ {}", from.format("%a %d %b")));
                }
                items.push(TreeItem {
                    kind: if t.check { ItemKind::TodoTask } else { ItemKind::TodoNote },
                    id: t.key(),
                    depth: 1 + t.depth as u16,
                    label,
                    unread: (t.important || t.heading) && !t.done,
                    mentions: 0,
                    expanded: false,
                    group: false,
                    color,
                    dim: t.done,
                });
            }
        }
        if !more.is_empty() {
            items.push(list_item(MORE_LISTS, 0, format!("More lists ({})", more.len()), tab.more_open));
            if tab.more_open {
                items.extend(more.into_iter().map(|l| list_item(&l.id, 1, name(l), false)));
            }
        }
        self.tree = items;
        let positioned = self.tab(self.service).positioned;
        match sel_id.and_then(|id| self.tree.iter().position(|t| t.id == id)) {
            Some(i) if positioned => self.tree_state.select(Some(i)),
            _ if !self.tree.is_empty() && !positioned => {
                self.tab_mut(self.service).positioned = true;
                let first = self.tree.iter().position(|t| matches!(t.kind, ItemKind::TodoTask | ItemKind::TodoNote)).unwrap_or(0);
                self.tree_state.select(Some(first));
            }
            _ => {}
        }
        let sel = self.tree_sel();
        self.set_tree_sel(sel);
    }

    /// The selected task or line: in the whole list when the main pane has
    /// focus, else in the sidebar.
    pub fn selected_task(&self) -> Option<&Task> {
        if self.in_page() {
            return self.page_task();
        }
        self.tree_task()
    }

    fn tree_task(&self) -> Option<&Task> {
        let item = self.tree.get(self.tree_sel()).filter(|t| matches!(t.kind, ItemKind::TodoTask | ItemKind::TodoNote))?;
        self.tasks().find(&item.id)
    }

    /// The main pane (showing a whole list) has focus, or had it before typing.
    pub fn in_page(&self) -> bool {
        let pane = match self.focus {
            Focus::Input | Focus::Command => self.last_pane,
            f => f,
        };
        pane == Focus::Messages && self.page().is_some()
    }

    /// The whole list in the main pane (Twos), once read.
    pub fn page(&self) -> Option<&todo::Page> {
        let tab = self.tab(self.service);
        tab.state.pages.get(tab.page.as_ref()?)
    }

    /// The main pane is waiting for its list.
    pub fn page_loading(&self) -> bool {
        let tab = self.tab(self.service);
        tab.page.as_ref().is_some_and(|l| !tab.state.pages.contains_key(l))
    }

    pub fn page_sel(&self) -> Option<usize> {
        self.tab(self.service).page_sel
    }

    fn page_task(&self) -> Option<&Task> {
        self.page()?.lines.get(self.page_sel()?)
    }

    /// Keeps the main pane's list in step with the sidebar selection (while
    /// the sidebar has focus) and reads it when it isn't loaded yet.
    pub fn sync_page(&mut self) {
        let s = self.service;
        let Some(api) = self.tab(s).api.clone().filter(Backend::has_pages) else { return };
        if !self.in_page() && self.focus != Focus::Input {
            let item = self.tree.get(self.tree_sel());
            let task = self.tree_task();
            let list = match item.map(|t| t.kind) {
                Some(ItemKind::TodoList) => item.map(|t| t.id.clone()).filter(|id| id != MORE_LISTS),
                _ => task.map(|t| t.list.clone()),
            };
            let list = list.or_else(|| self.tasks().default_list().map(String::from));
            let key = task.map(|t| t.key());
            let tab = self.tab_mut(s);
            tab.page_sel = key.and_then(|k| tab.state.pages.get(list.as_ref()?)?.lines.iter().position(|t| t.key() == k));
            tab.page = list;
        }
        let tab = self.tab_mut(s);
        let Some(list) = tab.page.clone() else { return };
        if let Some(p) = tab.state.pages.get(&list) {
            tab.page_sel = tab.page_sel.map(|i| i.min(p.lines.len().saturating_sub(1))).filter(|_| !p.lines.is_empty());
            return;
        }
        if !tab.state.loading.insert(list.clone()) {
            return;
        }
        self.spawn(async move {
            let page = api.page(&list).await;
            Some(AppEvent::Todo(s, TodoMsg::Page(list, page)))
        });
    }

    /// Keys of the main pane while it shows a whole list.
    fn page_key(&mut self, k: KeyEvent, pending: Option<char>, n: usize, counted: bool) {
        let Some(len) = self.page().map(|p| p.lines.len()) else { return };
        let last = len.saturating_sub(1);
        let sel = self.page_sel();
        let half = (self.msg_height / 2).max(1);
        let down = |by: usize| sel.map_or(0, |i| (i + by).min(last));
        let up = |by: usize| sel.map_or(0, |i| i.saturating_sub(by));
        let to = match k.code {
            KeyCode::Char('j') | KeyCode::Down => down(n),
            KeyCode::Char('k') | KeyCode::Up => up(n),
            KeyCode::Char('d') if is_ctrl(&k) => down(half),
            KeyCode::Char('u') if is_ctrl(&k) => up(half),
            KeyCode::PageDown => down(half * 2),
            KeyCode::PageUp => up(half * 2),
            KeyCode::Char('g') if pending == Some('g') => if counted { (n - 1).min(last) } else { 0 },
            KeyCode::Char('g') => {
                self.pending = Some('g');
                return;
            }
            KeyCode::Char('G') | KeyCode::End => if counted { (n - 1).min(last) } else { last },
            KeyCode::Home => 0,
            KeyCode::Char('h') | KeyCode::Left => {
                self.focus = Focus::Tree;
                return;
            }
            // A sub-list opens in place (h goes back to the sidebar's list); a link in the browser.
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right | KeyCode::Char('o') => {
                let Some(t) = self.page_task() else { return };
                if let Some(sub) = t.sublist.clone() {
                    let tab = self.tab_mut(self.service);
                    tab.page = Some(sub);
                    tab.page_sel = Some(0);
                } else if !t.url.is_empty() {
                    let url = t.url.clone();
                    if let Err(e) = open_url(&url) {
                        self.status = format!("could not open {url}: {e}");
                    }
                }
                return;
            }
            _ => return,
        };
        if len > 0 {
            self.tab_mut(self.service).page_sel = Some(to);
        }
    }

    /// The list new tasks go to: the selected one, else the default list.
    fn target_list(&self) -> Option<String> {
        if self.in_page() {
            return self.page().map(|p| p.list.clone());
        }
        let item = self.tree.get(self.tree_sel());
        match item.map(|t| t.kind) {
            Some(ItemKind::TodoList) => item.map(|t| t.id.clone()),
            Some(ItemKind::TodoTask | ItemKind::TodoNote) => self.selected_task().map(|t| t.list.clone()),
            _ => None,
        }
        .or_else(|| self.tasks().default_list().map(String::from))
    }

    /// Ticks a task off (or back on). It stays visible, struck through, until
    /// the next sync, so a mistaken `x` can be undone with another `x`.
    fn toggle_task(&mut self, s: Service, key: &str) {
        let tab = self.tab_mut(s);
        let Some(api) = tab.api.clone() else { return };
        let Some(t) = tab.state.find(key) else { return };
        let (list, id, done, title) = (t.list.clone(), t.id.clone(), !t.done, t.title.clone());
        tab.state.set_done(key, done);
        self.show_tasks_in_cal();
        self.tree_dirty = true;
        self.spawn(async move {
            Some(AppEvent::Todo(s, match api.set_done(&list, &id, done).await {
                Ok(()) => TodoMsg::Changed(format!("{} \"{title}\"", if done { "done:" } else { "not done:" }), false),
                Err(e) => TodoMsg::Error(e),
            }))
        });
    }

    fn todo_key(&mut self, k: KeyEvent, pending: Option<char>) -> bool {
        match k.code {
            KeyCode::Char('n') | KeyCode::Char('i') | KeyCode::Char('a') => {
                if !self.tasks_connected() {
                    self.status = format!("connect {} first (see the right pane)", self.service.name());
                } else {
                    self.mode = Mode::Normal;
                    self.enter_insert();
                }
            }
            KeyCode::Char('e') | KeyCode::Char('c') => {
                if let Some(t) = self.selected_task() {
                    let (key, text) = (t.key(), t.to_entry());
                    self.mode = Mode::Edit(key);
                    self.input.set(&text);
                    self.enter_insert();
                }
            }
            KeyCode::Char('x') | KeyCode::Char(' ') if self.selected_task().is_some_and(|t| t.check) => {
                let key = self.selected_task().map(|t| t.key()).unwrap_or_default();
                self.toggle_task(self.service, &key);
            }
            KeyCode::Char('d') if pending == Some('d') => {
                let s = self.service;
                let (Some(api), Some(t)) = (self.tab(s).api.clone(), self.selected_task()) else { return true };
                let (key, list, id, title) = (t.key(), t.list.clone(), t.id.clone(), t.title.clone());
                self.tab_mut(s).state.remove(&key);
                self.show_tasks_in_cal();
                self.tree_dirty = true;
                self.spawn(async move {
                    Some(AppEvent::Todo(s, match api.delete(&list, &id).await {
                        Ok(()) => TodoMsg::Changed(format!("deleted \"{title}\""), false),
                        Err(e) => TodoMsg::Error(e),
                    }))
                });
            }
            KeyCode::Char('d') if !is_ctrl(&k) => self.pending = Some('d'),
            KeyCode::Char('r') => self.refresh(),
            _ => return false,
        }
        true
    }

    fn cal_command(&mut self, arg: &str) {
        let (sub, rest) = arg.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((arg, ""));
        match sub {
            "setup" | "login" => {
                let creds = if sub == "setup" {
                    match Creds::from_client_file(rest) {
                        Ok(c) => c,
                        Err(e) => {
                            self.status = format!("E: {e}");
                            return;
                        }
                    }
                } else {
                    // A previous login's own client, else the one built into this release.
                    match Creds::load().or_else(cal::builtin_client) {
                        Some(c) => c,
                        None => {
                            self.status = "run :cal setup <path to the client JSON> first".into();
                            return;
                        }
                    }
                };
                self.status = "finish the sign-in in your browser…".into();
                self.spawn(async move {
                    Some(AppEvent::Cal(match cal::login(creds, open_url).await {
                        Ok(c) => CalMsg::LoggedIn(c),
                        Err(e) => CalMsg::Error(format!("Google sign-in failed: {e}")),
                    }))
                });
            }
            "logout" => {
                Creds::remove();
                self.google = None;
                self.cal = CalState::default();
                self.show_tasks_in_cal();
                self.tree_dirty = true;
                self.status = "Google Calendar disconnected".into();
            }
            "refresh" | "sync" | "" => self.cal_sync(),
            _ => self.status = "usage: :cal setup <client.json> · :cal login · :cal logout · :cal refresh".into(),
        }
    }

    /// Calendar-mode keys; false when the key is a plain agenda motion.
    fn cal_key(&mut self, k: KeyEvent, pending: Option<char>) -> bool {
        let grid = self.cal_view != CalView::Agenda;
        match k.code {
            KeyCode::Char('n') | KeyCode::Char('i') | KeyCode::Char('a') => {
                if self.google.is_none() {
                    self.status = "set up Google Calendar first (see the calendar pane)".into();
                } else {
                    self.mode = Mode::Normal;
                    if grid && self.input.is_empty() {
                        // Start from the slot under the cursor; only the title is left to type.
                        let end = self.cal_cursor + chrono::Duration::hours(1);
                        let c = self.cal_cursor;
                        self.input.set(&format!("{} {}-{} ", c.format("%Y-%m-%d"), c.format("%H:%M"), end.format("%H:%M")));
                    }
                    self.enter_insert();
                }
            }
            KeyCode::Char('v') => {
                // Keep the selected event selected in the next view.
                let key = self.selected_event().map(|e| e.key());
                self.cal_view = self.cal_view.next();
                if self.cal_view == CalView::Agenda {
                    self.rebuild_tree();
                    if let Some(i) = key.and_then(|k| self.tree.iter().position(|t| t.id == k)) {
                        self.set_tree_sel(i);
                    }
                } else if let Some(k) = key {
                    self.cal_select(&k);
                }
                self.status = format!("calendar: {} view", self.cal_view.name());
            }
            KeyCode::Enter if self.selected_event().is_some() => self.cal_detail = true,
            KeyCode::Tab if grid => self.cal_cycle(),
            KeyCode::Char('w') if grid && !is_ctrl(&k) => self.cal_jump(true),
            KeyCode::Char('b') if grid && !is_ctrl(&k) => self.cal_jump(false),
            KeyCode::Char('t') if grid => self.cal_goto(chrono::Local::now()),
            // Tasks shown in the calendar: x ticks them off; they're edited in their own tab.
            KeyCode::Char('x') if pending != Some('g') && self.selected_event().is_some_and(|e| self.cal.is_task(e)) => {
                if let Some((key, s)) = self.selected_event().map(|e| (e.id.clone(), Self::task_service(&e.cal))) {
                    self.toggle_task(s, &key);
                }
            }
            KeyCode::Char('e' | 'c' | 'o' | 'd') if self.selected_event().is_some_and(|e| self.cal.is_task(e)) => {
                let s = self.selected_event().map(|e| Self::task_service(&e.cal)).unwrap_or(Service::Tasks);
                self.status = format!("a {} task: x ticks it off; edit it in its tab ({}gt)", s.name(), s.index() + 1);
            }
            KeyCode::Char('e') | KeyCode::Char('c') => {
                if let Some(e) = self.selected_event() {
                    let (key, text) = (e.key(), e.to_entry());
                    self.mode = Mode::Edit(key);
                    self.input.set(&text);
                    self.enter_insert();
                }
            }
            KeyCode::Char('d') if pending == Some('d') => {
                let (Some(g), Some(e)) = (self.google.clone(), self.selected_event()) else { return true };
                let (cal, id, title) = (e.cal.clone(), e.id.clone(), e.title.clone());
                self.spawn(async move {
                    Some(AppEvent::Cal(match g.delete(&cal, &id).await {
                        Ok(()) => CalMsg::Changed(format!("deleted \"{title}\"")),
                        Err(e) => CalMsg::Error(e),
                    }))
                });
            }
            KeyCode::Char('d') if !is_ctrl(&k) => self.pending = Some('d'),
            KeyCode::Char('x') if pending == Some('g') => self.open_selected_url(),
            KeyCode::Char('o') => self.open_selected_url(),
            KeyCode::Char('t') => self.jump_to_today(),
            KeyCode::Char('r') => self.refresh(),
            _ => return false,
        }
        true
    }

    fn current_messages(&self) -> &[Message] {
        self.current
            .as_ref()
            .and_then(|c| self.state.messages.get(c))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn selected_message(&self) -> Option<&Message> {
        self.current_messages().get(self.msg_sel?)
    }

    // ---- events -------------------------------------------------------

    pub fn on_event(&mut self, ev: AppEvent) {
        self.dirty = true;
        match ev {
            AppEvent::Gateway(GwEvent::Dispatch(t, d)) => self.on_dispatch(&t, d.get()),
            AppEvent::Gateway(GwEvent::Status(s)) => self.conn = s,
            AppEvent::Gateway(GwEvent::Fatal(s)) => {
                self.conn = "offline".into();
                self.status = s;
            }
            AppEvent::Messages { channel, older, result } => self.on_messages(channel, older, result),
            AppEvent::Sent(m) => {
                self.state.push_message(m);
                self.tree_dirty = true;
            }
            AppEvent::Error(e) => self.status = e,
            AppEvent::Muted(settings, text) => {
                self.state.apply_guild_settings(settings);
                self.status = text;
                self.tree_dirty = true;
            }
            AppEvent::Wa(s, ev) => self.on_wa(s, ev),
            AppEvent::Pic { key, pic } => self.pics.finish(key, pic),
            AppEvent::Cal(msg) => self.on_cal(msg),
            AppEvent::Todo(s, msg) => self.on_todo(s, msg),
            AppEvent::Update(tag) => {
                self.status = format!("discommand {tag} is out: run git pull, then the install script again");
                self.update = Some(tag);
            }
        }
    }

    fn on_wa(&mut self, s: Service, ev: WaEvent) {
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
                if new && !msg.from_me && !viewing && !chat.is_some_and(|c| c.is_group) {
                    bell();
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
            WaEvent::File { path } => match open_file(&path) {
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
            WaEvent::Error { msg } => self.status = format!("{}: {msg}", s.name()),
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

    fn on_dispatch(&mut self, t: &str, d: &str) {
        match t {
            "READY" => match serde_json::from_str::<Ready>(d) {
                Ok(r) => {
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
            bell();
        }
        if self.state.channels.get(&ch).is_some_and(|c| c.is_dm()) {
            self.state.sort_dms();
        }
        if viewing {
            self.clamp_sel();
        }
    }

    /// `:r`: reloads what the current mode shows.
    fn refresh(&mut self) {
        self.msg_sel = None;
        match self.service {
            Service::Discord => {
                let Some(ch) = self.current.clone() else {
                    self.status = "open a channel first".into();
                    return;
                };
                self.state.complete.remove(&ch);
                self.fetch(ch, None);
            }
            Service::WhatsApp | Service::Matrix => {
                // WhatsApp pushes messages; reconnecting is what fetches missed ones.
                let s = self.service;
                wa::send(self.cmd_of(s), json!({"c": "reconnect"}));
                if let Some(chat) = self.open_of(s).clone() {
                    if self.chats_mut(s).loading.insert(chat.clone()) {
                        self.chats_mut(s).version += 1;
                        wa::send(self.cmd_of(s), json!({"c": "history", "chat": chat}));
                    }
                }
            }
            Service::Calendar => {
                if self.google.is_none() {
                    self.status = "Google Calendar isn't connected (see the calendar pane)".into();
                    return;
                }
                self.cal_sync();
            }
            Service::Tasks | Service::Twos => {
                if !self.tasks_connected() {
                    self.status = format!("{} isn't connected (see the right pane)", self.service.name());
                    return;
                }
                self.todo_sync(self.service);
            }
        }
        self.status = REFRESHING.into();
    }

    fn done_refreshing(&mut self) {
        if self.status == REFRESHING {
            self.status.clear();
        }
    }

    fn on_messages(&mut self, channel: String, older: bool, result: Result<Vec<Message>, String>) {
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

    // ---- actions ------------------------------------------------------

    fn spawn<F>(&self, fut: F)
    where
        F: std::future::Future<Output = Option<AppEvent>> + Send + 'static,
    {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Some(ev) = fut.await {
                let _ = tx.send(ev);
            }
        });
    }

    fn fetch(&mut self, channel: String, before: Option<String>) {
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

    fn fetch_older(&mut self) {
        if matches!(self.service, Service::Calendar | Service::Tasks | Service::Twos) {
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
    fn read_all(&mut self, services: &[Service]) {
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

    fn mark_read(&mut self, channel: &str) {
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
    fn open_chat(&mut self, id: String) {
        self.reveal_in_tree(&id);
        match self.service {
            Service::Discord => self.open_channel(id),
            Service::WhatsApp | Service::Matrix => self.open_wa_chat(id),
            Service::Calendar => {
                self.cal_select(&id);
                self.focus = Focus::Tree;
            }
            Service::Tasks | Service::Twos => self.focus = Focus::Tree,
        }
    }

    /// Shift+Tab: forward, or backward with the right Shift key.
    fn cycle_service(&mut self) {
        self.switch_service(self.service.step(if right_shift_held() { -1 } else { 1 }));
    }

    fn switch_service(&mut self, to: Service) {
        if to == self.service {
            return;
        }
        if self.mode != Mode::Normal {
            self.mode = Mode::Normal;
            self.input.clear();
        }
        let old = self.service;
        self.service = to;
        self.saved_trees[old.index()] = std::mem::take(&mut self.tree_state);
        self.tree_state = std::mem::take(&mut self.saved_trees[self.service.index()]);
        self.saved_inputs[old.index()] = std::mem::take(&mut self.input);
        self.input = std::mem::take(&mut self.saved_inputs[self.service.index()]);
        self.msg_sel = None;
        self.msg_scroll = 0;
        self.pending = None;
        self.count = None;
        self.rebuild_tree();
        match self.service {
            Service::WhatsApp | Service::Matrix => {
                let s = self.service;
                if let Some(jid) = self.open_of(s).clone() {
                    if self.chats(s).chats.get(&jid).is_some_and(|c| c.unread > 0) {
                        wa::send(self.cmd_of(s), json!({"c": "seen", "chat": jid}));
                    }
                }
            }
            Service::Discord => {
                if let Some(ch) = self.current.clone() {
                    self.mark_read(&ch);
                }
            }
            Service::Calendar | Service::Tasks | Service::Twos => {}
        }
        if self.current_chat().is_none() && self.focus != Focus::Tree {
            self.focus = Focus::Tree;
        }
    }

    fn reveal_in_tree(&mut self, id: &str) {
        if self.service != Service::Discord {
            if self.service.is_tasks() {
                let list = self.tasks().find(id).map(|t| t.list.clone());
                let (more, folded) = match self.tasks().lists.iter().find(|l| l.id == id) {
                    Some(l) => (l.more, false),
                    None => (false, self.tasks().lists.iter().any(|l| Some(&l.id) == list.as_ref() && l.folded)),
                };
                let tab = self.tab_mut(self.service);
                tab.more_open |= more;
                // A task in a folded list: unfold it.
                if let Some(list) = list.filter(|_| folded) {
                    tab.toggled.insert(list);
                }
            }
            self.rebuild_tree();
            if let Some(i) = self.tree.iter().position(|t| t.id == id) {
                self.set_tree_sel(i);
            }
            return;
        }
        if let Some(c) = self.state.channels.get(id) {
            if let Some(g) = c.guild_id.clone() {
                self.expanded.insert(g);
            } else {
                self.dms_open = true;
            }
            if let Some(p) = c.parent_id.clone() {
                self.collapsed.remove(&p);
            }
        }
        self.rebuild_tree();
        if let Some(i) = self.tree.iter().position(|t| t.id == id && matches!(t.kind, ItemKind::Channel | ItemKind::Dm)) {
            self.set_tree_sel(i);
        }
    }

    fn enter_insert(&mut self) {
        if self.focus != Focus::Input && self.focus != Focus::Command {
            self.last_pane = self.focus;
        }
        self.focus = Focus::Input;
    }

    fn leave_insert(&mut self) {
        self.focus = if self.last_pane == Focus::Tree { Focus::Tree } else { Focus::Messages };
    }

    fn submit(&mut self) {
        let text = self.input.text();
        let content = text.trim_end().to_string();
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
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

    fn start_edit(&mut self, idx: usize) {
        let found = match self.service {
            Service::Discord => self
                .current_messages()
                .get(idx)
                .map(|m| (m.author.id == self.state.me.id, m.id.clone(), m.content.clone())),
            Service::WhatsApp | Service::Matrix => self
                .wa_messages()
                .get(idx)
                .map(|m| (m.from_me && m.kind == "text" && !m.deleted, m.id.clone(), m.text.clone())),
            Service::Calendar | Service::Tasks | Service::Twos => None,
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

    fn open_finder(&mut self) {
        let s = &self.state;
        let mut all = Vec::new();
        if self.service.is_wa_like() {
            let chats = self.chats(self.service);
            for jid in &chats.order {
                all.push((jid.clone(), chats.title(jid)));
            }
        }
        if self.service.is_tasks() {
            for t in &self.tasks().tasks {
                all.push((t.key(), format!("{} · {}", t.title, self.tasks().list_name(&t.list))));
            }
            for l in &self.tasks().lists {
                all.push((l.id.clone(), format!("≡ {}", l.name)));
            }
        }
        if self.service == Service::Calendar {
            for e in &self.cal.events {
                let when = if e.all_day { e.start.format("%a %d %b") } else { e.start.format("%a %d %b %H:%M") };
                all.push((e.key(), format!("{when} · {}", e.title)));
            }
        }
        for c in s.dms.iter().filter_map(|id| s.channels.get(id)).filter(|_| self.service == Service::Discord) {
            all.push((c.id.clone(), format!("@{}", s.dm_label(c))));
        }
        for g in s.guild_order.iter().filter_map(|id| s.guilds.get(id)).filter(|_| self.service == Service::Discord) {
            for (_, chans) in s.guild_layout(g) {
                for c in chans {
                    all.push((c.id.clone(), format!("#{} · {}", c.name.as_deref().unwrap_or("?"), g.name)));
                }
            }
        }
        let mut f = Finder { query: Input::default(), all, matches: Vec::new(), sel: 0 };
        f.refilter();
        self.finder = Some(f);
    }

    fn run_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        let (name, arg) = cmd.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((cmd, ""));
        match name {
            "" => {}
            "q" | "q!" | "qa" | "qa!" | "quit" | "wq" | "x" => self.quit = true,
            "h" | "help" => self.help = true,
            "logout" => {
                self.logout = true;
                self.quit = true;
            }
            "b" | "buffer" | "o" | "open" | "e" | "join" => {
                self.open_finder();
                if let Some(f) = self.finder.as_mut() {
                    f.query.set(arg);
                    f.refilter();
                    if !arg.is_empty() && !f.matches.is_empty() {
                        let id = f.all[f.matches[0]].0.clone();
                        self.finder = None;
                        self.open_chat(id);
                    }
                }
            }
            "wa" | "mx" | "matrix" | "cal" | "calendar" | "todo" | "tasks" | "twos" if !Self::command_service(name).built() => {
                self.status = format!("{} isn't part of this build", Self::command_service(name).name());
            }
            "wa" | "mx" | "matrix" => {
                let s = Self::command_service(name);
                match arg {
                    "receipts on" => wa::send(self.cmd_of(s), json!({"c": "receipts", "on": true})),
                    "receipts off" => wa::send(self.cmd_of(s), json!({"c": "receipts", "on": false})),
                    "receipts" => {
                        let on = self.chats(s).receipts;
                        self.status = format!("{} read receipts are {}", s.name(), if on { "on" } else { "off" })
                    }
                    "logout" => wa::send(self.cmd_of(s), json!({"c": "logout"})),
                    _ => self.status = format!("usage: :{name} receipts on|off · :{name} logout"),
                }
            }
            "cal" | "calendar" => self.cal_command(arg),
            "todo" | "tasks" => self.todo_command(arg),
            "twos" => self.twos_command(arg),
            "images" | "img" => {
                self.pics.enabled = match arg {
                    "on" => true,
                    "off" => false,
                    _ => !self.pics.enabled,
                };
                self.pics.version += 1;
                self.status = format!("images {}", if self.pics.enabled { "on" } else { "off" });
            }
            "r" | "refresh" => self.refresh(),
            "mute" => self.mute(true, arg),
            "unmute" => self.mute(false, arg),
            // The messaging service shown (else all of them), or `:read all`.
            "read" => {
                let all = [Service::Discord, Service::WhatsApp, Service::Matrix];
                if arg == "all" || !self.service.is_messaging() {
                    self.read_all(&all);
                } else {
                    self.read_all(&[self.service]);
                }
            }
            _ => self.status = format!("E492: Not an editor command: {cmd}"),
        }
    }

    /// `:mute [15m|1h|8h|24h]` / `:unmute`: the server, category, channel or
    /// DM selected in the sidebar (the open channel when the chat has focus).
    fn mute(&mut self, on: bool, arg: &str) {
        if self.service != Service::Discord {
            self.status = "muting is for Discord servers and channels".into();
            return;
        }
        let secs = match arg {
            "" => None,
            a => match a.strip_suffix(['m', 'h', 'd']).and_then(|n| n.parse::<i64>().ok()) {
                Some(n) if a.ends_with('m') => Some(n * 60),
                Some(n) if a.ends_with('h') => Some(n * 3600),
                Some(n) => Some(n * 86400),
                None => {
                    self.status = "usage: :mute [15m|1h|8h|24h|7d] · :unmute".into();
                    return;
                }
            },
        };
        let item = self.tree.get(self.tree_sel()).filter(|_| self.focus == Focus::Tree);
        let id = match item.map(|t| t.kind) {
            Some(ItemKind::Guild | ItemKind::Category | ItemKind::Channel | ItemKind::Dm) => item.map(|t| t.id.clone()),
            _ => self.current.clone(),
        };
        let Some(id) = id else {
            self.status = "select a server or channel first".into();
            return;
        };
        let end = secs.map(|s| (chrono::Utc::now() + chrono::TimeDelta::seconds(s)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        let config = json!({ "end_time": end, "selected_time_window": secs.unwrap_or(-1) });
        let (guild, name, body) = if let Some(g) = self.state.guilds.get(&id) {
            (id.clone(), g.name.clone(), json!({ "muted": on, "mute_config": config }))
        } else if let Some(c) = self.state.channels.get(&id) {
            let name = if c.is_dm() { self.state.dm_label(c) } else { format!("#{}", c.name.as_deref().unwrap_or("?")) };
            let guild = c.guild_id.clone().unwrap_or_else(|| "@me".into());
            (guild, name, json!({ "channel_overrides": { id.clone(): { "muted": on, "mute_config": config } } }))
        } else {
            return;
        };
        self.status = format!("{} {name}…", if on { "muting" } else { "unmuting" });
        let api = self.api.clone();
        self.spawn(async move {
            Some(match api.guild_settings(&guild, &body).await {
                Ok(s) => AppEvent::Muted(s, format!("{} {name}", if on { "muted" } else { "unmuted" })),
                Err(e) => AppEvent::Error(format!("Discord: {e}")),
            })
        });
    }

    fn command_service(name: &str) -> Service {
        match name {
            "wa" => Service::WhatsApp,
            "mx" | "matrix" => Service::Matrix,
            "cal" | "calendar" => Service::Calendar,
            "twos" => Service::Twos,
            _ => Service::Tasks,
        }
    }

    // ---- keys ---------------------------------------------------------

    pub fn on_term(&mut self, ev: Event, paste_burst: bool) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k, paste_burst),
            Event::Paste(s) => {
                if let Some(f) = self.finder.as_mut() {
                    f.query.insert_str(&s.replace('\n', " "));
                    f.refilter();
                } else if self.focus == Focus::Command {
                    self.cmdline.insert_str(&s.replace('\n', " "));
                } else {
                    self.enter_insert();
                    self.input.insert_str(&s);
                }
            }
            Event::Resize(..) => {}
            _ => return,
        }
        self.dirty = true;
    }

    fn on_key(&mut self, k: KeyEvent, paste_burst: bool) {
        if self.tree_dirty {
            self.rebuild_tree();
        }
        let ctrl = is_ctrl(&k);
        if ctrl && k.code == KeyCode::Char('c') {
            if self.focus == Focus::Input && !self.input.is_empty() {
                self.input.clear();
                self.mode = Mode::Normal;
            } else {
                self.status = "type :q to quit".into();
            }
            return;
        }
        if self.react_picker {
            self.react_picker = false;
            if let KeyCode::Char(c @ '1'..='5') = k.code {
                self.react(REACTIONS[c as usize - '1' as usize]);
            }
            return;
        }
        if self.help || self.cal_detail {
            self.help = false;
            self.cal_detail = false;
            return;
        }
        if self.finder.is_some() {
            self.finder_key(k);
            return;
        }
        match self.focus {
            Focus::Input => self.insert_key(k, paste_burst),
            Focus::Command => self.command_key(k),
            Focus::Tree | Focus::Messages => self.normal_key(k),
        }
    }

    fn finder_key(&mut self, k: KeyEvent) {
        let ctrl = is_ctrl(&k);
        let f = self.finder.as_mut().unwrap();
        match k.code {
            KeyCode::Esc => self.finder = None,
            KeyCode::Enter => {
                if let Some(&i) = f.matches.get(f.sel) {
                    let id = f.all[i].0.clone();
                    self.finder = None;
                    self.open_chat(id);
                }
            }
            KeyCode::Up => f.sel = f.sel.saturating_sub(1),
            KeyCode::Char('k') | KeyCode::Char('p') if ctrl => f.sel = f.sel.saturating_sub(1),
            KeyCode::Down => f.sel = (f.sel + 1).min(f.matches.len().saturating_sub(1)),
            KeyCode::Char('j') | KeyCode::Char('n') if ctrl => f.sel = (f.sel + 1).min(f.matches.len().saturating_sub(1)),
            _ => {
                if f.query.handle_key(&k) {
                    f.refilter();
                }
            }
        }
    }

    fn command_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => {
                self.cmdline.clear();
                self.focus = self.last_pane;
            }
            KeyCode::Enter => {
                let cmd = self.cmdline.text();
                self.cmdline.clear();
                self.focus = self.last_pane;
                self.run_command(&cmd);
            }
            KeyCode::Backspace if self.cmdline.is_empty() => self.focus = self.last_pane,
            _ => {
                self.cmdline.handle_key(&k);
            }
        }
    }

    fn insert_key(&mut self, k: KeyEvent, paste_burst: bool) {
        let mods = k.modifiers;
        match k.code {
            KeyCode::Enter if paste_burst || mods.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => {
                self.input.insert('\n')
            }
            KeyCode::Enter => self.submit(),
            KeyCode::BackTab => self.cycle_service(),
            KeyCode::Esc => {
                if matches!(self.mode, Mode::Edit(_)) {
                    self.input.clear();
                }
                self.mode = Mode::Normal;
                self.leave_insert();
            }
            KeyCode::Up if self.input.is_empty() && self.mode == Mode::Normal => {
                if let Some(idx) = self.last_own() {
                    self.start_edit(idx);
                }
            }
            _ => {
                self.input.handle_key(&k);
            }
        }
    }

    fn normal_key(&mut self, k: KeyEvent) {
        let ctrl = is_ctrl(&k);
        let pending = self.pending.take();

        if let KeyCode::Char(c @ '0'..='9') = k.code {
            if !ctrl && (c != '0' || self.count.is_some()) {
                let d = c as usize - '0' as usize;
                self.count = Some(self.count.unwrap_or(0).saturating_mul(10).saturating_add(d).min(9999));
                self.pending = pending;
                return;
            }
        }
        let count = self.count.take();
        let n = count.unwrap_or(1);

        // Services like vim tabs: gt next, gT previous, {n}gt the n-th.
        if pending == Some('g') {
            match k.code {
                KeyCode::Char('t') => {
                    let to = match count {
                        Some(i) => match Service::ALL.get(i.wrapping_sub(1)) {
                            Some(s) => *s,
                            None => {
                                self.status = format!("there are {} services", Service::COUNT);
                                return;
                            }
                        },
                        None => self.service.step(1),
                    };
                    self.switch_service(to);
                    return;
                }
                KeyCode::Char('T') => {
                    self.switch_service(self.service.step(-(n as isize)));
                    return;
                }
                _ => {}
            }
        }
        // Window motions: Ctrl+w h/l/w, Ctrl+h/Ctrl+l, Tab.
        if pending == Some('w') {
            match k.code {
                KeyCode::Char('h') | KeyCode::Left => self.focus = Focus::Tree,
                KeyCode::Char('l') | KeyCode::Right => self.focus = Focus::Messages,
                KeyCode::Char('w') | KeyCode::Char('p') => self.toggle_pane(),
                _ => {}
            }
            return;
        }
        // Global normal-mode keys (skipped while a multi-key command is pending).
        let global = if pending.is_some() { KeyCode::Null } else { k.code };
        match global {
            KeyCode::Char('w') if ctrl => {
                self.pending = Some('w');
                return;
            }
            KeyCode::Char('h') | KeyCode::Backspace if ctrl => {
                self.focus = Focus::Tree;
                return;
            }
            KeyCode::Char('l') if ctrl => {
                self.focus = Focus::Messages;
                return;
            }
            // In the calendar grid, Tab cycles events at the cursor instead.
            KeyCode::Tab if !(self.service == Service::Calendar && self.cal_view != CalView::Agenda) => {
                self.toggle_pane();
                return;
            }
            KeyCode::BackTab => {
                self.cycle_service();
                return;
            }
            // ö sits where ; is on US keyboards: the usual German vim mapping for ':'.
            KeyCode::Char(':' | 'ö' | 'Ö') => {
                self.last_pane = self.focus;
                self.focus = Focus::Command;
                return;
            }
            // '-' is the key in the US '/' position on QWERTZ; '#' is unshifted there.
            KeyCode::Char('/' | '-' | '#') | KeyCode::Char('k') if k.code != KeyCode::Char('k') || ctrl => {
                self.open_finder();
                return;
            }
            KeyCode::Char('?' | 'ß') => {
                self.help = true;
                return;
            }
            // Read all: every chat of the messaging service shown.
            KeyCode::Char('A') if self.service.is_messaging() => {
                self.read_all(&[self.service]);
                return;
            }
            KeyCode::Char('i') | KeyCode::Char('a') if !matches!(self.service, Service::Calendar | Service::Tasks | Service::Twos) => {
                if self.current_chat().is_some() {
                    self.enter_insert();
                } else {
                    self.status = "open a chat first".into();
                }
                return;
            }
            _ => {}
        }
        if self.service.is_tasks() {
            if self.todo_key(k, pending) {
            } else if self.in_page() {
                self.page_key(k, pending, n, count.is_some());
            } else {
                self.tree_key(k, pending, n, count.is_some());
            }
        } else if self.service == Service::Calendar {
            // One list to move through; the right pane only shows details.
            if self.cal_key(k, pending) {
            } else if self.cal_view == CalView::Agenda {
                self.tree_key(k, pending, n, count.is_some());
            } else {
                self.cal_grid_key(k, pending, n);
            }
        } else if self.focus == Focus::Tree {
            self.tree_key(k, pending, n, count.is_some());
        } else {
            self.messages_key(k, pending, n, count.is_some());
        }
    }

    fn toggle_pane(&mut self) {
        self.focus = if self.focus == Focus::Tree { Focus::Messages } else { Focus::Tree };
    }

    fn tree_key(&mut self, k: KeyEvent, pending: Option<char>, n: usize, counted: bool) {
        let ctrl = is_ctrl(&k);
        let sel = self.tree_sel();
        let half = (self.msg_height / 2).max(1);
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => self.set_tree_sel(sel + n),
            KeyCode::Char('k') | KeyCode::Up => self.set_tree_sel(sel.saturating_sub(n)),
            KeyCode::Char('d') if ctrl => self.set_tree_sel(sel + half),
            KeyCode::Char('u') if ctrl => self.set_tree_sel(sel.saturating_sub(half)),
            KeyCode::PageDown => self.set_tree_sel(sel + half * 2),
            KeyCode::PageUp => self.set_tree_sel(sel.saturating_sub(half * 2)),
            KeyCode::Char('g') if pending == Some('g') => self.set_tree_sel(if counted { n - 1 } else { 0 }),
            KeyCode::Char('g') => self.pending = Some('g'),
            KeyCode::Char('G') | KeyCode::End => self.set_tree_sel(if counted { n - 1 } else { usize::MAX }),
            KeyCode::Home => self.set_tree_sel(0),
            KeyCode::Char('z') => self.pending = Some('z'),
            KeyCode::Char('a') | KeyCode::Char('o') | KeyCode::Char('c') if pending == Some('z') => {
                let force = match k.code {
                    KeyCode::Char('o') => Some(true),
                    KeyCode::Char('c') => Some(false),
                    _ => None,
                };
                self.toggle(sel, force);
            }
            KeyCode::Char('M') if pending == Some('z') => {
                self.expanded.clear();
                self.dms_open = false;
                self.rebuild_tree();
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Char('o') | KeyCode::Right | KeyCode::Char(' ') => {
                let Some(item) = self.tree.get(sel) else { return };
                match item.kind {
                    ItemKind::Dm | ItemKind::Channel => {
                        let id = item.id.clone();
                        self.open_channel(id);
                    }
                    ItemKind::WaChat => {
                        let id = item.id.clone();
                        self.open_wa_chat(id);
                    }
                    _ => {
                        let force = matches!(k.code, KeyCode::Char('l') | KeyCode::Right).then_some(true);
                        self.toggle(sel, force);
                    }
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let Some(item) = self.tree.get(sel) else { return };
                if item.expanded && !matches!(item.kind, ItemKind::Dm | ItemKind::Channel | ItemKind::WaChat) {
                    self.toggle(sel, Some(false));
                } else if item.depth > 0 {
                    let depth = item.depth;
                    if let Some(p) = self.tree[..sel].iter().rposition(|t| t.depth < depth) {
                        self.set_tree_sel(p);
                    }
                }
            }
            _ => {}
        }
    }

    /// Expands/collapses a tree node (`force`: Some(true) open, Some(false) close).
    fn toggle(&mut self, idx: usize, force: Option<bool>) {
        let Some(item) = self.tree.get(idx) else { return };
        let open = force.unwrap_or(!item.expanded);
        let id = item.id.clone();
        match item.kind {
            ItemKind::DmHeader => self.dms_open = open,
            ItemKind::Guild => {
                if open {
                    self.expanded.insert(id);
                } else {
                    self.expanded.remove(&id);
                }
            }
            ItemKind::Category => {
                if open {
                    self.collapsed.remove(&id);
                } else {
                    self.collapsed.insert(id);
                }
            }
            ItemKind::TodoList if id == MORE_LISTS => self.tab_mut(self.service).more_open = open,
            ItemKind::TodoList => {
                let folded = self.tasks().lists.iter().any(|l| l.id == id && l.folded);
                let toggled = &mut self.tab_mut(self.service).toggled;
                if open == folded {
                    toggled.insert(id);
                } else {
                    toggled.remove(&id);
                }
            }
            _ => return,
        }
        self.rebuild_tree();
    }

    fn messages_key(&mut self, k: KeyEvent, pending: Option<char>, n: usize, counted: bool) {
        let ctrl = is_ctrl(&k);
        let len = self.current_len();
        let half = (self.msg_height / 4).max(1);
        let up = |app: &mut App, by: usize| {
            let target = match app.msg_sel {
                None if len > 0 => Some(len.saturating_sub(by)),
                None => None,
                Some(i) => {
                    if i < by {
                        app.fetch_older();
                    }
                    Some(i.saturating_sub(by))
                }
            };
            app.msg_sel = target;
        };
        let down = |app: &mut App, by: usize| {
            app.msg_sel = match app.msg_sel {
                Some(i) if i + by < len => Some(i + by),
                _ => None,
            };
        };
        match k.code {
            KeyCode::Char('k') | KeyCode::Up => up(self, n),
            KeyCode::Char('j') | KeyCode::Down => down(self, n),
            KeyCode::Char('u') if ctrl => up(self, half),
            KeyCode::Char('d') if ctrl => down(self, half),
            KeyCode::Char('b') if ctrl => up(self, half * 2),
            KeyCode::Char('f') if ctrl => down(self, half * 2),
            KeyCode::PageUp => up(self, half * 2),
            KeyCode::PageDown => down(self, half * 2),
            KeyCode::Char('g') if pending == Some('g') => {
                if len > 0 {
                    self.msg_sel = Some(if counted { (n - 1).min(len - 1) } else { 0 });
                }
                if !counted {
                    self.fetch_older();
                }
            }
            KeyCode::Char('g') => self.pending = Some('g'),
            KeyCode::Char('G') | KeyCode::End => self.msg_sel = None,
            KeyCode::Home => {
                if len > 0 {
                    self.msg_sel = Some(0);
                }
                self.fetch_older();
            }
            KeyCode::Esc => self.msg_sel = None,
            KeyCode::Char('h') | KeyCode::Left => self.focus = Focus::Tree,
            KeyCode::Enter => self.enter_insert(),
            KeyCode::Char('x') if pending == Some('g') => self.open_selected_url(),
            KeyCode::Char('o') => self.open_selected_url(),
            // '+' is unshifted on German keyboards, '=' on US ones.
            KeyCode::Char('+') | KeyCode::Char('=') => {
                if self.selected_id().is_some() {
                    self.react_picker = true;
                } else {
                    self.status = "select a message first (k)".into();
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                let Some(id) = self.selected_id() else {
                    self.status = "select a message first (k)".into();
                    return;
                };
                self.mode = Mode::Reply { id, ping: k.code == KeyCode::Char('r') };
                self.enter_insert();
            }
            KeyCode::Char('e') | KeyCode::Char('c') => {
                if let Some(i) = self.msg_sel {
                    self.start_edit(i);
                }
            }
            KeyCode::Char('d') if pending == Some('d') => self.delete_selected(),
            KeyCode::Char('d') => {
                if self.msg_sel.is_some() {
                    self.pending = Some('d');
                }
            }
            _ => {}
        }
    }

    /// Toggles our reaction with `emoji` on the selected message.
    fn react(&mut self, emoji: &str) {
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
            Service::Calendar | Service::Tasks | Service::Twos => {}
        }
    }

    fn delete_selected(&mut self) {
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
            Service::Calendar | Service::Tasks | Service::Twos => {}
        }
    }

    fn open_selected_url(&mut self) {
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
        // A picture opens in full quality; then files, then links.
        let url = crate::state::full_picture(m)
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

const REFRESHING: &str = "refreshing…";

/// The reactions offered by the picker (`+`, then 1–5).
pub const REACTIONS: [&str; 5] = ["❤️", "👍", "😂", "💀", "😅"];

/// Whether the right Shift key (and not the left one) is held. Terminals
/// send the same Shift+Tab for both, so on Windows the physical key state
/// is asked for; elsewhere it can't be told apart (use `gT` there).
#[cfg(windows)]
fn right_shift_held() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LSHIFT, VK_RSHIFT};
    // SAFETY: plain query of the keyboard state.
    let down = |vk: u16| unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 };
    down(VK_RSHIFT) && !down(VK_LSHIFT)
}

#[cfg(not(windows))]
fn right_shift_held() -> bool {
    false
}

/// Ctrl without Alt. Windows reports AltGr (needed for @ { } [ ] \ | ~ on
/// German and other European layouts) as Ctrl+Alt; that must not trigger
/// Ctrl shortcuts.
fn is_ctrl(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && !k.modifiers.contains(KeyModifiers::ALT)
}

async fn fetch_pic(http: &reqwest::Client, url: &str, key: String) -> Option<Pic> {
    let resp = http.get(url).send().await.ok()?;
    if !resp.status().is_success() || resp.content_length().is_some_and(|n| n > 4 << 20) {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    tokio::task::spawn_blocking(move || pics::decode_for(&key, &bytes)).await.ok().flatten()
}

fn bell() {
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x07");
    let _ = out.flush();
}

/// File types that are opened directly after a download. Anything else
/// (.exe, .bat, .lnk, .js, …) could run code, so it is only shown in the
/// file manager.
const SAFE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jfif", "jpe", "png", "gif", "webp", "bmp", "heic", "mp4", "mov", "webm", "mkv", "avi", "3gp", "mp3", "ogg",
    "opus", "oga", "m4a", "aac", "wav", "flac", "pdf", "txt", "csv",
];

/// Opens a downloaded file if its type is safe; otherwise reveals it.
/// Returns whether it was opened.
fn open_file(path: &str) -> std::io::Result<bool> {
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

#[cfg(test)]
mod tests {
    use super::Service;

    #[test]
    fn services_cycle_both_ways() {
        let all = Service::ALL;
        let (first, last) = (all[0], all[all.len() - 1]);
        assert_eq!(last.step(1), first);
        assert_eq!(first.step(-1), last);
        assert_eq!(first.step(all.len() as isize + 1), first.step(1));
        assert_eq!(first.step(-(all.len() as isize) - 1), last);
        for (i, s) in all.iter().enumerate() {
            assert_eq!(s.index(), i);
        }
    }
}
