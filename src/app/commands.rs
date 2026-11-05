//! The `:` commands, and `:r` reloading what's shown.

use super::*;

impl App {
    /// `:r`: reloads what the current mode shows.
    pub(super) fn refresh(&mut self) {
        self.msg_sel = None;
        match self.service {
            Service::Discord => {
                let Some(ch) = self.current.clone() else {
                    self.status = "open a channel first".into();
                    return;
                };
                self.state.complete.remove(&ch);
                self.fetch(ch, None);
            }
            Service::WhatsApp | Service::Matrix => {
                // WhatsApp pushes messages; reconnecting is what fetches missed ones.
                let s = self.service;
                wa::send(self.cmd_of(s), json!({"c": "reconnect"}));
                if let Some(chat) = self.open_of(s).clone() {
                    if self.chats_mut(s).loading.insert(chat.clone()) {
                        self.chats_mut(s).version += 1;
                        wa::send(self.cmd_of(s), json!({"c": "history", "chat": chat}));
                    }
                }
            }
            Service::Calendar => {
                if self.google.is_none() {
                    self.status = "Google Calendar isn't connected (see the calendar pane)".into();
                    return;
                }
                self.cal_sync();
            }
            Service::Tasks | Service::Twos | Service::Moodle => {
                if !self.tasks_connected() {
                    self.status = format!("{} isn't connected (see the right pane)", self.service.name());
                    return;
                }
                self.todo_sync(self.service);
            }
        }
        self.status = REFRESHING.into();
    }

    pub(super) fn done_refreshing(&mut self) {
        if self.status == REFRESHING {
            self.status.clear();
        }
    }

