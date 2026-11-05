//! Reading login pages: their forms and fields, filling in a user name and
//! password, and pages that forward themselves by script or refresh tag.
//! Single sign-on logins (Moodle at universities, GroupWise) walk through
//! such pages without a browser.

use std::collections::HashMap;

/// An HTML form, as far as signing in needs it.
#[derive(Debug, Default)]
pub struct Form {
    pub action: String,
    pub method: String,
    /// (type, name, value) of inputs and named buttons.
    pub fields: Vec<(String, String, String)>,
}

impl Form {
    pub fn has_password(&self) -> bool {
        self.fields.iter().any(|(t, ..)| t == "password")
    }

    /// Only hidden fields (or none): a page that forwards itself by script
    /// (SAML, session checks).
    pub fn forwards(&self) -> bool {
        self.fields.iter().all(|(t, ..)| matches!(t.as_str(), "hidden" | "submit" | "button" | "radio" | "checkbox"))
    }

    /// The fields to send: hidden ones as they are, the user name into the
    /// first text field and the password into the password field.
    pub fn filled(&self, username: &str, password: &str) -> Vec<(String, String)> {
        let mut user_done = false;
        let mut out = Vec::new();
        for (t, name, value) in &self.fields {
            if name.is_empty() {
                continue;
            }
            match t.as_str() {
                "hidden" => out.push((name.clone(), value.clone())),
                "password" => out.push((name.clone(), password.to_string())),
                "text" | "email" | "" if !user_done => {
                    user_done = true;
                    out.push((name.clone(), username.to_string()));
                }
                // Pre-selected options (e.g. how long a consent is remembered).
                "radio" | "checkbox" => out.push((name.clone(), value.clone())),
                _ => {}
            }
        }
        // One button is pressed: the one that goes on ("proceed", "accept",
        // "login"), never one that rejects or cancels.
        let buttons: Vec<&(String, String, String)> = self.fields.iter().filter(|(t, n, _)| t == "submit" && !n.is_empty()).collect();
        let says = |f: &&&(String, String, String), words: &[&str]| {
            let both = format!("{} {}", f.1, f.2).to_ascii_lowercase();
            words.iter().any(|w| both.contains(w))
        };
        let pressed = buttons
            .iter()
            .find(|f| says(f, &["proceed", "accept", "akzept", "continue", "weiter", "login", "anmelden", "submit"]))
            .or_else(|| buttons.iter().find(|f| !says(f, &["reject", "ablehn", "cancel", "abbrech", "deny"])));
        if let Some((_, name, value)) = pressed {
            out.push((name.clone(), value.clone()));
        }
        out
    }
}

/// Decodes the HTML entities found in attribute values.
pub fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let c = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            e if e.starts_with("#x") || e.starts_with("#X") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The attributes of a tag's text (`<input type="x" name='y' checked>`).
pub fn attributes(tag: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let b = tag.as_bytes();
    let mut i = tag.find(char::is_whitespace).unwrap_or(tag.len());
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let start = i;
        while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'=' && b[i] != b'>' {
            i += 1;
        }
        let name = tag[start..i].to_ascii_lowercase();
        if name.is_empty() {
            break;
        }
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let end = tag[i + 1..].find(q as char).map_or(b.len(), |e| i + 1 + e);
                value = unescape(&tag[i + 1..end]);
                i = end + 1;
            } else {
                let start = i;
                while i < b.len() && !b[i].is_ascii_whitespace() && b[i] != b'>' {
                    i += 1;
                }
                value = unescape(&tag[start..i]);
            }
        }
        out.insert(name, value);
    }
    out
}

/// The forms of a page with their inputs (and named buttons).
pub fn forms(html: &str) -> Vec<Form> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(start) = lower[pos..].find("<form").map(|i| pos + i) {
        let Some(tag_end) = tag_close(&lower, start) else { break };
        let end = lower[tag_end..].find("</form").map_or(html.len(), |i| tag_end + i);
        let attrs = attributes(&html[start + 1..tag_end]);
        let mut form = Form {
            action: attrs.get("action").cloned().unwrap_or_default(),
            method: attrs.get("method").cloned().unwrap_or_else(|| "get".into()).to_ascii_lowercase(),
            fields: Vec::new(),
        };
        let mut i = tag_end;
        while let Some(t) = lower[i..end].find('<').map(|j| i + j) {
            let Some(close) = tag_close(&lower[..end], t) else { break };
            let tag = &lower[t + 1..close];
            if tag.starts_with("input") || tag.starts_with("button") {
                let a = attributes(&html[t + 1..close]);
                let kind = a.get("type").cloned().unwrap_or_else(|| if tag.starts_with("button") { "submit".into() } else { "text".into() });
                let kind = kind.to_ascii_lowercase();
                let unchecked = matches!(kind.as_str(), "radio" | "checkbox") && !a.contains_key("checked");
                if let Some(name) = a.get("name").filter(|_| !unchecked) {
                    form.fields.push((kind, name.clone(), a.get("value").cloned().unwrap_or_default()));
                }
            }
            i = close + 1;
        }
        out.push(form);
        pos = end.max(tag_end + 1);
    }
    out
}

