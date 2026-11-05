//! The mail tab's backend: GroupWise folders as lists in the sidebar, each
//! folder's mails as a page, each mail as a page of its own (marked read
//! when opened), attachments as files to open, and sending.
//!
//! The session is signed in on first use and again whenever GroupWise says
//! it ended: the whole login (university, Microsoft, the authenticator) runs
//! by itself, which takes a few seconds.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use chrono::{DateTime, Local, Utc};

use super::soap::{self, escape, Session};
use super::xml::{self, Node};
use super::{login, Creds};
use crate::planner::moodle;
use crate::planner::todo::{Page, Task, TaskList};

/// The page id of a mail ("gwmail:<item id>").
pub const MAIL: &str = "gwmail:";
/// Attachments, opened like Moodle's files (`moodle::FILE` + this + id + name).
const ATTACHMENT: &str = "gwatt:";
const ATTACHMENT_URL: &str = "https://gw.uni-regensburg.de/attachment";
/// Mails shown per folder (the newest), and how far back they're looked for.
const SHOWN: usize = 150;
const DAYS: i64 = 120;
/// The session ended (GroupWise's codes for it).
const SESSION_GONE: [&str; 2] = ["59910", "59920"];

/// What's kept of a mail for replying: who wrote it, to whom, and its text.
#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub subject: String,
    pub from: (String, String),
    pub to: Vec<(String, String)>,
    pub cc: Vec<(String, String)>,
    pub date: String,
    pub text: String,
}

pub struct GroupWise {
    creds: Creds,
    http: reqwest::Client,
    session: tokio::sync::Mutex<Option<Session>>,
    /// The folders (id → name), for page titles.
    folders: Mutex<HashMap<String, String>>,
    /// Each folder's unread count (as GroupWise counts it, mails of any age).
    unread: Mutex<HashMap<String, usize>>,
    /// The folders mail can be moved to, in order: (id, name, depth, sign).
    tree: Mutex<Vec<Folder>>,
    /// The mails read so far, for replying.
    meta: Mutex<HashMap<String, Meta>>,
    /// The address books searched for name completion: (id, the university's
    /// directory rather than a personal book), and whether they're known yet.
    books: tokio::sync::Mutex<Option<Vec<(String, bool)>>>,
    /// The signed-in user's address, left out when replying to all.
    me: Mutex<String>,
}

