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

    /// Today's list (where new things go), and the open todos of all lists.
    pub async fn sync(&self) -> Result<(Vec<TaskList>, Vec<Task>), String> {
        let today: TList = serde_json::from_value(self.get(&["today"], &[("max_text", "1")]).await?["list"].take())
            .map_err(|e| format!("Twos: {e}"))?;

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
        let mut lists = vec![TaskList { id: today.id.clone(), name: "Today".into(), default: true }];
        for t in &things {
            if !lists.iter().any(|l| l.id == t.list_id) {
                let name = names.get(&t.list_id).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| "(untitled list)".into());
                lists.push(TaskList { id: t.list_id.clone(), name, default: false });
            }
        }
        let tasks = things
            .into_iter()
            .map(|t| {
                let (date, at) = due.get(t.id.as_str()).copied().unzip();
                Task {
                    title: if t.text.trim().is_empty() { "(no text)".into() } else { t.text },
                    list: t.list_id,
                    note: t.note,
                    due: date,
                    reminder: at.flatten(),
                    done: false,
                    important: t.favorited,
                    created: t.created.and_then(|c| DateTime::parse_from_rfc3339(&c).ok()).map(|d| d.with_timezone(&Local)),
                    id: t.id,
                }
            })
            .collect();
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
    fn recognizes_keys() {
        assert!(is_key("twos_a1b2c3d4_secretpart"));
        assert!(!is_key("hello"));
        assert!(!is_key("twos_a1b2 c3d4_x"));
    }
}