/// Where the tag starting at `start` (its `<`) ends: the first `>` outside
/// quotes. Values may contain `>` (a SAML or WS-Federation answer is a
/// whole XML document in a hidden field).
fn tag_close(html: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (i, c) in html[start..].char_indices() {
        match (quote, c) {
            (None, '"' | '\x27') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '>') => return Some(start + i),
            _ => {}
        }
    }
    None
}

/// Whether a page asks for a one-time code (two-factor), which can't be
/// filled in automatically.
pub fn asks_for_code(form: &Form) -> bool {
    !form.has_password()
        && form.fields.iter().any(|(t, name, _)| {
            let n = name.to_ascii_lowercase();
            matches!(t.as_str(), "text" | "number" | "tel") && ["otp", "totp", "code", "token", "pin"].iter().any(|k| n.contains(k))
        })
}

/// Where a page sends the browser on by itself without a form: a refresh
/// tag (`<meta http-equiv="refresh" content="0;url=…">`) or a script
/// (`location.href = '…'`, `window.location.replace("…")`, …).
pub fn page_redirect(page: &str) -> Option<String> {
    let lower = page.to_ascii_lowercase();
    if let Some(i) = lower.find("http-equiv=\"refresh\"").or_else(|| lower.find("http-equiv='refresh'")) {
        let tag_start = lower[..i].rfind('<')?;
        let tag_end = tag_start + lower[tag_start..].find('>')?;
        let content = attributes(&page[tag_start + 1..tag_end]).remove("content")?;
        if let Some(u) = content.to_ascii_lowercase().find("url=") {
            return Some(content[u + 4..].trim().trim_matches(['\'', '"']).to_string());
        }
    }
    let mut pos = 0;
    while let Some(i) = lower[pos..].find("location").map(|i| pos + i) {
        pos = i + 8;
        let mut rest = &page[pos..];
        for prefix in [".href", ".replace", ".assign"] {
            if rest.to_ascii_lowercase().starts_with(prefix) {
                rest = &rest[prefix.len()..];
            }
        }
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=').or_else(|| rest.strip_prefix('(')) else { continue };
        let rest = rest.trim_start();
        let Some(q) = rest.chars().next().filter(|c| *c == '\'' || *c == '"') else { continue };
        let Some(end) = rest[1..].find(q) else { continue };
        let target = unescape(&rest[1..1 + end]).replace("\\/", "/");
        if !target.is_empty() && !target.starts_with('#') && !target.to_ascii_lowercase().starts_with("javascript:") {
            return Some(target);
        }
    }
    None
}


#[cfg(test)]
mod tests {
    use super::forms;

    #[test]
    fn keeps_xml_in_hidden_fields() {
        // A WS-Federation answer: a whole XML document as a value, with `>` in it.
        let page = r#"<form method="POST" action="https://login.microsoftonline.com/login.srf"><input type="hidden" name="wa" value="wsignin1.0"><input type="hidden" name="wresult" value="<t:RequestSecurityTokenResponse xmlns:t='x'><a>1</a></t:RequestSecurityTokenResponse>"><input type="hidden" name="wctx" value="c=1&amp;d=2"></form>"#;
        let f = &forms(page)[0];
        let field = |n: &str| f.fields.iter().find(|(_, name, _)| name == n).map(|(_, _, v)| v.as_str());
        assert_eq!(field("wresult"), Some("<t:RequestSecurityTokenResponse xmlns:t='x'><a>1</a></t:RequestSecurityTokenResponse>"));
        assert_eq!(field("wctx"), Some("c=1&d=2"));
        assert_eq!(f.fields.len(), 3);
    }
}
