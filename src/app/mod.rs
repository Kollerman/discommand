//! The application: all state, and what happens on each event and key.
//!
//! The types and `App` itself are here, with what the UI reads from it. Each
//! area of the app adds its part in a file of its own:
//!
//! - `chat`: Discord, WhatsApp and Matrix (events, sending, reading)
//! - `calendar`: the calendar tab
//! - `tasks`: the task tabs (Microsoft To Do, Twos, Moodle)
//! - `voice_messages`: playing and transcribing voice messages, the alert
//! - `tree`: the sidebar
//! - `keys`: what each key does
//! - `pdf_viewer`: the PDF viewer
//! - `authenticator`: the authenticator tab (`:totp`)
//! - `commands`: the `:` commands

mod calendar;
mod chat;
mod commands;
mod authenticator;
mod keys;
mod pdf_viewer;
mod tasks;
mod tree;
mod voice_messages;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use base64::Engine;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::text::Line;
use ratatui::widgets::ListState;
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Semaphore;

use crate::audio::{sound, voice};
use crate::chats::wa::{self, WaEvent, WaMsg, WaState};
use crate::discord::api::Api;
use crate::discord::gateway::GwEvent;
use crate::discord::model::*;
use crate::discord::state::State;
use crate::planner::cal::{self, CalState, Creds, Google, Synced};
use crate::planner::moodle::{self, Moodle};
use crate::planner::todo::{self, Backend, Task, TaskList, Todo, TodoState};
use crate::planner::twos::{self, Twos};
use crate::system::open::{open_file, open_url};
use crate::ui::pics::{self, Pic, PicRef, PicSrc, Pics};
use crate::ui::text::{find_url, Input};
use crate::ui::view::ViewMsg;

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
    /// A voice message was transcribed (its `voice::key`).
    Transcript(String, Result<String, String>),
    /// A voice message started playing (its `voice::key`), this long.
    Playing(String, Result<std::time::Duration, String>),
    /// From the PDF viewer (opening, text, pictures of pages).
    Pdf(pdf_viewer::PdfMsg),
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
    /// A file linked from a page was downloaded (its path), to be opened.
    File(Result<String, String>),
    /// An action was prepared and checked: ask before doing it.
    Confirm(Result<Box<Confirm>, String>),
    /// An action on a page finished (the page to read again, and what happened).
    Done(String, Result<String, String>),
}

/// A question before an action that changes something on a site (handing
/// in an assignment): what will happen, and the action on yes.
pub struct Confirm {
    pub title: String,
    pub lines: Vec<String>,
    /// What y does ("upload", "hand in").
    pub yes: String,
    action: ConfirmAction,
}

