//! Twos (twosapp.com) via its REST API: open todos of all lists, today's
//! list for new ones, and reminders as due times. Each user signs in with a
//! personal API key (Twos → Settings → API Keys).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate};
use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::secret;
use crate::todo::{self, Task, TaskList};

const API: &str = "https://www.twosapp.com/api/v1";
const SECRET_NAME: &str = "twos";
/// Pages of 50 things read per sync, at most (Twos allows 1000 requests an hour).
const MAX_PAGES: usize = 40;

pub fn load_key() -> Option<String> {
    let key = String::from_utf8(secret::load_secret(SECRET_NAME)?).ok()?;
    Some(key.trim().to_string()).filter(|k| !k.is_empty())
}

pub fn save_key(key: &str) -> Result<(), String> {
    secret::save_secret(SECRET_NAME, key.as_bytes()).map(|_| ()).map_err(|e| e.to_string())
}

pub fn remove_key() {
    secret::remove_secret(SECRET_NAME);
}

/// Whether text looks like a Twos API key ("twos_<id>_<secret>").
pub fn is_key(key: &str) -> bool {
    key.starts_with("twos_") && key.len() > 12 && !key.contains(char::is_whitespace)
}

#[derive(Deserialize)]
struct TList {
    id: String,
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct Thing {
    id: String,
    #[serde(default)]
    list_id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    completed: bool,
    #[serde(default)]
    canceled: bool,
    #[serde(default)]
    favorited: bool,
    created: Option<String>,
    /// todo, none (a plain note), dash, number, bullet or photo.
    #[serde(default, rename = "type")]
    kind: String,
    /// Indent level.
    #[serde(default)]
    tabs: u32,
    #[serde(default)]
    header: bool,
    #[serde(default)]
    subheader: bool,
}

/// A Twos thing as a line of a task tab.
fn to_task(t: Thing, list: &str, pos: u32, due: Option<NaiveDate>, reminder: Option<DateTime<Local>>, from: Option<NaiveDate>) -> Task {
    let marker = match t.kind.as_str() {
        "bullet" => "• ",
        "dash" => "– ",
        "number" => "1. ",
        "photo" => "[photo] ",
        _ => "",
    };
    let empty = t.text.trim().is_empty();
    Task {
        id: t.id,
        list: list.to_string(),
        title: if empty && t.kind == "todo" { "(no text)".into() } else { t.text },
        note: t.note,
        due,
        reminder,
        done: t.completed || t.canceled,
        important: t.favorited,
        created: t.created.and_then(|c| DateTime::parse_from_rfc3339(&c).ok()).map(|d| d.with_timezone(&Local)),
        check: t.kind == "todo",
        marker: marker.to_string(),
        heading: t.header || t.subheader,
        depth: t.tabs.min(6) as u8,
        pos,
        from,
    }
}

/// Day lists are ordinary lists titled with their date. Today's title shows
/// the format; titles in it from about a year around today are days.
struct Days(HashMap<String, NaiveDate>);

/// Date formats a day list title may have (the account's one is detected).
const DAY_FORMATS: &[&str] = &[
    "%a %b %d %Y",
    "%a, %b %-d, %Y",
    "%A, %B %-d, %Y",
    "%a %b %-d, %Y",
    "%a, %b %-d %Y",
    "%A %B %-d, %Y",
    "%b %-d, %Y",
    "%B %-d, %Y",
    "%a, %-d %b %Y",
    "%A, %-d %B %Y",
    "%-d %b %Y",
    "%-d %B %Y",
    "%Y-%m-%d",
    "%-m/%-d/%Y",
    "%m/%d/%Y",
    "%-m/%-d/%y",
    "%d.%m.%Y",
    "%-d.%-m.%Y",
    "%A, %B %-d",
    "%a, %b %-d",
    "%b %-d",
];

impl Days {
    fn new(today_title: &str, today: NaiveDate) -> Days {
        let title = today_title.trim();
        let formats: Vec<&str> = match DAY_FORMATS.iter().find(|f| today.format(f).to_string() == title) {
            Some(f) => vec![f],
            None => DAY_FORMATS.to_vec(),
        };
        let mut map = HashMap::new();
        // Nearest days first, so a title without a year means the closest date.
        for n in 0..=400i64 {
            for off in if n == 0 { vec![0] } else { vec![-n, n] } {
                let Some(d) = today.checked_add_signed(chrono::TimeDelta::days(off)) else { continue };
                for f in &formats {
                    map.entry(d.format(f).to_string()).or_insert(d);
                }
            }
        }
        Days(map)
    }

