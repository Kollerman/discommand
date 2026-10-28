//! Inline pictures, drawn with half blocks: each cell is "▀" with the top
//! pixel as foreground and the bottom pixel as background. Low resolution,
//! but it works in every truecolor terminal and is just more text lines.
//!
//! Each picture has a small version (shown inline) and a large "hd" version
//! that is only loaded for the selected message.

use std::collections::{HashMap, HashSet, VecDeque};

use image::imageops::FilterType;

/// Largest inline picture, in terminal cells.
pub const MAX_COLS: u32 = 40;
pub const MAX_ROWS: u32 = 12;
/// Largest picture of the selected message (also capped by the pane size).
pub const HD_COLS: u32 = 160;
pub const HD_ROWS: u32 = 60;
/// Small pictures kept decoded; the cache is simply emptied when full.
const MAX_CACHED: usize = 500;
/// Large pictures kept decoded (about 60 KB each).
const MAX_HD: usize = 8;

#[derive(Clone, Debug)]
pub enum PicSrc {
    Url(String),
    /// A WhatsApp message; `full` downloads the original instead of the
    /// embedded thumbnail.
    Wa { chat: String, id: String, full: bool },
    /// The same for a Matrix message (`full`: a large copy, not the thumbnail).
    Mx { chat: String, id: String, full: bool },
}

#[derive(Clone, Debug)]
pub struct PicRef {
    pub key: String,
    pub src: PicSrc,
    /// Source size in pixels when known, to reserve space before loading.
    pub w: u32,
    pub h: u32,
    pub hd_src: PicSrc,
}

impl PicRef {
    pub fn hd_key(&self) -> String {
        format!("hd:{}", self.key)
    }
}

/// A decoded picture, already scaled to its size in cells (h is 2 × rows).
pub struct Pic {
    pub w: u32,
    pub h: u32,
    pub px: Vec<[u8; 3]>,
}

impl Pic {
    pub fn get(&self, x: u32, y: u32) -> [u8; 3] {
        let (x, y) = (x.min(self.w - 1), y.min(self.h - 1));
        self.px[(y * self.w + x) as usize]
    }
}

/// Cells (cols, rows) used by a w × h pixel picture; a cell holds 1 × 2 pixels.
pub fn cells(w: u32, h: u32, max_cols: u32, max_rows: u32) -> (u32, u32) {
    let (max_cols, max_rows) = (max_cols.max(1), max_rows.max(1));
    if w == 0 || h == 0 {
        return (max_cols.min(24), max_rows.min(6));
    }
    let cols = w.min(max_cols);
    let rows = (h as u64 * cols as u64 / (w as u64 * 2)).max(1) as u32;
    if rows <= max_rows {
        return (cols, rows);
    }
    let cols = (w as u64 * max_rows as u64 * 2 / h as u64).clamp(1, max_cols as u64) as u32;
    (cols, max_rows)
}

pub fn is_hd(key: &str) -> bool {
    key.starts_with("hd:")
}

/// Decodes and scales an image to the size its key calls for.
pub fn decode_for(key: &str, bytes: &[u8]) -> Option<Pic> {
    if is_hd(key) {
        decode(bytes, HD_COLS, HD_ROWS)
    } else {
        decode(bytes, MAX_COLS, MAX_ROWS)
    }
}

pub fn decode(bytes: &[u8], max_cols: u32, max_rows: u32) -> Option<Pic> {
    let img = image::load_from_memory(bytes).ok()?;
    let (cols, rows) = cells(img.width(), img.height(), max_cols, max_rows);
    let img = img.resize_exact(cols, rows * 2, FilterType::Triangle).to_rgba8();
    // Transparent areas (stickers) are blended onto black, the terminal background.
    let px = img
        .pixels()
        .map(|p| {
            let a = p[3] as u16;
            [(p[0] as u16 * a / 255) as u8, (p[1] as u16 * a / 255) as u8, (p[2] as u16 * a / 255) as u8]
        })
        .collect();
    Some(Pic { w: cols, h: rows * 2, px })
}

