//! The PDF viewer on screen: a title line, then the page (as a picture) or
//! the text. The picture is drawn last, after the screen is cleaned of
//! escape sequences, since it is one (Sixel, Kitty).

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::text::{truncate, wrap};
use super::{ACCENT, DIM};
use crate::pdf::Viewer;

/// Everything but the picture. Remembers where the page goes.
pub fn draw(f: &mut Frame, v: &mut Viewer, area: Rect) {
    let [title, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);
    let view = if v.text_view { "text" } else if v.fit_width { "page · width" } else { "page" };
    let zoom = if v.zoom == 1.0 { String::new() } else { format!(" · {:.0} %", v.zoom * 100.0) };
    let left = format!(" 📄 {} · page {} of {} · {view}{zoom}", v.name, v.page + 1, v.pages.len());
    let keys = "Tab text/page · w width · +- zoom · / search · o open · q close ";
    let room = (title.width as usize).saturating_sub(keys.len() + 2);
    let mut spans = vec![Span::styled(truncate(&left, room), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))];
    if title.width as usize > left.len() + keys.len() {
        let gap = title.width as usize - unicode_width::UnicodeWidthStr::width(left.as_str()) - keys.len();
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(keys, DIM));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), title);
    v.area = body;
    if v.text_view {
        draw_text(f, v, body);
    } else if !v.picture.as_ref().is_some_and(|(s, _)| s.page == v.page) {
        let msg = format!("drawing page {}…", v.page + 1);
        f.render_widget(Paragraph::new(Span::styled(msg, DIM)).centered(), Rect { y: body.y + body.height / 2, height: 1, ..body });
    }
}

fn draw_text(f: &mut Frame, v: &Viewer, area: Rect) {
    let Some(text) = &v.text else {
        f.render_widget(Paragraph::new(Span::styled("reading the text…", DIM)).centered(), area);
        return;
    };
    if text.is_empty() {
        let msg = "This PDF has no text (probably scanned): Tab shows its pages.";
        f.render_widget(Paragraph::new(Span::styled(msg, DIM)).centered(), area);
        return;
    }
    let width = area.width.saturating_sub(2) as usize;
    let needle = v.search.to_lowercase();
    let mut lines: Vec<Line> = Vec::new();
    let mut page = None;
    for (p, line) in text.iter().skip(v.line) {
        if lines.len() >= area.height as usize {
            break;
        }
        if page != Some(*p) {
            let mark = format!("── page {} ", p + 1);
            lines.push(Line::from(Span::styled(format!("{mark}{}", "─".repeat(width.saturating_sub(mark.chars().count()))), DIM)));
            page = Some(*p);
        }
        let hit = !needle.is_empty() && line.to_lowercase().contains(&needle);
        for part in wrap(line, width) {
            let style = if hit { Style::new().fg(Color::Yellow) } else { Style::new() };
            lines.push(Line::from(Span::styled(format!(" {part}"), style)));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// The page picture, over everything drawn before.
pub fn draw_picture(f: &mut Frame, v: &Viewer) {
    if v.text_view {
        return;
    }
    #[cfg(feature = "pdf")]
    if let Some((shot, picture)) = v.picture.as_ref().filter(|(s, _)| s.page == v.page) {
        f.render_widget(ratatui_image::Image::new(picture), shot.cells);
    }
    #[cfg(not(feature = "pdf"))]
    let _ = f;
}

/// The search being typed, in the command line.
pub fn draw_search(f: &mut Frame, v: &Viewer, area: Rect) -> bool {
    let Some(typed) = &v.typing else { return false };
    f.render_widget(Paragraph::new(format!("/{typed}")), area);
    f.set_cursor_position(Position::new(area.x + 1 + unicode_width::UnicodeWidthStr::width(typed.as_str()) as u16, area.y));
    true
}
