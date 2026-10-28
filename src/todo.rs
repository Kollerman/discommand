//! Task lists: the model shared by the task tabs, the backends behind them
//! (see `Backend`), and Microsoft To Do via Microsoft Graph: device-code
//! login, task sync, create/edit/complete/delete and reminders.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cal::{self, local_midnight};
use crate::secret;
use crate::twos::Twos;

const AUTH: &str = "https://login.microsoftonline.com/common/oauth2/v2.0";
const GRAPH: &str = "https://graph.microsoft.com/v1.0";
const SCOPE: &str = "Tasks.ReadWrite offline_access";
const SECRET_NAME: &str = "microsoft";
/// How often tasks are re-fetched while the app runs.
pub const POLL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Serialize, Deserialize)]
pub struct Creds {
    pub client_id: String,
    #[serde(default)]
    pub refresh_token: String,
}

impl Creds {
    pub fn load() -> Option<Creds> {
        serde_json::from_slice(&secret::load_secret(SECRET_NAME)?).ok()
    }

    pub fn save(&self) -> Result<(), String> {
        let data = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        secret::save_secret(SECRET_NAME, &data).map(|_| ()).map_err(|e| e.to_string())
    }

    pub fn remove() {
        secret::remove_secret(SECRET_NAME);
    }
}

/// The app id a release build ships with (set when building), so users only
/// sign in. Public client: there is no secret.
pub fn builtin_client_id() -> Option<String> {
    cal::build_var(std::env::var("DISCOMMAND_MS_CLIENT_ID").ok(), option_env!("DISCOMMAND_MS_CLIENT_ID"))
}

// ---- login (device code flow) -------------------------------------------------

pub struct DeviceCode {
    pub uri: String,
    pub code: String,
    device_code: String,
    interval: u64,
    expires: Instant,
}

/// Starts the login: the user opens `uri` and enters `code`.
pub async fn device_code(client_id: &str) -> Result<DeviceCode, String> {
    let v: Value = reqwest::Client::new()
        .post(format!("{AUTH}/devicecode"))
        .form(&[("client_id", client_id), ("scope", SCOPE)])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let field = |k: &str| v[k].as_str().map(String::from);
    match (field("user_code"), field("device_code")) {
        (Some(code), Some(device_code)) => Ok(DeviceCode {
            uri: field("verification_uri").unwrap_or_else(|| "https://microsoft.com/devicelogin".into()),
            code,
            device_code,
            interval: v["interval"].as_u64().unwrap_or(5),
            expires: Instant::now() + Duration::from_secs(v["expires_in"].as_u64().unwrap_or(900)),
        }),
        _ => Err(format!(
            "Microsoft refused the login: {}",
            v["error_description"].as_str().unwrap_or("unknown error (is \"Allow public client flows\" on?)")
        )),
    }
}

/// Waits until the user finished the login in the browser.
pub async fn finish_login(client_id: String, dc: DeviceCode) -> Result<Creds, String> {
    let http = reqwest::Client::new();
    let mut interval = dc.interval;
    while Instant::now() < dc.expires {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let v: Value = http
            .post(format!("{AUTH}/token"))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", client_id.as_str()),
                ("device_code", dc.device_code.as_str()),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        if let Some(rt) = v["refresh_token"].as_str() {
            return Ok(Creds { client_id, refresh_token: rt.to_string() });
        }
        match v["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("authorization_declined") => return Err("the sign-in was declined".into()),
            Some("expired_token") => break,
            _ => return Err(v["error_description"].as_str().unwrap_or("sign-in failed").to_string()),
        }
    }
    Err("the code expired; run :todo login again".into())
}

// ---- model ------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct TaskList {
    pub id: String,
    pub name: String,
    pub default: bool,
    /// Order among the other lists (the default list comes first anyway).
    pub pos: u32,
}

