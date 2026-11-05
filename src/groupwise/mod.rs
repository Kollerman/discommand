//! GroupWise at the University of Regensburg (gw.uni-regensburg.de): signing
//! in through the university's login and Microsoft's second factor (`login`),
//! and GroupWise's SOAP interface behind it (`soap`). So far only the login;
//! `discommand --groupwise-login` tries it.
//!
//! The UR user name and password are stored encrypted like the Discord token,
//! so the login can be repeated whenever the session ends; the second factor
//! comes from discommand's authenticator (`:totp`).

pub mod login;
pub mod soap;

use serde::{Deserialize, Serialize};

use crate::system::secret;

const SECRET_NAME: &str = "groupwise";

#[derive(Serialize, Deserialize)]
pub struct Creds {
    /// The UR user name (`abc12345`).
    pub username: String,
    pub password: String,
}

impl Creds {
    pub fn load() -> Option<Creds> {
        serde_json::from_slice(&secret::load_secret(SECRET_NAME)?).ok()
    }

    fn save(&self) -> Result<(), String> {
        let data = serde_json::to_vec(self).map_err(|e| e.to_string())?;
        secret::save_secret(SECRET_NAME, &data).map(|_| ()).map_err(|e| e.to_string())
    }

    pub fn remove() {
        secret::remove_secret(SECRET_NAME);
    }
}

/// The UR login stored for Moodle, if Moodle is connected through it.
fn moodle_login() -> Option<(String, String)> {
    let m = crate::planner::moodle::Creds::load()?;
    (m.site.contains("uni-regensburg.de") && !m.username.is_empty() && !m.password.is_empty()).then_some((m.username, m.password))
}

/// `--groupwise-login`: asks for the UR login (or takes Moodle's), signs in
/// all the way and tells how far it got.
pub async fn login_interactive() -> Result<(), String> {
    let creds = match Creds::load() {
        Some(c) => {
            println!("Using the stored GroupWise login ({}).", c.username);
            c
        }
        None => {
            let reuse = moodle_login().filter(|(user, _)| ask(&format!("Use the university login stored for Moodle ({user})? [Y/n] ")));
            let (username, password) = match reuse {
                Some(login) => login,
                None => {
                    print!("UR user name (abc12345): ");
                    let username = read_line()?;
                    let password = rpassword::prompt_password("Password (input hidden): ").map_err(|e| e.to_string())?;
                    (username, password)
                }
            };
            Creds { username, password }
        }
    };
    if crate::system::totp::current().is_none() {
        println!("Note: no authenticator key is set up (:totp setup KEY in discommand); if Microsoft asks for a code,");
        println!("only an approval in the Microsoft Authenticator app on your phone can be used.");
    }
    let say = |s: String| println!("  {s}");
    let tokens = login::login(&creds.username, &creds.password, &say).await?;
    println!("  signed in at my.ur.de{}", if tokens.refresh.is_some() { " (with a token to stay signed in)" } else { "" });
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
    let session = soap::login(&http, &creds.username, &tokens.access).await?;
    creds.save()?;
    println!("Logged in to GroupWise as {}. The login is stored encrypted.", session.name);
    // Only a try: the session isn't needed any longer.
    let _ = soap::logout(&http, &session).await;
    Ok(())
}

fn ask(question: &str) -> bool {
    print!("{question}");
    matches!(read_line().as_deref().map(str::to_ascii_lowercase).as_deref(), Ok("" | "y" | "yes" | "j" | "ja"))
}

fn read_line() -> Result<String, String> {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(line.trim().to_string())
}
