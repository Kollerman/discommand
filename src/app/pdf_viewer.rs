//! The PDF viewer: opening a PDF (from a chat, Moodle or `:pdf`), what each
//! key does in it, and making the pictures of its pages in the background.

use std::path::PathBuf;

use super::keys::is_ctrl;
use super::*;
use crate::pdf::view::{self, Picture, Shot};
use crate::pdf::{self, Viewer};

pub enum PdfMsg {
    /// A PDF from Discord is in the cache: open it.
    Downloaded(PathBuf),
    Opened(u64, PathBuf, Result<(Arc<Vec<u8>>, Vec<(f32, f32)>), String>),
    Text(u64, Result<Vec<String>, String>),
    Picture(u64, Shot, Result<(Picture, Arc<image::RgbaImage>), String>),
}

/// Lines (text view) or cells (page view) moved by `j`/`k`.
const STEP: u16 = 3;

impl App {
    /// Opens a PDF file in the viewer; without the viewer built in, in the
    /// system's PDF program.
    pub(super) fn open_pdf(&mut self, path: PathBuf) {
        if !pdf::BUILT {
            let shown = path.display().to_string();
            self.status = match open_file(&shown) {
                Ok(_) => format!("opened {shown} ({})", pdf::NOT_BUILT),
                Err(e) => format!("could not open {shown}: {e}"),
            };
            return;
        }
        self.pdf_id += 1;
        let id = self.pdf_id;
        self.status = "opening the PDF…".into();
        self.spawn(async move {
            let file = path.clone();
            let result = tokio::task::spawn_blocking(move || pdf::load(&file)).await.map_err(|e| e.to_string()).and_then(|r| r);
            Some(AppEvent::Pdf(PdfMsg::Opened(id, path, result)))
        });
    }

    /// A Discord attachment: downloaded into the cache, then shown.
    pub(super) fn open_pdf_link(&mut self, url: String, name: String) {
        self.status = format!("downloading {name}…");
        let http = self.pic_http.clone();
        self.spawn(async move {
            let path = crate::system::secret::cache_dir().join("discord").join(crate::planner::moodle::safe_name(&name));
            match download(&http, &url, &path).await {
                Ok(()) => Some(AppEvent::Pdf(PdfMsg::Downloaded(path))),
                Err(e) => Some(AppEvent::Error(e)),
            }
        });
    }

