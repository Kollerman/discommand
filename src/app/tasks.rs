//! The task tabs (Microsoft To Do, Twos, Moodle): syncing, the task tree,
//! pages (a list or a Moodle course shown in full), ticking tasks off, and
//! Moodle uploads and hand-ins (always asked before).

use super::*;
use super::keys::is_ctrl;

impl App {
    pub(super) fn tab(&self, s: Service) -> &TaskTab {
        match s {
            Service::Twos => &self.twos,
            Service::Moodle => &self.moodle,
            _ => &self.todo,
        }
    }

    pub(super) fn tab_mut(&mut self, s: Service) -> &mut TaskTab {
        match s {
            Service::Twos => &mut self.twos,
            Service::Moodle => &mut self.moodle,
            _ => &mut self.todo,
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
        self.todo.state.due_count(today) + self.twos.state.due_count(today) + self.moodle.state.due_count(today)
    }

    /// Shows the dated tasks of all task apps in the calendar.
    pub(super) fn show_tasks_in_cal(&mut self) {
        let mut events = self.todo.state.as_events(TODO_CAL);
        events.extend(self.twos.state.as_events(TWOS_CAL));
        events.extend(self.moodle.state.as_events(MOODLE_CAL));
        let moodle = match &self.moodle.api {
            Some(Backend::Moodle(m)) => m.site_name().to_string(),
            _ => "Moodle".into(),
        };
        self.cal.set_tasks(
            events,
            &[(TODO_CAL, "Microsoft To Do", [37, 100, 207]), (TWOS_CAL, "Twos", [235, 140, 50]), (MOODLE_CAL, &moodle, [249, 128, 18])],
        );
    }

    /// The task tab a task shown in the calendar comes from.
    pub(super) fn task_service(cal: &str) -> Service {
        match cal {
            TWOS_CAL => Service::Twos,
            MOODLE_CAL => Service::Moodle,
            _ => Service::Tasks,
        }
    }

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

    pub(super) fn on_todo(&mut self, s: Service, msg: TodoMsg) {
        let poll = self.tab(s).api.as_ref().map_or(todo::POLL, Backend::poll);
        match msg {
            TodoMsg::Synced(lists, tasks, pages) => {
                let st = &mut self.tab_mut(s).state;
                st.syncing = false;
                st.next_poll = Some(std::time::Instant::now() + poll);
                st.apply(lists, tasks, pages);
                self.done_refreshing();
            }
            TodoMsg::File(Ok(path)) if crate::pdf::is_pdf(&path) => self.open_pdf(path.into()),
            TodoMsg::File(Ok(path)) => match open_file(&path) {
                Ok(true) => self.status = format!("opened {path}"),
                Ok(false) => self.status = format!("saved {path}; not opened because this file type can run code"),
                Err(e) => self.status = format!("could not open {path}: {e}"),
            },
            TodoMsg::File(Err(e)) => self.status = e,
            TodoMsg::Confirm(Ok(c)) => {
                self.status.clear();
                self.confirm = Some(c);
            }
            TodoMsg::Confirm(Err(e)) => self.status = e,
            TodoMsg::Done(page, result) => {
                self.status = match result {
                    Ok(m) => format!("✔ {m}"),
                    Err(e) => format!("✖ {e}"),
                };
                // Read the page (and the deadlines) again, to show what Moodle has now.
                let st = &mut self.tab_mut(s).state;
                st.pages.remove(&page);
                st.loading.remove(&page);
                self.todo_sync(s);
            }
            TodoMsg::Page(list, page) => match page {
                Ok(p) => {
                    let tab = self.tab_mut(s);
                    if list.starts_with(moodle::CONVERSATION) && tab.page.as_deref() == Some(list.as_str()) {
                        tab.page_sel = Some(p.lines.len().saturating_sub(1));
                    }
                    let st = &mut tab.state;
                    st.loading.remove(&list);
                    // Opening a conversation marked it read: its "new" goes.
                    let read = list.starts_with(moodle::CONVERSATION);
                    if read {
                        moodle::conversation_read(st, &list);
                    }
                    st.pages.insert(list, p);
                    self.tree_dirty |= read;
                }
                // Stays marked as loading, so it's tried again at the next sync, not on every redraw.
                Err(e) => self.status = e,
            },
            TodoMsg::Error(e) => {
                let st = &mut self.tab_mut(s).state;
                st.syncing = false;
                st.login = None;
                // Offline: try again in a minute (other errors at the next regular sync).
                let offline = e.contains(moodle::OFFLINE);
                st.next_poll = Some(std::time::Instant::now() + if offline { std::time::Duration::from_secs(60) } else { poll });
                st.status = if offline { "offline, retrying".into() } else { "sync failed".into() };
                self.status = if offline { format!("{} {e}: trying again in a minute", s.name()) } else { e };
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

    pub(super) fn todo_command(&mut self, arg: &str) {
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

    pub(super) fn moodle_command(&mut self, arg: &str) {
        match arg {
            "logout" => {
                moodle::Creds::remove();
                self.moodle = TaskTab::default();
                self.show_tasks_in_cal();
                self.tree_dirty = true;
                self.status = "Moodle disconnected".into();
            }
            "refresh" | "sync" | "" => self.todo_sync(Service::Moodle),
            "login" => self.status = "sign in from a terminal: discommand --moodle-login (the password is typed hidden)".into(),
            _ => self.status = "usage: :moodle refresh · :moodle logout (log in: discommand --moodle-login)".into(),
        }
    }

    pub(super) fn twos_command(&mut self, arg: &str) {
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
        pane == Focus::Messages && self.tab(self.service).page.is_some()
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
                Some(ItemKind::TodoList) => item.map(|t| t.id.clone()).filter(|id| id != MORE_LISTS && !id.starts_with(GROUP)),
                _ => task.map(|t| t.list.clone()),
            };
            let list = list.or_else(|| self.tasks().default_list().map(String::from));
            let key = task.map(|t| t.key());
            let tab = self.tab_mut(s);
            tab.page_sel = key.and_then(|k| tab.state.pages.get(list.as_ref()?)?.lines.iter().position(|t| t.key() == k));
            tab.page = list;
            tab.back.clear();
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
    pub(super) fn page_key(&mut self, k: KeyEvent, pending: Option<char>, n: usize, counted: bool) {
        // Back: to the page this one was opened from, else to the sidebar
        // (also while the page is still loading).
        if matches!(k.code, KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace) {
            let tab = self.tab_mut(self.service);
            match tab.back.pop() {
                Some((page, sel)) => {
                    tab.page = Some(page);
                    tab.page_sel = sel;
                }
                None => self.focus = Focus::Tree,
            }
            return;
        }
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
            // A sub-list opens in place (h goes back to the sidebar's list), a
            // file downloads and opens, a link goes to the browser; o always
            // goes to the browser when there's a link.
            KeyCode::Char('F') if self.tab(self.service).page.as_deref().is_some_and(|p| p.starts_with(moodle::ASSIGN)) => {
                let cmid = self.tab(self.service).page.as_deref().and_then(|p| p.strip_prefix(moodle::ASSIGN)).unwrap_or("").to_string();
                self.assign_upload(cmid);
                return;
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right
                if self.page_task().and_then(|t| t.sublist.as_deref()).is_some_and(|s| s.starts_with(moodle::ASSIGN_UPLOAD) || s.starts_with(moodle::ASSIGN_SUBMIT)) =>
            {
                let action = self.page_task().and_then(|t| t.sublist.clone()).unwrap_or_default();
                if let Some(cmid) = action.strip_prefix(moodle::ASSIGN_UPLOAD) {
                    self.assign_upload(cmid.to_string());
                } else if let Some(cmid) = action.strip_prefix(moodle::ASSIGN_SUBMIT) {
                    self.assign_submit(cmid.to_string());
                }
                return;
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right | KeyCode::Char('o') => {
                let Some(t) = self.page_task() else { return };
                let browser = k.code == KeyCode::Char('o') && !t.url.is_empty();
                let file = t.sublist.as_deref().and_then(|s| s.strip_prefix(moodle::FILE)).map(String::from);
                if let (Some(url), false) = (file, browser) {
                    let s = self.service;
                    let Some(api) = self.tab(s).api.clone() else { return };
                    self.status = format!("downloading {}…", t.title);
                    self.spawn(async move { Some(AppEvent::Todo(s, TodoMsg::File(api.download(&url).await))) });
                } else if let (Some(sub), false) = (t.sublist.clone(), browser) {
                    let tab = self.tab_mut(self.service);
                    if let Some(current) = tab.page.replace(sub.clone()) {
                        tab.back.push((current, tab.page_sel));
                    }
                    // A chat opens at its newest message, everything else at the top.
                    tab.page_sel = match tab.state.pages.get(&sub) {
                        Some(p) if sub.starts_with(moodle::CONVERSATION) => Some(p.lines.len().saturating_sub(1)),
                        _ => Some(0),
                    };
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
    pub(super) fn target_list(&self) -> Option<String> {
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
    pub(super) fn toggle_task(&mut self, s: Service, key: &str) {
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

    pub(super) fn todo_key(&mut self, k: KeyEvent, pending: Option<char>) -> bool {
        // Read-only backends (Moodle): no adding, editing or deleting.
        let editable = self.tab(self.service).api.as_ref().is_none_or(Backend::editable);
        let edit_key = matches!(k.code, KeyCode::Char('n' | 'i' | 'a' | 'e' | 'c')) || (k.code == KeyCode::Char('d') && !is_ctrl(&k));
        if !editable && edit_key {
            self.status = todo::READ_ONLY.into();
            return true;
        }
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

    fn moodle_api(&self) -> Option<Arc<Moodle>> {
        match &self.moodle.api {
            Some(Backend::Moodle(m)) => Some(m.clone()),
            _ => None,
        }
    }

    /// Upload to an assignment: pick the file, check it against the
    /// assignment's rules, then ask (nothing is sent before the yes).
    fn assign_upload(&mut self, cmid: String) {
        let Some(m) = self.moodle_api() else { return };
        let picked = crate::system::pick::file("Upload to your assignment submission");
        self.dirty = true;
        let path = match picked {
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(e) => {
                self.status = format!("{e} (for assignments, uploading needs the file dialog)");
                return;
            }
        };
        self.status = "checking the file and the assignment…".into();
        let s = self.service;
        self.spawn(async move {
            let plan = m.prepare_upload(&cmid, path).await;
            Some(AppEvent::Todo(s, TodoMsg::Confirm(plan.map(|p| Box::new(upload_question(p))))))
        });
    }

    /// Hand an assignment in: read the declaration, then ask.
    fn assign_submit(&mut self, cmid: String) {
        let Some(m) = self.moodle_api() else { return };
        self.status = "reading the hand-in page…".into();
        let s = self.service;
        self.spawn(async move {
            let plan = m.prepare_submit(&cmid).await;
            Some(AppEvent::Todo(s, TodoMsg::Confirm(plan.map(|p| Box::new(submit_question(p))))))
        });
    }

    pub(super) fn run_confirmed(&mut self, c: Confirm) {
        let Some(m) = self.moodle_api() else { return };
        let s = Service::Moodle;
        match c.action {
            ConfirmAction::Upload(plan) => {
                self.status = format!("uploading {} ({})…", plan.name, moodle::size(plan.size));
                self.spawn(async move {
                    let page = format!("{}{}", moodle::ASSIGN, plan.cmid);
                    Some(AppEvent::Todo(s, TodoMsg::Done(page, m.upload(&plan).await)))
                });
            }
            ConfirmAction::Submit(plan) => {
                self.status = "handing in…".into();
                self.spawn(async move {
                    let page = format!("{}{}", moodle::ASSIGN, plan.cmid);
                    Some(AppEvent::Todo(s, TodoMsg::Done(page, m.submit(&plan).await)))
                });
            }
        }
    }
}

/// The question before an upload: everything that matters for an assignment.
fn upload_question(p: moodle::UploadPlan) -> Confirm {
    let mut lines = vec![format!("Assignment:  {}", p.info.title)];
    lines.extend(p.info.dates.iter().map(|d| format!("             {d}")));
    if let Some(left) = p.info.get("Verbleibende Zeit").or_else(|| p.info.get("Time remaining")) {
        lines.push(format!("             {left}"));
    }
    if let Some(st) = p.info.get("Abgabestatus").or_else(|| p.info.get("Submission status")) {
        lines.push(format!("Status now:  {st}"));
    }
    lines.push(String::new());
    lines.push(format!("File:        {} ({})", p.name, moodle::size(p.size)));
    lines.push(format!("From:        {}", p.path.display()));
    if p.replaces() {
        lines.push(format!("⚠ Replaces the file of the same name already in your submission: {}", p.name));
    }
    let others: Vec<&String> = p.existing.iter().filter(|f| **f != p.name).collect();
    if !others.is_empty() {
        lines.push(format!("Stays in your submission: {}", others.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")));
    }
    let mut rules = Vec::new();
    if p.maxbytes > 0 {
        rules.push(format!("at most {} per file", moodle::size(p.maxbytes as u64)));
    }
    if p.maxfiles > 0 {
        rules.push(format!("up to {} files", p.maxfiles));
    }
    if !p.accepted.is_empty() {
        rules.push(format!("types: {}", p.accepted.join(" ")));
    }
    if !rules.is_empty() {
        lines.push(format!("Rules (checked, the file fits): {}", rules.join("; ")));
    }
    lines.push(String::new());
    lines.push(if p.info.can_submit {
        "Moodle saves your submission as a draft; hand it in afterwards with ✅ Hand in.".into()
    } else {
        "Moodle saves your submission. If the course asks for it, hand it in afterwards (✅ appears then).".into()
    });
    lines.push("Afterwards discommand checks on the assignment page that the file is there.".into());
    Confirm { title: "Upload to your assignment?".into(), lines, yes: "upload".into(), action: ConfirmAction::Upload(p) }
}

/// The question before handing in: the files, and the declaration in full.
fn submit_question(p: moodle::SubmitPlan) -> Confirm {
    let mut lines = vec![format!("Assignment:  {}", p.info.title)];
    lines.extend(p.info.dates.iter().map(|d| format!("             {d}")));
    if let Some(left) = p.info.get("Verbleibende Zeit").or_else(|| p.info.get("Time remaining")) {
        lines.push(format!("             {left}"));
    }
    lines.push(String::new());
    if p.info.files.is_empty() {
        lines.push("⚠ Your submission has no files.".into());
    } else {
        lines.push("These files are handed in:".into());
        lines.extend(p.info.files.iter().map(|f| format!("  📄 {f}")));
    }
    lines.push(String::new());
    if let Some(st) = &p.statement {
        lines.push("By pressing y you agree to this declaration:".into());
        lines.push(format!("  “{st}”"));
        lines.push(String::new());
    }
    lines.push("⚠ After handing in you usually can't change your submission any more.".into());
    Confirm { title: "Hand in for grading?".into(), lines, yes: "hand in".into(), action: ConfirmAction::Submit(p) }
}
