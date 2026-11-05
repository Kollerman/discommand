//! The compose window (GroupWise tab), laid out like GroupWise's own "New
//! Message": a title bar, To / Cc / Subject, the text, and what can be done.

use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use super::{ACCENT, DIM};
use crate::app::mail::{Compose, Field};

pub fn draw(f: &mut Frame, c: &Compose, area: Rect) {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(format!(" ✉ {} ", c.title), Style::new().fg(Color::White).bg(ACCENT).add_modifier(Modifier::BOLD)))
        .title_bottom(Line::from(vec![
            Span::styled(" Ctrl+S ", Style::new().fg(Color::White).bg(ACCENT).add_modifier(Modifier::BOLD)),
            Span::styled(
                if c.typing { " send · Tab next field · Esc stop typing " } else { " send · : commands (:w send, :q! discard) · i type · Esc Esc discard " },
                DIM,
            ),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 12 || inner.height < 6 {
        return;
    }
    let label_w = 9u16;
    let field_w = inner.width.saturating_sub(label_w + 1) as usize;
    let mut cursor = None;
    let mut dropdown_at = None;
    let mut y = inner.y;
    // The header fields, one line each (scrolled to the cursor when long).
    for field in [Field::To, Field::Cc, Field::Subject] {
        let active = c.field == field;
        let input = c.input(field);
        let (lines, (_, col)) = input.layout(usize::MAX / 2);
        let text = lines.first().cloned().unwrap_or_default();
        let skip = col.saturating_sub(field_w.saturating_sub(1));
        let shown: String = text.chars().skip(skip).take(field_w).collect();
        let label_style = if active { Style::new().fg(ACCENT).add_modifier(Modifier::BOLD) } else { DIM };
        let hint = match field {
            Field::To | Field::Cc if text.is_empty() => "name@uni-regensburg.de, …",
            Field::Subject if text.is_empty() => "Subject",
            _ => "",
        };
        let value = if shown.is_empty() { Span::styled(hint, Style::new().fg(Color::DarkGray)) } else { Span::raw(shown) };
        f.render_widget(Paragraph::new(Line::from(vec![Span::styled(format!(" {:<7} ", field.label()), label_style), value])), Rect::new(inner.x, y, inner.width, 1));
        if active {
            cursor = Some(Position::new(inner.x + label_w + (col - skip) as u16, y));
            dropdown_at = Some(y + 1);
        }
        y += 1;
        // The line under each field, as in the mail window.
        let rule = if active { Style::new().fg(ACCENT) } else { Style::new().fg(Color::Indexed(238)) };
        f.render_widget(Paragraph::new(Span::styled("─".repeat(inner.width.saturating_sub(label_w + 1) as usize), rule)), Rect::new(inner.x + label_w, y, inner.width - label_w, 1));
        y += 1;
    }
    y += 1;
    // The text, wrapped, scrolled so the cursor stays visible; the original
    // (for a reply) dimmed below it.
    let body_area = Rect::new(inner.x + 1, y, inner.width.saturating_sub(2), (inner.y + inner.height).saturating_sub(y));
    if body_area.height == 0 {
        return;
    }
    let width = body_area.width as usize;
    let (body, (row, col)) = c.body.layout(width.max(1));
    let empty = c.body.is_empty();
    let mut lines: Vec<Line> = if empty && c.field != Field::Body {
        vec![Line::from(Span::styled("Write your mail here", Style::new().fg(Color::DarkGray)))]
    } else {
        body.iter().map(|l| Line::from(l.clone())).collect()
    };
    let quote = c.quote_lines();
    if !quote.is_empty() {
        lines.push(Line::from(""));
        for q in quote {
            for part in super::text::wrap(&q, width) {
                lines.push(Line::from(Span::styled(part, DIM)));
            }
        }
    }
    let height = body_area.height as usize;
    let top = (row + 1).saturating_sub(height);
    f.render_widget(Paragraph::new(lines.into_iter().skip(top).collect::<Vec<_>>()), body_area);
    if c.field == Field::Body {
        cursor = Some(Position::new(body_area.x + col as u16, body_area.y + (row - top) as u16));
    }
    if let (Some(top), true) = (dropdown_at, c.showing()) {
        draw_suggestions(f, c, Rect::new(inner.x + label_w, top, inner.width.saturating_sub(label_w + 1), inner.y + inner.height - top));
    }
    if let Some(p) = cursor.filter(|_| c.typing) {
        f.set_cursor_position(p);
    }
}

/// The address suggestions, in a box over what's under the field.
fn draw_suggestions(f: &mut Frame, c: &Compose, room: Rect) {
    let shown = c.suggestions.len().min(room.height.saturating_sub(2) as usize);
    if shown == 0 {
        return;
    }
    let width = c.suggestions.iter().map(|(n, e)| n.chars().count() + e.chars().count() + 4).max().unwrap_or(20).clamp(24, room.width as usize) as u16;
    let area = Rect::new(room.x, room.y, width, shown as u16 + 2);
    let lines: Vec<Line> = c
        .suggestions
        .iter()
        .take(shown)
        .enumerate()
        .map(|(i, (name, email))| {
            let style = if i == c.picked { Style::new().bg(ACCENT).fg(Color::White) } else { Style::new() };
            let mut spans = vec![Span::styled(format!(" {name} "), style.add_modifier(Modifier::BOLD))];
            spans.push(Span::styled(format!("{email} "), if i == c.picked { style } else { DIM }));
            Line::from(spans)
        })
        .collect();
    f.render_widget(Clear, area);
    let block = Block::new().borders(Borders::ALL).border_style(Style::new().fg(ACCENT)).title_bottom(Span::styled(" ↑↓ pick · Tab take ", DIM));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// The move menu (`m`), as GroupWise's "Move to folder": the folders as a
/// tree, the one picked highlighted.
pub fn draw_move(f: &mut Frame, m: &crate::app::mail::MoveMenu, screen: Rect) {
    let width = m.folders.iter().map(|(_, l)| l.chars().count() + 4).max().unwrap_or(20).clamp(30, screen.width.saturating_sub(4) as usize) as u16;
    let height = (m.folders.len() as u16 + 2).min(screen.height.saturating_sub(2));
    let area = Rect::new(screen.x + (screen.width.saturating_sub(width)) / 2, screen.y + (screen.height.saturating_sub(height)) / 2, width, height);
    let room = height.saturating_sub(2) as usize;
    let top = (m.picked + 1).saturating_sub(room);
    let lines: Vec<Line> = m
        .folders
        .iter()
        .enumerate()
        .skip(top)
        .take(room)
        .map(|(i, (id, label))| {
            let here = *id == m.from;
            let style = if i == m.picked {
                Style::new().bg(ACCENT).fg(Color::White).add_modifier(Modifier::BOLD)
            } else if here {
                DIM
            } else {
                Style::new()
            };
            Line::from(Span::styled(format!(" {label}{} ", if here { "  (here)" } else { "" }), style))
        })
        .collect();
    let what = if m.ids.len() == 1 { "1 mail".to_string() } else { format!("{} mails", m.ids.len()) };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(format!(" MOVE {what} TO FOLDER "), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)))
        .title_bottom(Span::styled(" j/k pick · ⏎ move · Esc close ", DIM));
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(block), area);
}