    pub(super) fn on_pdf(&mut self, msg: PdfMsg) {
        match msg {
            PdfMsg::Downloaded(path) => self.open_pdf(path),
            PdfMsg::Opened(id, path, result) if id == self.pdf_id => match result {
                Ok((data, pages)) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let count = pages.len();
                    let viewer = Viewer::new(id, name.clone(), path, data.clone(), pages);
                    self.status = if viewer.text_view {
                        format!("{name}: {count} pages · this terminal can't show pictures, so here is the text (Tab: blocky pages)")
                    } else {
                        format!("{name}: {count} pages")
                    };
                    self.pdf = Some(viewer);
                    self.clear_screen = true;
                    self.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || pdf::text(&data)).await.map_err(|e| e.to_string()).and_then(|r| r);
                        Some(AppEvent::Pdf(PdfMsg::Text(id, result)))
                    });
                }
                Err(e) => self.status = e,
            },
            PdfMsg::Text(id, result) => {
                let Some(v) = self.pdf.as_mut().filter(|v| v.id == id) else { return };
                let lines = match result {
                    Ok(pages) => view::lines(pages),
                    Err(e) => {
                        self.status = format!("couldn't read the text: {e}");
                        Vec::new()
                    }
                };
                v.text = Some(lines);
                v.go(v.page, false);
            }
            PdfMsg::Picture(id, shot, result) => {
                let Some(v) = self.pdf.as_mut().filter(|v| v.id == id) else { return };
                v.making = None;
                match result {
                    Ok((picture, page)) => {
                        // Where the old picture showed and the new one doesn't, it must go.
                        if v.picture.as_ref().is_some_and(|(s, _)| s.cells != shot.cells) {
                            self.clear_screen = true;
                        }
                        v.picture = Some((shot, picture));
                        v.drawn = Some((shot.page, shot.width, page));
                    }
                    Err(e) => self.status = e,
                }
            }
            PdfMsg::Opened(..) => {}
        }
    }

    /// After drawing: makes the picture the page view needs, if it isn't
    /// there yet. One at a time; the newest wish is made next.
    pub fn load_pdf_page(&mut self) {
        let Some(v) = self.pdf.as_mut() else { return };
        if v.text_view || v.making.is_some() {
            return;
        }
        let area = v.area;
        let Some(shot) = v.shot(area) else { return };
        if v.picture.as_ref().is_some_and(|(s, _)| *s == shot) {
            return;
        }
        v.making = Some(shot);
        let drawn = v.drawn.as_ref().filter(|(p, w, _)| *p == shot.page && *w == shot.width).map(|d| d.2.clone());
        let (data, id) = (v.data.clone(), v.id);
        self.spawn(async move {
            let result = tokio::task::spawn_blocking(move || pdf::picture(&data, shot, drawn)).await.map_err(|e| e.to_string()).and_then(|r| r);
            Some(AppEvent::Pdf(PdfMsg::Picture(id, shot, result)))
        });
    }

    /// `:pdf [path]`: a PDF from the computer (without a path, from the file dialog).
    pub(super) fn pdf_command(&mut self, arg: &str) {
        let path = match arg.trim().trim_matches('"') {
            "" => {
                let picked = crate::system::pick::file("Open a PDF");
                self.dirty = true;
                match picked {
                    Ok(Some(p)) => p,
                    Ok(None) => return,
                    Err(e) => {
                        self.status = e.replace(":attach", ":pdf");
                        return;
                    }
                }
            }
            p => PathBuf::from(p),
        };
        self.open_pdf(path);
    }

    /// Keys while a PDF is shown.
    pub(super) fn pdf_key(&mut self, k: KeyEvent) {
        let ctrl = is_ctrl(&k);
        let pending = self.pending.take();
        let Some(v) = self.pdf.as_mut() else { return };
        // Typing a search (after `/`).
        if let Some(typed) = v.typing.as_mut() {
            match k.code {
                KeyCode::Esc => v.typing = None,
                KeyCode::Enter => {
                    v.search = v.typing.take().unwrap_or_default();
                    self.pdf_search(false);
                }
                KeyCode::Backspace if typed.is_empty() => v.typing = None,
                KeyCode::Backspace => {
                    typed.pop();
                }
                KeyCode::Char(c) if !ctrl => typed.push(c),
                _ => {}
            }
            return;
        }
        let half = (v.area.height / 2).max(1);
        let lines = v.text.as_ref().map_or(0, Vec::len);
        match k.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.pdf = None;
                self.clear_screen = true;
                self.status.clear();
            }
            KeyCode::Char(':' | 'ö' | 'Ö') => {
                self.last_pane = self.focus;
                self.focus = Focus::Command;
            }
            KeyCode::Tab => {
                v.text_view = !v.text_view;
                if v.text_view {
                    v.go(v.page, false);
                } else {
                    v.top = 0;
                }
                self.clear_screen = true;
            }
            KeyCode::Char('d') if ctrl => scroll(v, half as isize, lines),
            KeyCode::Char('u') if ctrl => scroll(v, -(half as isize), lines),
            KeyCode::Char('j') | KeyCode::Down => scroll(v, STEP as isize, lines),
            KeyCode::Char('k') | KeyCode::Up => scroll(v, -(STEP as isize), lines),
            KeyCode::Char('l') | KeyCode::Right | KeyCode::PageDown | KeyCode::Char(' ') if v.page + 1 < v.pages.len() => {
                v.go(v.page + 1, false);
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::PageUp | KeyCode::Backspace if v.page > 0 => v.go(v.page - 1, false),
            KeyCode::Char('L') => v.left = v.left.saturating_add(STEP * 4),
            KeyCode::Char('H') => v.left = v.left.saturating_sub(STEP * 4),
            KeyCode::Char('g') if pending == Some('g') => v.go(0, false),
            KeyCode::Char('g') => self.pending = Some('g'),
            KeyCode::Char('G') | KeyCode::End => v.go(v.pages.len() - 1, false),
            KeyCode::Home => v.go(0, false),
            KeyCode::Char('w') => {
                v.fit_width = !v.fit_width;
                v.zoom = 1.0;
                (v.top, v.left) = (0, 0);
                self.clear_screen = true;
            }
            KeyCode::Char('+' | '=') => {
                v.zoom = (v.zoom * 1.25).min(4.0);
                self.clear_screen = true;
            }
            KeyCode::Char('-') => {
                v.zoom = (v.zoom / 1.25).max(0.5);
                self.clear_screen = true;
            }
            KeyCode::Char('/' | '#') => v.typing = Some(String::new()),
            KeyCode::Char('n') => self.pdf_search(false),
            KeyCode::Char('N') => self.pdf_search(true),
            KeyCode::Char('o') => {
                let path = v.path.display().to_string();
                if let Err(e) = open_file(&path) {
                    self.status = format!("could not open {path}: {e}");
                }
            }
            _ => {}
        }
    }

    /// Goes to the next (or previous) line with what was searched for.
    fn pdf_search(&mut self, back: bool) {
        let Some(v) = self.pdf.as_mut() else { return };
        if v.search.is_empty() {
            return;
        }
        if v.text.is_none() {
            self.status = "the text is still being read…".into();
            return;
        }
        // In the page view, from this page's start (so a match on it is found).
        let from = if v.text_view { v.line } else { v.text.as_ref().and_then(|t| t.iter().position(|(p, _)| *p == v.page)).unwrap_or(0).saturating_sub(1) };
        match v.find(from, back) {
            Some(i) => {
                v.line = i;
                v.follow_line();
                if !v.text_view {
                    v.top = 0;
                }
                self.status = format!("“{}” on page {}", v.search, v.page + 1);
            }
            None => self.status = format!("“{}” isn't in this PDF", v.search),
        }
    }
}

