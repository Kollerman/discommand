//! QR codes printed to the terminal before the UI starts (logins).

use std::io::Write;

use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};

/// Clears the screen and prints `lines` followed by a QR code of `data`.
pub fn show(lines: &[&str], data: &str) {
    use qrcode::render::unicode::Dense1x2;
    let Ok(qr) = qrcode::QrCode::new(data.as_bytes()) else { return };
    // Inverted for dark terminals: light modules are drawn as blocks.
    let art = qr.render::<Dense1x2>().dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).build();
    let mut out = std::io::stdout();
    let _ = execute!(out, Clear(ClearType::All), MoveTo(0, 0));
    for l in lines {
        println!("{l}");
    }
    println!("\n{art}");
    let _ = out.flush();
}