impl GroupWise {
    pub fn new(creds: Creds) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().unwrap_or_default();
        Self {
            creds,
            http,
            session: tokio::sync::Mutex::new(None),
            folders: Mutex::new(HashMap::new()),
            tree: Mutex::new(Vec::new()),
            unread: Mutex::new(HashMap::new()),
            meta: Mutex::new(HashMap::new()),
            books: tokio::sync::Mutex::new(None),
            me: Mutex::new(String::new()),
        }
    }

    /// New mail is looked for this often.
    pub fn poll(&self) -> Duration {
        Duration::from_secs(120)
    }

    /// People whose name or address starts with `prefix`, as GroupWise Web
    /// suggests them while an address is typed: from the books in name
    /// completion (Frequent Contacts first), then the university's directory.
    pub async fn suggest(&self, prefix: &str) -> Vec<(String, String)> {
        let prefix = prefix.trim();
        if prefix.chars().count() < 2 {
            return Vec::new();
        }
        let books = {
            let mut known = self.books.lock().await;
            if known.is_none() {
                *known = self.name_completion_books().await.ok();
            }
            known.clone().unwrap_or_default()
        };
        let mut found: Vec<(String, String)> = Vec::new();
        // Personal books first (they hold the people written to most).
        for (id, system) in books.iter().filter(|(_, s)| !s).chain(books.iter().filter(|(_, s)| *s)) {
            let request = if *system {
                format!("<methods:getSABItemsRequest><match>{}</match><count>10</count><view>sort-first-name default name email id uuid title department flags</view></methods:getSABItemsRequest>", escape(prefix))
            } else {
                let p = escape(prefix);
                let begins: String = ["name", "fullName/firstName", "fullName/lastName", "emailList/email", "email", "username"]
                    .iter()
                    .map(|f| format!("<element xsi:type=\"types:FilterEntry\"><op>begins</op><field>{f}</field><value>{p}</value></element>"))
                    .collect();
                format!(
                    "<methods:getItemsRequest><container>{}</container><view>default name email id uuid title department flags</view><filter><element xsi:type=\"types:FilterGroup\"><op>and</op><element xsi:type=\"types:FilterEntry\"><op>isOf</op><field>@type</field><value>Contact Group Resource</value></element><element xsi:type=\"types:FilterGroup\"><op>or</op>{begins}</element></element></filter><count>20</count></methods:getItemsRequest>",
                    escape(id)
                )
            };
            let action = if *system { "getSABItemsRequest" } else { "getItemsRequest" };
            let Ok(root) = self.call(action, &request).await else { continue };
            for item in root.find("items").map(|i| i.all("item").collect::<Vec<_>>()).unwrap_or_default() {
                if let Some(p) = contact(item).filter(|p| !found.iter().any(|f| f.1.eq_ignore_ascii_case(&p.1))) {
                    found.push(p);
                }
            }
            if found.len() >= 8 {
                break;
            }
        }
        found.truncate(8);
        found
    }

    /// The address books GroupWise Web searches while typing: the system
    /// directory, and the personal ones marked for name completion.
    async fn name_completion_books(&self) -> Result<Vec<(String, bool)>, String> {
        let root = self.call("getAddressBookListRequest", "<methods:getAddressBookListRequest><view>default</view></methods:getAddressBookListRequest>").await?;
        let yes = |n: &Node, f: &str| n.text_at(f).is_some_and(|v| v == "1" || v == "true");
        Ok(root
            .find("books")
            .map(|b| b.all("book").collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|b| {
                let system = !yes(b, "isPersonal") && !yes(b, "isFrequentContacts");
                (system || yes(b, "includeInNCC") || yes(b, "isFrequentContacts")).then(|| (b.text_at("id").unwrap_or("").to_string(), system))
            })
            .filter(|(id, _)| !id.is_empty())
            .collect())
    }

    /// The folders mail can be moved to (as at the last sync).
    pub fn folders(&self) -> Vec<Folder> {
        self.tree.lock().map(|t| t.clone()).unwrap_or_default()
    }

    /// Moves mails from one folder to another.
    pub async fn move_items(&self, ids: &[String], from: &str, to: &str) -> Result<(), String> {
        let items: String = ids
            .iter()
            .map(|id| format!("<item><id>{}</id><container>{}</container><from>{}</from></item>", escape(id), escape(to), escape(from)))
            .collect();
        self.call("moveItemsRequest", &format!("<methods:moveItemsRequest>{items}</methods:moveItemsRequest>")).await.map(|_| ())
    }

    /// Deletes mails (GroupWise puts them in the Trash).
    pub async fn remove_items(&self, ids: &[String], from: &str) -> Result<(), String> {
        let items: String = ids.iter().map(|id| format!("<item>{}</item>", escape(id))).collect();
        let request = format!("<methods:removeItemsRequest><container>{}</container><items>{items}</items></methods:removeItemsRequest>", escape(from));
        self.call("removeItemsRequest", &request).await.map(|_| ())
    }

    /// Every unread mail of a folder, however old (the folder's page only
    /// shows the newest).
    pub async fn all_unread(&self, folder: &str) -> Result<Vec<String>, String> {
        let request = format!("<methods:getItemsRequest><container>{}</container><view>id status peek</view></methods:getItemsRequest>", escape(folder));
        let root = self.call("getItemsRequest", &request).await?;
        Ok(root
            .find("items")
            .map(|i| i.all("item").collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .filter(|item| item.at("status/read").is_none_or(|r| r.text.trim() != "1" && r.text.trim() != "true"))
            .filter_map(|item| item.text_at("id").map(String::from))
            .collect())
    }

    /// Marks mails read.
    pub async fn mark_read(&self, ids: &[String]) -> Result<(), String> {
        let items: String = ids.iter().map(|id| format!("<item>{}</item>", escape(id))).collect();
        self.call("markReadRequest", &format!("<methods:markReadRequest><items>{items}</items></methods:markReadRequest>")).await.map(|_| ())
    }

    /// A mail read earlier (for replying).
    pub fn meta(&self, id: &str) -> Option<Meta> {
        self.meta.lock().ok()?.get(id).cloned()
    }

    pub fn me(&self) -> String {
        self.me.lock().map(|m| m.clone()).unwrap_or_default()
    }

    async fn sign_in(&self) -> Result<Session, String> {
        let quiet = |_: String| {};
        let tokens = login::login(&self.creds.username, &self.creds.password, &quiet).await?;
        let session = soap::login(&self.http, &self.creds.username, &tokens.access).await?;
        if let Ok(mut me) = self.me.lock() {
            *me = session.email.clone();
        }
        Ok(session)
    }

    /// One call with the session (signing in first, or again if it ended).
    async fn call(&self, action: &str, body: &str) -> Result<Node, String> {
        let mut session = self.session.lock().await;
        for attempt in 0..2 {
            if session.is_none() {
                *session = Some(self.sign_in().await?);
            }
            let s = session.as_ref().ok_or("no session")?;
            let answer = soap::call(&self.http, action, body, Some(&s.id), &s.poa).await?;
            let root = xml::parse(&answer).ok_or("GroupWise sent something unreadable")?;
            let code = root.find("status").and_then(|s| s.text_at("code")).unwrap_or("0").to_string();
            if SESSION_GONE.contains(&code.as_str()) && attempt == 0 {
                *session = None;
                continue;
            }
            if code != "0" {
                let what = root.find("status").and_then(|s| s.text_at("description")).map(|d| format!(": {d}")).unwrap_or_default();
                return Err(format!("GroupWise error {code}{what}"));
            }
            return Ok(root);
        }
        Err("GroupWise keeps ending the session".into())
    }

    /// The mail folders as lists, and the mailbox's mails (so its unread
    /// ones show right away).
    pub async fn sync(&self) -> Result<(Vec<TaskList>, Vec<Task>, Vec<Page>), String> {
        let root = self
            .call("getFolderListRequest", "<methods:getFolderListRequest><parent>folders</parent><view>default unreadCount</view><recurse>1</recurse></methods:getFolderListRequest>")
            .await?;
        let folders: Vec<&Node> = root.find("folders").map(|f| f.all("folder").collect()).unwrap_or_default();
        let names: HashMap<String, String> = folders.iter().filter_map(|f| Some((f.text_at("id")?.to_string(), f.text_at("name")?.to_string()))).collect();
        let mut lists = Vec::new();
        let mut mailbox = None;
        for f in &folders {
            let (Some(id), Some(name)) = (f.text_at("id"), f.text_at("name")) else { continue };
            let kind = f.text_at("folderType").unwrap_or("");
            let Some((pos, emoji)) = folder_kind(kind) else { continue };
            let unread = f.text_at("unreadCount").and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
            if let Ok(mut u) = self.unread.lock() {
                u.insert(id.to_string(), unread);
            }
            if kind == "Mailbox" {
                mailbox = Some(id.to_string());
            }
            // Folders inside folders (not the top ones) are grouped under their parent.
            let parent = f.text_at("parent").and_then(|p| names.get(p)).filter(|_| kind.is_empty() || kind == "Normal");
            lists.push(TaskList {
                id: id.to_string(),
                name: if unread > 0 { format!("{name} ({unread} new)") } else { name.to_string() },
                emoji: emoji.into(),
                pos,
                default: kind == "Mailbox",
                group: parent.filter(|p| folders.iter().any(|g| g.text_at("name") == Some(p.as_str()) && g.text_at("folderType") == Some("Cabinet"))).cloned().unwrap_or_default(),
                ..TaskList::default()
            });
        }
        if let Ok(mut t) = self.tree.lock() {
            *t = folder_tree(&folders);
        }
        if let Ok(mut f) = self.folders.lock() {
            *f = names;
        }
        let pages = match mailbox {
            Some(id) => vec![self.folder_page(&id).await?],
            None => Vec::new(),
        };
        Ok((lists, Vec::new(), pages))
    }

    /// A folder's or a mail's page.
    pub async fn page(&self, list: &str) -> Result<Page, String> {
        match list.strip_prefix(MAIL) {
            Some(id) => self.mail_page(list, id).await,
            None => self.folder_page(list).await,
        }
    }

    /// A folder: its newest mails, unread ones marked.
    async fn folder_page(&self, folder: &str) -> Result<Page, String> {
        let since = (Utc::now() - chrono::Duration::days(DAYS)).format("%Y-%m-%dT%H:%M:%SZ");
        let filtered = format!(
            "<methods:getItemsRequest><container>{}</container><view>default peek</view><filter><element xsi:type=\"types:FilterEntry\"><op>gte</op><field>delivered</field><value>{since}</value></element></filter><count>500</count></methods:getItemsRequest>",
            escape(folder)
        );
        // Without the date filter, if this post office doesn't take it.
        let root = match self.call("getItemsRequest", &filtered).await {
            Ok(r) => r,
            Err(_) => {
                let all = format!("<methods:getItemsRequest><container>{}</container><view>default peek</view><count>{SHOWN}</count></methods:getItemsRequest>", escape(folder));
                self.call("getItemsRequest", &all).await?
            }
        };
        let mut mails: Vec<(DateTime<Local>, Task)> = Vec::new();
        for item in root.find("items").map(|i| i.all("item").collect::<Vec<_>>()).unwrap_or_default() {
            let Some(id) = item.text_at("id") else { continue };
            let kind = item.attr("type").unwrap_or("").rsplit(':').next().unwrap_or("").to_string();
            let when = item.text_at("delivered").or(item.text_at("created")).and_then(parse_time).unwrap_or_default();
            let unread = item.at("status/read").is_none_or(|r| r.text.trim() != "1" && r.text.trim() != "true");
            let from = name_of(item.at("distribution/from")).unwrap_or_default();
            let subject = item.text_at("subject").unwrap_or("(no subject)");
            let mark = match kind.as_str() {
                "Appointment" => "📅 ",
                "Task" => "☐ ",
                "Note" => "🗒 ",
                "PhoneMessage" => "📞 ",
                _ => "",
            };
            let clip = if item.child("hasAttachment").is_some_and(|a| a.text.trim() != "0" && a.text.trim() != "false") { " 📎" } else { "" };
            mails.push((
                when,
                Task {
                    id: id.to_string(),
                    list: folder.to_string(),
                    title: format!("{}{mark}{subject}{clip}  · {from} · {}", if unread { "● " } else { "" }, short_time(when)),
                    important: unread,
                    sublist: Some(format!("{MAIL}{id}")),
                    ..Task::default()
                },
            ));
        }
        mails.sort_by(|a, b| b.0.cmp(&a.0));
        let mut lines: Vec<Task> = mails.into_iter().take(SHOWN).map(|(_, t)| t).collect();
        let shown_unread = lines.iter().filter(|l| l.important).count();
        if lines.is_empty() {
            lines.push(Task { title: format!("No mails from the last {DAYS} days."), ..Task::default() });
        }
        // Unread mails too old to be shown still count: say so, and how to clear them.
        let counted = self.unread.lock().ok().and_then(|u| u.get(folder).copied()).unwrap_or(0);
        if counted > shown_unread {
            let older = counted - shown_unread;
            lines.push(Task { title: String::new(), ..Task::default() });
            lines.push(Task {
                title: format!("{older} older unread mail{} not shown (only the last {DAYS} days are) · A marks all read", if older == 1 { " is" } else { "s are" }),
                ..Task::default()
            });
        }
        for (i, l) in lines.iter_mut().enumerate() {
            l.pos = i as u32 + 1;
        }
        let title = self.folders.lock().ok().and_then(|f| f.get(folder).cloned()).unwrap_or_else(|| "Mail".into());
        Ok(Page { list: folder.to_string(), title, emoji: "✉".into(), lines })
    }

    /// One mail: who, when, its text and attachments. Marked read.
    async fn mail_page(&self, list: &str, id: &str) -> Result<Page, String> {
        let body = format!("<methods:getItemRequest><id>{}</id><view>default message attachments recipients peek</view></methods:getItemRequest>", escape(id));
        let root = self.call("getItemRequest", &body).await?;
        let item = root.find("item").ok_or("GroupWise didn't send the mail")?;
        let subject = item.text_at("subject").unwrap_or("(no subject)").to_string();
        let from = person(item.at("distribution/from"));
        let mut to = Vec::new();
        let mut cc = Vec::new();
        for r in item.at("distribution/recipients").map(|r| r.all("recipient").collect::<Vec<_>>()).unwrap_or_default() {
            let p = person(Some(r));
            match r.text_at("distType").unwrap_or("TO") {
                "CC" => cc.push(p),
                "BC" => {}
                _ => to.push(p),
            }
        }
        let when = item.text_at("delivered").or(item.text_at("created")).and_then(parse_time);
        let date = when.map(|t| t.format("%a %d %b %Y, %H:%M").to_string()).unwrap_or_default();
        let (text, plain) = message_text(item);

        let site = reqwest::Url::parse("https://gw.uni-regensburg.de/").map_err(|e| e.to_string())?;
        let mut lines = vec![Task { title: subject.clone(), heading: true, ..Task::default() }];
        let who = |people: &[(String, String)]| people.iter().map(show_person).collect::<Vec<_>>().join(", ");
        lines.push(Task { title: format!("From  {}", show_person(&from)), ..Task::default() });
        if !to.is_empty() {
            lines.push(Task { title: format!("To    {}", who(&to)), ..Task::default() });
        }
        if !cc.is_empty() {
            lines.push(Task { title: format!("Cc    {}", who(&cc)), ..Task::default() });
        }
        lines.push(Task { title: format!("Date  {date}"), ..Task::default() });
        for a in item.at("attachments").map(|a| a.all("attachment").collect::<Vec<_>>()).unwrap_or_default() {
            let (Some(aid), Some(name)) = (a.text_at("id"), a.text_at("name")) else { continue };
            if a.text_at("hidden").is_some_and(|h| h == "1" || h == "true") || matches!(name, "Mime.822" | "TEXT.htm" | "Header") {
                continue;
            }
            let size = a.text_at("size").and_then(|s| s.parse::<u64>().ok()).map(|s| format!(" ({})", moodle::size(s))).unwrap_or_default();
            lines.push(Task {
                title: format!("📎 {name}{size}"),
                sublist: Some(format!("{}{ATTACHMENT}{aid}\n{name}", moodle::FILE)),
                ..Task::default()
            });
        }
        lines.push(Task { title: String::new(), ..Task::default() });
        lines.extend(moodle::html_text_lines(list, &text, &site));
        lines.push(Task { title: String::new(), ..Task::default() });
        lines.push(Task { title: "n new · r reply · R reply all · m move · dd delete".into(), ..Task::default() });

        if let Ok(mut m) = self.meta.lock() {
            m.insert(id.to_string(), Meta { subject: subject.clone(), from, to, cc, date, text: plain });
        }
        if item.at("status/read").is_none_or(|r| r.text.trim() != "1" && r.text.trim() != "true") {
            let mark = format!("<methods:markReadRequest><items><item>{}</item></items></methods:markReadRequest>", escape(id));
            let _ = self.call("markReadRequest", &mark).await;
        }
        let mut page = Page { list: list.to_string(), title: subject, emoji: "✉".into(), lines };
        for (i, l) in page.lines.iter_mut().enumerate() {
            l.list = list.to_string();
            l.pos = i as u32 + 1;
        }
        Ok(page)
    }

    /// Downloads an attachment into the cache; where it was saved.
    pub async fn download(&self, what: &str) -> Result<String, String> {
        let rest = what.strip_prefix(ATTACHMENT).ok_or("not an attachment")?;
        let (id, name) = rest.split_once('\n').unwrap_or((rest, "attachment"));
        // A session for its address (the call signs in if needed).
        self.call("getTimestampRequest", "<methods:getTimestampRequest/>").await.ok();
        let (session, poa) = {
            let s = self.session.lock().await;
            let s = s.as_ref().ok_or("not signed in")?;
            (s.id.clone(), s.poa.clone())
        };
        let mut url = reqwest::Url::parse(ATTACHMENT_URL).map_err(|e| e.to_string())?;
        url.query_pairs_mut().append_pair("id", id).append_pair("session", &session);
        let resp = self
            .http
            .get(url)
            .header("X-POA-Address", base64::engine::general_purpose::STANDARD.encode(&poa))
            .header("X-User-Agent", "discommand")
            .send()
            .await
            .map_err(|e| format!("couldn't download {name}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("couldn't download {name} (HTTP {})", resp.status().as_u16()));
        }
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        let dir = crate::system::secret::cache_dir().join("groupwise");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(moodle::safe_name(name));
        std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
        Ok(path.display().to_string())
    }

    /// Sends a mail (plain text). `to` and `cc`: (name, address).
    pub async fn send(&self, to: &[(String, String)], cc: &[(String, String)], subject: &str, text: &str) -> Result<(), String> {
        if to.is_empty() && cc.is_empty() {
            return Err("no one to send it to".into());
        }
        let recipient = |(name, email): &(String, String), kind: &str| {
            format!(
                "<recipient><displayName>{}</displayName><email>{}</email><distType>{kind}</distType><recipType>User</recipType></recipient>",
                escape(if name.is_empty() { email } else { name }),
                escape(email)
            )
        };
        let all: String = to.iter().map(|p| recipient(p, "TO")).chain(cc.iter().map(|p| recipient(p, "CC"))).collect();
        let to_line = to.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>().join(", ");
        let cc_line = cc.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>().join(", ");
        let body = base64::engine::general_purpose::STANDARD.encode(text.replace('\n', "\r\n"));
        let request = format!(
            r#"<methods:sendItemRequest><item xsi:type="types:Mail"><subject>{}</subject><distribution><to>{}</to><cc>{}</cc><recipients>{all}</recipients></distribution><message><part contentType="text/plain">{body}</part></message></item></methods:sendItemRequest>"#,
            escape(subject),
            escape(&to_line),
            escape(&cc_line),
        );
        self.call("sendItemRequest", &request).await.map(|_| ())
    }

}

/// A folder in the move menu.
#[derive(Clone, Debug, Default)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub depth: usize,
    pub sign: String,
}

