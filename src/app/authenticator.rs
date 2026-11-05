//! The authenticator tab: the current 6-digit code for logins with a second
//! factor (see `system::totp`). It has no place in the Shift+Tab / `gt`
//! cycle; only `:totp` opens it, and those keys (or `q`) leave it.

use super::*;
use crate::system::totp;

impl App {
    /// `:totp` opens the tab; `:totp setup KEY` stores the key from a login's
    /// "other authenticator app" setup, `:totp forget` removes it.
    pub(super) fn totp_command(&mut self, arg: &str) {
        if !totp::BUILT {
            self.status = totp::NOT_BUILT.into();
            return;
        }
        let (sub, rest) = arg.split_once(' ').unwrap_or((arg, ""));
        match sub {
            "setup" if rest.trim().is_empty() => {
                self.status = "usage: :totp setup KEY (the key shown under \"can't scan the image?\")".into();
            }
            "setup" => match totp::save(rest) {
                Ok(_) => {
                    self.status = "authenticator set up: enter the code on the setup page to confirm".into();
                    self.totp = true;
                }
                Err(e) => self.status = e,
            },
            "forget" => {
                self.status = if totp::forget() { "authenticator key deleted".into() } else { "no authenticator key was set up".into() };
            }
            "" => {
                self.totp = true;
                self.status.clear();
            }
            _ => self.status = "usage: :totp · :totp setup KEY · :totp forget".into(),
        }
    }

    /// Keys in the authenticator tab. Shift+Tab and `gt` leave it for the
    /// other tabs as usual.
    pub(super) fn totp_key(&mut self, k: KeyEvent) {
        let leaving = match k.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.totp = false;
                return;
            }
            KeyCode::BackTab => true,
            KeyCode::Char('t' | 'T') => self.pending == Some('g'),
            // `:` for commands, `g` starting `gt`.
            KeyCode::Char(':' | 'ö' | 'Ö' | 'g') => false,
            _ => return,
        };
        if leaving {
            self.totp = false;
        }
        self.normal_key(k);
    }
}
