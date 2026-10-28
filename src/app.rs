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
    Wa(WaEvent),
    Pic { key: String, pic: Option<Pic> },
    Cal(CalMsg),
    /// From the task tab of this service.
    Todo(Service, TodoMsg),
}

pub enum TodoMsg {
    Synced(Vec<TaskList>, Vec<Task>),
    Error(String),
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
            Service::Calendar => "Google Calendar",
            Service::Tasks => "Microsoft To Do",
            Service::Twos => "Twos",
        }
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

/// A task tab: one task app's lists and tasks.
#[derive(Default)]
pub struct TaskTab {
    pub state: TodoState,
    api: Option<Backend>,
    /// Task lists folded in the sidebar.
    collapsed: HashSet<String>,
    positioned: bool,
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
}

impl App {
    pub fn new(api: Api, tx: UnboundedSender<AppEvent>, gw: UnboundedSender<String>, wa_cmd: UnboundedSender<String>) -> Self {
        Self {
            state: State::default(),
            api,
            tx,
            gw,
            service: Service::ALL[0],
            wa: WaState::default(),
            wa_cmd,
            wa_current: None,
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
        }
    }

    // ---- service-neutral accessors (used by the UI) ---------------------

    pub fn current_chat(&self) -> Option<&str> {
        match self.service {
            Service::Discord => self.current.as_deref(),
            Service::WhatsApp => self.wa_current.as_deref(),
            Service::Calendar | Service::Tasks | Service::Twos => None,
        }
    }

    pub fn chat_title(&self, id: &str) -> String {
        match self.service {
            Service::Discord => self.state.channel_title(id),
            Service::WhatsApp => self.wa.title(id),
            Service::Calendar | Service::Tasks | Service::Twos => String::new(),
        }
    }

    pub fn author_of(&self, id: &str) -> Option<String> {
        match self.service {
            Service::Discord => self.current_messages().iter().find(|m| m.id == id).map(|m| m.author.name().to_string()),
            Service::WhatsApp => self.wa_messages().iter().find(|m| m.id == id).map(|m| m.sender_name.clone()),
            Service::Calendar | Service::Tasks | Service::Twos => None,
        }
    }

    pub fn cache_key(&self, id: &str) -> String {
        match self.service {
            Service::Discord => format!("d:{id}"),
            Service::WhatsApp => format!("w:{id}"),
            Service::Calendar => format!("c:{id}"),
            Service::Tasks => format!("t:{id}"),
            Service::Twos => format!("2:{id}"),
        }
    }

