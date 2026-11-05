//! Keys: the vim-style modes (normal, insert, command), the finder, and what
//! each key does in the sidebar and the message pane.

use super::*;

impl App {
    /// Shift+Tab: forward, or backward with the right Shift key.
    fn cycle_service(&mut self) {
        self.switch_service(self.service.step(if right_shift_held() { -1 } else { 1 }));
    }

    pub(super) fn switch_service(&mut self, to: Service) {
        if to == self.service {
            return;
        }
        if self.mode != Mode::Normal {
            self.mode = Mode::Normal;
            self.input.clear();
        }
        let old = self.service;
        self.service = to;
        self.saved_trees[old.index()] = std::mem::take(&mut self.tree_state);
        self.tree_state = std::mem::take(&mut self.saved_trees[self.service.index()]);
        self.saved_inputs[old.index()] = std::mem::take(&mut self.input);
        self.input = std::mem::take(&mut self.saved_inputs[self.service.index()]);
        self.msg_sel = None;
        self.msg_scroll = 0;
        self.pending = None;
        self.count = None;
        self.rebuild_tree();
        match self.service {
            Service::WhatsApp | Service::Matrix => {
                let s = self.service;
                if let Some(jid) = self.open_of(s).clone() {
                    if self.chats(s).chats.get(&jid).is_some_and(|c| c.unread > 0) {
                        wa::send(self.cmd_of(s), json!({"c": "seen", "chat": jid}));
                    }
                }
            }
            Service::Discord => {
                if let Some(ch) = self.current.clone() {
                    self.mark_read(&ch);
                }
            }
            Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle | Service::GroupWise => {}
        }
        if self.current_chat().is_none() && self.focus != Focus::Tree {
            self.focus = Focus::Tree;
        }
    }

    pub(super) fn enter_insert(&mut self) {
        if self.focus != Focus::Input && self.focus != Focus::Command {
            self.last_pane = self.focus;
        }
        self.focus = Focus::Input;
    }

    pub(super) fn leave_insert(&mut self) {
        self.focus = if self.last_pane == Focus::Tree { Focus::Tree } else { Focus::Messages };
    }

    pub(super) fn open_finder(&mut self) {
        let s = &self.state;
        let mut all = Vec::new();
        if self.service.is_wa_like() {
            let chats = self.chats(self.service);
            for jid in &chats.order {
                all.push((jid.clone(), chats.title(jid)));
            }
        }
        if self.service.is_tasks() {
            for t in &self.tasks().tasks {
                all.push((t.key(), format!("{} · {}", t.title, self.tasks().list_name(&t.list))));
            }
            for l in &self.tasks().lists {
                all.push((l.id.clone(), format!("≡ {}", l.name)));
            }
        }
        if self.service == Service::Calendar {
            for e in &self.cal.events {
                let when = if e.all_day { e.start.format("%a %d %b") } else { e.start.format("%a %d %b %H:%M") };
                all.push((e.key(), format!("{when} · {}", e.title)));
            }
        }
        for c in s.dms.iter().filter_map(|id| s.channels.get(id)).filter(|_| self.service == Service::Discord) {
            all.push((c.id.clone(), format!("@{}", s.dm_label(c))));
        }
        for g in s.guild_order.iter().filter_map(|id| s.guilds.get(id)).filter(|_| self.service == Service::Discord) {
            for (_, chans) in s.guild_layout(g) {
                for c in chans {
                    all.push((c.id.clone(), format!("#{} · {}", c.name.as_deref().unwrap_or("?"), g.name)));
                }
            }
        }
        let mut f = Finder { query: Input::default(), all, matches: Vec::new(), sel: 0 };
        f.refilter();
        self.finder = Some(f);
    }

