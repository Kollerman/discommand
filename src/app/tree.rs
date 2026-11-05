//! The sidebar: the tree of servers, chats, calendars and task lists for the
//! service shown, which folders are open, and keeping them across restarts.

use super::*;

impl App {
    pub(super) fn tree_sel(&self) -> usize {
        self.tree_state.selected().unwrap_or(0)
    }

    pub(super) fn set_tree_sel(&mut self, i: usize) {
        let max = self.tree.len().saturating_sub(1);
        self.tree_state.select(Some(i.min(max)));
    }

    pub fn rebuild_tree(&mut self) {
        self.tree_dirty = false;
        match self.service {
            Service::WhatsApp | Service::Matrix => return self.rebuild_wa_tree(),
            Service::Calendar => return self.rebuild_cal_tree(),
            Service::Tasks | Service::Twos | Service::Moodle | Service::GroupWise => return self.rebuild_todo_tree(),
            Service::Discord => {}
        }
        let sel_key = self.tree.get(self.tree_sel()).map(|t| (t.kind, t.id.clone()));
        let s = &self.state;
        let mut items = Vec::new();

        let mut dm_unread = false;
        let mut dm_mentions = 0;
        for c in s.dms.iter().filter_map(|id| s.channels.get(id)) {
            dm_unread |= s.shows_unread(c);
            dm_mentions += s.mention_count(&c.id);
        }
        items.push(TreeItem {
            kind: ItemKind::DmHeader,
            id: String::new(),
            depth: 0,
            label: "Direct Messages".into(),
            unread: dm_unread,
            mentions: dm_mentions,
            expanded: self.dms_open,
            group: false,
            color: None,
            dim: false,
        });
        if self.dms_open {
            for c in s.dms.iter().filter_map(|id| s.channels.get(id)) {
                items.push(TreeItem {
                    kind: ItemKind::Dm,
                    id: c.id.clone(),
                    depth: 1,
                    label: s.dm_label(c),
                    unread: s.shows_unread(c),
                    mentions: s.mention_count(&c.id),
                    expanded: false,
                    group: false,
                    color: None,
                    dim: s.channel_muted(c),
                });
            }
        }

        for g in s.guild_order.iter().filter_map(|id| s.guilds.get(id)) {
            let open = self.expanded.contains(&g.id);
            let (unread, mentions) = s.guild_unread(g);
            items.push(TreeItem {
                kind: ItemKind::Guild,
                id: g.id.clone(),
                depth: 0,
                label: g.name.clone(),
                unread,
                mentions,
                expanded: open,
                group: false,
                color: None,
                dim: s.is_muted(&g.id),
            });
            if !open {
                continue;
            }
            for (cat, chans) in s.guild_layout(g) {
                let depth = match cat {
                    Some(cat) => {
                        let shut = self.collapsed.contains(&cat.id);
                        items.push(TreeItem {
                            kind: ItemKind::Category,
                            id: cat.id.clone(),
                            depth: 1,
                            label: cat.name.clone().unwrap_or_default().to_uppercase(),
                            unread: shut && chans.iter().any(|c| s.shows_unread(c)),
                            mentions: if shut { chans.iter().map(|c| s.mention_count(&c.id)).sum() } else { 0 },
                            expanded: !shut,
                            group: false,
                            color: None,
                            dim: false,
                        });
                        if shut {
                            continue;
                        }
                        2
                    }
                    None => 1,
                };
                for c in chans {
                    items.push(TreeItem {
                        kind: ItemKind::Channel,
                        id: c.id.clone(),
                        depth,
                        label: c.name.clone().unwrap_or_default(),
                        unread: s.shows_unread(c),
                        mentions: s.mention_count(&c.id),
                        expanded: false,
                        group: false,
                        color: None,
                        dim: s.channel_muted(c),
                    });
                }
            }
        }

        self.tree = items;
        if let Some((k, id)) = sel_key {
            if let Some(i) = self.tree.iter().position(|t| t.kind == k && t.id == id) {
                self.tree_state.select(Some(i));
            }
        }
        let sel = self.tree_sel();
        self.set_tree_sel(sel);
    }

    fn rebuild_wa_tree(&mut self) {
        let sel_id = self.tree.get(self.tree_sel()).map(|t| t.id.clone());
        let chats = self.chats(self.service);
        let tree = chats
            .order
            .iter()
            .filter_map(|jid| chats.chats.get(jid))
            .map(|c| TreeItem {
                kind: ItemKind::WaChat,
                id: c.jid.clone(),
                depth: 0,
                label: if c.pinned > 0 { format!("📌 {}", chats.label(&c.jid)) } else { chats.label(&c.jid) },
                unread: c.unread > 0,
                mentions: c.unread,
                expanded: false,
                group: c.is_group,
                color: None,
                dim: false,
            })
            .collect();
        self.tree = tree;
        if let Some(id) = sel_id {
            if let Some(i) = self.tree.iter().position(|t| t.id == id) {
                self.tree_state.select(Some(i));
            }
        }
        let sel = self.tree_sel();
        self.set_tree_sel(sel);
    }