    /// Changes whenever loaded messages of either service change.
    pub fn msg_version(&self) -> u64 {
        self.state.version + self.wa.version + self.pics.version
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
            }
        }
    }

    pub fn chat_loading(&self, id: &str) -> bool {
        match self.service {
            Service::Discord => self.loading.contains(id),
            Service::WhatsApp => self.wa.loading.contains(id),
            Service::Calendar | Service::Tasks | Service::Twos => false,
        }
    }

    pub fn chat_complete(&self, id: &str) -> bool {
        match self.service {
            Service::Discord => self.state.complete.contains(id),
            Service::WhatsApp => self.wa.complete.contains(id),
            Service::Calendar | Service::Tasks | Service::Twos => false,
        }
    }

    pub fn view_messages(&self, id: &str) -> Option<Vec<ViewMsg>> {
        match self.service {
            Service::Discord => self.state.view_messages(id),
            Service::WhatsApp => self.wa.view_messages(id),
            Service::Calendar | Service::Tasks | Service::Twos => None,
        }
    }

    fn wa_messages(&self) -> &[WaMsg] {
        self.wa_current
            .as_ref()
            .and_then(|c| self.wa.messages.get(c))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn current_len(&self) -> usize {
        match self.service {
            Service::Discord => self.current_messages().len(),
            Service::WhatsApp => self.wa_messages().len(),
            Service::Calendar | Service::Tasks | Service::Twos => 0,
        }
    }

    fn last_own(&self) -> Option<usize> {
        match self.service {
            Service::Discord => {
                let me = &self.state.me.id;
                self.current_messages().iter().rposition(|m| &m.author.id == me)
            }
            Service::WhatsApp => self.wa_messages().iter().rposition(|m| m.from_me && !m.deleted),
            Service::Calendar | Service::Tasks | Service::Twos => None,
        }
    }

    fn selected_id(&self) -> Option<String> {
        match self.service {
            Service::Discord => self.selected_message().map(|m| m.id.clone()),
            Service::WhatsApp => self.wa_messages().get(self.msg_sel?).map(|m| m.id.clone()),
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
            Service::WhatsApp => return self.rebuild_wa_tree(),
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
            dm_unread |= s.is_unread(c);
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
                    unread: s.is_unread(c),
                    mentions: s.mention_count(&c.id),
                    expanded: false,
                    group: false,
                    color: None,
                    dim: false,
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
                dim: false,
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
                            unread: shut && chans.iter().any(|c| s.is_unread(c)),
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
                        unread: s.is_unread(c),
                        mentions: s.mention_count(&c.id),
                        expanded: false,
                        group: false,
                        color: None,
                        dim: false,
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
        self.tree = self
            .wa
            .order
            .iter()
            .filter_map(|jid| self.wa.chats.get(jid))
            .map(|c| TreeItem {
                kind: ItemKind::WaChat,
                id: c.jid.clone(),
                depth: 0,
                label: self.wa.label(&c.jid),
                unread: c.unread > 0,
                mentions: c.unread,
                expanded: false,
                group: c.is_group,
                color: None,
                dim: false,
            })
            .collect();
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
                Ok((lists, tasks)) => TodoMsg::Synced(lists, tasks),
                Err(e) => TodoMsg::Error(e),
            }))
        });
    }

    fn on_todo(&mut self, s: Service, msg: TodoMsg) {
        match msg {
            TodoMsg::Synced(lists, tasks) => {
                let st = &mut self.tab_mut(s).state;
                st.syncing = false;
                st.next_poll = Some(std::time::Instant::now() + todo::POLL);
                st.apply(lists, tasks);
                self.done_refreshing();
            }
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
        for l in &tab.state.lists {
            let tasks: Vec<&Task> = tab.state.tasks.iter().filter(|t| t.list == l.id).collect();
            let open = tasks.iter().filter(|t| !t.done).count();
            let shut = tab.collapsed.contains(&l.id);
            items.push(TreeItem {
                kind: ItemKind::TodoList,
                id: l.id.clone(),
                depth: 0,
                label: if open > 0 { format!("{} ({open})", l.name) } else { l.name.clone() },
                unread: false,
                mentions: 0,
                expanded: !shut,
                group: false,
                color: None,
                dim: false,
            });
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
                items.push(TreeItem {
                    kind: ItemKind::TodoTask,
                    id: t.key(),
                    depth: 1,
                    label: match due {
                        Some(d) => format!("{}  · {d}", t.title),
                        None => t.title.clone(),
                    },
                    unread: t.important && !t.done,
                    mentions: 0,
                    expanded: false,
                    group: false,
                    color,
                    dim: t.done,
                });
            }
        }
        self.tree = items;
        let positioned = self.tab(self.service).positioned;
        match sel_id.and_then(|id| self.tree.iter().position(|t| t.id == id)) {
            Some(i) if positioned => self.tree_state.select(Some(i)),
            _ if !self.tree.is_empty() && !positioned => {
                self.tab_mut(self.service).positioned = true;
                let first = self.tree.iter().position(|t| t.kind == ItemKind::TodoTask).unwrap_or(0);
                self.tree_state.select(Some(first));
            }
            _ => {}
        }
        let sel = self.tree_sel();
        self.set_tree_sel(sel);
    }

    pub fn selected_task(&self) -> Option<&Task> {
        let item = self.tree.get(self.tree_sel()).filter(|t| t.kind == ItemKind::TodoTask)?;
        self.tasks().find(&item.id)
    }

    /// The list new tasks go to: the selected one, else the default list.
    fn target_list(&self) -> Option<String> {
        let item = self.tree.get(self.tree_sel());
        match item.map(|t| t.kind) {
            Some(ItemKind::TodoList) => item.map(|t| t.id.clone()),
            Some(ItemKind::TodoTask) => self.selected_task().map(|t| t.list.clone()),
            _ => None,
        }
        .or_else(|| self.tasks().default_list().map(String::from))
    }

    /// Ticks a task off (or back on). It stays visible, struck through, until
    /// the next sync, so a mistaken `x` can be undone with another `x`.
    fn toggle_task(&mut self, s: Service, key: &str) {
        let tab = self.tab_mut(s);
        let Some(api) = tab.api.clone() else { return };
        let Some(t) = tab.state.find_mut(key) else { return };
        t.done = !t.done;
        let (list, id, done, title) = (t.list.clone(), t.id.clone(), t.done, t.title.clone());
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
            KeyCode::Char('x') | KeyCode::Char(' ') if self.selected_task().is_some() => {
                let key = self.selected_task().map(|t| t.key()).unwrap_or_default();
                self.toggle_task(self.service, &key);
            }
            KeyCode::Char('d') if pending == Some('d') => {
                let s = self.service;
                let (Some(api), Some(t)) = (self.tab(s).api.clone(), self.selected_task()) else { return true };
                let (key, list, id, title) = (t.key(), t.list.clone(), t.id.clone(), t.title.clone());
                self.tab_mut(s).state.tasks.retain(|t| t.key() != key);
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
            AppEvent::Wa(ev) => self.on_wa(ev),
            AppEvent::Pic { key, pic } => self.pics.finish(key, pic),
            AppEvent::Cal(msg) => self.on_cal(msg),
            AppEvent::Todo(s, msg) => self.on_todo(s, msg),
        }
    }

    fn on_wa(&mut self, ev: WaEvent) {
        match ev {
            WaEvent::Linked { me, name, receipts } => {
                self.wa.me = me;
                self.wa.name = name;
                self.wa.receipts = receipts;
                if self.wa.conn.is_empty() {
                    self.wa.conn = "connecting…".into();
                }
            }
            WaEvent::Chats { chats } => {
                self.wa.set_chats(chats);
                self.tree_dirty = true;
            }
            WaEvent::Chat { chat } => {
                let viewing = self.service == Service::WhatsApp && self.wa_current.as_deref() == Some(chat.jid.as_str());
                if viewing && chat.unread > 0 {
                    wa::send(&self.wa_cmd, json!({"c": "seen", "chat": chat.jid}));
                }
                self.wa.upsert_chat(chat);
                self.tree_dirty = true;
            }
            WaEvent::Msg { msg } => {
                let viewing = self.service == Service::WhatsApp && self.wa_current.as_deref() == Some(msg.chat.as_str());
                let chat = self.wa.chats.get(&msg.chat);
                // Edits and reactions also arrive as "msg"; only new incoming DMs ring.
                let new = chat.map_or(true, |c| msg.ts >= c.last_ts) && !msg.edited && !msg.deleted;
                if new && !msg.from_me && !viewing && !chat.is_some_and(|c| c.is_group) {
                    bell();
                }
                self.wa.upsert_msg(msg);
                if viewing {
                    self.clamp_sel();
                }
            }
            WaEvent::History { chat, older, msgs, complete } => {
                let viewing = self.service == Service::WhatsApp && self.wa_current.as_deref() == Some(chat.as_str());
                if !older && viewing {
                    self.done_refreshing();
                }
                let added = self.wa.history(chat, older, msgs, complete);
                if viewing {
                    if let Some(sel) = self.msg_sel.as_mut() {
                        *sel += added;
                    }
                }
            }
            WaEvent::Status { status } => self.wa.conn = status,
            WaEvent::Receipts { on } => {
                self.wa.receipts = on;
                self.status = format!("WhatsApp read receipts {}", if on { "on" } else { "off" });
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
            WaEvent::Error { msg } => self.status = format!("WhatsApp: {msg}"),
            WaEvent::Names => {
                if let Some(chat) = self.wa.forget_messages(self.wa_current.as_deref()) {
                    wa::send(&self.wa_cmd, json!({"c": "history", "chat": chat}));
                }
                self.clamp_sel();
                self.tree_dirty = true;
            }
            WaEvent::Fatal { msg } | WaEvent::LoggedOut { msg } => {
                self.wa.conn = "offline".into();
                self.status = format!("WhatsApp: {msg} (restart discommand to link again)");
            }
            WaEvent::Exited => {
                self.wa.conn = "offline".into();
                if self.status.is_empty() {
                    self.status = "WhatsApp helper stopped".into();
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
            Service::WhatsApp => {
                // WhatsApp pushes messages; reconnecting is what fetches missed ones.
                wa::send(&self.wa_cmd, json!({"c": "reconnect"}));
                if let Some(chat) = self.wa_current.clone() {
                    if self.wa.loading.insert(chat.clone()) {
                        self.wa.version += 1;
                        wa::send(&self.wa_cmd, json!({"c": "history", "chat": chat}));
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
        if self.service == Service::WhatsApp {
            let Some(chat) = self.wa_current.clone() else { return };
            if self.wa.complete.contains(&chat) || self.wa.loading.contains(&chat) {
                return;
            }
            let Some(first) = self.wa_messages().first() else { return };
            let cmd = json!({"c": "history", "chat": chat, "before_ts": first.ts, "before_id": first.id});
            self.wa.loading.insert(chat);
            self.wa.version += 1;
            wa::send(&self.wa_cmd, cmd);
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

    pub fn open_wa_chat(&mut self, jid: String) {
        if self.wa_current.as_deref() == Some(jid.as_str()) {
            self.enter_insert();
            return;
        }
        self.msg_sel = None;
        self.msg_scroll = 0;
        if self.mode != Mode::Normal {
            self.mode = Mode::Normal;
            self.input.clear();
        }
        if !self.wa.messages.contains_key(&jid) && self.wa.loading.insert(jid.clone()) {
            self.wa.version += 1;
            wa::send(&self.wa_cmd, json!({"c": "history", "chat": jid}));
        }
        if self.wa.chats.get(&jid).is_some_and(|c| c.unread > 0) {
            wa::send(&self.wa_cmd, json!({"c": "seen", "chat": jid}));
        }
        self.wa_current = Some(jid);
        self.tree_dirty = true;
        self.enter_insert();
    }

    /// Opens a chat of the active service and selects it in the sidebar.
    fn open_chat(&mut self, id: String) {
        self.reveal_in_tree(&id);
        match self.service {
            Service::Discord => self.open_channel(id),
            Service::WhatsApp => self.open_wa_chat(id),
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
            Service::WhatsApp => {
                if let Some(jid) = self.wa_current.clone() {
                    if self.wa.chats.get(&jid).is_some_and(|c| c.unread > 0) {
                        wa::send(&self.wa_cmd, json!({"c": "seen", "chat": jid}));
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
        if self.service == Service::WhatsApp {
            let Some(chat) = self.wa_current.clone() else {
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
            wa::send(&self.wa_cmd, cmd);
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
            Service::WhatsApp => self
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
        if self.service == Service::WhatsApp {
            for jid in &self.wa.order {
                all.push((jid.clone(), self.wa.title(jid)));
            }
        }
        if self.service.is_tasks() {
            for t in &self.tasks().tasks {
                all.push((t.key(), format!("{} · {}", t.title, self.tasks().list_name(&t.list))));
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
            "wa" | "cal" | "calendar" | "todo" | "tasks" | "twos" if !Self::command_service(name).built() => {
                self.status = format!("{} isn't part of this build", Self::command_service(name).name());
            }
            "wa" => match arg {
                "receipts on" => wa::send(&self.wa_cmd, json!({"c": "receipts", "on": true})),
                "receipts off" => wa::send(&self.wa_cmd, json!({"c": "receipts", "on": false})),
                "receipts" => {
                    self.status = format!("WhatsApp read receipts are {}", if self.wa.receipts { "on" } else { "off" })
                }
                "logout" => wa::send(&self.wa_cmd, json!({"c": "logout"})),
                _ => self.status = "usage: :wa receipts on|off · :wa logout".into(),
            },
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
            "read" => {
                // Mark every channel read locally (no network traffic).
                for (id, c) in &self.state.channels {
                    self.state.read.insert(id.clone(), c.last_id());
                }
                self.state.mentions.clear();
                self.tree_dirty = true;
            }
            _ => self.status = format!("E492: Not an editor command: {cmd}"),
        }
    }

    fn command_service(name: &str) -> Service {
        match name {
            "wa" => Service::WhatsApp,
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
            if !self.todo_key(k, pending) {
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
            ItemKind::TodoList => {
                if open {
                    self.tab_mut(self.service).collapsed.remove(&id);
                } else {
                    self.tab_mut(self.service).collapsed.insert(id);
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
            Service::WhatsApp => {
                let Some(m) = self.msg_sel.and_then(|i| self.wa_messages().get(i)) else { return };
                if m.deleted {
                    return;
                }
                let cmd = json!({"c": "react", "chat": m.chat, "id": m.id, "text": emoji});
                wa::send(&self.wa_cmd, cmd);
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
            Service::WhatsApp => {
                let Some(m) = self.msg_sel.and_then(|i| self.wa_messages().get(i)) else { return };
                let cmd = json!({"c": "delete", "chat": m.chat, "id": m.id});
                wa::send(&self.wa_cmd, cmd);
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
        if self.service == Service::WhatsApp {
            let Some(m) = self.msg_sel.and_then(|i| self.wa_messages().get(i)) else {
                self.status = "select a message first (k)".into();
                return;
            };
            if m.media && !m.deleted {
                let cmd = json!({"c": "download", "chat": m.chat, "id": m.id});
                wa::send(&self.wa_cmd, cmd);
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
        let url = m
            .attachments
            .first()
            .map(|a| a.url.clone())
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
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "heic", "mp4", "mov", "webm", "mkv", "avi", "3gp", "mp3", "ogg",
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
fn open_url(url: &str) -> std::io::Result<()> {
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