    fn date(&self, title: &str) -> Option<NaiveDate> {
        self.0.get(title.trim()).copied()
    }
}

#[derive(Deserialize)]
struct Reminder {
    #[serde(default)]
    thing_id: String,
    #[serde(default)]
    at: String,
    #[serde(default)]
    all_day: bool,
}

impl Reminder {
    /// (due date, reminder time); all-day reminders only have the date.
    fn due(&self) -> Option<(NaiveDate, Option<DateTime<Local>>)> {
        if self.all_day {
            // The date as written: converting midnight between zones could shift it.
            let d = NaiveDate::parse_from_str(self.at.get(..10)?, "%Y-%m-%d").ok()?;
            return Some((d, None));
        }
        let t = DateTime::parse_from_rfc3339(&self.at).ok()?.with_timezone(&Local);
        Some((t.date_naive(), Some(t)))
    }
}

pub struct Twos {
    http: reqwest::Client,
    key: String,
    /// List titles by id, kept between syncs (Twos has a list per day).
    names: Mutex<HashMap<String, String>>,
}

impl Twos {
    pub fn new(key: String) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default();
        Self { http, key, names: Mutex::new(HashMap::new()) }
    }

    fn url(segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = Url::parse(API).unwrap();
        url.path_segments_mut().unwrap().extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    async fn call(&self, method: Method, url: Url, body: Option<Value>) -> Result<Value, String> {
        for attempt in 0..3u32 {
            let mut req = self.http.request(method.clone(), url.clone()).bearer_auth(&self.key);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            let wait = crate::cal::retry_after(&resp);
            let text = resp.text().await.map_err(|e| e.to_string())?;
            if status.as_u16() == 429 && attempt < 2 {
                tokio::time::sleep(Duration::from_secs(wait.unwrap_or(5).min(60))).await;
                continue;
            }
            if status.as_u16() == 401 {
                return Err("Twos rejected the API key; run :twos login <new key>".into());
            }
            if !status.is_success() {
                let msg = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v["error"].as_str().map(String::from))
                    .unwrap_or_else(|| status.to_string());
                return Err(format!("Twos: {msg}"));
            }
            return Ok(if text.is_empty() { Value::Null } else { serde_json::from_str(&text).map_err(|e| e.to_string())? });
        }
        Err("Twos: rate limit reached, will retry at the next sync".into())
    }