    pub fn on_term(&mut self, ev: Event, paste_burst: bool) {
        if matches!(ev, Event::Key(_) | Event::Paste(_)) {
            self.active();
        }
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k, paste_burst),
            Event::Paste(s) => {
                if self.compose.is_some() {
                    self.compose_paste(&s);
                    self.dirty = true;
                    return;
                }
                if let Some(f) = self.finder.as_mut() {
                    f.query.insert_str(&s.replace('\n', " "));
                    f.refilter();
                } else if self.focus == Focus::Command {
                    self.cmdline.insert_str(&s.replace('\n', " "));
                } else {
                    self.enter_insert();
                    self.input.insert_str(&s);
                }
            }
            Event::Resize(..) => {}
            Event::FocusGained => self.focused = Some(true),
            Event::FocusLost => self.focused = Some(false),
            _ => return,
        }
        self.dirty = true;
    }

    fn on_key(&mut self, k: KeyEvent, paste_burst: bool) {
        if self.tree_dirty {
            self.rebuild_tree();
        }
        let ctrl = is_ctrl(&k);
        if ctrl && k.code == KeyCode::Char('c') {
            if self.focus == Focus::Input && !self.input.is_empty() {
                self.input.clear();
                self.mode = Mode::Normal;
            } else {
                self.status = "type :q to quit".into();
            }
            return;
        }
        // Only y (or j) goes ahead; any other key cancels, nothing is sent.
        if let Some(c) = self.confirm.take() {
            if matches!(k.code, KeyCode::Char('y' | 'Y' | 'j' | 'J')) {
                self.run_confirmed(*c);
            } else {
                self.status = "cancelled: nothing was sent".into();
            }
            return;
        }
        if self.react_picker {
            self.react_picker = false;
            if let KeyCode::Char(c @ '1'..='5') = k.code {
                self.react(REACTIONS[c as usize - '1' as usize]);
            }
            return;
        }
        if self.help || self.cal_detail {
            self.help = false;
            self.cal_detail = false;
            return;
        }
        if self.finder.is_some() {
            self.finder_key(k);
            return;
        }
        if self.mail_move.is_some() {
            self.move_key(k);
            return;
        }
        if self.compose.is_some() && self.service == Service::GroupWise && self.focus != Focus::Command {
            self.compose_key(k);
            return;
        }
        if self.totp && self.focus != Focus::Command {
            self.totp_key(k);
            return;
        }
        if self.pdf.is_some() && self.focus != Focus::Command {
            self.pdf_key(k);
            return;
        }
        match self.focus {
            Focus::Input => self.insert_key(k, paste_burst),
            Focus::Command => self.command_key(k),
            Focus::Tree | Focus::Messages => self.normal_key(k),
        }
    }

    fn finder_key(&mut self, k: KeyEvent) {
        let ctrl = is_ctrl(&k);
        let f = self.finder.as_mut().unwrap();
        match k.code {
            KeyCode::Esc => self.finder = None,
            KeyCode::Enter => {
                if let Some(&i) = f.matches.get(f.sel) {
                    let id = f.all[i].0.clone();
                    self.finder = None;
                    self.open_chat(id);
                }
            }
            KeyCode::Up => f.sel = f.sel.saturating_sub(1),
            KeyCode::Char('k') | KeyCode::Char('p') if ctrl => f.sel = f.sel.saturating_sub(1),
            KeyCode::Down => f.sel = (f.sel + 1).min(f.matches.len().saturating_sub(1)),
            KeyCode::Char('j') | KeyCode::Char('n') if ctrl => f.sel = (f.sel + 1).min(f.matches.len().saturating_sub(1)),
            _ => {
                if f.query.handle_key(&k) {
                    f.refilter();
                }
            }
        }
    }

    fn command_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Esc => {
                self.cmdline.clear();
                self.focus = self.last_pane;
            }
            KeyCode::Enter => {
                let cmd = self.cmdline.text();
                self.cmdline.clear();
                self.focus = self.last_pane;
                self.run_command(&cmd);
            }
            KeyCode::Backspace if self.cmdline.is_empty() => self.focus = self.last_pane,
            _ => {
                self.cmdline.handle_key(&k);
            }
        }
    }

    fn insert_key(&mut self, k: KeyEvent, paste_burst: bool) {
        let mods = k.modifiers;
        match k.code {
            KeyCode::Enter if paste_burst || mods.intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) => {
                self.input.insert('\n')
            }
            KeyCode::Enter => self.submit(),
            KeyCode::BackTab => self.cycle_service(),
            KeyCode::Esc if self.pending_file().is_some() => {
                self.attachment = None;
                self.status = "file removed".into();
            }
            KeyCode::Esc => {
                if matches!(self.mode, Mode::Edit(_)) {
                    self.input.clear();
                }
                self.mode = Mode::Normal;
                self.leave_insert();
            }
            KeyCode::Up if self.input.is_empty() && self.mode == Mode::Normal => {
                if let Some(idx) = self.last_own() {
                    self.start_edit(idx);
                }
            }
            _ => {
                self.input.handle_key(&k);
            }
        }
    }

    pub(super) fn normal_key(&mut self, k: KeyEvent) {
        let ctrl = is_ctrl(&k);
        let pending = self.pending.take();

        if let KeyCode::Char(c @ '0'..='9') = k.code {
            if !ctrl && (c != '0' || self.count.is_some()) {
                let d = c as usize - '0' as usize;
                self.count = Some(self.count.unwrap_or(0).saturating_mul(10).saturating_add(d).min(9999));
                self.pending = pending;
                return;
            }
        }
        let count = self.count.take();
        let n = count.unwrap_or(1);

        // Services like vim tabs: gt next, gT previous, {n}gt the n-th.
        if pending == Some('g') {
            match k.code {
                KeyCode::Char('t') => {
                    let to = match count {
                        Some(i) => match Service::ALL.get(i.wrapping_sub(1)) {
                            Some(s) => *s,
                            None => {
                                self.status = format!("there are {} services", Service::COUNT);
                                return;
                            }
                        },
                        None => self.service.step(1),
                    };
                    self.switch_service(to);
                    return;
                }
                KeyCode::Char('T') => {
                    self.switch_service(self.service.step(-(n as isize)));
                    return;
                }
                _ => {}
            }
        }
        // Window motions: Ctrl+w h/l/w, Ctrl+h/Ctrl+l, Tab.
        if pending == Some('w') {
            match k.code {
                KeyCode::Char('h') | KeyCode::Left => self.focus = Focus::Tree,
                KeyCode::Char('l') | KeyCode::Right => self.focus = Focus::Messages,
                KeyCode::Char('w') | KeyCode::Char('p') => self.toggle_pane(),
                _ => {}
            }
            return;
        }
        // Global normal-mode keys (skipped while a multi-key command is pending).
        let global = if pending.is_some() { KeyCode::Null } else { k.code };
        match global {
            KeyCode::Char('w') if ctrl => {
                self.pending = Some('w');
                return;
            }
            KeyCode::Char('h') | KeyCode::Backspace if ctrl => {
                self.focus = Focus::Tree;
                return;
            }
            KeyCode::Char('l') if ctrl => {
                self.focus = Focus::Messages;
                return;
            }
            // In the calendar grid, Tab cycles events at the cursor instead.
            KeyCode::Tab if !(self.service == Service::Calendar && self.cal_view != CalView::Agenda) => {
                self.toggle_pane();
                return;
            }
            KeyCode::BackTab => {
                self.cycle_service();
                return;
            }
            // ö sits where ; is on US keyboards: the usual German vim mapping for ':'.
            KeyCode::Char(':' | 'ö' | 'Ö') => {
                self.last_pane = self.focus;
                self.focus = Focus::Command;
                return;
            }
            // '-' is the key in the US '/' position on QWERTZ; '#' is unshifted there.
            KeyCode::Char('/' | '-' | '#') | KeyCode::Char('k') if k.code != KeyCode::Char('k') || ctrl => {
                self.open_finder();
                return;
            }
            KeyCode::Char('?' | 'ß') => {
                self.help = true;
                return;
            }
            // F: pick a file to send in the open chat.
            KeyCode::Char('F') if self.service.is_messaging() => {
                self.attach(None);
                return;
            }
            // Read all: every chat of the messaging service shown.
            KeyCode::Char('A') if self.service.is_messaging() => {
                self.read_all(&[self.service]);
                return;
            }
            KeyCode::Char('i') | KeyCode::Char('a') if !matches!(self.service, Service::Calendar | Service::Tasks | Service::Twos | Service::Moodle | Service::GroupWise) => {
                if self.current_chat().is_some() {
                    self.enter_insert();
                } else {
                    self.status = "open a chat first".into();
                }
                return;
            }
            _ => {}
        }
        if self.service.is_tasks() {
            if self.todo_key(k, pending) {
            } else if self.in_page() {
                self.page_key(k, pending, n, count.is_some());
            } else {
                self.tree_key(k, pending, n, count.is_some());
            }
        } else if self.service == Service::Calendar {
            // One list to move through; the right pane only shows details.
            if self.cal_key(k, pending) {
            } else if self.cal_view == CalView::Agenda {
                self.tree_key(k, pending, n, count.is_some());
            } else {
                self.cal_grid_key(k, pending, n);
            }
        } else if self.focus == Focus::Tree {
            self.tree_key(k, pending, n, count.is_some());
        } else {
            self.messages_key(k, pending, n, count.is_some());
        }
    }

    fn toggle_pane(&mut self) {
        self.focus = if self.focus == Focus::Tree { Focus::Messages } else { Focus::Tree };
    }

    fn tree_key(&mut self, k: KeyEvent, pending: Option<char>, n: usize, counted: bool) {
        let ctrl = is_ctrl(&k);
        let sel = self.tree_sel();
        let half = (self.msg_height / 2).max(1);
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => self.set_tree_sel(sel + n),
            KeyCode::Char('k') | KeyCode::Up => self.set_tree_sel(sel.saturating_sub(n)),
            KeyCode::Char('d') if ctrl => self.set_tree_sel(sel + half),
            KeyCode::Char('u') if ctrl => self.set_tree_sel(sel.saturating_sub(half)),
            KeyCode::PageDown => self.set_tree_sel(sel + half * 2),
            KeyCode::PageUp => self.set_tree_sel(sel.saturating_sub(half * 2)),
            KeyCode::Char('g') if pending == Some('g') => self.set_tree_sel(if counted { n - 1 } else { 0 }),
            KeyCode::Char('g') => self.pending = Some('g'),
            KeyCode::Char('G') | KeyCode::End => self.set_tree_sel(if counted { n - 1 } else { usize::MAX }),
            KeyCode::Home => self.set_tree_sel(0),
            KeyCode::Char('z') => self.pending = Some('z'),
            KeyCode::Char('a') | KeyCode::Char('o') | KeyCode::Char('c') if pending == Some('z') => {
                let force = match k.code {
                    KeyCode::Char('o') => Some(true),
                    KeyCode::Char('c') => Some(false),
                    _ => None,
                };
                self.toggle(sel, force);
            }
            KeyCode::Char('M') if pending == Some('z') => {
                self.expanded.clear();
                self.dms_open = false;
                self.rebuild_tree();
            }
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Char('o') | KeyCode::Right | KeyCode::Char(' ') => {
                let Some(item) = self.tree.get(sel) else { return };
                match item.kind {
                    ItemKind::Dm | ItemKind::Channel => {
                        let id = item.id.clone();
                        self.open_channel(id);
                    }
                    ItemKind::WaChat => {
                        let id = item.id.clone();
                        self.open_wa_chat(id);
                    }
                    _ => {
                        let force = matches!(k.code, KeyCode::Char('l') | KeyCode::Right).then_some(true);
                        self.toggle(sel, force);
                    }
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                let Some(item) = self.tree.get(sel) else { return };
                if item.expanded && !matches!(item.kind, ItemKind::Dm | ItemKind::Channel | ItemKind::WaChat) {
                    self.toggle(sel, Some(false));
                } else if item.depth > 0 {
                    let depth = item.depth;
                    if let Some(p) = self.tree[..sel].iter().rposition(|t| t.depth < depth) {
                        self.set_tree_sel(p);
                    }
                }
            }
            _ => {}
        }
    }

    fn messages_key(&mut self, k: KeyEvent, pending: Option<char>, n: usize, counted: bool) {
        let ctrl = is_ctrl(&k);
        let len = self.current_len();
        let half = (self.msg_height / 4).max(1);
        let up = |app: &mut App, by: usize| {
            let target = match app.msg_sel {
                None if len > 0 => Some(len.saturating_sub(by)),
                None => None,
                Some(i) => {
                    if i < by {
                        app.fetch_older();
                    }
                    Some(i.saturating_sub(by))
                }
            };
            app.msg_sel = target;
        };
        let down = |app: &mut App, by: usize| {
            app.msg_sel = match app.msg_sel {
                Some(i) if i + by < len => Some(i + by),
                _ => None,
            };
        };
        match k.code {
            KeyCode::Char('k') | KeyCode::Up => up(self, n),
            KeyCode::Char('j') | KeyCode::Down => down(self, n),
            KeyCode::Char('u') if ctrl => up(self, half),
            KeyCode::Char('d') if ctrl => down(self, half),
            KeyCode::Char('b') if ctrl => up(self, half * 2),
            KeyCode::Char('f') if ctrl => down(self, half * 2),
            KeyCode::PageUp => up(self, half * 2),
            KeyCode::PageDown => down(self, half * 2),
            KeyCode::Char('g') if pending == Some('g') => {
                if len > 0 {
                    self.msg_sel = Some(if counted { (n - 1).min(len - 1) } else { 0 });
                }
                if !counted {
                    self.fetch_older();
                }
            }
            KeyCode::Char('g') => self.pending = Some('g'),
            KeyCode::Char('G') | KeyCode::End => self.msg_sel = None,
            KeyCode::Home => {
                if len > 0 {
                    self.msg_sel = Some(0);
                }
                self.fetch_older();
            }
            KeyCode::Esc => self.msg_sel = None,
            KeyCode::Char('h') | KeyCode::Left => self.focus = Focus::Tree,
            KeyCode::Enter => self.enter_insert(),
            KeyCode::Char('x') if pending == Some('g') => self.open_selected_url(),
            KeyCode::Char('o') => self.open_selected_url(),
            KeyCode::Char('t') => self.transcribe_selected(),
            KeyCode::Char('p') => self.play_selected(),
            // '+' is unshifted on German keyboards, '=' on US ones.
            KeyCode::Char('+') | KeyCode::Char('=') => {
                if self.selected_id().is_some() {
                    self.react_picker = true;
                } else {
                    self.status = "select a message first (k)".into();
                }
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                let Some(id) = self.selected_id() else {
                    self.status = "select a message first (k)".into();
                    return;
                };
                self.mode = Mode::Reply { id, ping: k.code == KeyCode::Char('r') };
                self.enter_insert();
            }
            KeyCode::Char('e') | KeyCode::Char('c') => {
                if let Some(i) = self.msg_sel {
                    self.start_edit(i);
                }
            }
            KeyCode::Char('d') if pending == Some('d') => self.delete_selected(),
            KeyCode::Char('d') => {
                if self.msg_sel.is_some() {
                    self.pending = Some('d');
                }
            }
            _ => {}
        }
    }
}

/// Whether the right Shift key (and not the left one) is held. Terminals
/// send the same Shift+Tab for both, so on Windows the physical key state
/// is asked for; elsewhere it can't be told apart (use `gT` there).
#[cfg(windows)]
fn right_shift_held() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LSHIFT, VK_RSHIFT};
    // SAFETY: plain query of the keyboard state.
    let down = |vk: u16| unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 };
    down(VK_RSHIFT) && !down(VK_LSHIFT)
}

#[cfg(not(windows))]
fn right_shift_held() -> bool {
    false
}

/// Ctrl without Alt. Windows reports AltGr (needed for @ { } [ ] \ | ~ on
/// German and other European layouts) as Ctrl+Alt; that must not trigger
/// Ctrl shortcuts.
pub(super) fn is_ctrl(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && !k.modifiers.contains(KeyModifiers::ALT)
}
