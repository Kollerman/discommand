//! Moodle (any site): courses, their contents and upcoming deadlines.
//!
//! Three ways in, picked by what the site allows:
//! - the official app interface (web services with a token), where the site
//!   has "mobile web services" on; the password is used once and not kept;
//! - otherwise Moodle's own login form, like a browser, then the same
//!   internal interface (`lib/ajax/service.php`) Moodle's pages use. The
//!   password is kept (encrypted) to sign in again when the session ends;
//! - sites that sign in through the university's login page (single sign-on,
//!   e.g. Shibboleth/SAML): discommand walks that chain of pages like a
//!   browser (forwarding forms, then the login form with user name and
//!   password). The password is kept (encrypted) for signing in again;
//! - if that fails (e.g. two-factor codes): you log in in the browser and
//!   hand over its Moodle session cookie once. It's kept alive while
//!   discommand runs.
//!
//! Courses are lists in a task tab: the sidebar shows them and a
//! "Deadlines" list; a course opens as a page of its sections and activities.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, TimeZone};
use reqwest::cookie::Jar;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::secret;
use crate::todo::{Page, Task, TaskList};

const SECRET_NAME: &str = "moodle";
const DEADLINES: &str = "deadlines";
/// Deadlines this long past still show (as overdue).
const OVERDUE_DAYS: i64 = 14;

#[derive(Clone, Serialize, Deserialize)]
pub struct Creds {
    /// https://host[/path], without a trailing slash.
    pub site: String,
    pub site_name: String,
    /// The app interface's token, if the site has it on.
    #[serde(default)]
    pub token: String,
    /// Otherwise the login form's user name and password.
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Or the browser's session cookie (single sign-on sites): name, value.
    #[serde(default)]
    pub cookie: Option<(String, String)>,
    /// Single sign-on: where the university login starts (with `username`
    /// and `password` filled in on its login page).
    #[serde(default)]
    pub sso: String,
}

impl Creds {
    pub fn load() -> Option<Creds> {
        serde_json::from_slice(&secret::load_secret(SECRET_NAME)?).ok()
    }

    fn save(&self) -> Result<(), String> {
        let data = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        secret::save_secret(SECRET_NAME, &data).map(|_| ()).map_err(|e| e.to_string())
    }

    pub fn remove() {
        secret::remove_secret(SECRET_NAME);
    }
}

/// "elearning.uni-x.de" or a full URL → "https://elearning.uni-x.de".
fn normalize_site(input: &str) -> Result<String, String> {
    let s = input.trim().trim_end_matches('/');
    let s = if s.contains("://") { s.to_string() } else { format!("https://{s}") };
    let url = Url::parse(&s).map_err(|_| format!("{input} isn't a web address"))?;
    if url.scheme() != "https" {
        return Err("the Moodle site must use https (the password is sent to it)".into());
    }
    // A pasted page address ("…/login/index.php", "…/my/") points at the site.
    let mut path = url.path().trim_end_matches('/').to_string();
    for page in ["/login/index.php", "/my", "/my/courses.php", "/index.php"] {
        if let Some(p) = path.strip_suffix(page) {
            path = p.to_string();
        }
    }
    Ok(format!("https://{}{}", url.host_str().unwrap_or(""), path))
}

/// The text after `prefix` up to the next `"` in `html`.
fn between<'a>(html: &'a str, prefix: &str) -> Option<&'a str> {
    let start = html.find(prefix)? + prefix.len();
    let len = html[start..].find('"')?;
    Some(&html[start..start + len])
}

fn http() -> reqwest::Client {
    client(Arc::new(Jar::default()))
}

fn client(jar: Arc<Jar>) -> reqwest::Client {
    reqwest::Client::builder()
        .cookie_provider(jar)
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("Mozilla/5.0 (compatible; discommand/", env!("CARGO_PKG_VERSION"), ")"))
        .build()
        .unwrap_or_default()
}

