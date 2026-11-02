//! The state of an open PDF: which page, which view, how far scrolled and
//! zoomed; and how big the picture of a page is for the space it gets.

use std::path::PathBuf;
use std::sync::Arc;

use ratatui::layout::Rect;

/// A page encoded for the terminal (Sixel, Kitty, or half blocks).
#[cfg(feature = "pdf")]
pub type Picture = ratatui_image::protocol::Protocol;
#[cfg(not(feature = "pdf"))]
pub type Picture = ();

/// What a picture shows: the page, how wide it's drawn (pixels), which
/// part of it (in pixels: left, top, width, height) and in which cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shot {
    pub page: usize,
    pub width: u32,
    pub crop: (u32, u32, u32, u32),
    pub cells: Rect,
}

pub struct Viewer {
    /// Shown in the title.
    pub name: String,
    /// The file, for opening it in another program (`o`).
    pub path: PathBuf,
    pub data: Arc<Vec<u8>>,
    /// Each page's size in points.
    pub pages: Vec<(f32, f32)>,
    pub page: usize,
    /// The text view (else pages are drawn).
    pub text_view: bool,
    /// The page as wide as the screen (else the whole page fits).
    pub fit_width: bool,
    /// On top of that: 1 is as fitted.
    pub zoom: f32,
    /// Scrolled into the page picture, in cells (down, right).
    pub top: u16,
    pub left: u16,
    /// The text: each line and its page; `None` while it's read.
    pub text: Option<Vec<(usize, String)>>,
    /// The first line shown in the text view.
    pub line: usize,
    /// What was searched for last (`/`), and the search being typed.
    pub search: String,
    pub typing: Option<String>,
    /// Where the page view was drawn last (set while drawing).
    pub area: Rect,
    /// The picture shown, and the one being made.
    pub picture: Option<(Shot, Picture)>,
    pub making: Option<Shot>,
    /// The last page drawn (page, width), kept so scrolling only crops it.
    pub drawn: Option<(usize, u32, Arc<image::RgbaImage>)>,
    /// Which opening this is: answers for an earlier one are dropped.
    pub id: u64,
}

impl Viewer {
    pub fn new(id: u64, name: String, path: PathBuf, data: Arc<Vec<u8>>, pages: Vec<(f32, f32)>) -> Self {
        Self {
            name,
            path,
            data,
            pages,
            page: 0,
            // Without pictures in the terminal, the text is the useful view.
            text_view: !pictures(),
            fit_width: false,
            zoom: 1.0,
            top: 0,
            left: 0,
            text: None,
            line: 0,
            search: String::new(),
            typing: None,
            area: Rect::default(),
            picture: None,
            making: None,
            drawn: None,
            id,
        }
    }

    /// The picture the page view needs now, for `area` (cells), if any;
    /// scrolling is kept within the page.
    pub fn shot(&mut self, area: Rect) -> Option<Shot> {
        let (cw, ch) = cell_size();
        let &(pw, ph) = self.pages.get(self.page)?;
        if area.width == 0 || area.height == 0 || pw <= 0.0 || ph <= 0.0 {
            return None;
        }
        let (box_w, box_h) = (area.width as f32 * cw as f32, area.height as f32 * ch as f32);
        let fit = if self.fit_width { box_w } else { box_w.min(box_h * pw / ph) };
        let width = (fit * self.zoom).round().clamp(16.0, 4000.0) as u32;
        let height = (width as f32 * ph / pw).round() as u32;
        // Scrolled no further than to the page's end.
        let max_top = height.div_ceil(ch as u32).saturating_sub(area.height as u32) as u16;
        let max_left = width.div_ceil(cw as u32).saturating_sub(area.width as u32) as u16;
        self.top = self.top.min(max_top);
        self.left = self.left.min(max_left);
        let (x, y) = (self.left as u32 * cw as u32, self.top as u32 * ch as u32);
        let crop = (x, y, (width - x).min(box_w as u32), (height - y).min(box_h as u32));
        // A page narrower than the space is centered.
        let cols = (crop.2.div_ceil(cw as u32) as u16).min(area.width);
        let rows = (crop.3.div_ceil(ch as u32) as u16).min(area.height);
        let cells = Rect::new(area.x + (area.width - cols) / 2, area.y, cols, rows);
        Some(Shot { page: self.page, width, crop, cells })
    }

    /// Whether the page view is at the top or bottom of the page.
    pub fn at_top(&self) -> bool {
        self.top == 0
    }

    pub fn at_bottom(&mut self) -> bool {
        let top = self.top;
        self.top = u16::MAX;
        let area = self.area;
        self.shot(area);
        let end = self.top;
        self.top = top;
        top >= end
    }

