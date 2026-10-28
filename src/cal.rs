//! Google Calendar: OAuth login (desktop loopback flow with PKCE), event
//! sync, create/edit/delete, reminders and a small natural event parser.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::{DateTime, Datelike, Days, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Weekday};
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::secret;

const SCOPE: &str = "https://www.googleapis.com/auth/calendar";
const API: &str = "https://www.googleapis.com/calendar/v3";
const SECRET_NAME: &str = "google";
/// How often events are re-fetched while the app runs.
pub const POLL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Serialize, Deserialize)]
pub struct Creds {
    pub client_id: String,
    pub client_secret: String,
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

    /// Reads the OAuth client JSON downloaded from Google Cloud Console.
    pub fn from_client_file(path: &str) -> Result<Creds, String> {
        let text = std::fs::read_to_string(path.trim().trim_matches('"')).map_err(|e| format!("{path}: {e}"))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("not a client JSON file: {e}"))?;
        let c = v.get("installed").or_else(|| v.get("web")).unwrap_or(&v);
        let field = |k: &str| c.get(k).and_then(Value::as_str).map(String::from);
        match (field("client_id"), field("client_secret")) {
            (Some(client_id), Some(client_secret)) => Ok(Creds { client_id, client_secret, refresh_token: String::new() }),
            _ => Err("the file has no client_id / client_secret (download it for a \"Desktop app\" client)".into()),
        }
    }
}

/// Revokes a sign-in at Google, so its refresh token stops working.
pub async fn revoke(refresh_token: &str) -> Result<(), String> {
    let resp = reqwest::Client::new()
        .post("https://oauth2.googleapis.com/revoke")
        .form(&[("token", refresh_token)])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(resp.status().to_string())
    }
}

/// The value of a variable set at runtime, or else baked in at build time.
pub fn build_var(runtime: Option<String>, compiled: Option<&'static str>) -> Option<String> {
    runtime.or(compiled.map(String::from)).filter(|v| !v.trim().is_empty())
}

/// The OAuth client a release build ships with (set when building), so users
/// only sign in. For installed apps Google doesn't treat the secret as secret.
pub fn builtin_client() -> Option<Creds> {
    let id = build_var(std::env::var("DISCOMMAND_GOOGLE_CLIENT_ID").ok(), option_env!("DISCOMMAND_GOOGLE_CLIENT_ID"))?;
    let secret = build_var(std::env::var("DISCOMMAND_GOOGLE_CLIENT_SECRET").ok(), option_env!("DISCOMMAND_GOOGLE_CLIENT_SECRET"))?;
    Some(Creds { client_id: id, client_secret: secret, refresh_token: String::new() })
}

// ---- OAuth -------------------------------------------------------------

fn random_string(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let _ = getrandom::fill(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// Waits (up to 5 minutes) for the browser to come back to the loopback
/// `listener` with a query that `done` accepts, answers it with a short
/// page, and returns the query string. Other requests get a 404.
pub async fn await_redirect(listener: &TcpListener, done: impl Fn(&HashMap<String, String>) -> bool) -> Result<String, String> {
    tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let (mut sock, _) = listener.accept().await.map_err(|e| e.to_string())?;
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let url = Url::parse(&format!("http://localhost{path}")).ok();
            let query: HashMap<String, String> = url.as_ref().map(|u| u.query_pairs().into_owned().collect()).unwrap_or_default();
            if !done(&query) {
                // Favicon requests and the like: answer and keep waiting.
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                continue;
            }
            let body = if query.contains_key("error") {
                "Sign-in was cancelled. You can close this tab."
            } else {
                "Signed in. You can close this tab and return to discommand."
            };
            let html = format!("<!doctype html><meta charset=utf-8><title>discommand</title><p style=\"font:16px sans-serif;margin:3em\">{body}</p>");
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                html.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            return Ok(url.and_then(|u| u.query().map(String::from)).unwrap_or_default());
        }
    })
    .await
    .map_err(|_| "sign-in timed out (5 minutes)".to_string())?
}

