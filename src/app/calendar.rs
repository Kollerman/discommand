//! The calendar tab: syncing Google Calendar, moving through the agenda and
//! the time grid, and its keys and commands.

use super::*;
use super::keys::is_ctrl;

impl App {
    /// Selects the first event that hasn't ended yet.
    pub(super) fn jump_to_today(&mut self) {
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
    pub(super) fn cal_select(&mut self, key: &str) {
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

    pub(super) fn cal_grid_key(&mut self, k: KeyEvent, pending: Option<char>, n: usize) {
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

    pub(super) fn on_cal(&mut self, msg: CalMsg) {
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

    pub(super) fn cal_command(&mut self, arg: &str) {
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
    pub(super) fn cal_key(&mut self, k: KeyEvent, pending: Option<char>) -> bool {
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
}