    /// Goes to a page (at its top, or its bottom when going back).
    pub fn go(&mut self, page: usize, bottom: bool) {
        self.page = page.min(self.pages.len().saturating_sub(1));
        self.top = if bottom { u16::MAX } else { 0 };
        if let Some(text) = &self.text {
            if let Some(i) = text.iter().position(|(p, _)| *p == self.page) {
                self.line = i;
            }
        }
    }

    /// The text view moved: the page follows it.
    pub fn follow_line(&mut self) {
        if let Some((p, _)) = self.text.as_ref().and_then(|t| t.get(self.line)) {
            self.page = *p;
        }
    }

    /// The next line (after `from`, or before it going back) that contains
    /// the search, ignoring case; wraps around.
    pub fn find(&self, from: usize, back: bool) -> Option<usize> {
        let text = self.text.as_ref()?;
        let needle = self.search.to_lowercase();
        if needle.is_empty() || text.is_empty() {
            return None;
        }
        let n = text.len();
        (1..=n).map(|i| if back { (from + n - i % n) % n } else { (from + i) % n }).find(|&i| text[i].1.to_lowercase().contains(&needle))
    }
}

/// The text of each page as lines, each with its page.
pub fn lines(pages: Vec<String>) -> Vec<(usize, String)> {
    pages.into_iter().enumerate().flat_map(|(p, text)| text.lines().map(|l| (p, l.to_string())).collect::<Vec<_>>()).collect()
}

#[cfg(feature = "pdf")]
mod terminal {
    use std::sync::OnceLock;

    use ratatui_image::picker::{Picker, ProtocolType};

    static PICKER: OnceLock<Picker> = OnceLock::new();

    /// Asks the terminal what it can show and how big its cells are. Must
    /// run before anything else reads the terminal's input.
    pub fn detect() {
        let mut picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::from_fontsize((10, 20)));
        // Windows Terminal (1.22 and later) shows Sixel even where the
        // question isn't answered.
        if picker.protocol_type() == ProtocolType::Halfblocks && std::env::var_os("WT_SESSION").is_some() {
            picker = Picker::from_fontsize((10, 20));
            picker.set_protocol_type(ProtocolType::Sixel);
        }
        picker.set_background_color([255, 255, 255, 255]);
        let _ = PICKER.set(picker);
    }

    pub fn picker() -> Picker {
        PICKER.get().cloned().unwrap_or_else(|| Picker::from_fontsize((10, 20)))
    }
}

#[cfg(feature = "pdf")]
pub use terminal::{detect, picker};

/// Asks the terminal what it can show (a no-op without the feature).
#[cfg(not(feature = "pdf"))]
pub fn detect() {}

/// Whether the terminal shows real pictures (not just half blocks).
pub fn pictures() -> bool {
    #[cfg(feature = "pdf")]
    return picker().protocol_type() != ratatui_image::picker::ProtocolType::Halfblocks;
    #[cfg(not(feature = "pdf"))]
    false
}

/// A cell's size in pixels.
fn cell_size() -> (u16, u16) {
    #[cfg(feature = "pdf")]
    let (w, h) = picker().font_size();
    #[cfg(not(feature = "pdf"))]
    let (w, h) = (10, 20);
    (w.max(1), h.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewer() -> Viewer {
        // A4 and a landscape slide.
        Viewer::new(1, "a.pdf".into(), PathBuf::new(), Arc::new(Vec::new()), vec![(595.0, 842.0), (960.0, 540.0)])
    }

    #[test]
    fn fits_the_whole_page_or_its_width() {
        let mut v = viewer();
        let area = Rect::new(0, 0, 100, 40);
        let (cw, ch) = cell_size();
        let s = v.shot(area).unwrap();
        // The whole page: as tall as the space, centered.
        assert_eq!(s.crop.3, (40 * ch) as u32);
        assert!(s.cells.width < 100 && s.cells.x > 0);
        v.fit_width = true;
        let s = v.shot(area).unwrap();
        assert_eq!(s.width, (100 * cw) as u32);
        assert_eq!(s.cells, Rect::new(0, 0, 100, 40));
    }

    #[test]
    fn scrolls_only_within_the_page() {
        let mut v = viewer();
        v.fit_width = true;
        v.area = Rect::new(0, 0, 100, 40);
        assert!(v.at_top() && !v.at_bottom());
        v.top = 10_000;
        let s = v.shot(v.area).unwrap();
        assert!(v.top > 0);
        assert_eq!(s.crop.1 + s.crop.3, (s.width as f32 * 842.0 / 595.0).round() as u32);
        assert!(v.at_bottom());
    }

    #[test]
    fn searches_around_the_end() {
        let mut v = viewer();
        v.text = Some(lines(vec!["Einführung\nHCI".into(), "Übung zu hci".into()]));
        v.search = "hci".into();
        assert_eq!(v.find(1, false), Some(2));
        assert_eq!(v.find(2, false), Some(1));
        assert_eq!(v.find(1, true), Some(2));
        v.search = "nichts".into();
        assert_eq!(v.find(0, false), None);
    }
}
