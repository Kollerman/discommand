//! PDFs, shown in the app (the `pdf` feature): each page drawn as a picture
//! where the terminal can show pictures (Sixel, Kitty), and the text of the
//! pages, readable and searchable everywhere. `engine` does the reading.

#[cfg(feature = "pdf")]
pub mod engine;
pub mod view;

pub use view::Viewer;

/// Whether this build shows PDFs.
pub const BUILT: bool = cfg!(feature = "pdf");

/// Said when a PDF would be shown but the feature isn't built in.
pub const NOT_BUILT: &str = "the PDF viewer isn't built in: run the install script again and answer y for \"PDF viewer\"";

/// Reads a PDF file: its bytes and the size of each page.
pub fn load(path: &std::path::Path) -> Result<(std::sync::Arc<Vec<u8>>, Vec<(f32, f32)>), String> {
    #[cfg(feature = "pdf")]
    {
        let data = std::sync::Arc::new(std::fs::read(path).map_err(|e| format!("couldn't read {}: {e}", path.display()))?);
        let pages = engine::pages(&data)?;
        Ok((data, pages))
    }
    #[cfg(not(feature = "pdf"))]
    {
        let _ = path;
        Err(NOT_BUILT.into())
    }
}

/// The text of each page.
pub fn text(data: &std::sync::Arc<Vec<u8>>) -> Result<Vec<String>, String> {
    #[cfg(feature = "pdf")]
    return engine::text(data);
    #[cfg(not(feature = "pdf"))]
    {
        let _ = data;
        Err(NOT_BUILT.into())
    }
}

/// The picture of (part of) a page, see `engine::picture`.
pub fn picture(
    data: &std::sync::Arc<Vec<u8>>,
    shot: view::Shot,
    drawn: Option<std::sync::Arc<image::RgbaImage>>,
) -> Result<(view::Picture, std::sync::Arc<image::RgbaImage>), String> {
    #[cfg(feature = "pdf")]
    return engine::picture(data, shot, drawn);
    #[cfg(not(feature = "pdf"))]
    {
        let _ = (data, shot, drawn);
        Err(NOT_BUILT.into())
    }
}

/// Whether a file name or link is a PDF's.
pub fn is_pdf(name: &str) -> bool {
    let name = name.split(['?', '#']).next().unwrap_or(name);
    name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".pdf")
}

#[cfg(test)]
mod tests {
    use super::is_pdf;

    #[test]
    fn knows_pdfs() {
        assert!(is_pdf("Folien 01.PDF"));
        assert!(is_pdf("https://cdn.discordapp.com/a/b/script.pdf?ex=1&is=2"));
        assert!(!is_pdf("pdf"));
        assert!(!is_pdf("notes.pdf.exe"));
    }
}