/// The folders below the user's home, in the order the sidebar has them, each
/// followed by those inside it; only folders mail can be in (and the task
/// list, as in GroupWise).
fn folder_tree(all: &[&Node]) -> Vec<Folder> {
    let kind = |f: &Node| f.text_at("folderType").unwrap_or("").to_string();
    let root = all.iter().find(|f| kind(f) == "Root").and_then(|f| f.text_at("id")).unwrap_or("").to_string();
    let mut out = Vec::new();
    fn walk(all: &[&Node], parent: &str, depth: usize, out: &mut Vec<Folder>) {
        let mut children: Vec<(u32, String, &Node)> = all
            .iter()
            .filter(|f| f.text_at("parent").unwrap_or("") == parent)
            .filter_map(|f| {
                let kind = f.text_at("folderType").unwrap_or("");
                let pos = if kind == "Checklist" { 6 } else { folder_kind(kind)?.0 };
                Some((pos, f.text_at("name").unwrap_or("").to_string(), *f))
            })
            .collect();
        children.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));
        for (_, name, f) in children {
            let kind = f.text_at("folderType").unwrap_or("");
            let sign = if kind == "Checklist" { "✅" } else { folder_kind(kind).map(|k| k.1).unwrap_or("📁") };
            let id = f.text_at("id").unwrap_or("").to_string();
            out.push(Folder { id: id.clone(), name, depth, sign: sign.into() });
            walk(all, &id, depth + 1, out);
        }
    }
    walk(all, &root, 0, &mut out);
    out
}

