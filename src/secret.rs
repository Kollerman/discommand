//! Protection of secrets at rest.
//!
//! - Windows: the Discord token is encrypted with DPAPI (bound to the Windows
//!   user account), and the data and cache folders are marked for EFS
//!   encryption, so the WhatsApp session database is encrypted too.
//! - Elsewhere: files are readable by the owner only (0600 / 0700).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub fn data_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("discommand"))
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("discommand")
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    io::Write::write_all(&mut opts.open(path)?, data)
}

/// Overwrites a file before deleting it (best effort; SSDs may keep copies).
pub fn wipe(path: &Path) {
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::write(path, vec![0u8; meta.len() as usize]);
    }
    let _ = fs::remove_file(path);
}

fn secret_path(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) { format!("{name}.dpapi") } else { name.to_string() };
    data_dir().map(|d| d.join(file))
}

/// Stores a secret: DPAPI-encrypted on Windows, owner-only elsewhere.
pub fn save_secret(name: &str, data: &[u8]) -> io::Result<PathBuf> {
    let path = secret_path(name).ok_or_else(|| io::Error::other("no config directory"))?;
    create_private_dir(path.parent().unwrap())?;
    #[cfg(windows)]
    let data = dpapi::protect(data).ok_or_else(|| io::Error::other("DPAPI encryption failed"))?;
    write_private(&path, &data)?;
    Ok(path)
}

pub fn load_secret(name: &str) -> Option<Vec<u8>> {
    let data = fs::read(secret_path(name)?).ok()?;
    #[cfg(windows)]
    let data = dpapi::unprotect(&data)?;
    Some(data)
}

pub fn remove_secret(name: &str) {
    if let Some(p) = secret_path(name) {
        wipe(&p);
    }
}

pub fn load_token() -> Option<String> {
    if let Some(t) = load_secret("token") {
        return String::from_utf8(t).ok().map(|t| t.trim().to_string());
    }
    // Migrate a plain-text token from older versions (Windows only; elsewhere
    // the secret file is that same plain-text file).
    let legacy = data_dir()?.join("token");
    let token = fs::read_to_string(&legacy).ok()?.trim().to_string();
    if cfg!(windows) && save_token(&token).is_ok() {
        wipe(&legacy);
    }
    Some(token)
}

pub fn save_token(token: &str) -> io::Result<PathBuf> {
    save_secret("token", token.as_bytes())
}

pub fn remove_token() {
    remove_secret("token");
    if let Some(legacy) = data_dir().map(|d| d.join("token")) {
        wipe(&legacy);
    }
}

/// Encrypts the data and cache folders with EFS where available (Windows
/// Pro/Enterprise). New files inherit it; existing files are converted
/// once. Must run while the WhatsApp helper is not using its database.
pub fn protect_dirs() {
    #[cfg(windows)]
    for dir in [data_dir(), Some(cache_dir())].into_iter().flatten() {
        if create_private_dir(&dir).is_ok() {
            efs::encrypt_tree(&dir);
        }
    }
}

#[cfg(windows)]
mod dpapi {
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    /// Extra entropy so other programs can't decrypt the blob by accident.
    const ENTROPY: &[u8] = b"discommand/discord-token/v1";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: the API returned a buffer of cbData bytes that we must free.
        unsafe {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            LocalFree(out.pbData as _);
            v
        }
    }

    pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        let entropy = blob(ENTROPY);
        // SAFETY: all pointers are valid for the duration of the call.
        let ok = unsafe {
            CryptProtectData(&blob(data), null(), &entropy, null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        (ok != 0).then(|| take(out))
    }

    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        let entropy = blob(ENTROPY);
        // SAFETY: as above.
        let ok = unsafe {
            CryptUnprotectData(&blob(data), null_mut(), &entropy, null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
        };
        (ok != 0).then(|| take(out))
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn round_trip() {
            let blob = super::protect(b"secret-token").unwrap();
            assert!(!blob.windows(12).any(|w| w == b"secret-token"));
            assert_eq!(super::unprotect(&blob).unwrap(), b"secret-token");
        }
    }
}

#[cfg(windows)]
mod efs {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Storage::FileSystem::{EncryptFileW, GetFileAttributesW, FILE_ATTRIBUTE_ENCRYPTED, INVALID_FILE_ATTRIBUTES};

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn encrypt(p: &Path) {
        let w = wide(p);
        // SAFETY: w is a NUL-terminated wide string.
        unsafe {
            let attrs = GetFileAttributesW(w.as_ptr());
            if attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_ENCRYPTED == 0 {
                // Fails harmlessly where EFS is unavailable (e.g. Windows Home).
                EncryptFileW(w.as_ptr());
            }
        }
    }

    pub fn encrypt_tree(dir: &Path) {
        encrypt(dir);
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    encrypt_tree(&p);
                } else {
                    encrypt(&p);
                }
            }
        }
    }
}