    pub(super) fn run_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        let (name, arg) = cmd.split_once(' ').map(|(a, b)| (a, b.trim())).unwrap_or((cmd, ""));
        match name {
            "" => {}
            "q" | "q!" | "qa" | "qa!" | "quit" | "wq" | "x" => self.quit = true,
            "h" | "help" => self.help = true,
            "logout" => {
                self.logout = true;
                self.quit = true;
            }
            "b" | "buffer" | "o" | "open" | "e" | "join" => {
                self.open_finder();
                if let Some(f) = self.finder.as_mut() {
                    f.query.set(arg);
                    f.refilter();
                    if !arg.is_empty() && !f.matches.is_empty() {
                        let id = f.all[f.matches[0]].0.clone();
                        self.finder = None;
                        self.open_chat(id);
                    }
                }
            }
            "wa" | "mx" | "matrix" | "cal" | "calendar" | "todo" | "tasks" | "twos" | "moodle" if !Self::command_service(name).built() => {
                self.status = format!("{} isn't part of this build", Self::command_service(name).name());
            }
            "wa" | "mx" | "matrix" => {
                let s = Self::command_service(name);
                match arg {
                    "receipts on" => wa::send(self.cmd_of(s), json!({"c": "receipts", "on": true})),
                    "receipts off" => wa::send(self.cmd_of(s), json!({"c": "receipts", "on": false})),
                    "receipts" => {
                        let on = self.chats(s).receipts;
                        self.status = format!("{} read receipts are {}", s.name(), if on { "on" } else { "off" })
                    }
                    "logout" => wa::send(self.cmd_of(s), json!({"c": "logout"})),
                    _ => self.status = format!("usage: :{name} receipts on|off · :{name} logout"),
                }
            }
            "cal" | "calendar" => self.cal_command(arg),
            "todo" | "tasks" => self.todo_command(arg),
            "twos" => self.twos_command(arg),
            "moodle" => self.moodle_command(arg),
            "attach" | "file" | "send" => self.attach(Some(arg)),
            "pdf" => self.pdf_command(arg),
            "totp" => self.totp_command(arg),
            "images" | "img" => {
                self.pics.enabled = match arg {
                    "on" => true,
                    "off" => false,
                    _ => !self.pics.enabled,
                };
                self.pics.version += 1;
                self.status = format!("images {}", if self.pics.enabled { "on" } else { "off" });
            }
            "startup" => {
                let on = match arg {
                    "true" | "on" => Some(true),
                    "false" | "off" => Some(false),
                    _ => None,
                };
                self.status = match on {
                    _ if !sound::BUILT => sound::NOT_BUILT.into(),
                    None => format!("the startup sound is {} (:startup true or :startup false)", if sound::startup_on() { "on" } else { "off" }),
                    Some(on) => match sound::set_startup(on) {
                        Ok(()) => format!("startup sound {}", if on { "on" } else { "off" }),
                        Err(e) => format!("couldn't save that: {e}"),
                    },
                };
            }
            "r" | "refresh" => self.refresh(),
            "mute" => self.mute(true, arg),
            "unmute" => self.mute(false, arg),
            // The messaging service shown (else all of them), or `:read all`.
            "read" => {
                let all = [Service::Discord, Service::WhatsApp, Service::Matrix];
                if arg == "all" || !self.service.is_messaging() {
                    self.read_all(&all);
                } else {
                    self.read_all(&[self.service]);
                }
            }
            _ => self.status = format!("E492: Not an editor command: {cmd}"),
        }
    }

    /// `:mute [15m|1h|8h|24h]` / `:unmute`: the server, category, channel or
    /// DM selected in the sidebar (the open channel when the chat has focus).
    fn mute(&mut self, on: bool, arg: &str) {
        if self.service != Service::Discord {
            self.status = "muting is for Discord servers and channels".into();
            return;
        }
        let secs = match arg {
            "" => None,
            a => match a.strip_suffix(['m', 'h', 'd']).and_then(|n| n.parse::<i64>().ok()) {
                Some(n) if a.ends_with('m') => Some(n * 60),
                Some(n) if a.ends_with('h') => Some(n * 3600),
                Some(n) => Some(n * 86400),
                None => {
                    self.status = "usage: :mute [15m|1h|8h|24h|7d] · :unmute".into();
                    return;
                }
            },
        };
        let item = self.tree.get(self.tree_sel()).filter(|_| self.focus == Focus::Tree);
        let id = match item.map(|t| t.kind) {
            Some(ItemKind::Guild | ItemKind::Category | ItemKind::Channel | ItemKind::Dm) => item.map(|t| t.id.clone()),
            _ => self.current.clone(),
        };
        let Some(id) = id else {
            self.status = "select a server or channel first".into();
            return;
        };
        let end = secs.map(|s| (chrono::Utc::now() + chrono::TimeDelta::seconds(s)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        let config = json!({ "end_time": end, "selected_time_window": secs.unwrap_or(-1) });
        let (guild, name, body) = if let Some(g) = self.state.guilds.get(&id) {
            (id.clone(), g.name.clone(), json!({ "muted": on, "mute_config": config }))
        } else if let Some(c) = self.state.channels.get(&id) {
            let name = if c.is_dm() { self.state.dm_label(c) } else { format!("#{}", c.name.as_deref().unwrap_or("?")) };
            let guild = c.guild_id.clone().unwrap_or_else(|| "@me".into());
            (guild, name, json!({ "channel_overrides": { id.clone(): { "muted": on, "mute_config": config } } }))
        } else {
            return;
        };
        self.status = format!("{} {name}…", if on { "muting" } else { "unmuting" });
        let api = self.api.clone();
        self.spawn(async move {
            Some(match api.guild_settings(&guild, &body).await {
                Ok(s) => AppEvent::Muted(s, format!("{} {name}", if on { "muted" } else { "unmuted" })),
                Err(e) => AppEvent::Error(format!("Discord: {e}")),
            })
        });
    }

    fn command_service(name: &str) -> Service {
        match name {
            "wa" => Service::WhatsApp,
            "mx" | "matrix" => Service::Matrix,
            "cal" | "calendar" => Service::Calendar,
            "twos" => Service::Twos,
            "moodle" => Service::Moodle,
            _ => Service::Tasks,
        }
    }
}

const REFRESHING: &str = "refreshing…";