/// Whether the login page has Moodle's own user name and password form (not
/// just the guest button and links to a single sign-on page).
fn has_password_form(page: &str) -> bool {
    page.contains(r#"id="username""#) && page.contains(r#"id="password""#)
}

// ---- login (in the terminal: `discommand --moodle-login`) --------------------------

fn prompt(label: &str) -> Result<String, String> {
    use std::io::Write;
    print!("{label}");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(line.trim().to_string())
}

/// Asks for the site and account, checks them and saves the login.
pub async fn login_interactive() -> Result<(), String> {
    let site = normalize_site(&prompt("Moodle site (e.g. elearning.uni-regensburg.de): ")?)?;
    let config = public_config(&site).await?;
    let site_name = config["sitename"].as_str().unwrap_or("Moodle").to_string();
    println!("Found: {site_name}");
    let app = config["enablemobilewebservice"].as_i64() == Some(1);
    let (page, cookie_name) = login_page(&site).await?;
    if !app && !has_password_form(&page) {
        let Some(start) = sso_start(&site, &config, &page) else {
            return login_in_browser(site, site_name, cookie_name).await;
        };
        println!("{site_name} signs in through its university login page; discommand can do that for you.");
        let username = prompt("User name (as on that page): ")?;
        let password = rpassword::prompt_password("Password (input hidden): ").map_err(|e| e.to_string())?;
        let creds = Creds { site: site.clone(), site_name: site_name.clone(), token: String::new(), username, password, cookie: None, sso: start };
        match Moodle::new(creds.clone()).check().await {
            Ok(name) => {
                creds.save()?;
                println!("Connected to {site_name} as {name} (via the university login; the password is stored encrypted).");
                return Ok(());
            }
            Err(e) => {
                println!("\nThat didn't work: {e}");
                println!("Falling back to logging in in the browser.");
                return login_in_browser(site, site_name, cookie_name).await;
            }
        }
    }
    let username = prompt("User name: ")?;
    let password = rpassword::prompt_password("Password (input hidden): ").map_err(|e| e.to_string())?;

    let mut creds = Creds { site: site.clone(), site_name, token: String::new(), username, password, cookie: None, sso: String::new() };
    // The app interface first: then no password needs to be kept.
    if app {
        match app_token(&site, &creds.username, &creds.password).await {
            Ok(token) => {
                creds.token = token;
                creds.password.clear();
            }
            Err(e) => println!("The app interface didn't work ({e}); trying the login form."),
        }
    }
    let moodle = Moodle::new(creds.clone());
    let name = moodle.check().await?;
    creds.save()?;
    let how = if creds.token.is_empty() { "the login form; the password is stored encrypted" } else { "the app interface" };
    println!("Connected to {} as {name} (via {how}).", creds.site_name);
    Ok(())
}

/// Where a site's single sign-on starts: the login button Moodle lists for
/// it (the app settings name it), else a link to an auth plugin on the page.
fn sso_start(site: &str, config: &Value, login_page: &str) -> Option<String> {
    let listed = config["identityproviders"].as_array().into_iter().flatten().filter_map(|p| p["url"].as_str()).next();
    let linked = || {
        ["/auth/shibboleth/", "/auth/saml2/", "/auth/cas/"].iter().find_map(|plugin| {
            let i = login_page.find(&format!("{site}{plugin}"))?;
            let end = login_page[i..].find(['"', '\'']).map(|e| i + e)?;
            Some(unescape(&login_page[i..end]))
        })
    };
    listed.map(String::from).or_else(linked)
}

/// The login page, and the name of the site's session cookie.
async fn login_page(site: &str) -> Result<(String, String), String> {
    let resp = http().get(format!("{site}/login/index.php")).send().await.map_err(|e| format!("couldn't reach {site}: {e}"))?;
    let name = resp.cookies().map(|c| c.name().to_string()).find(|n| n.starts_with("MoodleSession")).unwrap_or_else(|| "MoodleSession".into());
    Ok((resp.text().await.map_err(|e| e.to_string())?, name))
}

/// Single sign-on: log in in the browser, then paste its session cookie.
async fn login_in_browser(site: String, site_name: String, cookie_name: String) -> Result<(), String> {
    let host = Url::parse(&site).ok().and_then(|u| u.host_str().map(String::from)).unwrap_or_default();
    let url = format!("{site}/my/");
    println!("\n{site_name} signs in through its own login page, so log in in your browser:");
    println!("\n  {url}\n");
    let _ = crate::app::open_url(&url);
    println!("Once you're in, hand discommand the browser's Moodle session (one time):");
    println!("  1. Press F12 (developer tools).");
    println!("  2. Chrome/Edge: Application → Cookies → https://{host}");
    println!("     Firefox:     Storage → Cookies → https://{host}");
    println!("  3. Copy the value of \"{cookie_name}\" and paste it here.");
    println!("Keep the browser tab or close it, but don't log out there: that ends this session too.\n");
    loop {
        let input = rpassword::prompt_password(format!("{cookie_name} value (input hidden): ")).map_err(|e| e.to_string())?;
        // "Name=value" or the bare value.
        let value = input.trim().trim_start_matches(&format!("{cookie_name}=")).trim_matches(['"', ';', ' ']).to_string();
        if value.is_empty() {
            return Err("no session given".into());
        }
        let creds = Creds {
            site: site.clone(),
            site_name: site_name.clone(),
            token: String::new(),
            username: String::new(),
            password: String::new(),
            cookie: Some((cookie_name.clone(), value)),
            sso: String::new(),
        };
        match Moodle::new(creds.clone()).check().await {
            Ok(name) => {
                creds.save()?;
                println!("Connected to {site_name} as {name} (via the browser's session).");
                println!("If it ends (e.g. after the PC was off for hours), run discommand --moodle-login again.");
                return Ok(());
            }
            Err(e) => println!("That didn't work ({e}). Copy the value again (the whole of it), or press Enter to stop."),
        }
    }
}

/// The settings every Moodle publishes for its app, without a login.
async fn public_config(site: &str) -> Result<Value, String> {
    let body = json!([{ "index": 0, "methodname": "tool_mobile_get_public_config", "args": {} }]);
    let resp: Value = http()
        .post(format!("{site}/lib/ajax/service-nologin.php?info=tool_mobile_get_public_config"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("couldn't reach {site}: {e}"))?
        .json()
        .await
        .map_err(|_| format!("{site} doesn't look like a Moodle site"))?;
    let first = &resp[0];
    if first["error"].as_bool() == Some(false) {
        Ok(first["data"].clone())
    } else {
        // Very old Moodles lack this; the login form may still work.
        Ok(json!({ "sitename": "Moodle" }))
    }
}

async fn app_token(site: &str, username: &str, password: &str) -> Result<String, String> {
    let v: Value = http()
        .post(format!("{site}/login/token.php"))
        .form(&[("username", username), ("password", password), ("service", "moodle_mobile_app")])
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    v["token"].as_str().map(String::from).ok_or_else(|| v["error"].as_str().unwrap_or("no token").to_string())
}

// ---- single sign-on: walking the login pages -------------------------------------

/// An HTML form, as far as signing in needs it.
#[derive(Debug, Default)]
struct Form {
    action: String,
    method: String,
    /// (type, name, value) of inputs and named buttons.
    fields: Vec<(String, String, String)>,
}

impl Form {
    fn has_password(&self) -> bool {
        self.fields.iter().any(|(t, ..)| t == "password")
    }

    /// Only hidden fields (or none): a page that forwards itself by script
    /// (SAML, session checks).
    fn forwards(&self) -> bool {
        self.fields.iter().all(|(t, ..)| matches!(t.as_str(), "hidden" | "submit" | "button" | "radio" | "checkbox"))
    }

    /// The fields to send: hidden ones as they are, the user name into the
    /// first text field and the password into the password field.
    fn filled(&self, username: &str, password: &str) -> Vec<(String, String)> {
        let mut user_done = false;
        let mut out = Vec::new();
        for (t, name, value) in &self.fields {
            if name.is_empty() {
                continue;
            }
            match t.as_str() {
                "hidden" => out.push((name.clone(), value.clone())),
                "password" => out.push((name.clone(), password.to_string())),
                "text" | "email" | "" if !user_done => {
                    user_done = true;
                    out.push((name.clone(), username.to_string()));
                }
                // Pre-selected options (e.g. how long a consent is remembered).
                "radio" | "checkbox" => out.push((name.clone(), value.clone())),
                _ => {}
            }
        }
        // One button is pressed: the one that goes on ("proceed", "accept",
        // "login"), never one that rejects or cancels.
        let buttons: Vec<&(String, String, String)> = self.fields.iter().filter(|(t, n, _)| t == "submit" && !n.is_empty()).collect();
        let says = |f: &&&(String, String, String), words: &[&str]| {
            let both = format!("{} {}", f.1, f.2).to_ascii_lowercase();
            words.iter().any(|w| both.contains(w))
        };
        let pressed = buttons
            .iter()
            .find(|f| says(f, &["proceed", "accept", "akzept", "continue", "weiter", "login", "anmelden", "submit"]))
            .or_else(|| buttons.iter().find(|f| !says(f, &["reject", "ablehn", "cancel", "abbrech", "deny"])));
        if let Some((_, name, value)) = pressed {
            out.push((name.clone(), value.clone()));
        }
        out
    }
}

/// Decodes the HTML entities found in attribute values.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let c = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The attributes of a tag's text (`<input type="x" name='y' checked>`).
fn attributes(tag: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let b = tag.as_bytes();
    let mut i = tag.find(char::is_whitespace).unwrap_or(tag.len());
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'>' {
            i += 1;
        }
        let name = tag[start..i].to_ascii_lowercase();
        if name.is_empty() {
            break;
        }
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let end = tag[i + 1..].find(q as char).map_or(b.len(), |e| i + 1 + e);
                value = unescape(&tag[i + 1..end]);
                i = end + 1;
            } else {
                let start = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
                value = unescape(&tag[start..i]);
            }
        }
        out.insert(name, value);
    }
    out
}

/// The forms of a page with their inputs (and named buttons).
fn forms(html: &str) -> Vec<Form> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<form").map(|i| pos + i) {
        let Some(tag_end) = lower[start..].find('>').map(|i| start + i) else { break };
        let end = lower[tag_end..].find("</form").map_or(html.len(), |i| tag_end + i);
        let attrs = attributes(&html[start + 1..tag_end]);
        let mut form = Form {
            action: attrs.get("action").cloned().unwrap_or_default(),
            method: attrs.get("method").cloned().unwrap_or_else(|| "get".into()).to_ascii_lowercase(),
            fields: Vec::new(),
        };
        let mut i = tag_end;
        while let Some(t) = lower[i..end].find('<').map(|j| i + j) {
            let Some(close) = lower[t..end].find('>').map(|j| t + j) else { break };
            let tag = &lower[t + 1..close];
            if tag.starts_with("input") || tag.starts_with("button") {
                let a = attributes(&html[t + 1..close]);
                let kind = a.get("type").cloned().unwrap_or_else(|| if tag.starts_with("button") { "submit".into() } else { "text".into() });
                let kind = kind.to_ascii_lowercase();
                let unchecked = matches!(kind.as_str(), "radio" | "checkbox") && !a.contains_key("checked");
                if let Some(name) = a.get("name").filter(|_| !unchecked) {
                    form.fields.push((kind, name.clone(), a.get("value").cloned().unwrap_or_default()));
                }
            }
            i = close + 1;
        }
        out.push(form);
        pos = end.max(tag_end + 1);
    }
    out
}