/// Where a folder goes in the sidebar and its sign, by GroupWise's folder
/// type; `None` for folders that aren't about mail.
fn folder_kind(kind: &str) -> Option<(u32, &'static str)> {
    Some(match kind {
        "Mailbox" => (1, "📥"),
        "SentItems" => (2, "📤"),
        "Draft" => (3, "📝"),
        "Cabinet" => (4, "🗄"),
        "JunkMail" => (8, "🚫"),
        "Trash" => (9, "🗑"),
        "" | "Normal" => (5, "📁"),
        _ => return None,
    })
}

/// (name, address) of an address book entry: its name, and its primary
/// address (or the first, or a group's).
fn contact(item: &Node) -> Option<(String, String)> {
    let list = item.child("emailList");
    let email = list
        .and_then(|l| l.attr("primary").map(String::from).or_else(|| l.text_at("email").map(String::from)))
        .or_else(|| item.text_at("email").map(String::from))
        .filter(|e| e.contains('@'))?;
    let name = item
        .text_at("name")
        .map(String::from)
        .or_else(|| item.text_at("fullName/displayName").map(String::from))
        .unwrap_or_default();
    Some((name, email))
}

/// (name, address) of a sender or recipient.
fn person(n: Option<&Node>) -> (String, String) {
    let Some(n) = n else { return Default::default() };
    (n.text_at("displayName").unwrap_or("").to_string(), n.text_at("email").unwrap_or("").to_string())
}