    async fn get(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<Value, String> {
        self.call(Method::GET, Self::url(segments, query), None).await
    }

    /// Like the app: **Today** is today's whole day list (notes, headings and
    /// done to-dos included), with the unfinished to-dos of earlier days
    /// rolled over on top. Then upcoming days and other lists with open
    /// to-dos.
    pub async fn sync(&self) -> Result<(Vec<TaskList>, Vec<Task>), String> {
        let date = Local::now().date_naive();
        let mut v = self.get(&["today"], &[]).await?;
        let today: TList = serde_json::from_value(v["list"].take()).map_err(|e| format!("Twos: {e}"))?;
        let page: Vec<Thing> = serde_json::from_value(v["things"].take()).unwrap_or_default();

        let mut things: Vec<Thing> = Vec::new();
        for page in 0..MAX_PAGES {
            let p = page.to_string();
            let mut v = self.get(&["things"], &[("type", "todo"), ("completed", "false"), ("page", &p)]).await?;
            let batch: Vec<Thing> = serde_json::from_value(v["things"].take()).map_err(|e| format!("Twos: {e}"))?;
            things.extend(batch.into_iter().filter(|t| !t.completed && !t.canceled));
            if !v["has_more"].as_bool().unwrap_or(false) {
                break;
            }
        }

        // Due times: Twos lists only upcoming reminders.
        let mut v = self.get(&["reminders"], &[]).await?;
        let reminders: Vec<Reminder> = serde_json::from_value(v["reminders"].take()).unwrap_or_default();
        let due: HashMap<&str, (NaiveDate, Option<DateTime<Local>>)> =
            reminders.iter().filter_map(|r| Some((r.thing_id.as_str(), r.due()?))).collect();

        self.learn_names(&things).await?;
        let names = self.names.lock().unwrap().clone();
        let days = Days::new(&today.title, date);
        let mut pos = 0;
        let mut task = |t: Thing, list: &str, from: Option<NaiveDate>| {
            pos += 1;
            let (when, at) = due.get(t.id.as_str()).copied().unzip();
            to_task(t, list, pos, when, at.flatten(), from)
        };

        let mut lists = vec![TaskList { id: today.id.clone(), name: format!("Today · {}", date.format("%a %d %b")), default: true, pos: 0 }];
        let (mut rolled, mut upcoming, mut other) = (Vec::new(), Vec::new(), Vec::new());
        for t in things {
            if t.list_id == today.id {
                continue; // part of today's page
            }
            let title = names.get(&t.list_id).map(String::as_str).unwrap_or("");
            match days.date(title) {
                Some(d) if d < date => rolled.push((d, t)),
                Some(d) => upcoming.push((d, t)),
                None => other.push(t),
            }
        }
        // Rolled-over to-dos on top, oldest day first, as Twos shows them.
        rolled.sort_by(|a, b| (a.0, &a.1.created).cmp(&(b.0, &b.1.created)));
        let mut tasks: Vec<Task> = rolled.into_iter().map(|(d, t)| task(t, &today.id, Some(d))).collect();
        let mut number = 0;
        for t in page {
            // Numbered lines count up within a run of them.
            number = if t.kind == "number" { number + 1 } else { 0 };
            let mut line = task(t, &today.id, None);
            if number > 0 {
                line.marker = format!("{number}. ");
            }
            tasks.push(line);
        }
        upcoming.sort_by(|a, b| (a.0, &a.1.created).cmp(&(b.0, &b.1.created)));
        for (d, t) in upcoming {
            if !lists.iter().any(|l| l.id == t.list_id) {
                let name = match (d - date).num_days() {
                    1 => format!("Tomorrow · {}", d.format("%a %d %b")),
                    _ => d.format("%a %d %b %Y").to_string(),
                };
                lists.push(TaskList { id: t.list_id.clone(), name, default: false, pos: lists.len() as u32 });
            }
            let list = t.list_id.clone();
            tasks.push(task(t, &list, None));
        }
        for t in other {
            if !lists.iter().any(|l| l.id == t.list_id) {
                let name = names.get(&t.list_id).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| "(untitled list)".into());
                // After the days, by name.
                lists.push(TaskList { id: t.list_id.clone(), name, default: false, pos: u32::MAX });
            }
            let list = t.list_id.clone();
            tasks.push(task(t, &list, None));
        }
        Ok((lists, tasks))
    }

    /// Looks up the titles of lists not seen before: all lists page by page
    /// first, then one by one for the rest (day lists may not be listed).
    async fn learn_names(&self, things: &[Thing]) -> Result<(), String> {
        let unknown = |names: &HashMap<String, String>| -> Vec<String> {
            let mut ids: Vec<String> = things.iter().map(|t| t.list_id.clone()).filter(|id| !names.contains_key(id)).collect();
            ids.sort();
            ids.dedup();
            ids
        };
        if unknown(&self.names.lock().unwrap()).is_empty() {
            return Ok(());
        }
        for page in 0..20 {
            let p = page.to_string();
            let mut v = self.get(&["lists"], &[("page", &p)]).await?;
            let lists: Vec<TList> = serde_json::from_value(v["lists"].take()).unwrap_or_default();
            let mut names = self.names.lock().unwrap();
            names.extend(lists.into_iter().map(|l| (l.id, l.title)));
            if !v["has_more"].as_bool().unwrap_or(false) || unknown(&names).is_empty() {
                break;
            }
        }
        let rest = unknown(&self.names.lock().unwrap());
        for id in rest.into_iter().take(20) {
            let title = match self.get(&["fetch"], &[("id", &id)]).await {
                Ok(v) => v["title"].as_str().unwrap_or("").to_string(),
                Err(_) => String::new(),
            };
            self.names.lock().unwrap().insert(id, title);
        }
        Ok(())
    }

    pub async fn create(&self, list: &str, text: &str) -> Result<(), String> {
        let e = todo::parse(text, Local::now());
        let body = json!({ "list_id": list, "text": e.title, "type": "todo" });
        let v = self.call(Method::POST, Self::url(&["things"], &[]), Some(body)).await?;
        let id = v["thing"]["id"].as_str().ok_or("Twos: no id for the new thing")?.to_string();
        self.set_due(&id, &e, false).await
    }

