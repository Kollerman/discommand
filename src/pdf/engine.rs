//! Reading PDFs with hayro (pure Rust): pages drawn into pictures, and the
//! text of each page, put back into lines from where its letters are.

use std::sync::Arc;

use hayro::hayro_interpret::font::GlyphRun;
use hayro::hayro_interpret::util::TransformExt;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterCache, InterpreterSettings,
    SoftMask, interpret_page,
};
use hayro::hayro_syntax::Pdf;
use hayro::kurbo::{BezPath, Point, Rect};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings};

/// The widest a page is drawn, in pixels.
const MAX_WIDTH: u32 = 4000;

fn load(data: &Arc<Vec<u8>>) -> Result<Pdf, String> {
    Pdf::new(data.clone()).map_err(|e| format!("can't read this PDF ({e:?})"))
}

/// The size of each page, in points (width, height).
pub fn pages(data: &Arc<Vec<u8>>) -> Result<Vec<(f32, f32)>, String> {
    let pdf = load(data)?;
    let pages: Vec<_> = pdf.pages().iter().map(|p| p.render_dimensions()).collect();
    if pages.is_empty() {
        return Err("this PDF has no pages".into());
    }
    Ok(pages)
}

/// A page drawn `width` pixels wide, on white.
pub fn render(data: &Arc<Vec<u8>>, page: usize, width: u32) -> Result<image::RgbaImage, String> {
    let pdf = load(data)?;
    let page = pdf.pages().get(page).ok_or("no such page")?;
    let (w, _) = page.render_dimensions();
    let scale = width.clamp(16, MAX_WIDTH) as f32 / w.max(1.0);
    let settings = PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE };
    let pixmap = hayro::render(page, &RenderCache::new(), &InterpreterSettings::default(), &RenderSettings::default(), &settings);
    // On white everything is opaque, so the premultiplied pixels are plain RGBA.
    image::RgbaImage::from_raw(pixmap.width() as u32, pixmap.height() as u32, pixmap.data_as_u8_slice().to_vec())
        .ok_or_else(|| "the page couldn't be drawn".into())
}

/// The picture for `shot`: its page drawn (or `drawn`, if that's the same
/// page at the same width), cut to the part shown and encoded for the
/// terminal. Also the whole page drawn, to be kept.
pub fn picture(
    data: &Arc<Vec<u8>>,
    shot: super::view::Shot,
    drawn: Option<Arc<image::RgbaImage>>,
) -> Result<(super::view::Picture, Arc<image::RgbaImage>), String> {
    let page = match drawn {
        Some(d) => d,
        None => Arc::new(render(data, shot.page, shot.width)?),
    };
    let (x, y, w, h) = shot.crop;
    let part = image::imageops::crop_imm(&*page, x, y, w.min(page.width().saturating_sub(x)), h.min(page.height().saturating_sub(y))).to_image();
    let picture = super::view::picker()
        .new_protocol(image::DynamicImage::ImageRgba8(part), shot.cells, ratatui_image::Resize::Fit(None))
        .map_err(|e| format!("the page couldn't be shown: {e}"))?;
    Ok((picture, page))
}

/// The text of each page, one string per page.
pub fn text(data: &Arc<Vec<u8>>) -> Result<Vec<String>, String> {
    let pdf = load(data)?;
    let cache = InterpreterCache::new();
    Ok(pdf
        .pages()
        .iter()
        .map(|page| {
            let (w, h) = page.render_dimensions();
            let mut ctx = Context::new(
                page.initial_transform(true).to_kurbo(),
                Rect::new(0.0, 0.0, w as f64, h as f64),
                &cache,
                page.xref(),
                InterpreterSettings::default(),
            );
            let mut letters = Letters::default();
            interpret_page(page, &mut ctx, &mut letters);
            lines(letters.0)
        })
        .collect())
}

/// A letter on the page: where it starts (x, y, y growing downwards), its
/// size, and what it is.
struct Letter {
    x: f64,
    y: f64,
    size: f64,
    /// How wide it is, when the font says.
    width: Option<f64>,
    text: String,
}

/// Collects the letters of a page; everything else drawn is ignored.
#[derive(Default)]
struct Letters(Vec<Letter>);

impl<'a> Device<'a> for Letters {
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, _: &DrawMode) {
        // Invisible text counts too: it's what scanned pages have after OCR.
        for glyph in run.glyphs() {
            let Some(text) = glyph.as_unicode() else { continue };
            let t = props.transform * glyph.transform();
            let origin = t * Point::ZERO;
            // Glyphs are 1000 units high.
            let size = (t * Point::new(0.0, 1000.0) - origin).hypot();
            let width = match &**glyph {
                hayro::hayro_interpret::font::Glyph::Outline(g) => g.advance_width().map(|w| w as f64 / 1000.0 * size),
                _ => None,
            };
            let text = match text {
                hayro::hayro_interpret::hayro_cmap::BfString::Char(c) => c.to_string(),
                hayro::hayro_interpret::hayro_cmap::BfString::String(s) => s,
            };
            // Symbol fonts' private characters are almost always bullets.
            let text: String = text.chars().map(|c| if ('\u{E000}'..='\u{F8FF}').contains(&c) { '•' } else { c }).collect();
            self.0.push(Letter { x: origin.x, y: origin.y, size, width, text });
        }
    }
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// Letters into lines: those on about the same height form a line, read
/// left to right, with a space where the gap is wide, and an empty line
/// between paragraphs (where lines are further apart than usual).
fn lines(mut letters: Vec<Letter>) -> String {
    letters.retain(|l| l.size > 0.5 && !l.text.trim().is_empty() || l.text == " ");
    letters.sort_by(|a, b| a.y.total_cmp(&b.y));
    let mut rows: Vec<Vec<Letter>> = Vec::new();
    for l in letters {
        match rows.last_mut() {
            Some(row) if (row[0].y - l.y).abs() < row[0].size.max(l.size) * 0.4 => row.push(l),
            _ => rows.push(vec![l]),
        }
    }
    let mut out = String::new();
    let mut last: Option<(f64, f64)> = None;
    for mut row in rows {
        row.sort_by(|a, b| a.x.total_cmp(&b.x));
        let size = row.iter().map(|l| l.size).fold(0.0, f64::max);
        let y = row[0].y;
        if let Some((y0, size0)) = last {
            out.push('\n');
            if y - y0 > size.max(size0) * 1.9 {
                out.push('\n');
            }
        }
        last = Some((y, size));
        let mut line = String::new();
        let mut end: Option<f64> = None;
        for l in &row {
            if end.is_some_and(|e| l.x - e > l.size * 0.15) && !line.ends_with(' ') && l.text != " " {
                line.push(' ');
            }
            if !(l.text == " " && line.ends_with(' ')) {
                line.push_str(&l.text);
            }
            // Where it ends; without its width, about half its size per letter.
            end = Some(l.x + l.width.unwrap_or(l.size * 0.5 * l.text.chars().count() as f64));
        }
        out.push_str(line.trim_end());
    }
    out.trim_start_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::{Letter, lines};

    fn at(x: f64, y: f64, text: &str) -> Letter {
        Letter { x, y, size: 10.0, width: None, text: text.into() }
    }

    #[test]
    fn puts_letters_into_lines() {
        // Out of order, two lines, a word gap, then a paragraph further down.
        let letters = vec![at(5.0, 12.0, "b"), at(0.0, 12.0, "a"), at(0.0, 0.0, "H"), at(5.0, 0.5, "i"), at(20.0, 0.0, "x"), at(0.0, 40.0, "c")];
        assert_eq!(lines(letters), "Hi x\nab\n\nc");
    }
}

