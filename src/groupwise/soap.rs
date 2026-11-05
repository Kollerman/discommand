//! GroupWise's SOAP interface, as GroupWise Web uses it through the
//! university's proxy (`/soap`, with the post office named in a header).
//! Logging in with the login server's token, and `Client`: a session that
//! signs in again by itself when it ends, for the calls the mail tab makes.

use base64::Engine;

const URL: &str = "https://gw.uni-regensburg.de/soap";
/// The post office the proxy forwards to ("default" until it names another).
const DEFAULT_POA: &str = "default";
/// The status asking to log in at another post office.
const REDIRECT: &str = "59923";

/// A logged-in GroupWise session.
pub struct Session {
    pub id: String,
    /// The user's name as GroupWise shows it.
    pub name: String,
    /// The user's mail address.
    pub email: String,
    /// The post office the proxy forwards this session to.
    pub poa: String,
}

fn envelope(body: &str, session: Option<&str>) -> String {
    let header = session.map(|s| format!("<SOAP-ENV:Header><types:session>{}</types:session></SOAP-ENV:Header>", escape(s))).unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><SOAP-ENV:Envelope xmlns:SOAP-ENV="http://schemas.xmlsoap.org/soap/envelope/" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:types="http://schemas.novell.com/2005/01/GroupWise/types" xmlns:methods="http://schemas.novell.com/2005/01/GroupWise/methods">{header}<SOAP-ENV:Body>{body}</SOAP-ENV:Body></SOAP-ENV:Envelope>"#
    )
}

/// Sends one request (`action`: e.g. "loginRequest"); the answer's XML.
pub async fn call(http: &reqwest::Client, action: &str, body: &str, session: Option<&str>, poa: &str) -> Result<String, String> {
    let resp = http
        .post(URL)
        .header("Content-Type", "text/xml")
        .header("SOAPAction", action)
        .header("X-POA-Address", base64::engine::general_purpose::STANDARD.encode(poa))
        .header("X-User-Agent", "discommand")
        .body(envelope(body, session))
        .send()
        .await
        .map_err(|e| format!("couldn't reach GroupWise: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GroupWise answered HTTP {}", resp.status().as_u16()));
    }
    resp.text().await.map_err(|e| e.to_string())
}

/// Logs in with the login server's token (as GroupWise Web does after the
/// university login). Follows the proxy to another post office if told to.
pub async fn login(http: &reqwest::Client, user: &str, token: &str) -> Result<Session, String> {
    let body = format!(
        r#"<methods:loginRequest><auth xsi:type="types:NAMAuthentication"><types:username>{}</types:username><types:NAMtoken>{}</types:NAMtoken></auth><language>en</language><version>1.15</version><application>discommand</application><userid>1</userid><system>1</system></methods:loginRequest>"#,
        escape(user),
        escape(token)
    );
    let mut poa = DEFAULT_POA.to_string();
    for _ in 0..3 {
        let xml = call(http, "loginRequest", &body, None, &poa).await?;
        // This user's mailbox is on another post office (59923): the proxy
        // forwards there when it's named in the header.
        if text(&xml, "code").as_deref() == Some(REDIRECT) {
            let (Some(ip), Some(port)) = (text(&xml, "ipAddress"), text(&xml, "port")) else {
                return Err("GroupWise sent the login to another post office without saying which".into());
            };
            poa = format!("{ip}:{port}");
            continue;
        }
        check(&xml)?;
        let id = text(&xml, "session").ok_or("GroupWise sent no session")?;
        let name = text(&xml, "name").unwrap_or_else(|| user.to_string());
        let email = text(&xml, "email").unwrap_or_default();
        return Ok(Session { id, name, email, poa });
    }
    Err("GroupWise kept sending the login elsewhere".into())
}

/// Ends a session.
pub async fn logout(http: &reqwest::Client, session: &Session) -> Result<(), String> {
    let xml = call(http, "logoutRequest", "<methods:logoutRequest/>", Some(&session.id), &session.poa).await?;
    check(&xml)
}

/// The answer's status: 0 is fine.
fn check(xml: &str) -> Result<(), String> {
    match text(xml, "code").as_deref() {
        None | Some("0") => Ok(()),
        Some("53505") => Err("GroupWise didn't accept the login (53505)".into()),
        Some(code) => Err(match text(xml, "description") {
            Some(d) => format!("GroupWise error {code}: {d}"),
            None => format!("GroupWise error {code}"),
        }),
    }
}

/// The text of the first element with this local name (any namespace prefix).
fn text(xml: &str, name: &str) -> Option<String> {
    let mut rest = xml;
    while let Some(i) = rest.find('<') {
        rest = &rest[i + 1..];
        let end = rest.find('>')?;
        let tag = &rest[..end];
        let local = tag.split_whitespace().next().unwrap_or("").rsplit(':').next().unwrap_or("");
        if local == name && !tag.starts_with('/') && !tag.ends_with('/') {
            let body = &rest[end + 1..];
            let close = body.find("</")?;
            return Some(unescape(&body[..close]));
        }
    }
    None
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn unescape(s: &str) -> String {
    crate::system::login_pages::unescape(s)
}

#[cfg(test)]
mod tests {
    use super::{check, text};

    #[test]
    fn reads_answers() {
        // As the server answers a wrong login.
        let bad = r#"<s:Envelope xmlns:s="x"><s:Header/><s:Body><gwm:loginResponse><gwm:status><gwt:code>53505</gwt:code></gwm:status></gwm:loginResponse></s:Body></s:Envelope>"#;
        assert!(check(bad).is_err());
        let ok = r#"<gwm:loginResponse><gwm:session>abc&amp;d</gwm:session><gwm:userinfo><gwt:name>Ann Example</gwt:name></gwm:userinfo><gwm:status><gwt:code>0</gwt:code></gwm:status></gwm:loginResponse>"#;
        assert!(check(ok).is_ok());
        assert_eq!(text(ok, "session").as_deref(), Some("abc&d"));
        assert_eq!(text(ok, "name").as_deref(), Some("Ann Example"));
    }

    #[test]
    fn reads_the_post_office_to_go_to() {
        let redirect = r#"<gwm:loginResponse><gwm:redirectToHost><gwt:ipAddress>gw3.example.de</gwt:ipAddress><gwt:port>7191</gwt:port></gwm:redirectToHost><gwm:status><gwt:code>59923</gwt:code></gwm:status></gwm:loginResponse>"#;
        assert_eq!(text(redirect, "code").as_deref(), Some(super::REDIRECT));
        assert_eq!((text(redirect, "ipAddress").unwrap(), text(redirect, "port").unwrap()), ("gw3.example.de".into(), "7191".into()));
    }
}