/// Scrolls: lines in the text view; in the page view down the page, and on
/// to the next one at its end (back to the previous one at its top).
fn scroll(v: &mut Viewer, by: isize, lines: usize) {
    if v.text_view {
        v.line = v.line.saturating_add_signed(by).min(lines.saturating_sub(1));
        v.follow_line();
    } else if by > 0 {
        if v.at_bottom() {
            if v.page + 1 < v.pages.len() {
                v.go(v.page + 1, false);
            }
        } else {
            v.top = v.top.saturating_add(by as u16);
        }
    } else if v.at_top() {
        if v.page > 0 {
            v.go(v.page - 1, true);
        }
    } else {
        v.top = v.top.saturating_sub(by.unsigned_abs() as u16);
    }
}

/// Downloads a file (at most 100 MB).
async fn download(http: &reqwest::Client, url: &str, path: &std::path::Path) -> Result<(), String> {
    let failed = |e: String| format!("couldn't download the PDF: {e}");
    let resp = http.get(url).send().await.map_err(|e| failed(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(failed(format!("HTTP {}", resp.status().as_u16())));
    }
    if resp.content_length().is_some_and(|n| n > 100 << 20) {
        return Err("the PDF is too large (over 100 MB)".into());
    }
    let bytes = resp.bytes().await.map_err(|e| failed(e.to_string()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| failed(e.to_string()))?;
    }
    std::fs::write(path, bytes).map_err(|e| failed(e.to_string()))
}