enum ConfirmAction {
    Upload(moodle::UploadPlan),
    Submit(moodle::SubmitPlan),
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
    Moodle,
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
        #[cfg(feature = "moodle")]
        Service::Moodle,
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
            Service::Moodle => "Moodle",
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
        matches!(self, Service::Tasks | Service::Twos | Service::Moodle)
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
const MOODLE_CAL: &str = "moodle";
/// The sidebar section holding the lists without open to-dos.
const MORE_LISTS: &str = "\u{1}more";
/// Sidebar folders of lists (`TaskList::group`) have ids with this prefix.
const GROUP: &str = "\u{1}group:";

/// A task tab: one task app's lists and tasks.
#[derive(Default)]
pub struct TaskTab {
    pub state: TodoState,
    api: Option<Backend>,
    /// Task lists folded or unfolded in the sidebar (the opposite of how they start).
    open: HashMap<String, bool>,
    /// "More lists" is unfolded.
    more_open: bool,
    positioned: bool,
    /// The list shown whole in the main pane (Twos), and its selected line.
    page: Option<String>,
    page_sel: Option<usize>,
    /// Pages opened from other pages (Enter on a sub-list), to go back to with h.
    back: Vec<(String, Option<usize>)>,
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
    pub moodle: TaskTab,

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
    /// A file to send with the next message: (service, chat, path).
    pub attachment: Option<(Service, String, std::path::PathBuf)>,
    /// A question waiting for y (anything else cancels).
    pub confirm: Option<Box<Confirm>>,
    /// Transcripts of voice messages.
    pub voice: voice::Transcripts,
    /// The voice message playing (its `voice::key`) and when it ends.
    playing: Option<(String, Instant)>,
    /// Whether the window is focused, once the terminal has said.
    focused: Option<bool>,
    /// When the alert last sounded.
    alerted: Option<Instant>,
    /// The last key press or paste: after `AWAY` without one, Discord shows
    /// the account as away.
    last_input: Instant,
    away: bool,
    /// The status chosen in Discord (away is only set over "online").
    discord_status: String,
    /// The PDF shown (over everything else), and how many were opened.
    pub pdf: Option<crate::pdf::Viewer>,
    pdf_id: u64,
    /// The screen must be cleared before the next draw (pictures don't go
    /// away by themselves).
    pub clear_screen: bool,
    /// The authenticator tab is shown (`:totp`).
    pub totp: bool,
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
            moodle: TaskTab::new(
                moodle::Creds::load().filter(|_| Service::Moodle.built()).map(|c| Backend::Moodle(Arc::new(Moodle::new(c)))),
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
                .user_agent(crate::discord::api::user_agent())
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
            attachment: None,
            confirm: None,
            voice: voice::Transcripts::load(),
            playing: None,
            focused: None,
            alerted: None,
            last_input: Instant::now(),
            away: false,
            discord_status: String::new(),
            pdf: None,
            pdf_id: 0,
            clear_screen: false,
            totp: false,
        }
    }

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
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle => None,
        }
    }

    pub fn chat_title(&self, id: &str) -> String {
        match self.service {
            Service::Discord => self.state.channel_title(id),
            Service::WhatsApp | Service::Matrix => self.chats(self.service).title(id),
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle => String::new(),
        }
    }

    pub fn author_of(&self, id: &str) -> Option<String> {
        match self.service {
            Service::Discord => self.current_messages().iter().find(|m| m.id == id).map(|m| m.author.name().to_string()),
            Service::WhatsApp | Service::Matrix => self.wa_messages().iter().find(|m| m.id == id).map(|m| m.sender_name.clone()),
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle => None,
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
            Service::Moodle => format!("md:{id}"),
        }
    }

    /// Changes whenever loaded messages of either service change.
    pub fn msg_version(&self) -> u64 {
        self.state.version + self.wa.version + self.mx.version + self.pics.version + self.voice.version
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
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle => false,
        }
    }

    pub fn chat_complete(&self, id: &str) -> bool {
        match self.service {
            Service::Discord => self.state.complete.contains(id),
            Service::WhatsApp | Service::Matrix => self.chats(self.service).complete.contains(id),
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle => false,
        }
    }

    pub fn view_messages(&self, id: &str) -> Option<Vec<ViewMsg>> {
        let mut msgs = match self.service {
            Service::Discord => self.state.view_messages(id),
            Service::WhatsApp | Service::Matrix => self.chats(self.service).view_messages(id),
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle => None,
        }?;
        for m in &mut msgs {
            m.transcript = self.voice.get(&voice::key(self.service.name(), &m.id)).map(String::from);
        }
        Some(msgs)
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
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle => 0,
        }
    }

    fn last_own(&self) -> Option<usize> {
        match self.service {
            Service::Discord => {
                let me = &self.state.me.id;
                self.current_messages().iter().rposition(|m| &m.author.id == me)
            }
            Service::WhatsApp | Service::Matrix => self.wa_messages().iter().rposition(|m| m.from_me && !m.deleted),
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle => None,
        }
    }

    fn selected_id(&self) -> Option<String> {
        match self.service {
            Service::Discord => self.selected_message().map(|m| m.id.clone()),
            Service::WhatsApp | Service::Matrix => self.wa_messages().get(self.msg_sel?).map(|m| m.id.clone()),
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle => None,
        }
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
        let away = self.can_go_away().then(|| self.last_input + AWAY);
        // The authenticator's countdown moves every second.
        let tick = self.totp.then(|| Instant::now() + std::time::Duration::from_secs(1));
        [cal, grid, away, tick].into_iter().flatten().min()
    }

    /// Timers: reminders, polling, and redraws when events start or end.
    pub fn on_tick(&mut self) {
        if self.can_go_away() && self.last_input.elapsed() >= AWAY {
            self.away = true;
            self.presence("idle");
        }
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
        }
        if self.cal.next_poll.is_some_and(|t| t <= std::time::Instant::now()) {
            self.cal_sync();
        }
        for s in [Service::Tasks, Service::Twos, Service::Moodle] {
            let tasks: Vec<String> = self.tab_mut(s).state.due_reminders(now).into_iter().map(|t| format!("☐ {}", t.title)).collect();
            if !tasks.is_empty() {
                self.status = format!("⏰ {}: {}", s.name(), tasks.join(" · "));
            }
            if self.tab(s).state.next_poll.is_some_and(|t| t <= std::time::Instant::now()) {
                self.todo_sync(s);
            }
        }
        self.tree_dirty = true;
        self.dirty = true;
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
            AppEvent::Transcript(key, result) => {
                self.voice.finish(&key, &result);
                self.status = match result {
                    Ok(_) => "voice message transcribed".into(),
                    Err(e) => format!("transcription failed: {e}"),
                };
            }
            AppEvent::Playing(key, result) => match result {
                Ok(length) => {
                    self.playing = Some((key, Instant::now() + length));
                    self.status = format!("playing the voice message ({}:{:02}; p stops it)", length.as_secs() / 60, length.as_secs() % 60);
                }
                Err(e) => self.status = format!("couldn't play it: {e}"),
            },
            AppEvent::Pdf(msg) => self.on_pdf(msg),
            AppEvent::Wa(s, ev) => self.on_wa(s, ev),
            AppEvent::Pic { key, pic } => self.pics.finish(key, pic),
            AppEvent::Cal(msg) => self.on_cal(msg),
            AppEvent::Todo(s, msg) => self.on_todo(s, msg),
            AppEvent::Update(tag) => {
                self.status = format!("discommand {tag} is out ({}): run git pull, then the install script again", crate::system::update::kind(&tag));
                self.update = Some(tag);
            }
        }
    }

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
}

/// Without input this long, Discord shows the account as away.
const AWAY: std::time::Duration = std::time::Duration::from_secs(5 * 60);

async fn fetch_pic(http: &reqwest::Client, url: &str, key: String) -> Option<Pic> {
    let resp = http.get(url).send().await.ok()?;
    if !resp.status().is_success() || resp.content_length().is_some_and(|n| n > 4 << 20) {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    tokio::task::spawn_blocking(move || pics::decode_for(&key, &bytes)).await.ok().flatten()
}

/// The reactions offered by the picker (`+`, then 1–5).
pub const REACTIONS: [&str; 5] = ["❤️", "👍", "😂", "💀", "😅"];

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