/// Runs the browser login: listens on a loopback port for Google's
/// redirect and exchanges the code for a refresh token.
pub async fn login(mut creds: Creds, open: impl Fn(&str) -> std::io::Result<()>) -> Result<Creds, String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr().map_err(|e| e.to_string())?.port());
    let verifier = random_string(48);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_string(16);
    let url = Url::parse_with_params(
        "https://accounts.google.com/o/oauth2/v2/auth",
        &[
            ("client_id", creds.client_id.as_str()),
            ("redirect_uri", &redirect),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("state", &state),
        ],
    )
    .map_err(|e| e.to_string())?;
    open(url.as_str()).map_err(|e| format!("could not open the browser: {e}"))?;

    let query = await_redirect(&listener, |q| q.get("state") == Some(&state) || q.contains_key("error")).await?;
    let code = Url::parse(&format!("http://localhost/?{query}"))
        .ok()
        .and_then(|u| u.query_pairs().find(|(k, _)| k == "code").map(|(_, v)| v.into_owned()))
        .ok_or("sign-in was cancelled")?;

    let http = reqwest::Client::new();
    let resp: Value = http
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("code", code.as_str()),
            ("client_id", &creds.client_id),
            ("client_secret", &creds.client_secret),
            ("redirect_uri", &redirect),
            ("grant_type", "authorization_code"),
            ("code_verifier", &verifier),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    creds.refresh_token = resp["refresh_token"]
        .as_str()
        .ok_or_else(|| format!("token exchange failed: {}", resp["error_description"].as_str().unwrap_or("no refresh token")))?
        .to_string();
    Ok(creds)
}

// ---- model ---------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct CalInfo {
    pub id: String,
    pub name: String,
    pub color: Option<[u8; 3]>,
    pub primary: bool,
    pub writable: bool,
}

#[derive(Clone, Debug)]
pub struct Event {
    pub id: String,
    pub cal: String,
    pub title: String,
    pub description: String,
    pub location: String,
    pub start: DateTime<Local>,
    pub end: DateTime<Local>,
    pub all_day: bool,
    pub link: String,
    /// Popup reminders, in minutes before the start.
    pub reminders: Vec<i64>,
}

impl Event {
    pub fn key(&self) -> String {
        format!("{}\n{}", self.cal, self.id)
    }

    pub fn reminder_times(&self) -> impl Iterator<Item = DateTime<Local>> + '_ {
        self.reminders.iter().map(|m| self.start - chrono::Duration::minutes(*m))
    }

    /// One-line text for editing, in the syntax `parse` understands.
    pub fn to_entry(&self) -> String {
        let mut s = if self.all_day {
            format!("{} {}", self.start.format("%Y-%m-%d"), self.title)
        } else {
            format!("{} {}-{} {}", self.start.format("%Y-%m-%d"), self.start.format("%H:%M"), self.end.format("%H:%M"), self.title)
        };
        if !self.location.is_empty() {
            s.push_str(" @ ");
            s.push_str(&self.location);
        }
        s
    }
}