/// Whether a page asks for a one-time code (two-factor), which can't be
/// filled in automatically.
fn asks_for_code(form: &Form) -> bool {
    !form.has_password()
        && form.fields.iter().any(|(t, name, _)| {
            let n = name.to_ascii_lowercase();
            matches!(t.as_str(), "text" | "number" | "tel") && ["otp", "totp", "code", "token", "pin"].iter().any(|k| n.contains(k))
        })
}

/// Where a page sends the browser on by itself without a form: a refresh
/// tag (`<meta http-equiv="refresh" content="0;url=…">`) or a script
/// (`location.href = '…'`, `window.location.replace("…")`, …).
fn page_redirect(page: &str) -> Option<String> {
    let lower = page.to_ascii_lowercase();
    if let Some(i) = lower.find("http-equiv=\"refresh\"").or_else(|| lower.find("http-equiv='refresh'")) {
        let tag_start = lower[..i].rfind('<')?;
        let tag_end = tag_start + lower[tag_start..].find('>')?;
        let content = attributes(&page[tag_start + 1..tag_end]).remove("content")?;
        if let Some(u) = content.to_ascii_lowercase().find("url=") {
            return Some(content[u + 4..].trim().trim_matches(['\'', '"']).to_string());
        }
    }
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("location").map(|i| pos + i) {
        pos = i + 8;
        let mut rest = &page[pos..];
        for prefix in [".href", ".replace", ".assign"] {
            if rest.to_ascii_lowercase().starts_with(prefix) {
                rest = &rest[prefix.len()..];
            }
        }
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=').or_else(|| rest.strip_prefix('(')) else { continue };
        let rest = rest.trim_start();
        let Some(q) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') else { continue };
        let Some(end) = rest[1..].find(q) else { continue };
        let target = unescape(&rest[1..1 + end]).replace("\\/", "/");
        if !target.is_empty() && !target.starts_with('#') && !target.to_ascii_lowercase().starts_with("javascript:") {
            return Some(target);
        }
    }
    None
}

/// A trace of the login steps for finding out where a site's login differs:
/// addresses and form field names only, never values or passwords. On
/// with `DISCOMMAND_MOODLE_LOG=<file>`.
struct LoginLog(std::sync::Mutex<Vec<String>>);

impl LoginLog {
    fn open() -> LoginLog {
        LoginLog(std::sync::Mutex::new(Vec::new()))
    }

    fn note(&self, text: &str) {
        self.0.lock().unwrap().push(text.to_string());
    }

    fn page(&self, step: usize, url: &Url, status: u16, forms: &[Form]) {
        self.note(&format!("step {step}: {}{} (HTTP {status})", url.host_str().unwrap_or(""), url.path()));
        for f in forms {
            let fields: Vec<String> = f.fields.iter().map(|(t, n, _)| format!("{n}:{t}")).collect();
            let action = f.action.split('?').next().unwrap_or("");
            self.note(&format!("  form {} {action} [{}]", f.method, fields.join(", ")));
        }
    }

    /// The page the login got stuck on: its visible text and the start of
    /// its scripts (no form values).
    fn stuck_on(&self, page: &str) {
        let without = |s: &str, tag: &str| -> String {
            let lower = s.to_ascii_lowercase();
            let mut out = String::new();
            let mut pos = 0;
            while let Some(i) = lower[pos..].find(&format!("<{tag}")).map(|i| pos + i) {
                out.push_str(&s[pos..i]);
                pos = lower[i..].find(&format!("</{tag}>")).map_or(s.len(), |e| i + e + tag.len() + 3);
            }
            out.push_str(&s[pos.min(s.len())..]);
            out
        };
        let text = plain(&without(&without(page, "script"), "style"));
        let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        self.note(&format!("  page text: {}", text.chars().take(600).collect::<String>()));
        let lower = page.to_ascii_lowercase();
        let mut pos = 0;
        while let Some(i) = lower[pos..].find("<script").map(|i| pos + i) {
            let Some(open) = lower[i..].find('>').map(|o| i + o + 1) else { break };
            let close = lower[open..].find("</script").map_or(page.len(), |c| open + c);
            let code: String = page[open..close].split_whitespace().collect::<Vec<_>>().join(" ");
            if !code.is_empty() {
                self.note(&format!("  script: {}", code.chars().take(300).collect::<String>()));
            }
            pos = close;
        }
    }

    /// Writes the log next to the cache; returns where.
    fn save(&self) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("DISCOMMAND_MOODLE_LOG").map(std::path::PathBuf::from).unwrap_or_else(|| secret::cache_dir().join("moodle-login.txt"));
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!("discommand {} Moodle login steps (addresses and field names only)\n{}\n", env!("CARGO_PKG_VERSION"), self.0.lock().unwrap().join("\n"));
        std::fs::write(&path, text).ok().map(|_| path)
    }
}

// ---- client --------------------------------------------------------------------

pub struct Moodle {
    creds: Creds,
    http: reqwest::Client,
    /// The browser-style session's key (login-form access only).
    sesskey: tokio::sync::Mutex<Option<String>>,
    /// Course module of each deadline (to tick it off) and course names.
    cms: std::sync::Mutex<HashMap<String, i64>>,
    courses: std::sync::Mutex<HashMap<String, String>>,
}

impl Moodle {
    pub fn new(creds: Creds) -> Self {
        let jar = Arc::new(Jar::default());
        if let (Some((name, value)), Ok(url)) = (&creds.cookie, Url::parse(&creds.site)) {
            jar.add_cookie_str(&format!("{name}={value}; Path=/; Secure; HttpOnly"), &url);
        }
        Self {
            creds,
            http: client(jar),
            sesskey: tokio::sync::Mutex::new(None),
            cms: Default::default(),
            courses: Default::default(),
        }
    }

    pub fn site_name(&self) -> &str {
        &self.creds.site_name
    }

    /// How often it's synced; a browser session is kept alive by it.
    pub fn poll(&self) -> Duration {
        if self.creds.cookie.is_some() {
            Duration::from_secs(15 * 60)
        } else {
            POLL
        }
    }