/// A task, or (Twos) any line of a list: notes, bullets and headings too.
#[derive(Clone, Debug, Default)]
pub struct Task {
    pub id: String,
    pub list: String,
    pub title: String,
    pub note: String,
    pub due: Option<NaiveDate>,
    pub reminder: Option<DateTime<Local>>,
    pub done: bool,
    pub important: bool,
    pub created: Option<DateTime<Local>>,
    /// A to-do with a checkbox (false for notes, bullets and headings).
    pub check: bool,
    /// Twos line style: what's drawn before a line without a checkbox
    /// ("• ", "– ", "1. "), a heading, and the indent.
    pub marker: String,
    pub heading: bool,
    pub depth: u8,
    /// Place in its list as the app shows it (0: sorted by due date).
    pub pos: u32,
    /// Twos: the earlier day an unfinished to-do rolled over from.
    pub from: Option<NaiveDate>,
}

impl Task {
    pub fn key(&self) -> String {
        format!("{}\n{}", self.list, self.id)
    }

    /// One-line text for editing, in the syntax `cal::parse` understands.
    pub fn to_entry(&self) -> String {
        match (self.due, self.reminder) {
            (Some(d), Some(r)) => format!("{} {} {}", d.format("%Y-%m-%d"), r.format("%H:%M"), self.title),
            (Some(d), None) => format!("{} {}", d.format("%Y-%m-%d"), self.title),
            (None, Some(r)) => format!("{} {} {}", r.format("%Y-%m-%d"), r.format("%H:%M"), self.title),
            (None, None) => self.title.clone(),
        }
    }

    pub fn overdue(&self, today: NaiveDate) -> bool {
        self.check && !self.done && self.due.is_some_and(|d| d < today)
    }
}

/// The fields of a task typed as one line: "morgen 14:00 Arzt anrufen"
/// (due tomorrow, reminder at 14:00), "28.10. Steuer", or just a title.
pub struct Entry {
    pub title: String,
    pub due: Option<NaiveDate>,
    pub reminder: Option<DateTime<Local>>,
}

pub fn parse(text: &str, now: DateTime<Local>) -> Entry {
    match cal::parse(text, now) {
        Some(e) => {
            let title = if e.location.is_empty() { e.title } else { format!("{} @ {}", e.title, e.location) };
            let reminder = e.time.map(|(t, _)| cal::to_local(e.date.and_time(t)));
            Entry { title, due: Some(e.date), reminder }
        }
        None => Entry { title: text.trim().to_string(), due: None, reminder: None },
    }
}