#[derive(Deserialize)]
struct GCalList {
    #[serde(default)]
    items: Vec<GCal>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GCal {
    id: String,
    #[serde(default)]
    summary: String,
    summary_override: Option<String>,
    background_color: Option<String>,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    selected: bool,
    #[serde(default)]
    access_role: String,
    #[serde(default)]
    default_reminders: Vec<GReminder>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GEventList {
    #[serde(default)]
    items: Vec<GEvent>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GEvent {
    id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    location: String,
    #[serde(default)]
    start: GTime,
    #[serde(default)]
    end: GTime,
    #[serde(default)]
    html_link: String,
    reminders: Option<GReminders>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct GTime {
    date_time: Option<String>,
    date: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GReminders {
    #[serde(default)]
    use_default: bool,
    #[serde(default)]
    overrides: Vec<GReminder>,
}

#[derive(Deserialize, Clone)]
struct GReminder {
    #[serde(default)]
    method: String,
    #[serde(default)]
    minutes: i64,
}

fn popup_minutes(r: &[GReminder]) -> Vec<i64> {
    r.iter().filter(|r| r.method == "popup").map(|r| r.minutes).collect()
}

fn parse_color(hex: &str) -> Option<[u8; 3]> {
    let h = hex.strip_prefix('#')?;
    let n = u32::from_str_radix(h, 16).ok()?;
    (h.len() == 6).then(|| [(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

pub fn local_midnight(d: NaiveDate) -> DateTime<Local> {
    Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .unwrap_or_else(|| Local.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()))
}

fn parse_time(t: &GTime) -> Option<(DateTime<Local>, bool)> {
    if let Some(dt) = &t.date_time {
        return DateTime::parse_from_rfc3339(dt).ok().map(|d| (d.with_timezone(&Local), false));
    }
    let d = NaiveDate::parse_from_str(t.date.as_deref()?, "%Y-%m-%d").ok()?;
    Some((local_midnight(d), true))
}

/// Strips the HTML Google allows in descriptions.
pub fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let Some(j) = rest[i..].find('>') else {
            rest = &rest[i..];
            break;
        };
        let tag = rest[i + 1..i + j].trim_start_matches('/').to_ascii_lowercase();
        if tag.starts_with("br") || tag.starts_with('p') || tag.starts_with("li") || tag.starts_with("div") {
            out.push('\n');
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

// ---- API client ------------------------------------------------------------

/// Seconds a throttled response asks to wait (`Retry-After`).
pub fn retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()
}

pub struct Google {
    http: reqwest::Client,
    creds: Creds,
    token: tokio::sync::Mutex<Option<(String, Instant)>>,
}

pub struct Synced {
    pub account: String,
    pub calendars: Vec<CalInfo>,
    pub events: Vec<Event>,
}

impl Google {
    pub fn new(creds: Creds) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default();
        Self { http, creds, token: tokio::sync::Mutex::new(None) }
    }

    async fn access_token(&self) -> Result<String, String> {
        let mut cached = self.token.lock().await;
        if let Some((t, until)) = cached.as_ref() {
            if Instant::now() < *until {
                return Ok(t.clone());
            }
        }
        let resp: Value = self
            .http
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("client_id", self.creds.client_id.as_str()),
                ("client_secret", &self.creds.client_secret),
                ("refresh_token", &self.creds.refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let Some(token) = resp["access_token"].as_str() else {
            return Err(match resp["error"].as_str() {
                Some("invalid_grant") => "Google sign-in expired; run :cal login".into(),
                _ => format!("Google token refresh failed: {}", resp["error_description"].as_str().unwrap_or("unknown error")),
            });
        };
        let secs = resp["expires_in"].as_u64().unwrap_or(3600).saturating_sub(60);
        *cached = Some((token.to_string(), Instant::now() + Duration::from_secs(secs)));
        Ok(token.to_string())
    }

    async fn call(&self, method: Method, url: Url, body: Option<Value>) -> Result<Value, String> {
        for attempt in 0..4u32 {
            let token = self.access_token().await?;
            let mut req = self.http.request(method.clone(), url.clone()).bearer_auth(token);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            let wait = crate::cal::retry_after(&resp);
            let text = resp.text().await.map_err(|e| e.to_string())?;
            // Throttled: wait as long as the server asks (or back off) and retry.
            if (matches!(status.as_u16(), 429 | 503) || status.as_u16() == 403 && text.contains("ateLimitExceeded")) && attempt < 3 {
                let secs = wait.unwrap_or(2u64 << attempt).min(60);
                tokio::time::sleep(Duration::from_secs(secs)).await;
                continue;
            }
            if !status.is_success() {
                let msg = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(String::from))
                    .unwrap_or_else(|| status.to_string());
                return Err(format!("Google Calendar: {msg}"));
            }
            return Ok(if text.is_empty() { Value::Null } else { serde_json::from_str(&text).map_err(|e| e.to_string())? });
        }
        Err("Google Calendar: still throttled, will retry at the next sync".into())
    }

    fn url(segments: &[&str], query: &[(&str, &str)]) -> Url {
        let mut url = Url::parse(API).unwrap();
        // Calendar ids contain '@' and '#': push_segment percent-encodes them.
        url.path_segments_mut().unwrap().extend(segments);
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        url
    }

    /// Fetches calendars and events from a week ago to three months ahead.
    pub async fn sync(&self) -> Result<Synced, String> {
        let list: GCalList = serde_json::from_value(
            self.call(
                Method::GET,
                Self::url(
                    &["users", "me", "calendarList"],
                    &[("fields", "items(id,summary,summaryOverride,backgroundColor,primary,selected,accessRole,defaultReminders)")],
                ),
                None,
            )
            .await?,
        )
        .map_err(|e| e.to_string())?;
        let account = list.items.iter().find(|c| c.primary).map(|c| c.id.clone()).unwrap_or_default();
        let shown: Vec<&GCal> = list.items.iter().filter(|c| c.selected || c.primary).collect();

        let today = Local::now().date_naive();
        let from = local_midnight(today - Days::new(7)).to_rfc3339();
        let to = local_midnight(today + Days::new(92)).to_rfc3339();
        let fetches = shown.iter().map(|c| self.events(c, &from, &to));
        let mut events = Vec::new();
        for r in futures_util::future::join_all(fetches).await {
            events.extend(r?);
        }
        events.sort_by(|a, b| (a.start, !a.all_day, &a.title).cmp(&(b.start, !b.all_day, &b.title)));

        let calendars = shown
            .iter()
            .map(|c| CalInfo {
                id: c.id.clone(),
                name: c.summary_override.clone().unwrap_or_else(|| c.summary.clone()),
                color: c.background_color.as_deref().and_then(parse_color),
                primary: c.primary,
                writable: matches!(c.access_role.as_str(), "owner" | "writer"),
            })
            .collect();
        Ok(Synced { account, calendars, events })
    }

    async fn events(&self, cal: &GCal, from: &str, to: &str) -> Result<Vec<Event>, String> {
        let mut out = Vec::new();
        let mut page = String::new();
        loop {
            let mut q = vec![
                ("timeMin", from),
                ("timeMax", to),
                ("singleEvents", "true"),
                ("orderBy", "startTime"),
                ("maxResults", "250"),
                ("fields", "items(id,status,summary,description,location,start,end,htmlLink,reminders),nextPageToken"),
            ];
            if !page.is_empty() {
                q.push(("pageToken", &page));
            }
            let list: GEventList = serde_json::from_value(self.call(Method::GET, Self::url(&["calendars", &cal.id, "events"], &q), None).await?)
                .map_err(|e| e.to_string())?;
            for e in list.items {
                if e.status == "cancelled" {
                    continue;
                }
                let (Some((start, all_day)), Some((end, _))) = (parse_time(&e.start), parse_time(&e.end)) else { continue };
                let reminders = match &e.reminders {
                    Some(r) if !r.use_default => popup_minutes(&r.overrides),
                    _ => popup_minutes(&cal.default_reminders),
                };
                out.push(Event {
                    id: e.id,
                    cal: cal.id.clone(),
                    title: if e.summary.is_empty() { "(no title)".into() } else { e.summary },
                    description: plain_text(&e.description),
                    location: e.location,
                    start,
                    end,
                    all_day,
                    link: e.html_link,
                    reminders,
                });
            }
            match list.next_page_token {
                Some(t) => page = t,
                None => return Ok(out),
            }
        }
    }

    pub async fn create(&self, cal: &str, entry: &str) -> Result<(), String> {
        match parse(entry, Local::now()) {
            Some(e) => self.call(Method::POST, Self::url(&["calendars", cal, "events"], &[]), Some(e.body())).await.map(|_| ()),
            // No date or time we understand: let Google parse it ("Lunch with Ana tomorrow 1pm").
            None => self
                .call(Method::POST, Self::url(&["calendars", cal, "events", "quickAdd"], &[("text", entry)]), None)
                .await
                .map(|_| ()),
        }
    }

    pub async fn update(&self, cal: &str, id: &str, entry: &str) -> Result<(), String> {
        let e = parse(entry, Local::now()).ok_or("could not understand the date/time; use e.g. 2026-10-28 14:00-15:00 Title")?;
        self.call(Method::PATCH, Self::url(&["calendars", cal, "events", id], &[]), Some(e.body())).await.map(|_| ())
    }

    pub async fn delete(&self, cal: &str, id: &str) -> Result<(), String> {
        self.call(Method::DELETE, Self::url(&["calendars", cal, "events", id], &[]), None).await.map(|_| ())
    }
}

// ---- event entry parser ------------------------------------------------------

#[derive(Debug, PartialEq)]
pub struct Entry {
    pub title: String,
    pub location: String,
    pub date: NaiveDate,
    /// None for all-day events.
    pub time: Option<(NaiveTime, NaiveTime)>,
}

impl Entry {
    fn body(&self) -> Value {
        let (start, end) = match self.time {
            Some((s, e)) => {
                let at = |d: NaiveDate, t: NaiveTime| {
                    Local.from_local_datetime(&d.and_time(t)).earliest().map(|x| x.to_rfc3339()).unwrap_or_default()
                };
                // An end before the start means it runs past midnight.
                let end_day = if e <= s { self.date + Days::new(1) } else { self.date };
                (json!({ "dateTime": at(self.date, s) }), json!({ "dateTime": at(end_day, e) }))
            }
            None => (
                json!({ "date": self.date.format("%Y-%m-%d").to_string() }),
                json!({ "date": (self.date + Days::new(1)).format("%Y-%m-%d").to_string() }),
            ),
        };
        json!({ "summary": self.title, "location": self.location, "start": start, "end": end })
    }
}

fn weekday(word: &str) -> Option<Weekday> {
    Some(match word {
        "monday" | "mon" | "montag" => Weekday::Mon,
        "tuesday" | "tue" | "dienstag" => Weekday::Tue,
        "wednesday" | "wed" | "mittwoch" => Weekday::Wed,
        "thursday" | "thu" | "donnerstag" => Weekday::Thu,
        "friday" | "fri" | "freitag" => Weekday::Fri,
        "saturday" | "sat" | "samstag" | "sonnabend" => Weekday::Sat,
        "sunday" | "sun" | "sonntag" => Weekday::Sun,
        _ => return None,
    })
}

fn parse_date(word: &str, today: NaiveDate) -> Option<NaiveDate> {
    match word {
        "today" | "heute" => return Some(today),
        "tomorrow" | "morgen" => return Some(today + Days::new(1)),
        "übermorgen" | "uebermorgen" => return Some(today + Days::new(2)),
        _ => {}
    }
    if let Some(wd) = weekday(word) {
        let ahead = (wd.num_days_from_monday() + 7 - today.weekday().num_days_from_monday()) % 7;
        return Some(today + Days::new(ahead as u64));
    }
    if let Ok(d) = NaiveDate::parse_from_str(word, "%Y-%m-%d") {
        return Some(d);
    }
    // 28.10.2026, 28.10.26, 28.10., 28.10
    let parts: Vec<&str> = word.trim_end_matches('.').split('.').collect();
    let num = |s: &str| s.parse::<u32>().ok();
    match parts.as_slice() {
        [d, m] => {
            let date = NaiveDate::from_ymd_opt(today.year(), num(m)?, num(d)?)?;
            // A date that already passed this year means next year.
            Some(if date < today { NaiveDate::from_ymd_opt(today.year() + 1, num(m)?, num(d)?)? } else { date })
        }
        [d, m, y] => {
            let y = num(y)? as i32;
            NaiveDate::from_ymd_opt(if y < 100 { 2000 + y } else { y }, num(m)?, num(d)?)
        }
        _ => None,
    }
}

fn parse_clock(s: &str) -> Option<NaiveTime> {
    let s = s.trim_end_matches("uhr").trim_end_matches('h');
    let (h, m) = s.split_once(':').unwrap_or((s, "0"));
    NaiveTime::from_hms_opt(h.parse().ok()?, m.parse().ok()?, 0)
}

/// "14:00", "14:00-15:30", "9-11", "14uhr", "9h". Bare numbers are not
/// times, so titles like "Team 2" stay intact.
fn parse_times(word: &str) -> Option<(NaiveTime, NaiveTime)> {
    let explicit = |s: &str| s.contains(':') || s.ends_with("uhr") || s.ends_with('h');
    match word.split_once('-') {
        Some((a, b)) => Some((parse_clock(a)?, parse_clock(b)?)),
        None if explicit(word) => {
            let s = parse_clock(word)?;
            Some((s, s + chrono::Duration::hours(1)))
        }
        None => None,
    }
}

/// Parses "morgen 14:00 Zahnarzt @ Praxis Dr. X", "fri 9:30-11 standup",
/// "28.10. Geburtstag Ana". None when there is no date or time in it.
pub fn parse(text: &str, now: DateTime<Local>) -> Option<Entry> {
    let (main, location) = match text.split_once(" @ ") {
        Some((m, l)) => (m, l.trim().to_string()),
        None => (text, String::new()),
    };
    let today = now.date_naive();
    let (mut date, mut time) = (None, None);
    let mut title = Vec::new();
    for word in main.split_whitespace() {
        let lower = word.to_lowercase();
        if date.is_none() {
            if let Some(d) = parse_date(&lower, today) {
                date = Some(d);
                continue;
            }
        }
        if time.is_none() {
            if let Some(t) = parse_times(&lower) {
                time = Some(t);
                continue;
            }
        }
        title.push(word);
    }
    if date.is_none() && time.is_none() {
        return None;
    }
    Some(Entry { title: title.join(" "), location, date: date.unwrap_or(today), time })
}

// ---- grid layout -----------------------------------------------------------

/// Rows per day in the time grid (30 minutes each).
pub const SLOTS: usize = 48;

/// Start of the 30-minute slot containing `t`.
pub fn floor_slot(t: NaiveDateTime) -> NaiveDateTime {
    t.date().and_hms_opt(t.hour(), t.minute() / 30 * 30, 0).unwrap_or(t)
}

pub fn to_local(t: NaiveDateTime) -> DateTime<Local> {
    Local.from_local_datetime(&t).earliest().unwrap_or_else(|| Local.from_utc_datetime(&t))
}

/// Places overlapping events side by side, like a calendar app's day view.
/// Returns (index into `events`, lane, number of lanes in its overlap group).
pub fn layout_lanes(events: &[&Event]) -> Vec<(usize, usize, usize)> {
    let mut order: Vec<usize> = (0..events.len()).collect();
    order.sort_by_key(|&i| (events[i].start, std::cmp::Reverse(events[i].end)));
    let mut out = Vec::with_capacity(events.len());
    let mut group: Vec<(usize, usize)> = Vec::new();
    let mut lane_ends: Vec<DateTime<Local>> = Vec::new();
    let mut group_end: Option<DateTime<Local>> = None;
    for i in order {
        let e = events[i];
        if group_end.is_some_and(|end| e.start >= end) {
            out.extend(group.drain(..).map(|(i, lane)| (i, lane, lane_ends.len())));
            lane_ends.clear();
            group_end = None;
        }
        let lane = match lane_ends.iter().position(|end| *end <= e.start) {
            Some(l) => {
                lane_ends[l] = e.end;
                l
            }
            None => {
                lane_ends.push(e.end);
                lane_ends.len() - 1
            }
        };
        group.push((i, lane));
        group_end = Some(group_end.map_or(e.end, |g| g.max(e.end)));
    }
    out.extend(group.drain(..).map(|(i, lane)| (i, lane, lane_ends.len())));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Local> {
        // Tuesday, 27 Oct 2026, 10:00
        Local.with_ymd_and_hms(2026, 10, 27, 10, 0, 0).unwrap()
    }

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn parses_entries() {
        let e = parse("morgen 14:00 Zahnarzt @ Praxis Dr. X", now()).unwrap();
        assert_eq!(e, Entry { title: "Zahnarzt".into(), location: "Praxis Dr. X".into(), date: d(10, 28), time: Some((t(14, 0), t(15, 0))) });
        let e = parse("fri 9:30-11 standup", now()).unwrap();
        assert_eq!((e.date, e.time, e.title.as_str()), (d(10, 30), Some((t(9, 30), t(11, 0))), "standup"));
        let e = parse("28.10. Geburtstag Ana", now()).unwrap();
        assert_eq!((e.date, e.time), (d(10, 28), None));
        let e = parse("Team 2 sync 2026-11-03 16uhr", now()).unwrap();
        assert_eq!((e.title.as_str(), e.date, e.time), ("Team 2 sync", d(11, 3), Some((t(16, 0), t(17, 0)))));
        assert_eq!(parse("1.1. Neujahr", now()).unwrap().date, NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
        assert_eq!(parse("dienstag 8h gym", now()).unwrap().date, d(10, 27));
        assert!(parse("lunch with Ana", now()).is_none());
    }

    #[test]
    fn round_trips_through_edit_text() {
        let ev = Event {
            id: "x".into(),
            cal: "c".into(),
            title: "Review".into(),
            description: String::new(),
            location: "Room 4".into(),
            start: Local.with_ymd_and_hms(2026, 11, 2, 9, 0, 0).unwrap(),
            end: Local.with_ymd_and_hms(2026, 11, 2, 10, 30, 0).unwrap(),
            all_day: false,
            link: String::new(),
            reminders: vec![],
        };
        let e = parse(&ev.to_entry(), now()).unwrap();
        assert_eq!((e.title.as_str(), e.location.as_str(), e.date, e.time), ("Review", "Room 4", d(11, 2), Some((t(9, 0), t(10, 30)))));
    }

    #[test]
    fn lanes_for_overlaps() {
        let ev = |h0: u32, h1: u32| Event {
            id: format!("{h0}"),
            cal: "c".into(),
            title: String::new(),
            description: String::new(),
            location: String::new(),
            start: Local.with_ymd_and_hms(2026, 11, 2, h0, 0, 0).unwrap(),
            end: Local.with_ymd_and_hms(2026, 11, 2, h1, 0, 0).unwrap(),
            all_day: false,
            link: String::new(),
            reminders: vec![],
        };
        // 9-11 and 10-12 overlap (2 lanes); 11-12 reuses lane 0; 14-15 stands alone.
        let evs = [ev(9, 11), ev(10, 12), ev(11, 12), ev(14, 15)];
        let refs: Vec<&Event> = evs.iter().collect();
        let mut l = layout_lanes(&refs);
        l.sort();
        assert_eq!(l, vec![(0, 0, 2), (1, 1, 2), (2, 0, 2), (3, 0, 1)]);
        let t = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap().and_hms_opt(14, 47, 12).unwrap();
        assert_eq!(floor_slot(t).time(), NaiveTime::from_hms_opt(14, 30, 0).unwrap());
    }

    #[test]
    fn strips_html() {
        assert_eq!(plain_text("Hi&nbsp;<b>there</b><br>Join: <a href=\"x\">link</a> &amp; more"), "Hi there\nJoin: link & more");
    }
}

// ---- state -----------------------------------------------------------------

#[derive(Default)]
pub struct CalState {
    pub account: String,
    pub status: String,
    pub calendars: HashMap<String, CalInfo>,
    /// Everything shown: Google events plus To Do tasks with a due date.
    pub events: Vec<Event>,
    pub syncing: bool,
    pub next_poll: Option<Instant>,
    /// Reminders up to this time have already fired.
    pub reminded_until: Option<DateTime<Local>>,
    google: Vec<Event>,
    tasks: Vec<Event>,
    /// The task apps shown as calendars (also in `calendars`).
    task_cals: Vec<CalInfo>,
}

impl CalState {
    pub fn apply(&mut self, s: Synced) {
        self.account = s.account;
        self.calendars = s.calendars.into_iter().map(|c| (c.id.clone(), c)).collect();
        self.google = s.events;
        self.status = format!("synced {}", Local::now().format("%H:%M"));
        self.merge();
    }

    /// Tasks to show as all-day entries (see `todo::TodoState::as_events`),
    /// with the calendars they belong to: (id, name, color) per task app.
    pub fn set_tasks(&mut self, tasks: Vec<Event>, cals: &[(&str, &str, [u8; 3])]) {
        self.tasks = tasks;
        for c in self.task_cals.drain(..) {
            self.calendars.remove(&c.id);
        }
        self.task_cals = cals
            .iter()
            .filter(|(id, ..)| self.tasks.iter().any(|t| t.cal == *id))
            .map(|&(id, name, color)| CalInfo { id: id.into(), name: name.into(), color: Some(color), primary: false, writable: false })
            .collect();
        self.merge();
    }

    /// Whether an event is a task from a task app.
    pub fn is_task(&self, e: &Event) -> bool {
        self.task_cals.iter().any(|c| c.id == e.cal)
    }

    fn merge(&mut self) {
        for c in &self.task_cals {
            self.calendars.insert(c.id.clone(), c.clone());
        }
        self.events = self.google.iter().chain(&self.tasks).cloned().collect();
        self.events.sort_by(|a, b| (a.start, !a.all_day, &a.title).cmp(&(b.start, !b.all_day, &b.title)));
    }

    pub fn find(&self, key: &str) -> Option<&Event> {
        self.events.iter().find(|e| e.key() == key)
    }

    /// Calendar new events go to: the primary one, else the first writable.
    pub fn default_calendar(&self) -> Option<&str> {
        let mut cals: Vec<&CalInfo> = self.calendars.values().filter(|c| c.writable).collect();
        cals.sort_by_key(|c| !c.primary);
        cals.first().map(|c| c.id.as_str())
    }

    /// The next timed event that hasn't ended, if it starts within 12 hours.
    pub fn next_event(&self, now: DateTime<Local>) -> Option<&Event> {
        self.events
            .iter()
            .filter(|e| !e.all_day && e.end > now)
            .find(|e| e.start - now < chrono::Duration::hours(12))
    }

    /// Reminders that came due since the last call.
    pub fn due(&mut self, now: DateTime<Local>) -> Vec<&Event> {
        let since = self.reminded_until.replace(now).unwrap_or(now);
        self.events.iter().filter(|e| e.reminder_times().any(|t| t > since && t <= now)).collect()
    }

    /// When something time-based changes next: a reminder, an event starting
    /// or ending (statusline, dimming), or the next poll.
    pub fn next_wake(&self, now: DateTime<Local>) -> Option<Instant> {
        let soonest = self
            .events
            .iter()
            .flat_map(|e| e.reminder_times().chain([e.start, e.end]))
            .filter(|t| *t > now)
            .min();
        let at = soonest.and_then(|t| (t - now).to_std().ok()).map(|d| Instant::now() + d);
        match (at, self.next_poll) {
            (Some(a), Some(p)) => Some(a.min(p)),
            (a, p) => a.or(p),
        }
    }
}
