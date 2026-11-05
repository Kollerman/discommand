//! Talking to the computer itself: where data is kept and how secrets are
//! protected (`secret`), the check for new releases (`update`), the file
//! dialog (`pick`), opening links and files with other programs (`open`), the
//! authenticator codes for logins with a second factor (`totp`), and walking
//! through single sign-on login pages (`login_pages`).

pub mod login_pages;
pub mod open;
pub mod pick;
pub mod secret;
pub mod totp;
pub mod update;
