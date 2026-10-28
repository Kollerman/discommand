//! Rendering. Vim-like: sidebar split, statusline and a command line.

use chrono::{Datelike, Days, Local, NaiveDate, Timelike};
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, CalView, Focus, ItemKind, Mode, MsgCache, Service};
use crate::cal;
use crate::pics::{cells, Pic, PicRef, Pics, HD_COLS, MAX_COLS, MAX_ROWS};
use crate::text::{truncate, wrap};
use crate::view::{Reply, ViewMsg};

const ACCENT: Color = Color::Blue;
const WA_GREEN: Color = Color::Green;
const DIM: Style = Style::new().fg(Color::DarkGray);
const NAME_COLORS: [Color; 8] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::LightBlue,
    Color::LightRed,
    Color::LightGreen,
    Color::LightMagenta,
];

fn name_color(id: &str) -> Color {
    let h = id.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    NAME_COLORS[h as usize % NAME_COLORS.len()]
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, statusline, cmdline] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(f.area());

    let narrow = main.width < 70;
    if app.service == Service::Calendar && app.cal_view != CalView::Agenda {
        draw_cal_grid(f, app, main);
    } else if narrow {
        if app.focus == Focus::Tree {
            draw_tree(f, app, main, false);
        } else {
            draw_chat(f, app, main);
        }
    } else {
        let tw = (main.width / 4).clamp(22, 34);
        let [left, right] = Layout::horizontal([Constraint::Length(tw), Constraint::Min(10)]).areas(main);
        draw_tree(f, app, left, true);
        draw_chat(f, app, right);
    }
    draw_statusline(f, app, statusline);
    draw_cmdline(f, app, cmdline);

    if app.finder.is_some() {
        draw_finder(f, app);
    }
    if app.help {
        draw_help(f);
    }
    if app.cal_detail {
        draw_event_popup(f, app);
    }
    scrub(f);
}

/// Removes control and bidi-override characters from everything drawn.
/// Untrusted names and texts must never reach the terminal as escape
/// sequences (ratatui only filters these on some code paths).
fn scrub(f: &mut Frame) {
    let unsafe_char = |c: char| c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}');
    for cell in f.buffer_mut().content.iter_mut() {
        if cell.symbol().chars().any(unsafe_char) {
            cell.set_symbol(" ");
        }
    }
}

