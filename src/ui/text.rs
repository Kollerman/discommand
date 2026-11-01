//! Text helpers: wrapping, sanitizing and a small line editor.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Replaces tabs and strips control characters (keeps newlines).
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Greedy word wrap to `width` display columns; long words are hard-broken.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        let mut w = 0;
        for word in para.split(' ') {
            let ww = word.width();
            let sep = usize::from(!line.is_empty());
            if w + sep + ww <= width {
                if sep == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                w += sep + ww;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
                w = 0;
            }
            for ch in word.chars() {
                let cw = ch.width().unwrap_or(0);
                if w + cw > width && !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                    w = 0;
                }
                line.push(ch);
                w += cw;
            }
        }
        out.push(line);
    }
    out
}

/// Cuts `s` to `width` columns, appending an ellipsis when shortened.
pub fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Finds the first http(s) URL in `s`.
pub fn find_url(s: &str) -> Option<&str> {
    let start = s.find("https://").or_else(|| s.find("http://"))?;
    let rest = &s[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '>' | ')' | ']' | '"' | '\''))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// A single- or multi-line text editor buffer.
#[derive(Default, Clone)]
pub struct Input {
    chars: Vec<char>,
    cursor: usize,
}

impl Input {
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn set(&mut self, s: &str) {
        self.chars = s.chars().collect();
        self.cursor = self.chars.len();
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|&c| c != '\r') {
            self.insert(c);
        }
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    fn delete_word(&mut self) {
        while self.cursor > 0 && self.chars[self.cursor - 1] == ' ' {
            self.backspace();
        }
        while self.cursor > 0 && !matches!(self.chars[self.cursor - 1], ' ' | '\n') {
            self.backspace();
        }
    }

    fn line_start(&self) -> usize {
        self.chars[..self.cursor].iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self) -> usize {
        self.chars[self.cursor..].iter().position(|&c| c == '\n').map_or(self.chars.len(), |i| self.cursor + i)
    }

    /// Handles common editing keys. Returns false when the key wasn't used.
    pub fn handle_key(&mut self, k: &KeyEvent) -> bool {
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        // Ctrl+Alt is AltGr on Windows: treat it as a normal character.
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL) && !alt;
        match k.code {
            KeyCode::Char(c) if !ctrl => self.insert(c),
            KeyCode::Char('w') | KeyCode::Char('h') if ctrl => self.delete_word(),
            KeyCode::Backspace if ctrl || alt => self.delete_word(),
            KeyCode::Char('u') if ctrl => {
                let start = self.line_start();
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char('a') if ctrl => self.cursor = self.line_start(),
            KeyCode::Char('e') if ctrl => self.cursor = self.line_end(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Home => self.cursor = self.line_start(),
            KeyCode::End => self.cursor = self.line_end(),
            _ => return false,
        }
        true
    }

    /// Hard-wraps the buffer at `width`; returns lines and the cursor (row, col).
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(1);
        let mut lines = vec![String::new()];
        let mut w = 0;
        let mut cur = (0, 0);
        for (i, &c) in self.chars.iter().enumerate() {
            if c == '\n' {
                if i == self.cursor {
                    cur = (lines.len() - 1, w);
                }
                lines.push(String::new());
                w = 0;
                continue;
            }
            let cw = c.width().unwrap_or(0);
            if w + cw > width {
                lines.push(String::new());
                w = 0;
            }
            if i == self.cursor {
                cur = (lines.len() - 1, w);
            }
            lines.last_mut().unwrap().push(c);
            w += cw;
        }
        if self.cursor == self.chars.len() {
            if w >= width {
                lines.push(String::new());
                cur = (lines.len() - 1, 0);
            } else {
                cur = (lines.len() - 1, w);
            }
        }
        (lines, cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_words() {
        assert_eq!(wrap("hello world foo", 11), vec!["hello world", "foo"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
    }

    #[test]
    fn finds_urls() {
        assert_eq!(find_url("see <https://x.y/z> now"), Some("https://x.y/z"));
        assert_eq!(find_url("nothing"), None);
    }

    #[test]
    fn altgr_types_characters() {
        // Windows reports AltGr+Q ('@' on German keyboards) as Ctrl+Alt+'@'.
        let mut i = Input::default();
        i.handle_key(&KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        i.handle_key(&KeyEvent::new(KeyCode::Char('ä'), KeyModifiers::NONE));
        assert_eq!(i.text(), "@ä");
    }

    #[test]
    fn input_layout_cursor() {
        let mut i = Input::default();
        i.set("abcd");
        assert_eq!(i.layout(2), (vec!["ab".into(), "cd".into(), "".into()], (2, 0)));
    }
}