fn name_of(n: Option<&Node>) -> Option<String> {
    let (name, email) = person(n);
    Some(if name.is_empty() { email } else { name }).filter(|s| !s.is_empty())
}

fn show_person((name, email): &(String, String)) -> String {
    match (name.is_empty(), email.is_empty()) {
        (false, false) if name != email => format!("{name} <{email}>"),
        (false, _) => name.clone(),
        _ => email.clone(),
    }
}

/// The mail's text as HTML (its plain part, else its HTML part), and as plain
/// text for quoting.
fn message_text(item: &Node) -> (String, String) {
    let parts: Vec<&Node> = item.at("message").map(|m| m.all("part").collect()).unwrap_or_default();
    let decode = |p: &Node| {
        let raw: String = p.text.split_whitespace().collect();
        base64::engine::general_purpose::STANDARD.decode(raw).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_else(|_| p.text.clone())
    };
    let kind = |p: &&Node| p.attr("contentType").unwrap_or("text/plain").to_ascii_lowercase();
    if let Some(p) = parts.iter().find(|p| kind(p).starts_with("text/plain")) {
        // Plain text as HTML lines: line breaks kept, nothing taken as markup.
        let text = decode(p);
        return (text.lines().map(escape).collect::<Vec<_>>().join("<br>"), text);
    }
    let html = parts.iter().find(|p| kind(p).starts_with("text/html")).map(|p| decode(p)).unwrap_or_default();
    let plain = moodle::plain_text(&html);
    (html, plain)
}