fn draw_tree(f: &mut Frame, app: &mut App, area: Rect, split: bool) {
    let focused = app.focus == Focus::Tree;
    let block = if split {
        Block::new().borders(Borders::RIGHT).border_style(if focused { Style::new().fg(ACCENT) } else { DIM })
    } else {
        Block::new()
    };
    let current = app.current_chat();
    let items: Vec<ListItem> = app
        .tree
        .iter()
        .map(|t| {
            let indent = "  ".repeat(t.depth as usize);
            let fold = if t.expanded { "▾ " } else { "▸ " };
            let (icon, base) = match t.kind {
                ItemKind::DmHeader | ItemKind::Guild => (fold, Style::new().add_modifier(Modifier::BOLD)),
                ItemKind::Category => (fold, DIM),
                ItemKind::Channel => ("# ", Style::new().fg(Color::Gray)),
                ItemKind::Dm => ("@ ", Style::new().fg(Color::Gray)),
                ItemKind::WaChat if t.group => ("# ", Style::new().fg(Color::Gray)),
                ItemKind::WaChat => ("@ ", Style::new().fg(Color::Gray)),
                ItemKind::CalDay => ("", Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                ItemKind::CalEvent => ("● ", Style::new()),
                ItemKind::TodoList => (fold, Style::new().add_modifier(Modifier::BOLD)),
                ItemKind::TodoTask if t.dim => ("☑ ", Style::new()),
                ItemKind::TodoTask => ("☐ ", Style::new()),
            };
            let mut style = base;
            if t.unread {
                style = style.fg(Color::White).add_modifier(Modifier::BOLD);
            }
            if matches!(t.kind, ItemKind::Channel | ItemKind::Dm | ItemKind::WaChat) && current == Some(t.id.as_str()) {
                style = style.fg(Color::Cyan);
            }
            if t.dim {
                style = DIM;
                if t.kind == ItemKind::TodoTask {
                    style = style.add_modifier(Modifier::CROSSED_OUT);
                }
            }
            let icon_style = match t.color {
                Some([r, g, b]) if !t.dim => Style::new().fg(Color::Rgb(r, g, b)),
                _ => DIM,
            };
            let mut spans = vec![Span::raw(indent), Span::styled(icon, icon_style), Span::styled(t.label.clone(), style)];
            if t.mentions > 0 {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(format!(" {} ", t.mentions), Style::new().bg(Color::Red).fg(Color::White)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let highlight = if focused {
        Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
    } else {
        Style::new().add_modifier(Modifier::UNDERLINED)
    };
    let list = List::new(items).block(block).highlight_style(highlight);
    f.render_stateful_widget(list, area, &mut app.tree_state);
}

fn draw_chat(f: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.saturating_sub(1).max(1) as usize;
    let input_h = input_height(app, width);
    let [header, body, input] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(input_h)]).areas(area);

    // Header (like a vim tabline).
    let title = match app.current_chat() {
        Some(c) => app.chat_title(c),
        None if app.service == Service::Calendar => {
            app.selected_event().map(|e| e.title.clone()).unwrap_or_else(|| "Calendar".into())
        }
        None if app.service == Service::Tasks => app.selected_task().map(|t| t.title.clone()).unwrap_or_else(|| "Tasks".into()),
        None => "nothing open — / to find a chat, ? for help, shift+tab switches service".into(),
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {}", truncate(&title, width)), Style::new().add_modifier(Modifier::BOLD))))
            .style(Style::new().bg(Color::Black)),
        header,
    );

    if app.service == Service::Calendar {
        draw_event(f, app, body);
    } else if app.service == Service::Tasks {
        draw_task(f, app, body);
    } else {
        draw_messages(f, app, body);
    }

    draw_input(f, app, input);
}

fn input_height(app: &App, width: usize) -> u16 {
    app.input.layout(width.saturating_sub(2)).0.len().clamp(1, 8) as u16 + 1
}

fn draw_input(f: &mut Frame, app: &App, input: Rect) {
    let width = input.width.saturating_sub(1).max(1) as usize;
    let (input_lines, cursor) = app.input.layout(width.saturating_sub(2));
    let focused = app.focus == Focus::Input;
    let cal = app.service == Service::Calendar;
    let tasks = app.service == Service::Tasks;
    let label = match &app.mode {
        Mode::Normal if cal && focused => " new event — e.g. morgen 14:00 Zahnarzt @ Praxis ".into(),
        Mode::Normal if tasks && focused => " new task — e.g. morgen 14:00 Arzt anrufen (14:00 = reminder) ".into(),
        Mode::Edit(_) if tasks => " editing task ".into(),
        Mode::Normal => String::new(),
        Mode::Edit(_) if cal => " editing event ".into(),
        Mode::Edit(_) => " editing message ".into(),
        Mode::Reply { id, ping } => {
            let who = app.author_of(id).unwrap_or_else(|| "message".into());
            format!(" replying to {who}{} ", if *ping { "" } else { " (no ping)" })
        }
    };
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(if focused { Style::new().fg(Color::Green) } else { DIM })
        .title(Span::styled(label, Style::new().fg(Color::Yellow)));
    let inner = block.inner(input);
    f.render_widget(block, input);
    let visible = inner.height as usize;
    let skip = (cursor.0 + 1).saturating_sub(visible);
    let lines: Vec<Line> = input_lines
        .iter()
        .enumerate()
        .skip(skip)
        .take(visible)
        .map(|(i, l)| Line::from(vec![Span::styled(if i == 0 { "› " } else { "  " }, DIM), Span::raw(l.clone())]))
        .collect();
    if app.input.is_empty() && !focused {
        let hint = match app.service {
            Service::Calendar => "› press n to add an event",
            Service::Tasks => "› n new task · x done · e edit · dd delete",
            _ => "› press i to type",
        };
        f.render_widget(Paragraph::new(Line::from(Span::styled(hint, DIM))), inner);
    } else {
        f.render_widget(Paragraph::new(lines), inner);
    }
    if focused && app.finder.is_none() && !app.help && !app.cal_detail {
        f.set_cursor_position(Position::new(inner.x + 2 + cursor.1 as u16, inner.y + (cursor.0 - skip) as u16));
    }
}

/// Details of the selected task, login progress, or setup help.
fn draw_task(f: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(2).max(10) as usize;
    let label = |k: &str| Span::styled(format!("  {k:<10}"), DIM);
    let mut lines: Vec<Line> = vec![Line::from("")];
    if let Some((uri, code)) = &app.todo.login {
        lines.push(Line::from("  Sign in to Microsoft:"));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![label("Open"), Span::raw(uri.clone())]));
        lines.push(Line::from(vec![label("Code"), Span::styled(code.clone(), Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD))]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  (the page was opened in your browser; this updates by itself)", DIM)));
    } else if let Some(t) = app.selected_task() {
        let today = Local::now().date_naive();
        let state = if t.done { "done" } else if t.overdue(today) { "overdue" } else { "open" };
        lines.push(Line::from(vec![label("Status"), Span::raw(state)]));
        if let Some(d) = t.due {
            lines.push(Line::from(vec![label("Due"), Span::raw(d.format("%a %d %b %Y").to_string())]));
        }
        if let Some(r) = t.reminder {
            lines.push(Line::from(vec![label("Reminder"), Span::raw(r.format("%a %d %b %H:%M").to_string())]));
        }
        if t.important {
            lines.push(Line::from(vec![label("Important"), Span::raw("yes")]));
        }
        lines.push(Line::from(vec![label("List"), Span::raw(app.todo.list_name(&t.list).to_string())]));
        if !t.note.is_empty() {
            lines.push(Line::from(""));
            for l in wrap(&t.note, width.saturating_sub(2)) {
                lines.push(Line::from(format!("  {l}")));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  x done · e edit · dd delete · n new task", DIM)));
    } else if !app.todo_connected() && app.todo.status.is_empty() {
        let steps: &[&str] = if crate::todo::builtin_client_id().is_some() { TODO_SIGN_IN } else { TODO_SETUP };
        for l in steps {
            lines.push(Line::from(Span::raw(format!("  {l}"))));
        }
    } else {
        lines.push(Line::from(Span::styled("  select a task (j/k) · n new task · h/l fold lists", DIM)));
    }
    f.render_widget(Paragraph::new(lines), area);
}

const TODO_SIGN_IN: &[&str] = &[
    "Connect Microsoft To Do:",
    "",
    "Type  :todo login  and sign in with the code shown here.",
    "Works with personal and work/school Microsoft accounts.",
];

const TODO_SETUP: &[&str] = &[
    "Connect Microsoft To Do (one-time, ~5 minutes):",
    "",
    "1. Open portal.azure.com → \"App registrations\" → New registration.",
    "   Name: discommand. Supported account types: \"Accounts in any",
    "   organizational directory and personal Microsoft accounts\".",
    "   No redirect URI. Register.",
    "2. In the app: Authentication → \"Allow public client flows\" → Yes → Save.",
    "3. Copy the \"Application (client) ID\" from Overview.",
    "4. Here, type:  :todo setup <that id>",
    "   Then sign in at the page that opens, with the code shown here.",
    "",
    "If Azure asks for a directory/tenant first, a free Azure account works.",
];

/// Details of the selected calendar event, or setup help.
fn draw_event(f: &mut Frame, app: &App, area: Rect) {
    let width = area.width.saturating_sub(2).max(10) as usize;
    let label = |k: &str| Span::styled(format!("  {k:<10}"), DIM);
    let mut lines: Vec<Line> = vec![Line::from("")];
    if let Some(e) = app.selected_event() {
        let when = if e.all_day {
            let last = e.end - chrono::Duration::days(1);
            if last.date_naive() > e.start.date_naive() {
                format!("{} – {} (all day)", e.start.format("%a %d %b"), last.format("%a %d %b"))
            } else {
                format!("{} (all day)", e.start.format("%a %d %b %Y"))
            }
        } else if e.end.date_naive() == e.start.date_naive() {
            format!("{} – {}", e.start.format("%a %d %b %Y, %H:%M"), e.end.format("%H:%M"))
        } else {
            format!("{} – {}", e.start.format("%a %d %b %H:%M"), e.end.format("%a %d %b %H:%M"))
        };
        lines.push(Line::from(vec![label("When"), Span::raw(when)]));
        if !e.location.is_empty() {
            lines.push(Line::from(vec![label("Where"), Span::raw(e.location.clone())]));
        }
        if let Some(c) = app.cal.calendars.get(&e.cal) {
            let dot = match c.color {
                Some([r, g, b]) => Span::styled("● ", Style::new().fg(Color::Rgb(r, g, b))),
                None => Span::raw(""),
            };
            lines.push(Line::from(vec![label("Calendar"), dot, Span::raw(c.name.clone())]));
        }
        if !e.reminders.is_empty() {
            let mut r: Vec<String> = e
                .reminders
                .iter()
                .map(|m| match m {
                    0 => "at start".into(),
                    m if m % 1440 == 0 => format!("{} d before", m / 1440),
                    m if m % 60 == 0 => format!("{} h before", m / 60),
                    m => format!("{m} min before"),
                })
                .collect();
            r.dedup();
            lines.push(Line::from(vec![label("Reminder"), Span::raw(r.join(", "))]));
        }
        if !e.description.is_empty() {
            lines.push(Line::from(""));
            for l in wrap(&e.description, width.saturating_sub(2)) {
                lines.push(Line::from(format!("  {l}")));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  e edit · dd delete · o open in browser · n new event", DIM)));
    } else if !app.cal_connected() && app.cal.status.is_empty() {
        // Not connected yet: one command with a built-in app, else the setup steps.
        let steps: &[&str] = if cal::builtin_client().is_some() { CAL_SIGN_IN } else { CAL_SETUP };
        for l in steps {
            lines.push(Line::from(Span::raw(format!("  {l}"))));
        }
    } else {
        lines.push(Line::from(Span::styled("  select an event (j/k) · n new event · t today", DIM)));
    }
    f.render_widget(Paragraph::new(lines), area);
}

const CAL_SIGN_IN: &[&str] = &[
    "Connect Google Calendar:",
    "",
    "Type  :cal login  — your browser opens; sign in and allow access.",
    "",
    "Samsung Calendar: add your Google account in Samsung Calendar on the",
    "phone; events in a Google calendar then show up in both.",
];

const CAL_SETUP: &[&str] = &[
    "Connect Google Calendar (one-time, ~10 minutes):",
    "",
    "1. Open console.cloud.google.com and create a project (any name).",
    "2. APIs & Services → Library → \"Google Calendar API\" → Enable.",
    "3. APIs & Services → OAuth consent screen: User type External,",
    "   fill in the app name and your e-mail, then Audience →",
    "   \"Publish app\" (otherwise Google logs you out every 7 days).",
    "4. Credentials → Create credentials → OAuth client ID →",
    "   Application type \"Desktop app\" → Create → Download JSON.",
    "5. Here, type:  :cal setup C:\\path\\to\\client_secret_….json",
    "   Your browser opens; sign in and allow access. Google warns that",
    "   the app isn't verified: it's your own app, choose Continue.",
    "",
    "Samsung Calendar: add your Google account in Samsung Calendar on the",
    "phone; events in a Google calendar then show up in both.",
];

fn draw_messages(f: &mut Frame, app: &mut App, area: Rect) {
    let h = area.height as usize;
    app.msg_height = h;
    let Some(chat) = app.current_chat().map(String::from) else {
        let logo = vec![
            Line::from(""),
            Line::from(Span::styled("  discommand", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))),
            Line::from(Span::styled("  j/k move · l open · / find chat · shift+tab Discord/WhatsApp · ? help", DIM)),
        ];
        f.render_widget(Paragraph::new(logo), area);
        return;
    };
    let width = area.width.saturating_sub(1).max(4) as usize;
    let key = app.cache_key(&chat);
    let version = app.msg_version();
    // The selected message shows its pictures large (only if it has any).
    let hd = app
        .msg_sel
        .filter(|_| app.pics.enabled && app.focus == Focus::Messages)
        .filter(|s| app.msg_cache.as_ref().is_some_and(|c| c.key == key && c.pics.iter().any(|(i, _)| i == s)))
        .map(|s| (s, (h as u32).saturating_sub(4).max(MAX_ROWS)));
    let fresh = matches!(&app.msg_cache, Some(c) if c.key == key && c.width == width && c.version == version && c.hd == hd);
    if !fresh {
        let header = if app.chat_loading(&chat) {
            Some("  loading older messages…")
        } else if app.chat_complete(&chat) {
            Some("  — beginning of conversation —")
        } else {
            None
        };
        let (lines, pics) = match app.view_messages(&chat) {
            Some(msgs) => {
                let pics = msgs.iter().enumerate().flat_map(|(i, m)| m.pics.iter().map(move |p| (i, p.clone()))).collect();
                (build_lines(&msgs, header, width, &app.pics, hd), pics)
            }
            None => (vec![(usize::MAX, Line::from(Span::styled("  loading…", DIM)))], Vec::new()),
        };
        app.msg_cache = Some(MsgCache { key, width, version, lines, pics, hd });
    }
    let cache = app.msg_cache.as_ref().unwrap();
    let lines = &cache.lines;
    let total = lines.len();

    match app.msg_sel {
        None => app.msg_scroll = 0,
        Some(sel) => {
            let start = lines.iter().position(|(i, _)| *i == sel).unwrap_or(0);
            let end = lines.iter().rposition(|(i, _)| *i == sel).map_or(total, |e| e + 1);
            let mut top = total.saturating_sub(h + app.msg_scroll);
            if start < top {
                top = start;
            }
            if end > top + h {
                // Taller than the screen: show its beginning.
                top = if end - start > h { start } else { end.saturating_sub(h) };
            }
            app.msg_scroll = total.saturating_sub(h).saturating_sub(top);
        }
    }
    let top = total.saturating_sub(h + app.msg_scroll);

    // Load pictures only for messages on (or just around) the screen.
    if app.pics.enabled {
        let mut shown = lines.iter().skip(top).take(h).map(|(i, _)| *i).filter(|i| *i != usize::MAX);
        if let Some(first) = shown.next() {
            let last = shown.last().unwrap_or(first);
            let range = first.saturating_sub(3)..=last + 3;
            app.pics_wanted = cache
                .pics
                .iter()
                .filter(|(i, p)| range.contains(i) && app.pics.wants(&p.key))
                .map(|(_, p)| p.clone())
                .collect();
            if let Some((sel, _)) = hd {
                for (_, p) in cache.pics.iter().filter(|(i, _)| *i == sel) {
                    let key = p.hd_key();
                    if app.pics.wants(&key) {
                        app.pics_wanted.push(PicRef { key, src: p.hd_src.clone(), ..p.clone() });
                    }
                }
            }
        }
    }

    // Selection is applied here, not cached, so moving it needs no rebuild.
    let selected = if app.focus == Focus::Messages || app.mode != Mode::Normal { app.msg_sel } else { None };
    let shown: Vec<Line> = lines
        .iter()
        .skip(top)
        .take(h)
        .map(|(i, l)| {
            let mut l = l.clone();
            if selected == Some(*i) {
                l.style = l.style.bg(Color::DarkGray);
            }
            l
        })
        .collect();
    let pad = h.saturating_sub(shown.len()) as u16;
    let rect = Rect { y: area.y + pad, height: area.height - pad, ..area };
    f.render_widget(Paragraph::new(shown), rect);
}

/// Builds wrapped, styled lines tagged with their message index.
fn build_lines(
    msgs: &[ViewMsg],
    header: Option<&str>,
    width: usize,
    pics: &Pics,
    hd: Option<(usize, u32)>,
) -> Vec<(usize, Line<'static>)> {
    let mut out: Vec<(usize, Line<'static>)> = Vec::new();
    if let Some(h) = header {
        out.push((usize::MAX, Line::from(Span::styled(h.to_string(), DIM))));
    }

    let today = Local::now().date_naive();
    let mut prev: Option<(&str, i64, NaiveDate)> = None;

    for (i, m) in msgs.iter().enumerate() {
        let ts = m.time.map(|t| t.timestamp()).unwrap_or(0);
        let date = m.time.map(|t| t.date_naive()).unwrap_or(today);

        if prev.is_some_and(|(_, _, d)| d != date) {
            let label = format!(" {} ", date.format("%a %d %b %Y"));
            let side = width.saturating_sub(label.width()) / 2;
            out.push((usize::MAX, Line::from(Span::styled(format!("{}{label}{}", "─".repeat(side), "─".repeat(side)), DIM))));
        }

        if m.system {
            let text = format!("→ {} {}", m.author, m.body);
            for l in wrap(&text, width) {
                out.push((i, Line::from(Span::styled(l, DIM.add_modifier(Modifier::ITALIC)))));
            }
            prev = None;
            continue;
        }

        let grouped = matches!(m.reply, Reply::None)
            && prev.is_some_and(|(a, pt, d)| a == m.author_key && ts - pt < 420 && d == date);
        if !grouped {
            match &m.reply {
                Reply::To { name, text } => {
                    let head = format!("  ╭ {name}: ");
                    let room = width.saturating_sub(head.width());
                    out.push((i, Line::from(vec![Span::styled(head, DIM), Span::styled(truncate(text, room), DIM)])));
                }
                Reply::Missing => out.push((i, Line::from(Span::styled("  ╭ original message was deleted", DIM)))),
                Reply::None => {}
            }
            let time = match m.time {
                Some(t) if date == today => t.format("%H:%M").to_string(),
                Some(t) => t.format("%d/%m %H:%M").to_string(),
                None => String::new(),
            };
            out.push((
                i,
                Line::from(vec![
                    Span::styled(m.author.clone(), Style::new().fg(name_color(&m.author_key)).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("  {time}"), DIM),
                ]),
            ));
        }

        let text_style = if m.dim {
            DIM.add_modifier(Modifier::ITALIC)
        } else if m.ping {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new()
        };
        let body_lines = if m.body.is_empty() { Vec::new() } else { wrap(&m.body, width.saturating_sub(2)) };
        let n = body_lines.len();
        for (j, l) in body_lines.into_iter().enumerate() {
            let mut spans = vec![Span::raw("  "), Span::styled(l, text_style)];
            if j + 1 == n && m.edited {
                spans.push(Span::styled(" (edited)", DIM));
            }
            out.push((i, Line::from(spans)));
        }
        if pics.enabled {
            let max_cols = width.saturating_sub(2) as u32;
            let hd_rows = hd.filter(|(sel, _)| *sel == i).map(|(_, rows)| rows);
            for p in &m.pics {
                if let Some(rows) = hd_rows {
                    match pics.get(&p.hd_key()) {
                        Some(Some(pic)) => {
                            out.extend(pic_lines(pic, max_cols.min(HD_COLS), rows).into_iter().map(|l| (i, l)));
                            continue;
                        }
                        Some(None) => {}
                        None => out.push((i, Line::from(Span::styled("  ⋯ loading full quality", DIM)))),
                    }
                }
                match pics.get(&p.key) {
                    Some(Some(pic)) => out.extend(pic_lines(pic, max_cols, MAX_ROWS).into_iter().map(|l| (i, l))),
                    Some(None) => {}
                    None => {
                        // Reserve the space so the layout doesn't jump when it arrives.
                        let (_, rows) = cells(p.w, p.h, max_cols.min(MAX_COLS), MAX_ROWS);
                        out.push((i, Line::from(Span::styled("  ⋯ image", DIM))));
                        out.extend((1..rows).map(|_| (i, Line::default())));
                    }
                }
            }
        }
        if !m.reactions.is_empty() {
            out.push((i, Line::from(vec![Span::raw("  "), Span::styled(truncate(&m.reactions, width.saturating_sub(2)), DIM)])));
        }

        prev = Some((m.author_key.as_str(), ts, date));
    }
    out
}

/// Half-block rendering: "▀" with the top pixel as foreground, bottom as background.
fn pic_lines(pic: &Pic, max_cols: u32, max_rows: u32) -> Vec<Line<'static>> {
    let cols = cells(pic.w, pic.h, max_cols, max_rows).0;
    // Narrower than decoded: sample with a stride.
    let step = pic.w as f32 / cols as f32;
    let rows = ((pic.h as f32 / step / 2.0).ceil() as u32).max(1);
    let rgb = |[r, g, b]: [u8; 3]| Color::Rgb(r, g, b);
    (0..rows)
        .map(|r| {
            let (y0, y1) = (((2 * r) as f32 * step) as u32, ((2 * r + 1) as f32 * step) as u32);
            let mut spans = Vec::with_capacity(cols as usize + 1);
            spans.push(Span::raw("  "));
            for c in 0..cols {
                let x = (c as f32 * step) as u32;
                spans.push(Span::styled("▀", Style::new().fg(rgb(pic.get(x, y0))).bg(rgb(pic.get(x, y1)))));
            }
            Line::from(spans)
        })
        .collect()
}

fn draw_statusline(f: &mut Frame, app: &App, area: Rect) {
    let (mode, color) = match app.focus {
        Focus::Input => match app.mode {
            Mode::Edit(_) => (" EDIT ", Color::Magenta),
            Mode::Reply { .. } => (" REPLY ", Color::Green),
            Mode::Normal => (" INSERT ", Color::Green),
        },
        Focus::Command => (" COMMAND ", Color::Yellow),
        _ if app.finder.is_some() => (" FIND ", Color::Yellow),
        _ => (" NORMAL ", ACCENT),
    };
    let (service, service_bg, other) = match app.service {
        Service::Discord => {
            let n = app.wa.unread_chats();
            (" DISCORD ", Color::Indexed(99), if n > 0 { format!(" WhatsApp: {n} unread ") } else { String::new() })
        }
        Service::WhatsApp => {
            let n: u32 = app.state.mentions.values().sum();
            (" WHATSAPP ", WA_GREEN, if n > 0 { format!(" Discord: {n} mentions ") } else { String::new() })
        }
        Service::Calendar => (" CALENDAR ", Color::Yellow, String::new()),
        Service::Tasks => (" TASKS ", Color::LightBlue, String::new()),
    };
    // Open tasks due today or earlier, everywhere.
    let due = app.todo.due_count(Local::now().date_naive());
    let other = if due > 0 { format!("{other} ☐ {due} due ") } else { other };
    // The next event, everywhere.
    let now = chrono::Local::now();
    let other = match app.cal.next_event(now) {
        Some(e) if other.is_empty() || area.width > 110 => {
            let when = if e.start <= now { "now".to_string() } else { e.start.format("%H:%M").to_string() };
            format!("{other} ⏰ {when} {} ", truncate(&e.title, 24))
        }
        _ => other,
    };
    let pane = match app.focus {
        _ if app.service == Service::Calendar => format!(" {} ", app.cal_view.name()),
        Focus::Tree => " chats ".to_string(),
        _ => " messages ".to_string(),
    };
    let (name, conn) = match app.service {
        Service::Discord => (app.state.me.name().to_string(), app.conn.as_str()),
        Service::WhatsApp => (app.wa.name.clone(), app.wa.conn.as_str()),
        Service::Calendar if app.cal.status.is_empty() => ("Google Calendar".to_string(), "not connected"),
        Service::Calendar => (app.cal.account.clone(), app.cal.status.as_str()),
        Service::Tasks if app.todo.status.is_empty() => ("Microsoft To Do".to_string(), "not connected"),
        Service::Tasks => ("Microsoft To Do".to_string(), app.todo.status.as_str()),
    };
    let right = format!(" {name} · {conn} ");
    let mut spans = vec![
        Span::styled(mode, Style::new().bg(color).fg(Color::Black).add_modifier(Modifier::BOLD)),
        Span::styled(service, Style::new().bg(service_bg).fg(Color::Black).add_modifier(Modifier::BOLD)),
        Span::styled(pane.clone(), Style::new().bg(Color::DarkGray).fg(Color::White)),
        Span::styled(other.clone(), Style::new().bg(Color::Black).fg(Color::Yellow)),
    ];
    let used = mode.width() + service.width() + pane.width() + other.width() + right.width();
    spans.push(Span::styled(" ".repeat((area.width as usize).saturating_sub(used)), Style::new().bg(Color::Black)));
    let conn_color = if conn == "connected" || conn.starts_with("synced") { Color::Green } else { Color::Yellow };
    spans.push(Span::styled(right, Style::new().bg(Color::Black).fg(conn_color)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_cmdline(f: &mut Frame, app: &App, area: Rect) {
    if app.react_picker {
        let mut spans = vec![Span::styled(" react: ", Style::new().fg(Color::Yellow))];
        for (i, e) in crate::app::REACTIONS.iter().enumerate() {
            spans.push(Span::styled(format!("{} ", i + 1), Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
            spans.push(Span::raw(format!("{e}   ")));
        }
        spans.push(Span::styled("(same again removes it · esc cancels)", DIM));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    } else if app.focus == Focus::Command {
        let (lines, cur) = app.cmdline.layout(usize::MAX / 2);
        let text = lines.first().cloned().unwrap_or_default();
        f.render_widget(Paragraph::new(format!(":{text}")), area);
        f.set_cursor_position(Position::new(area.x + 1 + cur.1 as u16, area.y));
    } else if !app.status.is_empty() {
        let style = if app.status.starts_with("E") || app.status.contains("fail") {
            Style::new().fg(Color::Red)
        } else {
            Style::new()
        };
        f.render_widget(Paragraph::new(Span::styled(truncate(&app.status, area.width as usize), style)), area);
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_finder(f: &mut Frame, app: &App) {
    let Some(finder) = &app.finder else { return };
    let area = centered(f.area(), 64, 20);
    f.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" find channel ")
        .border_style(Style::new().fg(Color::Yellow));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [query, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    let (lines, cur) = finder.query.layout(usize::MAX / 2);
    f.render_widget(Paragraph::new(format!("/{}", lines[0])), query);
    f.set_cursor_position(Position::new(query.x + 1 + cur.1 as u16, query.y));

    let h = list.height as usize;
    let skip = (finder.sel + 1).saturating_sub(h);
    let items: Vec<Line> = finder
        .matches
        .iter()
        .enumerate()
        .skip(skip)
        .take(h)
        .map(|(i, &idx)| {
            let label = truncate(&finder.all[idx].1, list.width as usize);
            if i == finder.sel {
                Line::from(Span::styled(label, Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD)))
            } else {
                Line::from(label)
            }
        })
        .collect();
    f.render_widget(Paragraph::new(items), list);
}

const HELP: &[(&str, &str)] = &[
    ("", "NORMAL — anywhere"),
    ("i a", "insert mode (write message)"),
    (": ö", "command line"),
    ("/ - # ^k", "find channel"),
    ("shift+tab  gt", "next service (Discord → WhatsApp → Calendar)"),
    ("rshift+tab  gT", "previous service · {n}gt: the n-th one"),
    ("tab ^w w", "switch pane (^h / ^l also work)"),
    ("? ß", "this help"),
    ("", "NORMAL — channels"),
    ("j k  gg G", "move (counts like 5j work)"),
    ("l o ⏎", "open channel / expand"),
    ("h", "collapse / go to parent"),
    ("za zo zc zM", "toggle / open / close / close all"),
    ("", "NORMAL — messages"),
    ("j k ^u ^d", "select message (gg loads history)"),
    ("G esc", "jump to newest"),
    ("r R", "reply (with / without ping)"),
    ("e  dd", "edit / delete selected"),
    ("+  (or =)", "react: 1 ❤️ 2 👍 3 😂 4 💀 5 😅"),
    ("o gx", "open link / attachment / media"),
    ("", "CALENDAR"),
    ("hjkl  w b", "move 30 min / day · next/prev event"),
    ("tab", "next event at the cursor (incl. all-day)"),
    ("H L  t  v", "page · now · week/3 days/day/agenda"),
    ("n  e  dd  ⏎", "new at cursor / edit / delete / details"),
    ("o  r", "open in browser / sync"),
    ("", "TASKS"),
    ("n  e  dd", "new / edit / delete task"),
    ("x  space", "done / not done (also on tasks in the calendar)"),
    ("", "INSERT"),
    ("⏎  alt-⏎", "send / newline"),
    ("↑", "edit your last message (empty input)"),
    ("esc", "back to normal mode"),
    ("", "COMMANDS"),
    (":b name", "open first matching channel"),
    (":r", "reload chat / sync calendar"),
    (":read", "mark everything read (Discord)"),
    (":images on|off", "show pictures in chats (on)"),
    (":wa receipts on", "send WhatsApp read receipts (off)"),
    (":wa logout", "unlink WhatsApp"),
    (":cal setup FILE", "connect Google Calendar"),
    (":todo setup ID", "connect Microsoft To Do"),
    (":q  :logout", "quit / quit and forget token"),
];

fn draw_help(f: &mut Frame) {
    let area = centered(f.area(), 60, HELP.len() as u16 + 2);
    f.render_widget(Clear, area);
    let lines: Vec<Line> = HELP
        .iter()
        .map(|(k, d)| {
            if k.is_empty() {
                Line::from(Span::styled(*d, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)))
            } else {
                Line::from(vec![Span::styled(format!("  {k:<14}"), Style::new().fg(Color::Yellow)), Span::raw(*d)])
            }
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" help — any key closes ").border_style(Style::new().fg(ACCENT))),
        area,
    );
}

// ---- calendar time grid ------------------------------------------------------

/// Width of the hour column on the left.
const GUTTER: u16 = 6;
const GRIDLINE: Style = Style::new().fg(Color::Indexed(238));
/// Google's default blue, for calendars without a color.
const DEFAULT_EVENT: [u8; 3] = [66, 133, 244];

fn draw_cal_grid(f: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.saturating_sub(1).max(1) as usize;
    let input_h = input_height(app, width);
    let [grid, detail, input] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(2), Constraint::Length(input_h)]).areas(area);
    draw_grid(f, app, grid);
    draw_grid_detail(f, app, detail);
    draw_input(f, app, input);
}

/// Block colors for an event: its calendar's color (dimmed once past) with
/// readable text on top.
fn event_style(app: &App, e: &cal::Event, now: chrono::DateTime<Local>, selected: bool) -> Style {
    let [mut r, mut g, mut b] = app.cal.calendars.get(&e.cal).and_then(|c| c.color).unwrap_or(DEFAULT_EVENT);
    if e.end <= now {
        (r, g, b) = (r / 2, g / 2, b / 2);
    }
    let light = 299 * r as u32 + 587 * g as u32 + 114 * b as u32 > 150_000;
    let mut style = Style::new().bg(Color::Rgb(r, g, b)).fg(if light { Color::Black } else { Color::White });
    if selected {
        style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    }
    style
}

fn draw_grid(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width <= GUTTER + 4 || area.height < 3 {
        return;
    }
    let now = Local::now();
    let today = now.date_naive();
    let cur_day = app.cal_cursor.date();

    // Which days: a week starts on Monday; fewer days follow the cursor.
    let fit = ((area.width - GUTTER) / 10).max(1) as usize;
    let n = app.cal_view.days().clamp(1, fit);
    if n == 7 {
        app.cal_start = cur_day - Days::new(cur_day.weekday().num_days_from_monday() as u64);
    } else if cur_day < app.cal_start {
        app.cal_start = cur_day;
    } else if cur_day >= app.cal_start + Days::new(n as u64) {
        app.cal_start = cur_day - Days::new(n as u64 - 1);
    }
    let days: Vec<NaiveDate> = (0..n).map(|i| app.cal_start + Days::new(i as u64)).collect();

    let all_day: Vec<Vec<&cal::Event>> = days
        .iter()
        .map(|d| app.cal.events.iter().filter(|e| e.all_day && e.start.date_naive() <= *d && e.end.date_naive() > *d).collect())
        .collect();
    // As many all-day rows as the busiest day needs, up to a quarter of the height.
    let most = all_day.iter().map(Vec::len).max().unwrap_or(0);
    let ad_rows = most.min((area.height as usize / 4).max(1));
    let header_h = 1 + ad_rows as u16;
    let rows = area.height.saturating_sub(header_h) as usize;

    // Vertical scroll: keep the cursor in view; start a bit above it.
    let cur_slot = (app.cal_cursor.hour() * 2 + app.cal_cursor.minute() / 30) as usize;
    if !app.cal_scrolled {
        app.cal_top = cur_slot.saturating_sub(rows / 3);
        app.cal_scrolled = true;
    }
    if cur_slot < app.cal_top {
        app.cal_top = cur_slot;
    } else if cur_slot >= app.cal_top + rows {
        app.cal_top = cur_slot + 1 - rows;
    }
    app.cal_top = app.cal_top.min(cal::SLOTS.saturating_sub(rows));
    let top = app.cal_top;

    // Spread the width evenly; leftover columns go one each to the first days.
    let avail = (area.width - GUTTER) as usize;
    let col_x = |i: usize| area.x + GUTTER + (avail * i / n) as u16;
    let col_w = |i: usize| (avail * (i + 1) / n - avail * i / n) as u16;
    let grid_y = area.y + header_h;
    let selected = app.selected_event().map(|e| e.key());
    let app = &*app;
    let buf = f.buffer_mut();

    // Day headers and the all-day row.
    for (i, d) in days.iter().enumerate() {
        let w = col_w(i).saturating_sub(1);
        let label = if w >= 10 { d.format("%a %d %b") } else { d.format("%a %d") }.to_string();
        let mut style = Style::new().add_modifier(Modifier::BOLD);
        if *d == today {
            style = style.fg(Color::Yellow);
        }
        if *d == cur_day {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        buf.set_stringn(col_x(i) + 1, area.y, &label, w as usize, style);
        let list = &all_day[i];
        // With more events than rows, the last row says "+N more", and the
        // window scrolls so a selected event is always visible.
        let fits = if list.len() > ad_rows && ad_rows > 1 { ad_rows - 1 } else { ad_rows };
        let sel_idx = list.iter().position(|e| selected.as_deref() == Some(e.key().as_str())).unwrap_or(0);
        let first = (sel_idx + 1).saturating_sub(fits);
        for (r, e) in list.iter().skip(first).take(fits).enumerate() {
            let style = event_style(app, e, now, selected.as_deref() == Some(e.key().as_str()));
            let y = area.y + 1 + r as u16;
            buf.set_stringn(col_x(i) + 1, y, format!("{:<width$}", e.title, width = w as usize), w as usize, style);
        }
        if list.len() > fits && fits < ad_rows {
            let y = area.y + ad_rows as u16;
            buf.set_stringn(col_x(i) + 1, y, format!("+{} more (Tab)", list.len() - fits), w as usize, DIM);
        }
    }

    // Hours, day separators and hour lines.
    for r in 0..rows {
        let slot = top + r;
        if slot >= cal::SLOTS {
            break;
        }
        let y = grid_y + r as u16;
        if slot % 2 == 0 {
            buf.set_stringn(area.x, y, format!("{:02}:00", slot / 2), GUTTER as usize, DIM);
        }
        for i in 0..n {
            buf.set_stringn(col_x(i), y, "│", 1, GRIDLINE);
            if slot % 2 == 0 {
                let w = col_w(i).saturating_sub(1) as usize;
                buf.set_stringn(col_x(i) + 1, y, "╌".repeat(w), w, GRIDLINE);
            }
        }
    }

    // The current time.
    let now_slot = (now.hour() * 2 + now.minute() / 30) as usize;
    if let Some(i) = days.iter().position(|d| *d == today) {
        if (top..top + rows).contains(&now_slot) {
            let y = grid_y + (now_slot - top) as u16;
            let w = col_w(i).saturating_sub(1) as usize;
            buf.set_stringn(col_x(i) + 1, y, "━".repeat(w), w, Style::new().fg(Color::Red));
            buf.set_stringn(area.x, y, now.format("%H:%M").to_string(), GUTTER as usize, Style::new().fg(Color::Red));
        }
    }

    // Event blocks; overlapping events share the column in lanes.
    for (i, d) in days.iter().enumerate() {
        let (ds, de) = (cal::local_midnight(*d), cal::local_midnight(*d + Days::new(1)));
        let evs: Vec<&cal::Event> = app.cal.events.iter().filter(|e| !e.all_day && e.start < de && e.end > ds).collect();
        let inner = col_w(i).saturating_sub(1);
        for (k, lane, lanes) in cal::layout_lanes(&evs) {
            let e = evs[k];
            let s_slot = ((e.start.max(ds) - ds).num_minutes() / 30) as usize;
            let e_slot = (((e.end.min(de) - ds).num_minutes() + 29) / 30).max(s_slot as i64 + 1) as usize;
            let (vs, ve) = (s_slot.max(top), e_slot.min(top + rows));
            let lane_w = inner / lanes as u16;
            if vs >= ve || lane_w == 0 {
                continue;
            }
            // One column of gap between neighbouring blocks.
            let w = if lanes > 1 { lane_w.saturating_sub(1).max(1) } else { inner };
            let x = col_x(i) + 1 + lane_w * lane as u16;
            let style = event_style(app, e, now, selected.as_deref() == Some(e.key().as_str()));
            let mut text = vec![e.title.clone()];
            // Events spanning midnight show the days too.
            if e.start < ds || e.end > de {
                text.push(format!("{}–{}", e.start.format("%a %H:%M"), e.end.format("%a %H:%M")));
            } else {
                text.push(format!("{}–{}", e.start.format("%H:%M"), e.end.format("%H:%M")));
            }
            if !e.location.is_empty() {
                text.push(e.location.clone());
            }
            for (j, y) in (vs..ve).map(|s| grid_y + (s - top) as u16).enumerate() {
                let line = text.get(j).map(String::as_str).unwrap_or("");
                let st = if j == 0 { style.add_modifier(Modifier::BOLD) } else { style };
                buf.set_stringn(x, y, format!("{line:<width$}", width = w as usize), w as usize, st);
            }
        }
    }

    // The cursor, unless a timed event there is selected (it's highlighted instead).
    if !app.selected_event().is_some_and(|e| !e.all_day) {
        if let Some(i) = days.iter().position(|d| *d == cur_day) {
            if (top..top + rows).contains(&cur_slot) {
                let w = col_w(i).saturating_sub(1) as usize;
                let label = format!("+ {}", app.cal_cursor.format("%H:%M"));
                let y = grid_y + (cur_slot - top) as u16;
                buf.set_stringn(col_x(i) + 1, y, format!("{label:<w$}"), w, Style::new().bg(Color::DarkGray).fg(Color::White));
            }
        }
    }
}

/// Two lines under the grid: the event at the cursor, and the keys.
fn draw_grid_detail(f: &mut Frame, app: &App, area: Rect) {
    let w = area.width as usize;
    let line = match app.selected_event() {
        Some(e) => {
            let when = if e.all_day {
                let last = (e.end - chrono::Duration::days(1)).date_naive();
                if last > e.start.date_naive() {
                    format!("{} – {} (all day)", e.start.format("%a %d %b"), last.format("%a %d %b"))
                } else {
                    format!("{} (all day)", e.start.format("%a %d %b"))
                }
            } else {
                format!("{}–{}", e.start.format("%a %d %b %H:%M"), e.end.format("%H:%M"))
            };
            let mut s = format!(" {when}  {}", e.title);
            if !e.location.is_empty() {
                s.push_str(&format!("  @ {}", e.location));
            }
            if let Some(c) = app.cal.calendars.get(&e.cal) {
                s.push_str(&format!("  · {}", c.name));
            }
            if e.cal == crate::todo::TASKS_CALENDAR {
                s.push_str("  · x: done");
            }
            Line::from(Span::styled(truncate(&s, w), Style::new().add_modifier(Modifier::BOLD)))
        }
        None if !app.cal_connected() && app.cal.status.is_empty() => {
            let how = if cal::builtin_client().is_some() { "type :cal login" } else { "press v for the agenda view with setup steps" };
            Line::from(Span::styled(format!(" Google Calendar isn't connected: {how}"), DIM))
        }
        None => Line::from(Span::styled(format!(" {} — free", app.cal_cursor.format("%a %d %b %H:%M")), DIM)),
    };
    let keys = " ⇥ next event here · w/b next/prev event · n new · e edit · dd delete · ⏎ details · t now · v view";
    f.render_widget(Paragraph::new(vec![line, Line::from(Span::styled(truncate(keys, w), DIM))]), area);
}

fn draw_event_popup(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 76, 22);
    f.render_widget(Clear, area);
    let title = app.selected_event().map(|e| format!(" {} ", e.title)).unwrap_or_default();
    let block = Block::bordered().title(Span::styled(title, Style::new().add_modifier(Modifier::BOLD))).border_style(Style::new().fg(Color::Yellow));
    let inner = block.inner(area);
    f.render_widget(block, area);
    draw_event(f, app, inner);
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::app::{App, CalView, Service};
    use crate::cal::{CalInfo, Event};

    /// Renders the week grid with sample events; run with --ignored --nocapture to look at it.
    #[test]
    #[ignore]
    fn preview_week_grid() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (gw, _) = tokio::sync::mpsc::unbounded_channel();
        let (wa, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::api::Api::new("x").unwrap(), tx, gw, wa);
        app.service = Service::Calendar;
        app.cal_view = CalView::Week;
        let monday = Local::now().date_naive() - Days::new(Local::now().weekday().num_days_from_monday() as u64);
        let at = |d: u64, h: u32, m: u32| Local.from_local_datetime(&(monday + Days::new(d)).and_hms_opt(h, m, 0).unwrap()).unwrap();
        let ev = |id: &str, title: &str, cal: &str, s, e, all_day| Event {
            id: id.into(),
            cal: cal.into(),
            title: title.into(),
            description: String::new(),
            location: if id == "a" { "Praxis".into() } else { String::new() },
            start: s,
            end: e,
            all_day,
            link: String::new(),
            reminders: vec![],
        };
        app.cal.calendars.insert("w".into(), CalInfo { id: "w".into(), name: "Work".into(), color: Some([66, 133, 244]), primary: true, writable: true });
        app.cal.calendars.insert("p".into(), CalInfo { id: "p".into(), name: "Private".into(), color: Some([246, 191, 38]), primary: false, writable: true });
        app.cal.events = vec![
            ev("a", "Zahnarzt", "p", at(1, 9, 0), at(1, 10, 0), false),
            ev("b", "Standup", "w", at(1, 9, 30), at(1, 10, 0), false),
            ev("c", "Review", "w", at(2, 11, 0), at(2, 12, 30), false),
            ev("d", "Geburtstag Ana", "p", at(3, 0, 0), at(4, 0, 0), true),
            ev("f", "Urlaub", "w", at(3, 0, 0), at(5, 0, 0), true),
            ev("g", "Feiertag", "p", at(3, 0, 0), at(4, 0, 0), true),
            ev("h", "Planning", "w", at(1, 9, 0), at(1, 10, 0), false),
            ev("e", "Gym", "p", at(4, 18, 0), at(4, 19, 30), false),
        ];
        app.cal.status = "synced".into();
        app.cal_cursor = at(2, 11, 0).naive_local();
        app.cal.events.sort_by(|a, b| (a.start, !a.all_day, &a.title).cmp(&(b.start, !b.all_day, &b.title)));
        // Select the second all-day event on Thursday, as Tab would.
        app.cal_sel = Some(app.cal.events.iter().find(|e| e.title == "Urlaub").unwrap().key());
        let mut term = Terminal::new(TestBackend::new(110, 30)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        for y in 0..buf.area.height {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
            println!("{line}");
        }
    }
}