/// Asks Discord's media proxy for a copy at most `max_px` wide: a few KB
/// instead of the original.
pub fn discord_url(proxy_url: &str, w: u32, h: u32, max_px: u32) -> String {
    let (tw, th) = if w > 0 && h > 0 {
        let tw = w.min(max_px);
        (tw, (h as u64 * tw as u64 / w as u64).max(1) as u32)
    } else {
        (max_px, max_px * 3 / 5)
    };
    let sep = if proxy_url.contains('?') { '&' } else { '?' };
    format!("{proxy_url}{sep}width={tw}&height={th}")
}

pub struct Pics {
    done: HashMap<String, Option<Pic>>,
    pending: HashSet<String>,
    hd_order: VecDeque<String>,
    pub enabled: bool,
    /// Bumped when pictures arrive; keys the UI's line cache.
    pub version: u64,
}

impl Default for Pics {
    fn default() -> Self {
        Self { done: HashMap::new(), pending: HashSet::new(), hd_order: VecDeque::new(), enabled: true, version: 0 }
    }
}

impl Pics {
    /// None: not loaded yet. Some(None): failed or has no picture.
    pub fn get(&self, key: &str) -> Option<&Option<Pic>> {
        self.done.get(key)
    }

    pub fn wants(&self, key: &str) -> bool {
        !self.done.contains_key(key) && !self.pending.contains(key)
    }

    /// Marks a load as started; false when it already was.
    pub fn start(&mut self, key: &str) -> bool {
        self.wants(key) && self.pending.insert(key.to_string())
    }

    pub fn finish(&mut self, key: String, pic: Option<Pic>) {
        self.pending.remove(&key);
        if is_hd(&key) {
            self.hd_order.push_back(key.clone());
            while self.hd_order.len() > MAX_HD {
                if let Some(old) = self.hd_order.pop_front() {
                    self.done.remove(&old);
                }
            }
        } else if self.done.len() >= MAX_CACHED {
            self.done.retain(|k, _| is_hd(k));
        }
        self.done.insert(key, pic);
        self.version += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_keep_aspect() {
        assert_eq!(cells(400, 200, 40, 12), (40, 10)); // 2:1 → 40 × 20 px
        assert_eq!(cells(100, 1000, 40, 12), (2, 12)); // tall: limited by rows
        assert_eq!(cells(10, 10, 40, 12), (10, 5)); // small: never upscaled
        assert_eq!(cells(400, 200, 20, 12), (20, 5)); // narrow pane
        assert_eq!(cells(1600, 1200, 160, 60), (160, 60)); // hd: 4:3 fills it exactly
        assert_eq!(cells(1600, 1600, 160, 60), (120, 60)); // hd: square, limited by rows
    }

    #[test]
    fn decodes_png() {
        let mut buf = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(8, 4, image::Rgb([255, 0, 0]))
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let pic = decode_for("k", buf.get_ref()).unwrap();
        assert_eq!((pic.w, pic.h), (8, 4));
        assert_eq!(pic.get(3, 3), [255, 0, 0]);
    }

    #[test]
    fn discord_url_appends_size() {
        assert_eq!(discord_url("https://m/x.png?ex=1", 800, 400, 80), "https://m/x.png?ex=1&width=80&height=40");
        assert_eq!(discord_url("https://m/x.png", 0, 0, 80), "https://m/x.png?width=80&height=48");
    }

    #[test]
    fn keeps_few_hd_pictures() {
        let mut p = Pics::default();
        let pic = || Some(Pic { w: 1, h: 2, px: vec![[0; 3]; 2] });
        for i in 0..MAX_HD + 2 {
            p.finish(format!("hd:{i}"), pic());
        }
        p.finish("small".into(), pic());
        assert!(p.get("hd:0").is_none());
        assert!(p.get(&format!("hd:{}", MAX_HD + 1)).is_some());
        assert!(p.get("small").is_some());
    }
}