    fn rebuild_cal_tree(&mut self) {
        let sel_id = self.tree.get(self.tree_sel()).map(|t| t.id.clone());
        let now = chrono::Local::now();
        let today = now.date_naive();
        let mut items = Vec::new();
        let mut day = None;
        for e in &self.cal.events {
            let d = e.start.date_naive();
            if day != Some(d) {
                day = Some(d);
                let name = d.format("%a %d %b").to_string();
                let label = match (d - today).num_days() {
                    0 => format!("Today · {name}"),
                    1 => format!("Tomorrow · {name}"),
                    -1 => format!("Yesterday · {name}"),
                    _ => name,
                };
                items.push(TreeItem {
                    kind: ItemKind::CalDay,
                    id: format!("day:{d}"),
                    depth: 0,
                    label,
                    unread: d == today,
                    mentions: 0,
                    expanded: false,
                    group: false,
                    color: None,
                    dim: d < today,
                });
            }
            let time = if e.all_day { "all day".to_string() } else { e.start.format("%H:%M").to_string() };
            items.push(TreeItem {
                kind: ItemKind::CalEvent,
                id: e.key(),
                depth: 1,
                label: format!("{time}  {}", e.title),
                unread: false,
                mentions: 0,
                expanded: false,
                group: false,
                color: self.cal.calendars.get(&e.cal).and_then(|c| c.color),
                dim: e.end <= now,
            });
        }
        self.tree = items;
        match sel_id.and_then(|id| self.tree.iter().position(|t| t.id == id)) {
            Some(i) if self.cal_positioned => self.tree_state.select(Some(i)),
            _ if !self.cal.events.is_empty() => {
                self.cal_positioned = true;
                self.jump_to_today();
            }
            _ => {}
        }
        let sel = self.tree_sel();
        self.set_tree_sel(sel);
    }

    fn rebuild_todo_tree(&mut self) {
        let sel_id = self.tree.get(self.tree_sel()).map(|t| t.id.clone());
        let today = chrono::Local::now().date_naive();
        let mut items = Vec::new();
        let tab = self.tab(self.service);
        let list_item = |id: &str, depth: u16, label: String, expanded: bool| TreeItem {
            kind: ItemKind::TodoList,
            id: id.to_string(),
            depth,
            label,
            unread: false,
            mentions: 0,
            expanded,
            group: false,
            color: None,
            dim: false,
        };
        let name = |l: &TaskList| if l.emoji.is_empty() { l.name.clone() } else { format!("{} {}", l.emoji, l.name) };
        let (top, more): (Vec<&TaskList>, Vec<&TaskList>) = tab.state.lists.iter().partition(|l| !l.more);
        // Lists in a folder appear under it, where its first list would be.
        let mut seen_groups: Vec<&str> = Vec::new();
        let mut entries: Vec<(&TaskList, u16)> = Vec::new();
        for l in &top {
            if l.group.is_empty() {
                entries.push((l, 0));
                continue;
            }
            if seen_groups.contains(&l.group.as_str()) {
                continue;
            }
            seen_groups.push(&l.group);
            let members: Vec<&TaskList> = top.iter().copied().filter(|m| m.group == l.group).collect();
            let id = format!("{GROUP}{}", l.group);
            let folded = members.iter().all(|m| m.folded);
            let shut = !tab.open.get(&id).copied().unwrap_or(!folded);
            items.push(list_item(&id, 0, format!("📁 {} ({})", l.group, members.len()), !shut));
            if !shut {
                for m in members {
                    items.push(list_item(&m.id, 1, name(m), false));
                }
            }
        }
        for (l, depth) in entries {
            let pos = items.len();
            let tasks: Vec<&Task> = tab.state.tasks.iter().filter(|t| t.list == l.id).collect();
            let open = tasks.iter().filter(|t| t.check && !t.done).count();
            let shut = !tab.open.get(&l.id).copied().unwrap_or(!l.folded);
            let item = list_item(&l.id, depth, if open > 0 { format!("{} ({open})", name(l)) } else { name(l) }, !shut);
            // Top-level lists (Deadlines) come before the folders.
            let at = items.iter().position(|t| t.id.starts_with(GROUP)).unwrap_or(pos);
            items.insert(at, item);
            if shut {
                continue;
            }
            let mut at = at + 1;
            for t in tasks {
                let due = t.due.map(|d| match (d - today).num_days() {
                    0 => "today".to_string(),
                    1 => "tomorrow".to_string(),
                    _ => d.format("%a %d %b").to_string(),
                });
                let color = match t.due {
                    _ if t.done => None,
                    Some(d) if d < today => Some([230, 80, 80]),
                    Some(d) if d == today => Some([230, 190, 40]),
                    _ => None,
                };
                let mut label = format!("{}{}", t.marker, t.title);
                if let Some(d) = due {
                    label.push_str(&format!("  · {d}"));
                }
                if let Some(from) = t.from {
                    label.push_str(&format!("  ↪ {}", from.format("%a %d %b")));
                }
                items.insert(
                    at,
                    TreeItem {
                        kind: if t.check { ItemKind::TodoTask } else { ItemKind::TodoNote },
                        id: t.key(),
                        depth: depth + 1 + t.depth as u16,
                        label,
                        unread: (t.important || t.heading) && !t.done,
                        mentions: 0,
                        expanded: false,
                        group: false,
                        color,
                        dim: t.done,
                    },
                );
                at += 1;
            }
        }
        if !more.is_empty() {
            items.push(list_item(MORE_LISTS, 0, format!("More lists ({})", more.len()), tab.more_open));
            if tab.more_open {
                items.extend(more.into_iter().map(|l| list_item(&l.id, 1, name(l), false)));
            }
        }
        self.tree = items;
        let positioned = self.tab(self.service).positioned;
        match sel_id.and_then(|id| self.tree.iter().position(|t| t.id == id)) {
            Some(i) if positioned => self.tree_state.select(Some(i)),
            _ if !self.tree.is_empty() && !positioned => {
                self.tab_mut(self.service).positioned = true;
                let first = self.tree.iter().position(|t| matches!(t.kind, ItemKind::TodoTask | ItemKind::TodoNote)).unwrap_or(0);
                self.tree_state.select(Some(first));
            }
            _ => {}
        }
        let sel = self.tree_sel();
        self.set_tree_sel(sel);
    }

