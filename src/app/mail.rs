//! Writing mail in the GroupWise tab: a compose window in the right pane, as
//! in GroupWise's own "New Message": To, Cc, Subject and the text. `n` (or
//! `:mail`) opens an empty one, `r` / `R` on a mail a reply (to the sender /
//! to all) with the original quoted under it.
//!
//! Tab and Shift+Tab (or ↑ ↓ at the edges) move between the fields; Enter
//! moves on from a header field and starts a new line in the text. Ctrl+S
//! sends; Esc twice discards.

use super::*;
use crate::ui::text::Input;

/// The fields of the compose window, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    To,
    Cc,
    Subject,
    Body,
}

impl Field {
    const ALL: [Field; 4] = [Field::To, Field::Cc, Field::Subject, Field::Body];

    fn step(self, by: isize) -> Field {
        let i = Self::ALL.iter().position(|f| *f == self).unwrap_or(0) as isize;
        Self::ALL[(i + by).clamp(0, 3) as usize]
    }

    pub fn label(self) -> &'static str {
        match self {
            Field::To => "To",
            Field::Cc => "Cc",
            Field::Subject => "Subject",
            Field::Body => "",
        }
    }
}

/// A mail being written.
pub struct Compose {
    pub to: Input,
    pub cc: Input,
    pub subject: Input,
    pub body: Input,
    pub field: Field,
    /// "New message" or "Reply to Ann".
    pub title: String,
    /// The mail replied to: shown under the text, and sent quoted.
    #[cfg(feature = "groupwise")]
    pub quote: Option<crate::groupwise::mail::Meta>,
    /// Esc was pressed once on a mail with text in it.
    pub discard_asked: bool,
    /// Typing in a field (as insert mode); else `:` and the other normal keys work.
    pub typing: bool,
    /// Address suggestions for what's being typed in To or Cc (name,
    /// address), the one picked, and the text they were looked up for.
    pub suggestions: Vec<(String, String)>,
    pub picked: usize,
    pub suggested_for: String,
    /// The text being looked up right now.
    #[cfg_attr(not(feature = "groupwise"), allow(dead_code))]
    pub asking: Option<String>,
}

impl Compose {
    pub fn input(&self, f: Field) -> &Input {
        match f {
            Field::To => &self.to,
            Field::Cc => &self.cc,
            Field::Subject => &self.subject,
            Field::Body => &self.body,
        }
    }

    fn input_mut(&mut self, f: Field) -> &mut Input {
        match f {
            Field::To => &mut self.to,
            Field::Cc => &mut self.cc,
            Field::Subject => &mut self.subject,
            Field::Body => &mut self.body,
        }
    }

    /// Whether anything was written (text, or a subject).
    pub fn has_text(&self) -> bool {
        !self.body.text().trim().is_empty()
    }

    /// The address being typed: what follows the last comma in To or Cc.
    pub fn typed_address(&self) -> Option<String> {
        if !matches!(self.field, Field::To | Field::Cc) {
            return None;
        }
        let text = self.input(self.field).text();
        let last = text.rsplit([',', ';']).next().unwrap_or("").trim().to_string();
        Some(last).filter(|t| t.chars().count() >= 2 && !t.contains('<'))
    }

    /// Whether the suggestions shown belong to what's typed now.
    pub fn showing(&self) -> bool {
        !self.suggestions.is_empty() && self.typed_address().as_deref() == Some(self.suggested_for.as_str())
    }

    /// Puts the picked suggestion in place of what was typed.
    fn take_suggestion(&mut self) {
        let Some((name, email)) = self.suggestions.get(self.picked).cloned() else { return };
        let field = self.field;
        let text = self.input(field).text();
        let kept = text.rfind([',', ';']).map(|i| format!("{} ", &text[..=i])).unwrap_or_default();
        let person = if name.is_empty() { email } else { format!("{name} <{email}>") };
        self.input_mut(field).set(&format!("{kept}{person}, "));
        self.suggestions.clear();
    }

    /// The original, quoted, as shown under the text.
    pub fn quote_lines(&self) -> Vec<String> {
        #[cfg(feature = "groupwise")]
        if let Some(q) = &self.quote {
            return crate::groupwise::mail::quoted("", q).trim_start().lines().map(String::from).collect();
        }
        Vec::new()
    }
}