    /// Signs in through the login form (or picks up the browser's session);
    /// returns the session key.
    async fn sign_in(&self) -> Result<String, String> {
        if self.creds.cookie.is_some() {
            let ended = "the browser's Moodle session has ended: run discommand --moodle-login to log in again";
            let resp = self.http.get(format!("{}/my/", self.creds.site)).send().await.map_err(|e| e.to_string())?;
            // Logged out, Moodle sends you to the login page (which has a key too).
            if resp.url().path().ends_with("/login/index.php") {
                return Err(ended.into());
            }
            let page = resp.text().await.map_err(|e| e.to_string())?;
            return between(&page, r#""sesskey":""#).map(String::from).ok_or_else(|| ended.into());
        }
        if !self.creds.sso.is_empty() {
            return self.sign_in_sso().await;
        }
        let url = format!("{}/login/index.php", self.creds.site);
        let page = self.http.get(&url).send().await.map_err(|e| e.to_string())?.text().await.map_err(|e| e.to_string())?;
        if !has_password_form(&page) {
            return Err("this site signs in through its own login page: run discommand --moodle-login to log in in the browser".into());
        }
        let Some(token) = between(&page, r#"name="logintoken" value=""#) else {
            return Err("found no login token on the Moodle login page".into());
        };
        let form = [("username", self.creds.username.as_str()), ("password", self.creds.password.as_str()), ("logintoken", token), ("anchor", "")];
        let resp = self.http.post(&url).form(&form).send().await.map_err(|e| e.to_string())?;
        let landed = resp.url().path().to_string();
        let page = resp.text().await.map_err(|e| e.to_string())?;
        if landed.ends_with("/login/index.php") {
            return Err("Moodle didn't accept the user name or password".into());
        }
        between(&page, r#""sesskey":""#).map(String::from).ok_or_else(|| "signed in, but found no session key".into())
    }

    /// Walks the university login like a browser: forwarding pages are sent
    /// on, the login form gets user name and password, until Moodle is
    /// reached signed in. Returns the session key.
    async fn sign_in_sso(&self) -> Result<String, String> {
        let log = LoginLog::open();
        self.walk_sso(&log).await.map_err(|e| match log.save() {
            Some(path) => format!("{e} (steps logged in {})", path.display()),
            None => e,
        })
    }

    async fn walk_sso(&self, log: &LoginLog) -> Result<String, String> {
        let site = Url::parse(&self.creds.site).map_err(|e| e.to_string())?;
        let mut resp = self.http.get(&self.creds.sso).send().await.map_err(|e| e.to_string())?;
        let mut sent_password = false;
        for step in 1..=25 {
            let url = resp.url().clone();
            let status = resp.status().as_u16();
            let page = resp.text().await.map_err(|e| e.to_string())?;
            let at_moodle = url.host_str() == site.host_str();
            let all = forms(&page);
            log.page(step, &url, status, &all);
            if at_moodle && !url.path().contains("/login/") && !url.path().contains("/auth/") && !url.path().contains("Shibboleth.sso") {
                if let Some(key) = between(&page, r#""sesskey":""#) {
                    log.note("signed in");
                    return Ok(key.to_string());
                }
            }
            // The login form, then SAML hand-overs, then pages that move on by
            // script or a refresh tag, then other self-submitting forms.
            let saml = |f: &&Form| f.forwards() && f.fields.iter().any(|(_, n, _)| n.starts_with("SAML"));
            let form = all.iter().find(|f| f.has_password()).or_else(|| all.iter().find(saml));
            if form.is_none() {
                if let Some(next) = page_redirect(&page).and_then(|to| url.join(&to).ok()) {
                    log.note(&format!("follows a script/refresh redirect to {}{}", next.host_str().unwrap_or(""), next.path()));
                    resp = self.http.get(next).send().await.map_err(|e| e.to_string())?;
                    continue;
                }
            }
            let form = form.or_else(|| all.iter().find(|f| f.forwards()));
            let Some(form) = form else {
                log.stuck_on(&page);
                if all.iter().any(asks_for_code) {
                    return Err("the university login asks for a one-time code (two-factor), which discommand can't fill in".into());
                }
                if at_moodle && url.path().ends_with("/login/index.php") {
                    return Err("signed in at the university, but Moodle sent back to its login page".into());
                }
                return Err(format!("unexpected page during the university login ({}{})", url.host_str().unwrap_or(""), url.path()));
            };
            if form.has_password() {
                if sent_password {
                    log.stuck_on(&page);
                    return Err("the university login didn't accept the user name or password".into());
                }
                sent_password = true;
            }
            let fields = form.filled(&self.creds.username, &self.creds.password);
            let action = if form.action.is_empty() { url.clone() } else { url.join(&form.action).map_err(|e| e.to_string())? };
            let req = if form.method == "post" { self.http.post(action).form(&fields) } else { self.http.get(action).query(&fields) };
            resp = req.send().await.map_err(|e| e.to_string())?;
        }
        Err("the university login took too many steps".into())
    }

    /// Calls a Moodle function (the same ones its pages and app use).
    async fn call(&self, function: &str, args: Value) -> Result<Value, String> {
        if !self.creds.token.is_empty() {
            return self.call_token(function, &args).await;
        }
        for attempt in 0..2 {
            let key = {
                let mut key = self.sesskey.lock().await;
                if key.is_none() {
                    *key = Some(self.sign_in().await?);
                }
                key.clone().unwrap_or_default()
            };
            let body = json!([{ "index": 0, "methodname": function, "args": args }]);
            let url = format!("{}/lib/ajax/service.php?sesskey={key}&info={function}", self.creds.site);
            let resp: Value = self
                .http
                .post(url)
                .json(&body)
                .send()
                .await
                .map_err(|e| e.to_string())?
                .json()
                .await
                .map_err(|e| format!("Moodle: unexpected answer to {function}: {e}"))?;
            let first = &resp[0];
            if first["error"].as_bool() == Some(false) {
                return Ok(first["data"].clone());
            }
            let ex = if first.is_null() { &resp } else { &first["exception"] };
            let code = ex["errorcode"].as_str().unwrap_or("");
            // The session ended: sign in again once.
            if attempt == 0 && matches!(code, "servicerequireslogin" | "invalidsesskey" | "requireloginerror" | "sitepolicynotagreed") {
                *self.sesskey.lock().await = None;
                continue;
            }
            return Err(format!("Moodle: {}", ex["message"].as_str().unwrap_or(code)));
        }
        Err("Moodle: couldn't sign in again".into())
    }

    async fn call_token(&self, function: &str, args: &Value) -> Result<Value, String> {
        let mut form: Vec<(String, String)> = vec![
            ("wstoken".into(), self.creds.token.clone()),
            ("wsfunction".into(), function.into()),
            ("moodlewsrestformat".into(), "json".into()),
        ];
        for (k, v) in args.as_object().into_iter().flatten() {
            let v = match v {
                Value::Bool(b) => (*b as u8).to_string(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            form.push((k.clone(), v));
        }
        let v: Value = self
            .http
            .post(format!("{}/webservice/rest/server.php", self.creds.site))
            .form(&form)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        if v.get("exception").is_some() {
            if v["errorcode"].as_str() == Some("invalidtoken") {
                return Err("the Moodle login expired; run discommand --moodle-login".into());
            }
            return Err(format!("Moodle: {}", v["message"].as_str().unwrap_or("error")));
        }
        Ok(v)
    }

    /// Checks the login works; returns how many courses there are, as a name.
    async fn check(&self) -> Result<String, String> {
        let courses = self.courses().await?;
        let who = if self.creds.username.is_empty() { "you".to_string() } else { self.creds.username.clone() };
        Ok(format!("{who} ({} current courses)", courses.len()))
    }

    async fn courses(&self) -> Result<Vec<Value>, String> {
        let args = json!({ "offset": 0, "limit": 0, "classification": "inprogress" });
        let data = self.call("core_course_get_enrolled_courses_by_timeline_classification", args).await?;
        Ok(data["courses"].as_array().cloned().unwrap_or_default())
    }

    /// The deadline list and the current courses.
    pub async fn sync(&self) -> Result<(Vec<TaskList>, Vec<Task>, Vec<Page>), String> {
        let courses = self.courses().await?;
        let from = Local::now().timestamp() - OVERDUE_DAYS * 86_400;
        let args = json!({ "timesortfrom": from, "limitnum": 50, "limittononsuspendedevents": true });
        let events = self.call("core_calendar_get_action_events_by_timesort", args).await?;

        let mut lists = vec![TaskList { id: DEADLINES.into(), name: "Deadlines".into(), default: true, emoji: "⏰".into(), ..TaskList::default() }];
        let mut names = HashMap::new();
        for (i, c) in courses.iter().enumerate() {
            let id = format!("c{}", c["id"].as_i64().unwrap_or(0));
            let name = c["fullname"].as_str().unwrap_or("Course").to_string();
            names.insert(id.clone(), name.clone());
            lists.push(TaskList { id, name: plain(&name), pos: 1 + i as u32, emoji: "🎓".into(), ..TaskList::default() });
        }
        *self.courses.lock().unwrap() = names;

        let mut tasks: Vec<Task> = Vec::new();
        let mut cms = HashMap::new();
        for (i, e) in events["events"].as_array().into_iter().flatten().enumerate() {
            let Some(at) = e["timesort"].as_i64().and_then(|t| Local.timestamp_opt(t, 0).single()) else { continue };
            let id = format!("ev{}", e["id"].as_i64().unwrap_or(0));
            let url = e["url"].as_str().unwrap_or("").to_string();
            if let Some(cm) = cm_id(&url) {
                cms.insert(id.clone(), cm);
            }
            let title = e["activityname"].as_str().filter(|s| !s.is_empty()).or(e["name"].as_str()).unwrap_or("Deadline");
            let course = e["course"]["fullname"].as_str().unwrap_or("");
            tasks.push(Task {
                id,
                list: DEADLINES.into(),
                title: plain(title),
                note: format!("{} · {}", plain(course), plain(e["name"].as_str().unwrap_or(""))),
                due: Some(at.date_naive()),
                // A day ahead, so there's time left when the bell rings.
                reminder: Some(at - chrono::Duration::days(1)).filter(|r| *r > Local::now()),
                check: true,
                marker: module_icon(e["modulename"].as_str().unwrap_or("")).into(),
                sublist: activity_target(e["modulename"].as_str().unwrap_or(""), &url),
                url,
                pos: 1 + i as u32,
                ..Task::default()
            });
        }
        *self.cms.lock().unwrap() = cms;
        let page = Page { list: DEADLINES.into(), title: "Deadlines".into(), emoji: "⏰".into(), lines: tasks.clone() };
        Ok((lists, tasks, vec![page]))
    }

    /// A Moodle page (an activity, a forum discussion, …) as lines: its main
    /// content's text and links.
    async fn web_page(&self, list: &str, url: &str) -> Result<Page, String> {
        let (html, at) = self.fetch(url).await?;
        let (title, lines) = html_lines(list, &html, &at);
        Ok(Page { list: list.into(), title, emoji: "📄".into(), lines })
    }

    /// A page of the site with the session (signing in again if it ended).
    /// Returns the page and where it ended up after redirects.
    async fn fetch(&self, url: &str) -> Result<(String, Url), String> {
        let resp = self.get_signed_in(url).await?;
        let at = resp.url().clone();
        Ok((resp.text().await.map_err(|e| e.to_string())?, at))
    }

    async fn get_signed_in(&self, url: &str) -> Result<reqwest::Response, String> {
        if !self.creds.token.is_empty() {
            return Err("pages open only in the browser with this kind of login: press o".into());
        }
        for attempt in 0..2 {
            {
                let mut key = self.sesskey.lock().await;
                if key.is_none() {
                    *key = Some(self.sign_in().await?);
                }
            }
            let resp = self.http.get(url).send().await.map_err(|e| e.to_string())?;
            if attempt == 0 && resp.url().path().ends_with("/login/index.php") {
                *self.sesskey.lock().await = None;
                continue;
            }
            return Ok(resp);
        }
        Err("Moodle: couldn't sign in again".into())
    }

    /// Downloads a file (a link to it, or a file activity) into the cache;
    /// returns where it was saved.
    pub async fn download(&self, url: &str) -> Result<String, String> {
        let mut resp = self.get_signed_in(url).await?;
        // A file activity shown in a page rather than sent: take its file link.
        if resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|t| t.starts_with("text/html")) {
            let at = resp.url().clone();
            let page = resp.text().await.map_err(|e| e.to_string())?;
            let file = forms_free_links(&page)
                .into_iter()
                .filter_map(|href| at.join(&href).ok())
                .find(|u| u.path().contains("/pluginfile.php/"))
                .ok_or("this file can only be opened in the browser: press o")?;
            resp = self.get_signed_in(file.as_str()).await?;
        }
        if resp.content_length().is_some_and(|n| n > MAX_DOWNLOAD) {
            return Err("the file is too large to download here (over 256 MB): press o".into());
        }
        let name = file_name(&resp);
        let data = resp.bytes().await.map_err(|e| e.to_string())?;
        let dir = secret::cache_dir().join("moodle");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(safe_name(&name));
        std::fs::write(&path, &data).map_err(|e| e.to_string())?;
        Ok(path.to_string_lossy().into_owned())
    }

    /// A course's sections and activities, or a page or file of the site.
    pub async fn page(&self, list: &str) -> Result<Page, String> {
        if let Some(url) = list.strip_prefix(WEB) {
            return self.web_page(list, url).await;
        }
        let Some(course) = list.strip_prefix('c').and_then(|id| id.parse::<i64>().ok()) else {
            return Err("not a course".into());
        };
        let title = self.courses.lock().unwrap().get(list).map(|n| plain(n)).unwrap_or_default();
        // Moodle 4 pages read the course from this; older ones (and the app
        // interface) have the course contents call.
        let lines = match self.call("core_courseformat_get_state", json!({ "courseid": course })).await {
            Ok(state) => state_lines(list, &state)?,
            Err(_) => contents_lines(list, &self.call("core_course_get_contents", json!({ "courseid": course })).await?),
        };
        Ok(Page { list: list.into(), title, emoji: "🎓".into(), lines })
    }

    /// Ticks an activity off (only for activities completed by hand).
    pub async fn set_done(&self, id: &str, done: bool) -> Result<(), String> {
        let cm = self.cms.lock().unwrap().get(id).copied().ok_or("this deadline has no activity to tick off")?;
        let args = json!({ "cmid": cm, "completed": done });
        self.call("core_completion_update_activity_completion_status_manually", args)
            .await
            .map(|_| ())
            .map_err(|_| "this one is completed in Moodle itself (e.g. by handing it in)".into())
    }
}

/// The course module id in an activity link (".../view.php?id=123").
fn cm_id(url: &str) -> Option<i64> {
    let url = Url::parse(url).ok()?;
    if !url.path().contains("/mod/") {
        return None;
    }
    url.query_pairs().find(|(k, _)| k == "id").and_then(|(_, v)| v.parse().ok())
}

fn module_icon(module: &str) -> &'static str {
    match module {
        "assign" => "📝 ",
        "quiz" => "❓ ",
        "resource" => "📄 ",
        "folder" => "📁 ",
        "url" => "🔗 ",
        "page" | "book" => "📃 ",
        "forum" => "💬 ",
        "choice" | "feedback" | "questionnaire" => "🗳 ",
        "bigbluebuttonbn" | "zoom" => "🎥 ",
        "lesson" | "scorm" | "h5pactivity" => "🎯 ",
        _ => "• ",
    }
}

/// Text without HTML tags and entities (names may carry them).
fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut tag = false;
    for c in s.chars() {
        match c {
            '<' => tag = true,
            '>' => tag = false,
            c if !tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#039;", "'").replace("&nbsp;", " ").trim().to_string()
}

fn id_of(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_else(|| v.to_string())
}

fn section_line(list: &str, id: String, name: &str, pos: u32) -> Task {
    Task { id, list: list.into(), title: plain(name), heading: true, pos, ..Task::default() }
}

fn activity_line(list: &str, id: String, name: &str, module: &str, url: &str, pos: u32) -> Task {
    Task {
        id,
        list: list.into(),
        title: plain(name),
        marker: module_icon(module).into(),
        url: url.to_string(),
        sublist: activity_target(module, url),
        depth: 1,
        pos,
        ..Task::default()
    }
}

/// Where Enter on an activity leads: a file downloads and opens, a web link
/// goes to the browser (no target), everything else opens as a page here.
fn activity_target(module: &str, url: &str) -> Option<String> {
    if url.is_empty() {
        return None;
    }
    match module {
        "url" => None,
        "resource" => Some(format!("{FILE}{url}{}redirect=1", if url.contains('?') { '&' } else { '?' })),
        _ => Some(format!("{WEB}{url}")),
    }
}

/// Page ids of Moodle web pages shown in the app, and of files to download.
pub const WEB: &str = "url:";
pub const FILE: &str = "file:";

/// Lines from `core_courseformat_get_state` (a JSON string).
fn state_lines(list: &str, state: &Value) -> Result<Vec<Task>, String> {
    let state: Value = match state.as_str() {
        Some(s) => serde_json::from_str(s).map_err(|e| e.to_string())?,
        None => state.clone(),
    };
    let cms: HashMap<String, &Value> = state["cm"].as_array().into_iter().flatten().map(|c| (id_of(&c["id"]), c)).collect();
    let sections: HashMap<String, &Value> = state["section"].as_array().into_iter().flatten().map(|s| (id_of(&s["id"]), s)).collect();
    let mut lines = Vec::new();
    let order: Vec<String> = match state["course"]["sectionlist"].as_array() {
        Some(ids) => ids.iter().map(id_of).collect(),
        None => state["section"].as_array().into_iter().flatten().map(|s| id_of(&s["id"])).collect(),
    };
    for sid in order {
        let Some(s) = sections.get(&sid) else { continue };
        let activities: Vec<&Value> = s["cmlist"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| cms.get(&id_of(id)).copied())
            .filter(|c| c["uservisible"].as_bool() != Some(false) && c["module"].as_str() != Some("label"))
            .collect();
        if activities.is_empty() {
            continue;
        }
        let n = lines.len() as u32;
        lines.push(section_line(list, format!("s{sid}"), s["title"].as_str().unwrap_or(""), n));
        for c in activities {
            let n = lines.len() as u32;
            let url = c["url"].as_str().unwrap_or("");
            lines.push(activity_line(list, format!("cm{}", id_of(&c["id"])), c["name"].as_str().unwrap_or(""), c["module"].as_str().unwrap_or(""), url, n));
        }
    }
    Ok(lines)
}

/// Lines from `core_course_get_contents`.
fn contents_lines(list: &str, contents: &Value) -> Vec<Task> {
    let mut lines = Vec::new();
    for s in contents.as_array().into_iter().flatten() {
        let activities: Vec<&Value> = s["modules"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["uservisible"].as_bool() != Some(false) && m["modname"].as_str() != Some("label"))
            .collect();
        if activities.is_empty() {
            continue;
        }
        let n = lines.len() as u32;
        lines.push(section_line(list, format!("s{}", id_of(&s["id"])), s["name"].as_str().unwrap_or(""), n));
        for m in activities {
            let n = lines.len() as u32;
            let url = m["url"].as_str().unwrap_or("");
            lines.push(activity_line(list, format!("cm{}", id_of(&m["id"])), m["name"].as_str().unwrap_or(""), m["modname"].as_str().unwrap_or(""), url, n));
        }
    }
    lines
}

const MAX_DOWNLOAD: u64 = 256 << 20;

/// The file name a download is sent with (else the last part of its address).
fn file_name(resp: &reqwest::Response) -> String {
    let header = resp.headers().get(reqwest::header::CONTENT_DISPOSITION).and_then(|v| v.to_str().ok()).unwrap_or("");
    if let Some(i) = header.find("filename*=") {
        // filename*=UTF-8''name%20with%20spaces.pdf
        let v = header[i + 10..].split(';').next().unwrap_or("");
        if let Some(name) = v.split("''").nth(1) {
            return percent_decode(name.trim_matches('"'));
        }
    }
    if let Some(i) = header.find("filename=") {
        let v = header[i + 9..].split(';').next().unwrap_or("").trim().trim_matches('"');
        if !v.is_empty() {
            return v.to_string();
        }
    }
    let last = resp.url().path_segments().and_then(|mut s| s.next_back()).unwrap_or("download");
    percent_decode(last)
}

fn percent_decode(s: &str) -> String {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if let (b'%', Some(&h), Some(&l)) = (b[i], b.get(i + 1), b.get(i + 2)) {
            if let (Some(h), Some(l)) = (hex(h), hex(l)) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A file name that is safe to create in the download folder.
fn safe_name(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or("");
    let name: String = name
        .chars()
        .map(|c| if c < ' ' || "<>:\"/\\|?*".contains(c) || ('\u{202A}'..='\u{202E}').contains(&c) || ('\u{2066}'..='\u{2069}').contains(&c) { '_' } else { c })
        .collect();
    let name = name.trim_matches(['.', ' ']);
    if name.is_empty() {
        "download".into()
    } else {
        name.chars().rev().take(120).collect::<Vec<_>>().into_iter().rev().collect()
    }
}

/// The `href`s of a page's links.
fn forms_free_links(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("<a ").map(|i| pos + i) {
        let Some(close) = lower[i..].find('>').map(|c| i + c) else { break };
        if let Some(href) = attributes(&html[i + 1..close]).remove("href") {
            out.push(href);
        }
        pos = close;
    }
    out
}

/// A Moodle page's main content (`role="main"`) as lines: headings, text,
/// list items and links. Links to Moodle pages open as pages here, files
/// download, other links go to the browser. Returns the title too.
fn html_lines(list: &str, html: &str, base: &Url) -> (String, Vec<Task>) {
    let lower = html.to_ascii_lowercase();
    let title = lower
        .find("<title>")
        .and_then(|i| lower[i..].find("</title>").map(|e| plain(&html[i + 7..i + e])))
        .map(|t| t.split(" | ").next().unwrap_or("").to_string())
        .unwrap_or_default();
    let start = lower.find("role=\"main\"").and_then(|i| lower[..i].rfind('<')).unwrap_or(0);
    let end = ["id=\"page-footer\"", "<footer"].iter().filter_map(|m| lower[start..].find(m).map(|e| start + e)).min().unwrap_or(html.len());
    let (body, lbody) = (&html[start..end], &lower[start..end]);

    let mut lines: Vec<Task> = Vec::new();
    let mut text = String::new();
    let mut bullet = false;
    let mut heading = false;
    let mut push = |lines: &mut Vec<Task>, mut t: Task| {
        if lines.len() >= 800 {
            return;
        }
        t.list = list.to_string();
        t.pos = lines.len() as u32;
        t.id = format!("l{}", lines.len());
        // The same text twice in a row (icon link + text link): once.
        if lines.last().is_some_and(|l| l.title == t.title && l.sublist == t.sublist && l.url == t.url) {
            return;
        }
        lines.push(t);
    };
    let flush = |lines: &mut Vec<Task>, text: &mut String, bullet: &mut bool, heading: bool, push: &mut dyn FnMut(&mut Vec<Task>, Task)| {
        let t: String = unescape(text).split_whitespace().collect::<Vec<_>>().join(" ");
        text.clear();
        if !t.is_empty() {
            let marker = if *bullet { "• " } else { "" }.to_string();
            push(lines, Task { title: t, heading, marker, ..Task::default() });
            *bullet = false;
        }
    };
    let mut i = 0;
    while i < body.len() {
        let next = body[i..].find('<').map_or(body.len(), |n| i + n);
        text.push_str(&body[i..next]);
        i = next;
        if i >= body.len() {
            break;
        }
        let Some(close) = lbody[i..].find('>').map(|c| i + c) else { break };
        let tag = &lbody[i + 1..close];
        let name: String = tag.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '/').collect();
        match name.as_str() {
            "script" | "style" | "noscript" | "template" | "svg" | "select" | "textarea" => {
                let end = lbody[close..].find(&format!("</{name}")).map_or(body.len(), |e| close + e);
                i = lbody[end..].find('>').map_or(body.len(), |e| end + e + 1);
                continue;
            }
            "a" => {
                let href = attributes(&body[i + 1..close]).remove("href").unwrap_or_default();
                let inner_end = lbody[close..].find("</a").map_or(body.len(), |e| close + e);
                let label: String = plain(&body[close + 1..inner_end]).split_whitespace().collect::<Vec<_>>().join(" ");
                i = lbody[inner_end..].find('>').map_or(body.len(), |e| inner_end + e + 1);
                if let Some(line) = link_line(&href, &label, base) {
                    flush(&mut lines, &mut text, &mut bullet, heading, &mut push);
                    push(&mut lines, line);
                } else {
                    text.push(' ');
                    text.push_str(&label);
                    text.push(' ');
                }
                continue;
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                flush(&mut lines, &mut text, &mut bullet, heading, &mut push);
                heading = true;
            }
            "/h1" | "/h2" | "/h3" | "/h4" | "/h5" | "/h6" => {
                flush(&mut lines, &mut text, &mut bullet, heading, &mut push);
                heading = false;
            }
            "li" => {
                flush(&mut lines, &mut text, &mut bullet, heading, &mut push);
                bullet = true;
            }
            "td" | "th" | "/td" | "/th" | "span" | "/span" => text.push(' '),
            "br" | "p" | "/p" | "div" | "/div" | "tr" | "/tr" | "ul" | "/ul" | "ol" | "/ol" | "/li" | "table" | "/table" | "section"
            | "/section" | "dt" | "dd" | "/dt" | "/dd" | "blockquote" | "/blockquote" | "article" | "/article" | "header" | "/header" => {
                flush(&mut lines, &mut text, &mut bullet, heading, &mut push)
            }
            _ => {}
        }
        i = close + 1;
    }
    flush(&mut lines, &mut text, &mut bullet, heading, &mut push);
    (title, lines)
}

/// A link as a line: Moodle pages open here, files download, the rest goes
/// to the browser. None for links that aren't worth a line (anchors, scripts).
fn link_line(href: &str, label: &str, base: &Url) -> Option<Task> {
    let href = href.trim();
    if label.is_empty() || href.is_empty() || href.starts_with('#') || href.to_ascii_lowercase().starts_with("javascript:") {
        return None;
    }
    let url = base.join(href).ok()?;
    let same_site = url.host_str() == base.host_str();
    let path = url.path();
    let (marker, sublist) = if !matches!(url.scheme(), "http" | "https") {
        ("🔗 ", None)
    } else if same_site && (path.contains("/pluginfile.php/") || path.contains("/mod/resource/view.php")) {
        let target = if path.contains("/mod/resource/view.php") && !url.as_str().contains("redirect=") {
            format!("{}&redirect=1", url.as_str())
        } else {
            url.to_string()
        };
        ("📄 ", Some(format!("{FILE}{target}")))
    } else if same_site && !path.contains("/login/logout.php") {
        ("› ", Some(format!("{WEB}{url}")))
    } else {
        ("🔗 ", None)
    };
    Some(Task { title: label.to_string(), marker: marker.into(), url: url.to_string(), sublist, depth: 1, ..Task::default() })
}

/// When the deadline list is due for a look again.
pub const POLL: Duration = Duration::from_secs(30 * 60);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_are_normalized() {
        assert_eq!(normalize_site("elearning.uni-regensburg.de").unwrap(), "https://elearning.uni-regensburg.de");
        assert_eq!(normalize_site("https://moodle.x.org/moodle/login/index.php").unwrap(), "https://moodle.x.org/moodle");
        assert!(normalize_site("http://moodle.x.org").is_err());
    }

    #[test]
    fn reads_course_state() {
        let state = json!({
            "course": { "sectionlist": ["10", "11"] },
            "section": [
                { "id": "10", "title": "General", "cmlist": ["1", "2"] },
                { "id": "11", "title": "Week 1", "cmlist": ["3"] }
            ],
            "cm": [
                { "id": "1", "name": "Announcements", "module": "forum", "url": "https://m/mod/forum/view.php?id=1", "uservisible": true },
                { "id": "2", "name": "<b>Welcome</b>", "module": "label", "uservisible": true },
                { "id": "3", "name": "Slides &amp; notes", "module": "resource", "url": "https://m/mod/resource/view.php?id=3", "uservisible": true }
            ]
        });
        let lines = state_lines("c5", &Value::String(state.to_string())).unwrap();
        let shown: Vec<(&str, &str)> = lines.iter().map(|l| (l.marker.as_str(), l.title.as_str())).collect();
        assert_eq!(shown, [("", "General"), ("💬 ", "Announcements"), ("", "Week 1"), ("📄 ", "Slides & notes")]);
        assert!(lines[0].heading && lines[3].url.ends_with("id=3"));
    }

    #[test]
    fn tells_password_forms_from_guest_and_sso_pages() {
        let moodle_form = r#"<input type="text" name="username" id="username"><input type="password" name="password" id="password">"#;
        let guest_only = r#"<form id="guestlogin"><input type="hidden" name="username" value="guest" /><input type="hidden" name="password" value="guest" /></form><a href="/auth/shibboleth/index.php">Uni Login</a>"#;
        assert!(has_password_form(moodle_form));
        assert!(!has_password_form(guest_only));
    }

    #[test]
    fn reads_and_fills_login_forms() {
        // The university login form (NetIQ), as on my.ur.de.
        let page = r#"<form
          name="IDPLogin" method="POST"
          action='https://my.ur.de:8443/nidp/saml2/sso?sid=0&amp;sid=0' >
          <input type="hidden" name="option" value="credential" />
          <input aria-labelledby="usernameLabel" type="text" id="form-field-name" name="Ecom_User_ID">
          <input type="password" name="Ecom_Password" id="password">
          <button type="button" name="loginButton2">Anmelden</button>
        </form>"#;
        let f = &forms(page)[0];
        assert_eq!((f.method.as_str(), f.action.as_str()), ("post", "https://my.ur.de:8443/nidp/saml2/sso?sid=0&sid=0"));
        assert!(f.has_password() && !f.forwards());
        let filled = f.filled("abc12345", "pw");
        assert_eq!(filled, [("option".into(), "credential".into()), ("Ecom_User_ID".into(), "abc12345".into()), ("Ecom_Password".into(), "pw".into())]);

        // A SAML forwarding page: sent on as it is.
        let saml = r#"<form method="post" action="https://sp/Shibboleth.sso/SAML2/POST"><input type="hidden" name="RelayState" value="ss&#x3a;mem"/><input type="hidden" name="SAMLResponse" value="PD94"/><noscript><input type="submit" value="Continue"/></noscript></form>"#;
        let f = &forms(saml)[0];
        assert!(f.forwards());
        assert_eq!(f.filled("u", "p"), [("RelayState".into(), "ss:mem".into()), ("SAMLResponse".into(), "PD94".into())]);
    }

    #[test]
    fn accepts_consent_pages() {
        // Shibboleth's attribute release page, as a browser shows it.
        let page = r#"<form action="/idp/profile/SAML2/Redirect/SSO?execution=e1s3" method="post">
            <input type="hidden" name="csrf_token" value="t1">
            <input id="_shib_idp_doNotRememberConsent" type="radio" name="_shib_idp_consentOptions" value="_shib_idp_doNotRememberConsent">
            <input id="_shib_idp_rememberConsent" type="radio" name="_shib_idp_consentOptions" value="_shib_idp_rememberConsent" checked>
            <input type="submit" name="_eventId_AttributeReleaseRejected" value="Reject">
            <input type="submit" name="_eventId_proceed" value="Accept">
        </form>"#;
        let f = &forms(page)[0];
        assert!(f.forwards() && !f.has_password());
        assert_eq!(
            f.filled("u", "p"),
            [
                ("csrf_token".into(), "t1".into()),
                ("_shib_idp_consentOptions".into(), "_shib_idp_rememberConsent".into()),
                ("_eventId_proceed".into(), "Accept".into())
            ]
        );
        // The university form's button has no type, so it's a submit button; it's pressed.
        let netiq = r#"<form method="POST" action="/x"><input type="text" name="Ecom_User_ID"><input type="password" name="Ecom_Password"><button name="loginButton2" onClick="return imageSubmit()">Anmelden</button></form>"#;
        assert_eq!(forms(netiq)[0].filled("u", "p").last(), Some(&("loginButton2".into(), String::new())));
    }

    #[test]
    fn follows_script_and_refresh_redirects() {
        assert_eq!(page_redirect(r#"<script>top.location.href='https://my.ur.de:8443/nidp/app?sid=0';</script>"#).as_deref(), Some("https://my.ur.de:8443/nidp/app?sid=0"));
        assert_eq!(page_redirect(r#"<script>window.location.replace("/idp/next");</script>"#).as_deref(), Some("/idp/next"));
        assert_eq!(page_redirect(r#"<META HTTP-EQUIV="Refresh" CONTENT="0; URL=https://sp/x?a=1&amp;b=2">"#).as_deref(), Some("https://sp/x?a=1&b=2"));
        assert_eq!(page_redirect(r#"<a href="javascript:location.reload()">x</a><p>no redirect</p>"#), None);
    }

    #[test]
    fn finds_the_sso_start() {
        let config = json!({ "identityproviders": [{ "name": "GRIPS Login", "url": "https://m.x/auth/shibboleth/index.php" }] });
        assert_eq!(sso_start("https://m.x", &config, "").as_deref(), Some("https://m.x/auth/shibboleth/index.php"));
        let page = r#"<a href="https://m.x/auth/shibboleth/index.php?x=1&amp;y=2">Login</a>"#;
        assert_eq!(sso_start("https://m.x", &json!({}), page).as_deref(), Some("https://m.x/auth/shibboleth/index.php?x=1&y=2"));
    }

    /// Walks the real GRIPS chain with a made-up account: it must get through
    /// the forwarding pages to the university's login form and be refused there.
    #[test]
    #[ignore]
    fn grips_chain_reaches_the_login_form() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let creds = Creds {
            site: "https://elearning.uni-regensburg.de".into(),
            site_name: "GRIPS".into(),
            token: String::new(),
            username: "discommand-test-nobody".into(),
            password: "wrong".into(),
            cookie: None,
            sso: "https://elearning.uni-regensburg.de/auth/shibboleth/index.php".into(),
        };
        let result = rt.block_on(Moodle::new(creds).sign_in_sso());
        println!("{result:?}");
        let err = result.unwrap_err();
        assert!(err.starts_with("the university login didn't accept the user name or password"), "{err}");
    }

    #[test]
    fn reads_moodle_pages_as_lines() {
        let base = Url::parse("https://m.x/mod/folder/view.php?id=7").unwrap();
        let html = r##"<html><head><title>MMT: Übungsmaterial | GRIPS</title></head><body>
            <nav><a href="/my/">Dashboard</a></nav>
            <div id="page" role="main">
              <h2>Übungsmaterial</h2>
              <p>Alle Dateien für die <b>Übung</b> &amp; mehr.</p>
              <ul><li><a href="https://m.x/pluginfile.php/1/mod_folder/content/0/gimp.zip?forcedownload=1"><img alt=""><span>gimp.zip</span></a></li>
                  <li><a href="https://m.x/mod/forum/discuss.php?d=9">Fragen zur Übung</a></li>
                  <li><a href="https://www.gimp.org/">GIMP website</a></li>
                  <li><a href="#top">nach oben</a></li></ul>
              <script>var x = "<p>not text</p>";</script>
            </div>
            <footer id="page-footer"><a href="/admin/">Impressum</a></footer></body></html>"##;
        let (title, lines) = html_lines("url:x", html, &base);
        assert_eq!(title, "MMT: Übungsmaterial");
        let shown: Vec<(&str, &str)> = lines.iter().map(|l| (l.marker.as_str(), l.title.as_str())).collect();
        assert_eq!(
            shown,
            [("", "Übungsmaterial"), ("", "Alle Dateien für die Übung & mehr."), ("📄 ", "gimp.zip"), ("› ", "Fragen zur Übung"), ("🔗 ", "GIMP website"), ("• ", "nach oben")]
        );
        assert!(lines[0].heading);
        assert!(lines[2].sublist.as_deref().unwrap().starts_with("file:https://m.x/pluginfile.php/"));
        assert_eq!(lines[3].sublist.as_deref(), Some("url:https://m.x/mod/forum/discuss.php?d=9"));
        assert!(lines[4].sublist.is_none() && lines[4].url == "https://www.gimp.org/");
    }

    #[test]
    fn activities_open_the_right_way() {
        assert_eq!(activity_target("forum", "https://m/mod/forum/view.php?id=1").as_deref(), Some("url:https://m/mod/forum/view.php?id=1"));
        assert_eq!(activity_target("resource", "https://m/mod/resource/view.php?id=2").as_deref(), Some("file:https://m/mod/resource/view.php?id=2&redirect=1"));
        assert_eq!(activity_target("url", "https://m/mod/url/view.php?id=3"), None);
        assert_eq!(percent_decode("Folien%20VL%2001.pdf"), "Folien VL 01.pdf");
        assert_eq!(safe_name("../../x.exe"), "x.exe");
    }

    #[test]
    fn finds_activity_ids() {
        assert_eq!(cm_id("https://m/mod/assign/view.php?id=42"), Some(42));
        assert_eq!(cm_id("https://m/course/view.php?id=42"), None);
    }
}
