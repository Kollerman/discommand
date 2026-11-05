//! An authenticator app inside discommand: the 6-digit codes that change
//! every 30 seconds (TOTP, RFC 6238), as Google Authenticator or the
//! Microsoft Authenticator show them. Logins that accept a "TOTP
//! authenticator" as second factor (the University of Regensburg's MFA does)
//! can use these codes. The key is kept encrypted like the Discord token.
//! It's the `totp` feature; without it, `:totp` says how to add it.

#[cfg(feature = "totp")]
use hmac::{Hmac, Mac};
#[cfg(feature = "totp")]
use sha1::Sha1;

use super::secret;

const SECRET_NAME: &str = "totp";
/// How long each code is valid.
pub const STEP: u64 = 30;

/// Whether this build has the authenticator.
pub const BUILT: bool = cfg!(feature = "totp");

/// Said by `:totp` when it isn't built in.
pub const NOT_BUILT: &str = "the authenticator isn't built in: run the install script again and answer y for \"Authenticator\"";

/// Reads the key as the setup page shows it: base32 ("JBSW Y3DP …", spaces
/// and case don't matter) or a whole `otpauth://` link from the QR code.
#[cfg(feature = "totp")]
pub fn parse_key(text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim();
    let b32 = match text.strip_prefix("otpauth://") {
        Some(link) => link
            .split(['?', '&'])
            .find_map(|p| p.strip_prefix("secret="))
            .ok_or("the link has no secret=")?
            .to_string(),
        None => text.to_string(),
    };
    let key = base32(&b32).ok_or("that isn't a valid key (letters A–Z and digits 2–7)")?;
    if key.len() < 10 {
        return Err("that key is too short".into());
    }
    Ok(key)
}

#[cfg(feature = "totp")]
fn base32(text: &str) -> Option<Vec<u8>> {
    let (mut bits, mut n, mut out) = (0u64, 0u32, Vec::new());
    for c in text.chars().filter(|c| !c.is_whitespace() && *c != '-' && *c != '=') {
        let v = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u64 - 'A' as u64,
            c @ '2'..='7' => c as u64 - '2' as u64 + 26,
            _ => return None,
        };
        bits = bits << 5 | v;
        n += 5;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    Some(out)
}

/// The code for a point in time (seconds since 1970).
#[cfg(feature = "totp")]
pub fn code_at(key: &[u8], unix: u64) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("any key length works");
    mac.update(&(unix / STEP).to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let at = (hash[19] & 0xf) as usize;
    let n = u32::from_be_bytes([hash[at] & 0x7f, hash[at + 1], hash[at + 2], hash[at + 3]]);
    format!("{:06}", n % 1_000_000)
}

#[cfg(feature = "totp")]
fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The current code and how many seconds it is still valid, if a key is set up.
#[cfg(feature = "totp")]
pub fn current() -> Option<(String, u64)> {
    let key = secret::load_secret(SECRET_NAME)?;
    let t = now();
    Some((code_at(&key, t), STEP - t % STEP))
}

#[cfg(not(feature = "totp"))]
pub fn current() -> Option<(String, u64)> {
    None
}

/// Stores a key (as `parse_key` reads it). Returns the current code, so the
/// setup page can be confirmed with it.
#[cfg(feature = "totp")]
pub fn save(text: &str) -> Result<String, String> {
    let key = parse_key(text)?;
    secret::save_secret(SECRET_NAME, &key).map_err(|e| e.to_string())?;
    Ok(code_at(&key, now()))
}

#[cfg(not(feature = "totp"))]
pub fn save(_text: &str) -> Result<String, String> {
    Err(NOT_BUILT.into())
}

/// Forgets the key (`:totp forget`, `--logout-a`). Whether there was one.
pub fn forget() -> bool {
    let had = secret::load_secret(SECRET_NAME).is_some();
    secret::remove_secret(SECRET_NAME);
    had
}

#[cfg(all(test, feature = "totp"))]
mod tests {
    use super::parse_key;

    #[test]
    fn matches_rfc_6238() {
        // The RFC's SHA-1 key and times (its 8-digit codes, last six digits).
        use super::code_at;
        let key = b"12345678901234567890";
        assert_eq!(code_at(key, 59), "287082");
        assert_eq!(code_at(key, 1_111_111_109), "081804");
        assert_eq!(code_at(key, 2_000_000_000), "279037");
    }

    #[test]
    fn reads_keys_as_shown() {
        // "12345678901234567890" in base32, as setup pages show it.
        let want = b"12345678901234567890".to_vec();
        assert_eq!(parse_key("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap(), want);
        assert_eq!(parse_key("gezd gnbv gy3t qojq gezd gnbv gy3t qojq").unwrap(), want);
        assert_eq!(parse_key("otpauth://totp/UR:me?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&issuer=UR").unwrap(), want);
        assert!(parse_key("not a key!").is_err());
        assert!(parse_key("GEZD").is_err());
    }
}