/// The move menu (`m`): the folders, the one picked, and the mails to move.
pub struct MoveMenu {
    /// (folder id, how it's shown: indented, with its sign).
    pub folders: Vec<(String, String)>,
    pub picked: usize,
    pub ids: Vec<String>,
    pub from: String,
}

/// "1 mail", "3 mails".
#[cfg_attr(not(feature = "groupwise"), allow(dead_code))]
fn count(n: usize) -> String {
    if n == 1 { "1 mail".into() } else { format!("{n} mails") }
}

/// "Ann <a@x.de>, b@y.de" as a field's text.
#[cfg_attr(not(feature = "groupwise"), allow(dead_code))]
fn people_text(people: &[(String, String)]) -> String {
    people
        .iter()
        .map(|(n, e)| if n.is_empty() || n == e { e.clone() } else { format!("{n} <{e}>") })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A field's text as (name, address) pairs; what isn't an address is ignored.
#[cfg_attr(not(feature = "groupwise"), allow(dead_code))]
pub fn people(text: &str) -> Vec<(String, String)> {
    text.split([',', ';'])
        .filter_map(|p| {
            let p = p.trim();
            match (p.rfind('<'), p.rfind('>')) {
                (Some(a), Some(b)) if a < b => Some((p[..a].trim().trim_matches('"').to_string(), p[a + 1..b].trim().to_string())),
                _ => Some((String::new(), p.to_string())),
            }
        })
        .filter(|(_, e)| e.contains('@') && !e.contains(char::is_whitespace))
        .collect()
}

impl App {
    /// Keys while the compose window is open.
    pub(super) fn compose_key(&mut self, k: KeyEvent) {
        let ctrl = super::keys::is_ctrl(&k);
        let Some(c) = self.compose.as_mut() else { return };
        if k.code != KeyCode::Esc {
            c.discard_asked = false;
        }
        // With suggestions open: arrows pick, Tab or Enter take, Esc closes them.
        if c.showing() {
            match k.code {
                KeyCode::Down => c.picked = (c.picked + 1).min(c.suggestions.len() - 1),
                KeyCode::Up => c.picked = c.picked.saturating_sub(1),
                KeyCode::Tab | KeyCode::Enter => c.take_suggestion(),
                KeyCode::Esc => c.suggestions.clear(),
                _ => {}
            }
            if matches!(k.code, KeyCode::Down | KeyCode::Up | KeyCode::Tab | KeyCode::Enter | KeyCode::Esc) {
                return;
            }
        }
        if k.code == KeyCode::Char('s') && ctrl {
            return self.send_compose();
        }
        // Not typing (after Esc), as normal mode everywhere else: `:` for
        // commands (`:w` sends, `:q!` discards), i / Enter type again.
        if !c.typing {
            match k.code {
                KeyCode::Esc => {
                    if !c.has_text() || c.discard_asked {
                        self.compose = None;
                        self.status = "mail discarded".into();
                    } else {
                        c.discard_asked = true;
                        self.status = "Esc again discards this mail (:w or Ctrl+S sends it)".into();
                    }
                }
                KeyCode::Char(':' | 'ö' | 'Ö') => {
                    self.last_pane = self.focus;
                    self.focus = Focus::Command;
                }
                KeyCode::Char('i' | 'a') | KeyCode::Enter => {
                    c.typing = true;
                    self.status.clear();
                }
                KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => c.field = c.field.step(1),
                KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => c.field = c.field.step(-1),
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Esc => {
                c.typing = false;
                c.suggestions.clear();
                self.status = ": commands (:w sends, :q! discards) · i type · Esc Esc discard".into();
            }
            KeyCode::Tab => c.field = c.field.step(1),
            KeyCode::BackTab => c.field = c.field.step(-1),
            KeyCode::Enter if c.field == Field::Body => c.body.insert('\n'),
            KeyCode::Enter => c.field = c.field.step(1),
            KeyCode::Down if c.field != Field::Body => c.field = c.field.step(1),
            KeyCode::Up if c.field == Field::Body && !c.body.move_line(false) => c.field = Field::Subject,
            KeyCode::Up if c.field != Field::Body => c.field = c.field.step(-1),
            KeyCode::Down => {
                c.body.move_line(true);
            }
            KeyCode::Up => {}
            _ => {
                let field = c.field;
                c.input_mut(field).handle_key(&k);
            }
        }
        self.ask_suggestions();
    }

    /// `:w` (or `:wq`, `:x`, `:send`) sends the mail being written, `:q` closes
    /// it if nothing was written, `:q!` discards it. Whether it was one of them.
    pub(super) fn compose_command(&mut self, name: &str) -> bool {
        let Some(c) = self.compose.as_ref() else { return false };
        match name {
            "w" | "wq" | "x" | "send" => self.send_compose(),
            "q" | "quit" if c.has_text() => self.status = "the mail isn't sent: :w sends it, :q! discards it".into(),
            "q" | "quit" | "q!" | "discard" => {
                self.compose = None;
                self.status = "mail discarded".into();
            }
            _ => return false,
        }
        true
    }

    /// A paste goes into the field being typed in.
    pub(super) fn compose_paste(&mut self, text: &str) {
        let Some(c) = self.compose.as_mut() else { return };
        let text = if c.field == Field::Body { text.to_string() } else { text.replace(['\n', '\r'], " ") };
        let field = c.field;
        c.input_mut(field).insert_str(&text);
        self.ask_suggestions();
    }

    /// Opens the compose window (in the GroupWise tab).
    #[cfg_attr(not(feature = "groupwise"), allow(dead_code))]
    fn open_compose(&mut self, to: &[(String, String)], cc: &[(String, String)], subject: &str, title: String, #[cfg(feature = "groupwise")] quote: Option<crate::groupwise::mail::Meta>) {
        if self.service != Service::GroupWise {
            self.switch_service(Service::GroupWise);
        }
        let mut c = Compose {
            to: Input::default(),
            cc: Input::default(),
            subject: Input::default(),
            body: Input::default(),
            field: Field::To,
            title,
            #[cfg(feature = "groupwise")]
            quote,
            discard_asked: false,
            typing: true,
            suggestions: Vec::new(),
            picked: 0,
            suggested_for: String::new(),
            asking: None,
        };
        c.to.set(&people_text(to));
        c.cc.set(&people_text(cc));
        c.subject.set(subject);
        // A reply starts in the text; a new mail at its address.
        c.field = if to.is_empty() { Field::To } else if subject.is_empty() { Field::Subject } else { Field::Body };
        self.compose = Some(c);
        // The window is in the right pane (on narrow screens the only one).
        self.focus = Focus::Messages;
        self.status.clear();
    }
}

#[cfg(feature = "groupwise")]
impl App {
    fn mail_api(&self) -> Option<Arc<crate::groupwise::mail::GroupWise>> {
        match &self.groupwise.api {
            Some(Backend::GroupWise(g)) => Some(g.clone()),
            _ => None,
        }
    }

    /// `n` in the GroupWise tab, or `:mail [ADDRESS] [SUBJECT]`.
    pub(super) fn mail_command(&mut self, arg: &str) {
        if self.mail_api().is_none() {
            self.status = "GroupWise isn't connected: run discommand --groupwise-login once".into();
            return;
        }
        let arg = arg.trim();
        let (who, subject) = match arg.split_once(' ') {
            Some((w, s)) if w.contains('@') => (w, s.trim()),
            _ if arg.contains('@') => (arg, ""),
            _ => ("", arg),
        };
        self.open_compose(&people(who), &[], subject, "New message".into(), None);
    }

    /// `r` / `R` on an open mail.
    pub(super) fn mail_reply(&mut self, all: bool) {
        use crate::groupwise::mail::{reply_subject, MAIL};
        let (Some(api), Some(id)) = (self.mail_api(), self.groupwise.page.as_deref().and_then(|p| p.strip_prefix(MAIL))) else {
            self.status = "open a mail first (⏎), then r replies".into();
            return;
        };
        let Some(meta) = api.meta(id) else {
            self.status = "the mail is still loading".into();
            return;
        };
        let me = api.me().to_ascii_lowercase();
        let not_me = |p: &&(String, String)| !p.1.is_empty() && p.1.to_ascii_lowercase() != me;
        let mut to = vec![meta.from.clone()];
        let mut cc = Vec::new();
        if all {
            to.extend(meta.to.iter().filter(not_me).filter(|p| p.1 != meta.from.1).cloned());
            cc = meta.cc.iter().filter(not_me).cloned().collect();
        }
        let who = if meta.from.0.is_empty() { meta.from.1.clone() } else { meta.from.0.clone() };
        let title = if all { format!("Reply to all ({who} and others)") } else { format!("Reply to {who}") };
        let subject = reply_subject(&meta.subject);
        self.open_compose(&to, &cc, &subject, title, Some(meta));
    }

    /// Looks up addresses for what's being typed in To or Cc (one lookup
    /// at a time; the answer asks again if the text moved on meanwhile).
    pub(super) fn ask_suggestions(&mut self) {
        let Some(api) = self.mail_api() else { return };
        let Some(c) = self.compose.as_mut() else { return };
        let Some(typed) = c.typed_address() else {
            c.suggestions.clear();
            return;
        };
        if c.asking.is_some() || typed == c.suggested_for {
            return;
        }
        c.asking = Some(typed.clone());
        self.spawn(async move {
            let people = api.suggest(&typed).await;
            Some(AppEvent::Suggest(typed, people))
        });
    }

    pub(super) fn on_suggest(&mut self, typed: String, people: Vec<(String, String)>) {
        let Some(c) = self.compose.as_mut() else { return };
        c.asking = None;
        c.suggested_for = typed;
        c.suggestions = people;
        c.picked = 0;
        // Typed on while this was looked up: look up the newer text.
        self.ask_suggestions();
    }

    /// The mails an action is for, and their folder: the open mail; else
    /// the selected ones of the folder shown; else the one under the cursor.
    fn mail_targets(&self) -> Option<(Vec<String>, String)> {
        use crate::groupwise::mail::MAIL;
        let tab = &self.groupwise;
        let page = tab.page.as_deref()?;
        if let Some(id) = page.strip_prefix(MAIL) {
            let from = tab.back.iter().rev().map(|(p, _)| p.as_str()).find(|p| !p.starts_with(MAIL))?;
            return Some((vec![id.to_string()], from.to_string()));
        }
        let lines = &tab.state.pages.get(page)?.lines;
        let mails = || lines.iter().filter(|l| l.sublist.as_deref().is_some_and(|s| s.starts_with(MAIL)));
        let selected: Vec<String> = mails().filter(|l| self.mail_selected.contains(&l.id)).map(|l| l.id.clone()).collect();
        if !selected.is_empty() {
            return Some((selected, page.to_string()));
        }
        let current = self.page_sel().and_then(|i| lines.get(i)).filter(|l| l.sublist.as_deref().is_some_and(|s| s.starts_with(MAIL)))?;
        Some((vec![current.id.clone()], page.to_string()))
    }

    /// Shows which mails of the folder are selected (✔ before them).
    pub(super) fn mark_selected(&mut self) {
        let Some(page) = self.groupwise.page.clone() else { return };
        let selected = &self.mail_selected;
        if let Some(p) = self.groupwise.state.pages.get_mut(&page) {
            for l in &mut p.lines {
                l.marker = if selected.contains(&l.id) { "✔ ".into() } else { String::new() };
            }
        }
    }

    /// `x` / space: selects the mail under the cursor (or unselects it).
    pub(super) fn mail_toggle(&mut self) {
        let Some(id) = self.page_task().filter(|l| l.sublist.as_deref().is_some_and(|s| s.starts_with(crate::groupwise::mail::MAIL))).map(|l| l.id.clone()) else { return };
        if !self.mail_selected.remove(&id) {
            self.mail_selected.insert(id);
        }
        self.mark_selected();
        self.status = format!("{} selected · m move · dd delete · a all", self.mail_selected.len());
    }

    /// `a`: selects every mail of the folder shown (again: none).
    pub(super) fn mail_select_all(&mut self) {
        let Some(page) = self.groupwise.page.clone() else { return };
        let Some(p) = self.groupwise.state.pages.get(&page) else { return };
        let ids: Vec<String> = p.lines.iter().filter(|l| l.sublist.as_deref().is_some_and(|s| s.starts_with(crate::groupwise::mail::MAIL))).map(|l| l.id.clone()).collect();
        if !ids.is_empty() && ids.iter().all(|id| self.mail_selected.contains(id)) {
            self.mail_selected.clear();
            self.status = "nothing selected".into();
        } else {
            self.mail_selected.extend(ids.iter().cloned());
            self.status = format!("all {} selected · m move · dd delete · A mark read", ids.len());
        }
        self.mark_selected();
    }

    /// `A`: marks every unread mail of the folder shown read.
    pub(super) fn mail_all_read(&mut self) {
        let Some(api) = self.mail_api() else { return };
        let Some(page) = self.groupwise.page.clone().filter(|p| !p.starts_with(crate::groupwise::mail::MAIL)) else {
            self.status = "open a folder first".into();
            return;
        };
        // The ones shown lose their ● right away.
        let st = &mut self.groupwise.state;
        let shown: Vec<String> = st.pages.get(&page).map(|p| p.lines.iter().filter(|l| l.important).map(|l| l.id.clone()).collect()).unwrap_or_default();
        for id in &shown {
            crate::groupwise::mail::mail_read(st, &format!("{}{id}", crate::groupwise::mail::MAIL));
        }
        self.tree_dirty = true;
        self.status = "marking read…".into();
        // Then every unread one of the folder, also those too old to be shown.
        let s = Service::GroupWise;
        self.spawn(async move {
            let unread = match api.all_unread(&page).await {
                Ok(ids) => ids,
                Err(e) => return Some(AppEvent::Error(format!("not marked read: {e}"))),
            };
            if unread.is_empty() {
                return Some(AppEvent::Todo(s, TodoMsg::Changed("nothing unread here".into(), true)));
            }
            Some(match api.mark_read(&unread).await {
                Ok(()) => AppEvent::Todo(s, TodoMsg::Changed(format!("{} marked read", count(unread.len())), true)),
                Err(e) => AppEvent::Error(format!("not marked read: {e}")),
            })
        });
    }

    /// `m`: the move menu for the mails (see `mail_targets`).
    pub(super) fn mail_move_menu(&mut self) {
        let Some(api) = self.mail_api() else { return };
        let Some((ids, from)) = self.mail_targets() else {
            self.status = "select a mail first (j/k, x)".into();
            return;
        };
        let folders: Vec<(String, String)> = api
            .folders()
            .into_iter()
            .map(|f| (f.id, format!("{}{} {}", "   ".repeat(f.depth), f.sign, f.name)))
            .collect();
        if folders.is_empty() {
            self.status = "the folders aren't loaded yet".into();
            return;
        }
        let picked = folders.iter().position(|(id, _)| *id != from).unwrap_or(0);
        self.mail_move = Some(MoveMenu { folders, picked, ids, from });
    }

    /// Keys in the move menu: j/k pick, Enter moves, Esc closes.
    pub(super) fn move_key(&mut self, k: KeyEvent) {
        let Some(m) = self.mail_move.as_mut() else { return };
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mail_move = None,
            KeyCode::Char('j') | KeyCode::Down => m.picked = (m.picked + 1).min(m.folders.len() - 1),
            KeyCode::Char('k') | KeyCode::Up => m.picked = m.picked.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('l') => {
                let Some(m) = self.mail_move.take() else { return };
                let Some((to, label)) = m.folders.get(m.picked).cloned() else { return };
                if to == m.from {
                    self.status = "the mails are already there".into();
                    return;
                }
                let name = label.trim().split_once(' ').map(|(_, n)| n.to_string()).unwrap_or(label);
                self.mail_done(m.ids.clone());
                let Some(api) = self.mail_api() else { return };
                let s = Service::GroupWise;
                self.spawn(async move {
                    Some(match api.move_items(&m.ids, &m.from, &to).await {
                        Ok(()) => AppEvent::Todo(s, TodoMsg::Changed(format!("moved {} to {name}", count(m.ids.len())), true)),
                        Err(e) => AppEvent::Error(format!("not moved: {e}")),
                    })
                });
            }
            _ => {}
        }
    }

    /// `dd`: deletes the mails (into GroupWise's Trash).
    pub(super) fn mail_delete(&mut self) {
        let Some(api) = self.mail_api() else { return };
        let Some((ids, from)) = self.mail_targets() else {
            self.status = "select a mail first (j/k, x)".into();
            return;
        };
        self.mail_done(ids.clone());
        let s = Service::GroupWise;
        self.spawn(async move {
            Some(match api.remove_items(&ids, &from).await {
                Ok(()) => AppEvent::Todo(s, TodoMsg::Changed(format!("deleted {} (in the Trash)", count(ids.len())), true)),
                Err(e) => AppEvent::Error(format!("not deleted: {e}")),
            })
        });
    }

    /// Mails moved or deleted: unselected, and an open one is closed.
    fn mail_done(&mut self, ids: Vec<String>) {
        for id in &ids {
            self.mail_selected.remove(id);
        }
        let tab = &mut self.groupwise;
        if tab.page.as_deref().and_then(|p| p.strip_prefix(crate::groupwise::mail::MAIL)).is_some_and(|id| ids.iter().any(|i| i == id)) {
            if let Some((page, sel)) = tab.back.pop() {
                tab.page = Some(page);
                tab.page_sel = sel;
            }
        }
        self.status = "working…".into();
    }

    /// Ctrl+S: sends the mail in the compose window.
    fn send_compose(&mut self) {
        let Some(api) = self.mail_api() else { return };
        let Some(c) = self.compose.as_ref() else { return };
        let (to, cc) = (people(&c.to.text()), people(&c.cc.text()));
        if to.is_empty() && cc.is_empty() {
            self.status = "who to? type an address under To (name@uni-regensburg.de)".into();
            if let Some(c) = self.compose.as_mut() {
                c.field = Field::To;
            }
            return;
        }
        let subject = c.subject.text().trim().to_string();
        let text = c.body.text();
        let body = match &c.quote {
            Some(original) => crate::groupwise::mail::quoted(&text, original),
            None => text,
        };
        self.compose = None;
        self.status = "sending…".into();
        let s = Service::GroupWise;
        self.spawn(async move {
            Some(match api.send(&to, &cc, &subject, &body).await {
                Ok(()) => AppEvent::Todo(s, TodoMsg::Changed(format!("sent: {}", if subject.is_empty() { "(no subject)" } else { &subject }), false)),
                Err(e) => AppEvent::Error(format!("not sent: {e}")),
            })
        });
    }
}

#[cfg(not(feature = "groupwise"))]
impl App {
    pub(super) fn move_key(&mut self, _k: KeyEvent) {
        self.mail_move = None;
    }

    pub(super) fn mark_selected(&mut self) {}

    pub(super) fn mail_toggle(&mut self) {}

    pub(super) fn mail_select_all(&mut self) {}

    pub(super) fn mail_all_read(&mut self) {}

    pub(super) fn mail_move_menu(&mut self) {}

    pub(super) fn mail_delete(&mut self) {}

    pub(super) fn ask_suggestions(&mut self) {}

    pub(super) fn on_suggest(&mut self, _typed: String, _people: Vec<(String, String)>) {}

    pub(super) fn mail_reply(&mut self, _all: bool) {}

    pub(super) fn mail_command(&mut self, _arg: &str) {
        self.status = "GroupWise isn't built in (the groupwise feature)".into();
    }

    fn send_compose(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::people;

    #[test]
    fn takes_a_suggestion_for_the_address_typed() {
        use super::{Compose, Field};
        use crate::ui::text::Input;
        let mut c = Compose {
            to: Input::default(),
            cc: Input::default(),
            subject: Input::default(),
            body: Input::default(),
            field: Field::To,
            title: String::new(),
            #[cfg(feature = "groupwise")]
            quote: None,
            discard_asked: false,
            typing: true,
            suggestions: vec![("Raphael Wimmer".into(), "raphael.wimmer@ur.de".into())],
            picked: 0,
            suggested_for: "wim".into(),
            asking: None,
        };
        c.to.set("Ann <ann@ur.de>, wim");
        assert_eq!(c.typed_address().as_deref(), Some("wim"));
        assert!(c.showing());
        c.take_suggestion();
        assert_eq!(c.to.text(), "Ann <ann@ur.de>, Raphael Wimmer <raphael.wimmer@ur.de>, ");
        assert!(c.typed_address().is_none() && !c.showing());
    }

    #[test]
    fn reads_address_fields() {
        let p = people("Ann Example <ann@x.de>, b@y.de; nobody, \"Ben\" <ben@z.de>");
        assert_eq!(p, [("Ann Example".into(), "ann@x.de".into()), (String::new(), "b@y.de".into()), ("Ben".into(), "ben@z.de".into())]);
    }
}