    pub(super) fn reveal_in_tree(&mut self, id: &str) {
        if self.service != Service::Discord {
            if self.service.is_tasks() {
                let list = self.tasks().find(id).map(|t| t.list.clone());
                let (more, folded) = match self.tasks().lists.iter().find(|l| l.id == id) {
                    Some(l) => (l.more, false),
                    None => (false, self.tasks().lists.iter().any(|l| Some(&l.id) == list.as_ref() && l.folded)),
                };
                let tab = self.tab_mut(self.service);
                tab.more_open |= more;
                // A task in a folded list: unfold it.
                if let Some(list) = list.filter(|_| folded) {
                    tab.open.insert(list, true);
                }
            }
            self.rebuild_tree();
            if let Some(i) = self.tree.iter().position(|t| t.id == id) {
                self.set_tree_sel(i);
            }
            return;
        }
        if let Some(c) = self.state.channels.get(id) {
            if let Some(g) = c.guild_id.clone() {
                self.expanded.insert(g);
            } else {
                self.dms_open = true;
            }
            if let Some(p) = c.parent_id.clone() {
                self.collapsed.remove(&p);
            }
        }
        self.rebuild_tree();
        if let Some(i) = self.tree.iter().position(|t| t.id == id && matches!(t.kind, ItemKind::Channel | ItemKind::Dm)) {
            self.set_tree_sel(i);
        }
    }

    /// Which folders and lists of the task tabs (Moodle's course folders too)
    /// were opened or closed, kept for the next start.
    fn save_folds(&self) {
        let Some(path) = folds_path() else { return };
        let mut all = serde_json::Map::new();
        for s in [Service::Tasks, Service::Twos, Service::Moodle, Service::GroupWise] {
            let tab = self.tab(s);
            let mut open: serde_json::Map<String, serde_json::Value> = tab.open.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
            open.insert(MORE_LISTS.into(), json!(tab.more_open));
            all.insert(s.name().into(), serde_json::Value::Object(open));
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, serde_json::Value::Object(all).to_string());
    }

    pub fn load_folds(&mut self) {
        let Some(saved) = folds_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) else {
            return;
        };
        for s in [Service::Tasks, Service::Twos, Service::Moodle, Service::GroupWise] {
            let Some(map) = saved[s.name()].as_object() else { continue };
            let tab = self.tab_mut(s);
            for (k, v) in map {
                match (k.as_str(), v.as_bool()) {
                    (MORE_LISTS, Some(b)) => tab.more_open = b,
                    (_, Some(b)) => {
                        tab.open.insert(k.clone(), b);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Expands/collapses a tree node (`force`: Some(true) open, Some(false) close).
    pub(super) fn toggle(&mut self, idx: usize, force: Option<bool>) {
        let Some(item) = self.tree.get(idx) else { return };
        let open = force.unwrap_or(!item.expanded);
        let id = item.id.clone();
        match item.kind {
            ItemKind::DmHeader => self.dms_open = open,
            ItemKind::Guild => {
                if open {
                    self.expanded.insert(id);
                } else {
                    self.expanded.remove(&id);
                }
            }
            ItemKind::Category => {
                if open {
                    self.collapsed.remove(&id);
                } else {
                    self.collapsed.insert(id);
                }
            }
            ItemKind::TodoList if id == MORE_LISTS => {
                self.tab_mut(self.service).more_open = open;
                self.save_folds();
            }
            ItemKind::TodoList => {
                // Remembered as open or closed, also for the next start.
                self.tab_mut(self.service).open.insert(id, open);
                self.save_folds();
            }
            _ => return,
        }
        self.rebuild_tree();
    }
}

fn folds_path() -> Option<std::path::PathBuf> {
    crate::system::secret::data_dir().map(|d| d.join("folds.json"))
}
