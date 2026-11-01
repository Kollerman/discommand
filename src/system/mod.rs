//! Talking to the computer itself: where data is kept and how secrets are
//! protected (`secret`), the check for new releases (`update`), the file
//! dialog (`pick`) and opening links and files with other programs (`open`).

pub mod open;
pub mod pick;
pub mod secret;
pub mod update;