    pub async fn update(&self, id: &str, text: &str) -> Result<(), String> {
        let e = todo::parse(text, Local::now());
        self.call(Method::PATCH, Self::url(&["things", id], &[]), Some(json!({ "text": e.title }))).await?;
        self.set_due(id, &e, true).await
    }

    /// The due date/time becomes the thing's reminder (Twos has no due field).
    async fn set_due(&self, id: &str, e: &todo::Entry, clear: bool) -> Result<(), String> {
        let url = Self::url(&["things", id, "reminder"], &[]);
        let body = match (e.reminder, e.due) {
            (Some(at), _) => json!({ "at": at.to_rfc3339() }),
            // A naive time is read in the user's time zone.
            (None, Some(d)) => json!({ "at": d.format("%Y-%m-%dT00:00:00").to_string(), "all_day": true }),
            (None, None) if clear => {
                // No reminder to remove is fine.
                return match self.call(Method::DELETE, url, None).await {
                    Err(e) if !e.contains("not found") && !e.contains("404") => Err(e),
                    _ => Ok(()),
                };
            }
            (None, None) => return Ok(()),
        };
        self.call(Method::PUT, url, Some(body)).await.map(|_| ())
    }

    pub async fn set_done(&self, id: &str, done: bool) -> Result<(), String> {
        self.call(Method::PATCH, Self::url(&["things", id], &[]), Some(json!({ "completed": done }))).await.map(|_| ())
    }

    pub async fn delete(&self, id: &str) -> Result<(), String> {
        self.call(Method::DELETE, Self::url(&["things", id], &[]), None).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reminders_become_due_times() {
        let r = Reminder { thing_id: "t".into(), at: "2026-11-02T00:00:00.000Z".into(), all_day: true };
        assert_eq!(r.due(), Some((NaiveDate::from_ymd_opt(2026, 11, 2).unwrap(), None)));
        let r = Reminder { thing_id: "t".into(), at: "2026-11-02T09:30:00+01:00".into(), all_day: false };
        let (d, t) = r.due().unwrap();
        assert_eq!(t.unwrap().with_timezone(&chrono::FixedOffset::east_opt(3600).unwrap()).format("%H:%M").to_string(), "09:30");
        assert_eq!(Some(d), t.map(|t| t.date_naive()));
    }

    #[test]
    fn day_lists_are_recognized_by_title() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 28).unwrap();
        // The format of today's title is detected and used for the others.
        let days = Days::new("Wed Oct 28 2026", today);
        assert_eq!(days.date("Tue Oct 27 2026"), NaiveDate::from_ymd_opt(2026, 10, 27));
        assert_eq!(days.date("Fri Jan 01 2027"), NaiveDate::from_ymd_opt(2027, 1, 1));
        assert_eq!(days.date("Groceries"), None);
        let days = Days::new("Wednesday, October 28, 2026", today);
        assert_eq!(days.date("Monday, October 26, 2026"), NaiveDate::from_ymd_opt(2026, 10, 26));
        // Unknown format: every known one is tried.
        let days = Days::new("", today);
        assert_eq!(days.date("2026-10-20"), NaiveDate::from_ymd_opt(2026, 10, 20));
    }

    #[test]
    fn things_become_lines() {
        let thing = |kind: &str, completed: bool| Thing {
            id: "t".into(),
            list_id: "l".into(),
            text: "Milk".into(),
            note: String::new(),
            completed,
            canceled: false,
            favorited: false,
            created: None,
            kind: kind.into(),
            tabs: 1,
            header: false,
            subheader: false,
        };
        let t = to_task(thing("todo", true), "today", 3, None, None, None);
        assert!(t.check && t.done && t.marker.is_empty() && t.depth == 1 && t.pos == 3);
        let t = to_task(thing("bullet", false), "today", 4, None, None, None);
        assert!(!t.check && t.marker == "• ");
    }

    #[test]
    fn recognizes_keys() {
        assert!(is_key("twos_a1b2c3d4_secretpart"));
        assert!(!is_key("hello"));
        assert!(!is_key("twos_a1b2 c3d4_x"));
    }
}
