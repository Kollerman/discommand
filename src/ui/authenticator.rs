//! The authenticator tab: the current code in big digits, how long it's
//! still valid, or how to set it up.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::{ACCENT, DIM};
use crate::system::totp;

/// Digits five rows high.
const DIGITS: [[&str; 5]; 10] = [
    ["█▀▀█", "█  █", "█  █", "█  █", "█▄▄█"],
    [" ▄█ ", "  █ ", "  █ ", "  █ ", " ▄█▄"],
    ["█▀▀█", "   █", "▄▀▀▀", "█   ", "█▄▄▄"],
    ["█▀▀█", "   █", " ▀▀█", "   █", "█▄▄█"],
    ["█  █", "█  █", "▀▀▀█", "   █", "   █"],
    ["█▀▀▀", "█   ", "▀▀▀█", "   █", "▄▄▄█"],
    ["█▀▀▀", "█   ", "█▀▀█", "█  █", "█▄▄█"],
    ["█▀▀█", "   █", "  █ ", " █  ", " █  "],
    ["█▀▀█", "█  █", "█▀▀█", "█  █", "█▄▄█"],
    ["█▀▀█", "█  █", "▀▀▀█", "   █", "▄▄▄█"],
];

const SETUP: &[&str] = &[
    "No authenticator key yet. To set one up for a Microsoft login (UR's MFA):",
    "",
    "1. On mysignins.microsoft.com open Security info → Add sign-in method",
    "   → Authenticator app → \"I want to use a different authenticator app\".",
    "2. Under \"Can't scan image?\" the page shows a key.",
    "3. Here: :totp setup <key>   then enter the code shown on that page.",
    "",
    "Keep your phone app as a second method.",
];

pub fn draw(f: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    match totp::current() {
        Some((code, left)) => {
            // Six digits in two groups of three.
            for row in 0..5 {
                let mut s = String::new();
                for (i, d) in code.chars().filter_map(|c| c.to_digit(10)).enumerate() {
                    s.push_str(if i == 3 { "    " } else if i > 0 { "  " } else { "" });
                    s.push_str(DIGITS[d as usize][row]);
                }
                lines.push(Line::from(Span::styled(s, Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))).centered());
            }
            lines.push(Line::from(""));
            // A bar emptying over the 30 seconds; red near the end.
            let width = 32usize;
            let full = (left as usize * width).div_ceil(totp::STEP as usize);
            let color = if left <= 5 { Color::Red } else { ACCENT };
            lines.push(
                Line::from(vec![
                    Span::styled("█".repeat(full), Style::new().fg(color)),
                    Span::styled("░".repeat(width - full), DIM),
                ])
                .centered(),
            );
            lines.push(Line::from(Span::styled(format!("valid {left} s more"), DIM)).centered());
        }
        None => {
            let width = SETUP.iter().map(|l| l.chars().count()).max().unwrap_or(0);
            let pad = " ".repeat((area.width as usize).saturating_sub(width) / 2);
            lines.extend(SETUP.iter().map(|l| Line::from(format!("{pad}{l}"))));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("q back · shift+tab next tab · :totp forget removes the key", DIM)).centered());
    let top = (area.height as usize).saturating_sub(lines.len()) / 2;
    let mut all = vec![Line::from(""); top];
    all.extend(lines);
    f.render_widget(Paragraph::new(all), area);
}
