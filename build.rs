//! Bakes the publisher's app IDs into the binary from keys/keys.env (kept out
//! of git), so users only sign in. Environment variables set while building
//! take precedence over the file.

use std::fs;

const KEYS: &str = "keys/keys.env";
const VARS: &[&str] = &["DISCOMMAND_GOOGLE_CLIENT_ID", "DISCOMMAND_GOOGLE_CLIENT_SECRET", "DISCOMMAND_MS_CLIENT_ID"];

fn main() {
    println!("cargo:rerun-if-changed={KEYS}");
    for var in VARS {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let Ok(text) = fs::read_to_string(KEYS) else { return };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            println!("cargo:warning={KEYS}: ignoring line without '=': {line}");
            continue;
        };
        // Accept the names with or without the DISCOMMAND_ prefix.
        let name = name.trim();
        let var = if name.starts_with("DISCOMMAND_") { name.to_string() } else { format!("DISCOMMAND_{name}") };
        if !VARS.contains(&var.as_str()) {
            println!("cargo:warning={KEYS}: unknown key {name}");
            continue;
        }
        let value = value.trim().trim_matches('"');
        if !value.is_empty() && std::env::var(&var).is_err() {
            println!("cargo:rustc-env={var}={value}");
        }
    }
}