impl Entry {
    fn body(&self) -> Value {
        let utc = |t: DateTime<Local>| json!({ "dateTime": t.with_timezone(&Utc).format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC" });
        json!({
            "title": self.title,
            // To Do stores a due *date* as local midnight.
            "dueDateTime": self.due.map(|d| utc(local_midnight(d))),
            "reminderDateTime": self.reminder.map(utc),
            "isReminderOn": self.reminder.is_some(),
        })
    }
}

#[derive(Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    value: Vec<T>,
    #[serde(rename = "@odata.nextLink")]
    next: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GList {
    id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    wellknown_list_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GTask {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    importance: String,
    body: Option<GBody>,
    due_date_time: Option<GDateTime>,
    reminder_date_time: Option<GDateTime>,
    #[serde(default)]
    is_reminder_on: bool,
    created_date_time: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GBody {
    #[serde(default)]
    content: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GDateTime {
    date_time: String,
}

/// Graph times (requested in UTC) to local time.
fn from_graph(t: &GDateTime) -> Option<DateTime<Local>> {
    let n = NaiveDateTime::parse_from_str(&t.date_time, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    Some(Utc.from_utc_datetime(&n).with_timezone(&Local))
}

// ---- API client ------------------------------------------------------------

pub struct Todo {
    http: reqwest::Client,
    creds: tokio::sync::Mutex<Creds>,
    token: tokio::sync::Mutex<Option<(String, Instant)>>,
}

impl Todo {
    pub fn new(creds: Creds) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default();
        Self { http, creds: tokio::sync::Mutex::new(creds), token: tokio::sync::Mutex::new(None) }
    }

    async fn access_token(&self) -> Result<String, String> {
        let mut cached = self.token.lock().await;
        if let Some((t, until)) = cached.as_ref() {
            if Instant::now() < *until {
                return Ok(t.clone());
            }
        }
        let mut creds = self.creds.lock().await;
        let v: Value = self
            .http
            .post(format!("{AUTH}/token"))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", creds.client_id.as_str()),
                ("refresh_token", creds.refresh_token.as_str()),
                ("scope", SCOPE),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let Some(token) = v["access_token"].as_str() else {
            return Err(match v["error"].as_str() {
                Some("invalid_grant") => "Microsoft sign-in expired; run :todo login".into(),
                _ => format!("Microsoft token refresh failed: {}", v["error_description"].as_str().unwrap_or("unknown error")),
            });
        };
        // Microsoft rotates refresh tokens: keep the new one.
        if let Some(rt) = v["refresh_token"].as_str().filter(|rt| *rt != creds.refresh_token) {
            creds.refresh_token = rt.to_string();
            let _ = creds.save();
        }
        let secs = v["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60);
        *cached = Some((token.to_string(), Instant::now() + Duration::from_secs(secs)));
        Ok(token.to_string())
    }

    async fn call(&self, method: Method, url: Url, body: Option<Value>) -> Result<Value, String> {
        for attempt in 0..4u32 {
            let token = self.access_token().await?;
            let mut req = self.http.request(method.clone(), url.clone()).bearer_auth(token).header("Prefer", "outlook.timezone=\"UTC\"");
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            let wait = crate::cal::retry_after(&resp);
            let text = resp.text().await.map_err(|e| e.to_string())?;
            // Throttled: wait as long as the server asks (or back off) and retry.
            if matches!(status.as_u16(), 429 | 503) && attempt < 3 {
                let secs = wait.unwrap_or(2u64 << attempt).min(60);
                tokio::time::sleep(Duration::from_secs(secs)).await;
                continue;
            }
            if !status.is_success() {
                let msg = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(String::from))
                    .unwrap_or_else(|| status.to_string());
                return Err(format!("Microsoft To Do: {msg}"));
            }
            return Ok(if text.is_empty() { Value::Null } else { serde_json::from_str(&text).map_err(|e| e.to_string())? });
        }
        Err("Microsoft To Do: still throttled, will retry at the next sync".into())
    }

    fn url(segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = Url::parse(GRAPH).unwrap();
        // List and task ids contain characters that must be escaped.
        url.path_segments_mut().unwrap().extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    async fn pages<T: serde::de::DeserializeOwned>(&self, mut url: Url) -> Result<Vec<T>, String> {
        let mut out = Vec::new();
        loop {
            let page: Page<T> = serde_json::from_value(self.call(Method::GET, url, None).await?).map_err(|e| e.to_string())?;
            out.extend(page.value);
            match page.next.and_then(|n| Url::parse(&n).ok()) {
                Some(next) => url = next,
                None => return Ok(out),
            }
        }
    }

    /// Fetches all lists and their open tasks.
    pub async fn sync(&self) -> Result<(Vec<TaskList>, Vec<Task>), String> {
        let glists: Vec<GList> = self.pages(Self::url(&["me", "todo", "lists"], &[])).await?;
        let lists: Vec<TaskList> = glists
            .iter()
            .filter(|l| l.wellknown_list_name != "flaggedEmails")
            .map(|l| TaskList {
                id: l.id.clone(),
                name: l.display_name.clone(),
                default: l.wellknown_list_name == "defaultList",
                pos: 0,
            })
            .collect();
        // One list at a time: To Do lives in Exchange, which allows only a few
        // concurrent requests per mailbox and throttles beyond that.
        let mut tasks = Vec::new();
        for l in &lists {
            tasks.extend(self.open_tasks(&l.id).await?);
        }
        Ok((lists, tasks))
    }

    async fn open_tasks(&self, list: &str) -> Result<Vec<Task>, String> {
        let url = Self::url(&["me", "todo", "lists", list, "tasks"], &[("$filter", "status ne 'completed'"), ("$top", "100")]);
        let raw: Vec<GTask> = self.pages(url).await?;
        Ok(raw
            .into_iter()
            .map(|t| Task {
                id: t.id,
                list: list.to_string(),
                title: if t.title.is_empty() { "(no title)".into() } else { t.title },
                note: t.body.map(|b| cal::plain_text(&b.content)).unwrap_or_default(),
                due: t.due_date_time.as_ref().and_then(from_graph).map(|d| d.date_naive()),
                reminder: t.reminder_date_time.as_ref().filter(|_| t.is_reminder_on).and_then(from_graph),
                done: t.status == "completed",
                important: t.importance == "high",
                created: t.created_date_time.and_then(|c| DateTime::parse_from_rfc3339(&c).ok()).map(|d| d.with_timezone(&Local)),
                check: true,
                ..Task::default()
            })
            .collect())
    }

    pub async fn create(&self, list: &str, text: &str) -> Result<(), String> {
        let body = parse(text, Local::now()).body();
        self.call(Method::POST, Self::url(&["me", "todo", "lists", list, "tasks"], &[]), Some(body)).await.map(|_| ())
    }

    pub async fn update(&self, list: &str, id: &str, text: &str) -> Result<(), String> {
        let body = parse(text, Local::now()).body();
        self.call(Method::PATCH, Self::url(&["me", "todo", "lists", list, "tasks", id], &[]), Some(body)).await.map(|_| ())
    }

    pub async fn set_done(&self, list: &str, id: &str, done: bool) -> Result<(), String> {
        let body = json!({ "status": if done { "completed" } else { "notStarted" } });
        self.call(Method::PATCH, Self::url(&["me", "todo", "lists", list, "tasks", id], &[]), Some(body)).await.map(|_| ())
    }

    pub async fn delete(&self, list: &str, id: &str) -> Result<(), String> {
        self.call(Method::DELETE, Self::url(&["me", "todo", "lists", list, "tasks", id], &[]), None).await.map(|_| ())
    }
}

// ---- backends ------------------------------------------------------------------

/// The service behind a task tab. A new task app is another variant here.
#[derive(Clone)]
pub enum Backend {
    Microsoft(Arc<Todo>),
    Twos(Arc<Twos>),
}

impl Backend {
    /// All lists to show and their open tasks.
    pub async fn sync(&self) -> Result<(Vec<TaskList>, Vec<Task>), String> {
        match self {
            Backend::Microsoft(t) => t.sync().await,
            Backend::Twos(t) => t.sync().await,
        }
    }

    /// Adds a task typed as one line (see `parse`).
    pub async fn create(&self, list: &str, text: &str) -> Result<(), String> {
        match self {
            Backend::Microsoft(t) => t.create(list, text).await,
            Backend::Twos(t) => t.create(list, text).await,
        }
    }

    pub async fn update(&self, list: &str, id: &str, text: &str) -> Result<(), String> {
        match self {
            Backend::Microsoft(t) => t.update(list, id, text).await,
            Backend::Twos(t) => t.update(id, text).await,
        }
    }

    pub async fn set_done(&self, list: &str, id: &str, done: bool) -> Result<(), String> {
        match self {
            Backend::Microsoft(t) => t.set_done(list, id, done).await,
            Backend::Twos(t) => t.set_done(id, done).await,
        }
    }

    pub async fn delete(&self, list: &str, id: &str) -> Result<(), String> {
        match self {
            Backend::Microsoft(t) => t.delete(list, id).await,
            Backend::Twos(t) => t.delete(id).await,
        }
    }
}

// ---- state ---------------------------------------------------------------------

#[derive(Default)]
pub struct TodoState {
    pub lists: Vec<TaskList>,
    pub tasks: Vec<Task>,
    pub status: String,
    pub syncing: bool,
    pub next_poll: Option<Instant>,
    pub reminded_until: Option<DateTime<Local>>,
    /// Device-code login in progress: (uri, code).
    pub login: Option<(String, String)>,
}

impl TodoState {
    pub fn apply(&mut self, mut lists: Vec<TaskList>, tasks: Vec<Task>) {
        // The default list ("Tasks", Twos' Today) first, then in the backend's order, then by name.
        lists.sort_by(|a, b| {
            b.default.cmp(&a.default).then(a.pos.cmp(&b.pos)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        self.lists = lists;
        self.tasks = tasks;
        self.sort();
        self.status = format!("synced {}", Local::now().format("%H:%M"));
    }

    /// In the backend's order where it has one (Twos lists are pages);
    /// otherwise due tasks first (earliest first), then undated ones, newest first.
    pub fn sort(&mut self) {
        let key = |t: &Task| (t.pos, t.due.is_none(), t.due, std::cmp::Reverse(t.created));
        self.tasks.sort_by(|a, b| key(a).cmp(&key(b)));
    }

    pub fn find(&self, key: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.key() == key)
    }

    pub fn find_mut(&mut self, key: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.key() == key)
    }

    pub fn list_name(&self, id: &str) -> &str {
        self.lists.iter().find(|l| l.id == id).map(|l| l.name.as_str()).unwrap_or("")
    }

    pub fn default_list(&self) -> Option<&str> {
        self.lists.first().map(|l| l.id.as_str())
    }

    /// Open tasks due today or earlier.
    pub fn due_count(&self, today: NaiveDate) -> usize {
        self.tasks.iter().filter(|t| t.check && !t.done && t.due.is_some_and(|d| d <= today)).count()
    }

    /// Reminders that came due since the last call.
    pub fn due_reminders(&mut self, now: DateTime<Local>) -> Vec<&Task> {
        let since = self.reminded_until.replace(now).unwrap_or(now);
        self.tasks.iter().filter(|t| t.check && !t.done && t.reminder.is_some_and(|r| r > since && r <= now)).collect()
    }

    pub fn next_wake(&self, now: DateTime<Local>) -> Option<Instant> {
        let soonest = self.tasks.iter().filter(|t| t.check && !t.done).filter_map(|t| t.reminder).filter(|r| *r > now).min();
        let at = soonest.and_then(|t| (t - now).to_std().ok()).map(|d| Instant::now() + d);
        match (at, self.next_poll) {
            (Some(a), Some(p)) => Some(a.min(p)),
            (a, p) => a.or(p),
        }
    }

    /// Open tasks with a due date as all-day entries of calendar `cal`.
    pub fn as_events(&self, cal: &str) -> Vec<cal::Event> {
        self.tasks
            .iter()
            .filter(|t| t.check && !t.done)
            .filter_map(|t| {
                let d = t.due?;
                Some(cal::Event {
                    id: t.key(),
                    cal: cal.into(),
                    title: format!("☐ {}", t.title),
                    description: t.note.clone(),
                    location: String::new(),
                    start: local_midnight(d),
                    end: local_midnight(d + chrono::Days::new(1)),
                    all_day: true,
                    link: String::new(),
                    reminders: Vec::new(),
                })
            })
            .collect()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tasks() {
        let now = Local.with_ymd_and_hms(2026, 10, 27, 10, 0, 0).unwrap();
        let e = parse("morgen 14:00 Arzt anrufen", now);
        assert_eq!(e.title, "Arzt anrufen");
        assert_eq!(e.due, NaiveDate::from_ymd_opt(2026, 10, 28));
        assert_eq!(e.reminder.unwrap().format("%Y-%m-%d %H:%M").to_string(), "2026-10-28 14:00");
        let e = parse("Milch kaufen", now);
        assert_eq!((e.title.as_str(), e.due, e.reminder), ("Milch kaufen", None, None));
    }

    #[test]
    fn edit_text_round_trips() {
        let now = Local.with_ymd_and_hms(2026, 10, 27, 10, 0, 0).unwrap();
        let t = Task {
            id: "t".into(),
            list: "l".into(),
            title: "Steuer".into(),
            note: String::new(),
            due: NaiveDate::from_ymd_opt(2026, 11, 2),
            reminder: Some(Local.with_ymd_and_hms(2026, 11, 2, 9, 0, 0).unwrap()),
            done: false,
            important: false,
            created: None,
            check: true,
            ..Task::default()
        };
        let e = parse(&t.to_entry(), now);
        assert_eq!((e.title.as_str(), e.due, e.reminder), ("Steuer", t.due, t.reminder));
    }

    #[test]
    fn graph_times_are_utc() {
        let t = from_graph(&GDateTime { date_time: "2026-10-27T22:00:00.0000000".into() }).unwrap();
        assert_eq!(t.with_timezone(&Utc).format("%H:%M").to_string(), "22:00");
    }
}