fn parse_time(s: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .or_else(|| chrono::NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S").ok().map(|n| n.and_utc().fixed_offset()))
        .map(|t| t.with_timezone(&Local))
}

/// "14:05" today, "Tue 12 Nov" this year, else "12 Nov 2025".
fn short_time(t: DateTime<Local>) -> String {
    let now = Local::now();
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M").to_string()
    } else if t.format("%Y").to_string() == now.format("%Y").to_string() {
        t.format("%a %d %b").to_string()
    } else {
        t.format("%d %b %Y").to_string()
    }
}

/// The mail was opened: its "●" and its folder's count go.
pub fn mail_read(st: &mut crate::planner::todo::TodoState, page: &str) {
    let mut folder = None;
    for p in st.pages.values_mut() {
        if let Some(line) = p.lines.iter_mut().find(|l| l.important && l.sublist.as_deref() == Some(page)) {
            line.important = false;
            line.title = line.title.trim_start_matches("● ").to_string();
            folder = Some(p.list.clone());
        }
    }
    let Some(folder) = folder else { return };
    if let Some(l) = st.lists.iter_mut().find(|l| l.id == folder) {
        let (name, n) = match l.name.rsplit_once(" (") {
            Some((name, rest)) => (name.to_string(), rest.trim_end_matches(" new)").parse::<usize>().unwrap_or(1)),
            None => return,
        };
        l.name = if n > 1 { format!("{name} ({} new)", n - 1) } else { name };
    }
}

/// The unread mails in the mailbox, for the new-mail alert.
pub fn unread(st: &crate::planner::todo::TodoState) -> usize {
    st.lists
        .iter()
        .filter(|l| l.default)
        .filter_map(|l| l.name.rsplit_once(" (").and_then(|(_, r)| r.trim_end_matches(" new)").parse::<usize>().ok()))
        .sum()
}

/// "Re: …", once.
pub fn reply_subject(subject: &str) -> String {
    let lower = subject.to_ascii_lowercase();
    if lower.starts_with("re:") || lower.starts_with("aw:") {
        subject.to_string()
    } else {
        format!("Re: {subject}")
    }
}

/// A reply's text: what was typed, then the original, quoted.
pub fn quoted(typed: &str, original: &Meta) -> String {
    let who = if original.from.0.is_empty() { &original.from.1 } else { &original.from.0 };
    let quote: String = original.text.lines().map(|l| format!("> {l}\n")).collect();
    format!("{typed}\n\nOn {}, {who} wrote:\n{quote}", original.date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::todo::TodoState;

    #[test]
    fn reading_a_mail_lowers_the_count() {
        let mut st = TodoState::default();
        st.lists.push(TaskList { id: "box".into(), name: "Mailbox (2 new)".into(), default: true, ..TaskList::default() });
        let line = Task { title: "● Hi  · Ann · 10:00".into(), important: true, sublist: Some(format!("{MAIL}1")), ..Task::default() };
        st.pages.insert("box".into(), Page { list: "box".into(), lines: vec![line], ..Page::default() });
        assert_eq!(unread(&st), 2);
        mail_read(&mut st, &format!("{MAIL}1"));
        assert_eq!(st.lists[0].name, "Mailbox (1 new)");
        assert_eq!(st.pages["box"].lines[0].title, "Hi  · Ann · 10:00");
        assert_eq!(unread(&st), 1);
    }

    #[test]
    fn orders_folders_like_the_move_menu() {
        let xml = r#"<r><folders><folder><id>root</id><name>Timo Koller Home</name><folderType>Root</folderType></folder><folder><id>t</id><parent>root</parent><name>Trash</name><folderType>Trash</folderType></folder><folder><id>c</id><parent>root</parent><name>Cabinet</name><folderType>Cabinet</folderType></folder><folder><id>u</id><parent>c</parent><name>Uni</name></folder><folder><id>m</id><parent>root</parent><name>Mailbox</name><folderType>Mailbox</folderType></folder><folder><id>k</id><parent>root</parent><name>Calendar</name><folderType>Calendar</folderType></folder><folder><id>l</id><parent>root</parent><name>Tasklist</name><folderType>Checklist</folderType></folder></folders></r>"#;
        let root = crate::groupwise::xml::parse(xml).unwrap();
        let all: Vec<_> = root.find("folders").unwrap().all("folder").collect();
        let tree: Vec<_> = super::folder_tree(&all).into_iter().map(|f| (f.name, f.depth)).collect();
        assert_eq!(tree, [("Mailbox".to_string(), 0), ("Cabinet".into(), 0), ("Uni".into(), 1), ("Tasklist".into(), 0), ("Trash".into(), 0)]);
    }

    #[test]
    fn reads_address_book_entries() {
        let xml = r#"<r><items><item><name>Raphael Wimmer</name><emailList primary="raphael.wimmer@ur.de"><email>rw@x.de</email></emailList></item><item><name>Lehrstuhl</name><email>ls@ur.de</email></item><item><name>No address</name></item></items></r>"#;
        let root = crate::groupwise::xml::parse(xml).unwrap();
        let found: Vec<_> = root.find("items").unwrap().all("item").filter_map(super::contact).collect();
        assert_eq!(found, [("Raphael Wimmer".to_string(), "raphael.wimmer@ur.de".to_string()), ("Lehrstuhl".into(), "ls@ur.de".into())]);
    }

    #[test]
    fn replies_quote_and_prefix() {
        assert_eq!(reply_subject("Termin"), "Re: Termin");
        assert_eq!(reply_subject("AW: Termin"), "AW: Termin");
        let m = Meta { from: ("Ann".into(), "a@x.de".into()), date: "Tue 12 Nov".into(), text: "line 1\nline 2".into(), ..Meta::default() };
        assert_eq!(quoted("ok", &m), "ok\n\nOn Tue 12 Nov, Ann wrote:\n> line 1\n> line 2\n");
    }
}
