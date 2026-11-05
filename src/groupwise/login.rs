//! Signing in to GroupWise Web without a browser, the way the website does:
//!
//! 1. The university's login server (NetIQ Access Manager at my.ur.de) is
//!    asked for a login code for GroupWise Web (OpenID Connect with PKCE).
//! 2. It hands over to Microsoft, which asks for the account
//!    (`<id>@ads.uni-regensburg.de`) and sends it back to my.ur.de, where
//!    the UR user name and password are entered (the same as for Moodle).
//! 3. Microsoft asks for the second factor: the code of discommand's own
//!    authenticator (`:totp`), or a push to the phone app as a fallback.
//! 4. "Stay signed in?" yes; back to my.ur.de, which ends at
//!    `gw.uni-regensburg.de/login?code=…`. The code is swapped for a token,
//!    and the token logs in to GroupWise (see `soap`).
//!
//! Every page is read like a browser would, but nothing runs: forms are filled
//! from their fields, Microsoft's pages from the settings they carry
//! (`$Config`). `DISCOMMAND_GW_LOG=<file>` writes a trace of the steps.

use std::sync::Mutex;

use base64::Engine;
use reqwest::cookie::Jar;
use reqwest::redirect::Policy;
use reqwest::Url;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::system::login_pages::{forms, page_redirect};

const AUTHORIZE: &str = "https://my.ur.de:8443/nidp/oauth/nam/authz";
const TOKEN: &str = "https://my.ur.de:8443/nidp/oauth/nam/token";
/// GroupWise Web's own registration at the login server (public: it's in the
/// website's settings, `assets/config/nam/nam.json`).
const CLIENT_ID: &str = "6cdb2fc1-9093-4fe1-bf62-29c3239f78de";
const REDIRECT: &str = "https://gw.uni-regensburg.de/login";
/// Microsoft accounts of the university end in this.
const MS_DOMAIN: &str = "ads.uni-regensburg.de";
/// More steps than this and something loops.
const MAX_STEPS: usize = 40;

/// What the login gives: the token GroupWise accepts, and one to get a new
/// token without signing in again, if the login server gives one.
#[derive(Debug)]
pub struct Tokens {
    pub access: String,
    pub refresh: Option<String>,
}

/// Signs in with the UR user name (`abc12345`) and password. `say` hears
/// what's going on (and what to do on the phone, for a push).
pub async fn login(user: &str, password: &str, say: &(dyn Fn(String) + Sync)) -> Result<Tokens, String> {
    let trace = Trace::open();
    let result = walk(user, password, say, &trace).await;
    if let Err(e) = &result {
        trace.note(format!("failed: {e}"));
    }
    trace.save();
    result
}

async fn walk(user: &str, password: &str, say: &(dyn Fn(String) + Sync), trace: &Trace) -> Result<Tokens, String> {
    let user = user.trim();
    let ms_user = if user.contains('@') { user.to_string() } else { format!("{user}@{MS_DOMAIN}") };
    let uid = user.split('@').next().unwrap_or(user);
    let http = reqwest::Client::builder()
        .cookie_provider(std::sync::Arc::new(Jar::default()))
        .redirect(Policy::none())
        .user_agent(crate::discord::api::user_agent())
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;

    let verifier = random(32);
    let challenge = b64url(&Sha256::digest(verifier.as_bytes()));
    let state = random(16);
    let mut start = Url::parse(AUTHORIZE).map_err(|e| e.to_string())?;
    start
        .query_pairs_mut()
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", REDIRECT)
        .append_pair("response_type", "code")
        .append_pair("scope", "openid profile")
        .append_pair("state", &state)
        .append_pair("nonce", &random(16))
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("login_hint", &ms_user);

    say("signing in at my.ur.de…".into());
    let mut next = Step::Get(start);
    let mut password_sent = false;
    for _ in 0..MAX_STEPS {
        let (url, resp) = next.send(&http, trace).await?;
        if resp.status().is_redirection() {
            let to = resp.headers().get("location").and_then(|l| l.to_str().ok()).unwrap_or_default();
            let to = url.join(to).map_err(|e| e.to_string())?;
            if to.as_str().starts_with(REDIRECT) {
                return finish(&http, &to, &state, &verifier, trace).await;
            }
            next = Step::Get(to);
            continue;
        }
        let page = resp.text().await.map_err(|e| e.to_string())?;
        next = match ms_config(&page) {
            Some(config) => microsoft(&http, &url, &config, &page, &ms_user, say, trace).await?,
            None => {
                let sent_before = password_sent;
                let step = university(&url, &page, uid, password, &mut password_sent, trace)?;
                if password_sent && !sent_before {
                    say("my.ur.de: sending the UR user name and password…".into());
                }
                step
            }
        };
    }
    Err("the login went in circles (more than 40 pages)".into())
}

/// A request to make next.
enum Step {
    Get(Url),
    Post(Url, Vec<(String, String)>),
}

impl Step {
    async fn send(self, http: &reqwest::Client, trace: &Trace) -> Result<(Url, reqwest::Response), String> {
        let method = if matches!(self, Step::Post(..)) { "POST" } else { "GET" };
        let (url, req) = match self {
            Step::Get(u) => (u.clone(), http.get(u)),
            Step::Post(u, fields) => {
                trace.note(format!("  fields: {}", fields.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", ")));
                (u.clone(), http.post(u).form(&fields))
            }
        };
        let resp = req.send().await.map_err(|e| format!("couldn't reach {}: {e}", url.host_str().unwrap_or("")))?;
        trace.note(format!("{method} {}{} → {}", url.host_str().unwrap_or(""), url.path(), resp.status().as_u16()));
        Ok((url, resp))
    }
}

/// A page of the university's login server (or any page without
/// Microsoft's settings): the password form is filled in, pages that only
/// forward are forwarded.
fn university(url: &Url, page: &str, uid: &str, password: &str, password_sent: &mut bool, trace: &Trace) -> Result<Step, String> {
    let all = forms(page);
    for f in &all {
        trace.note(format!("  form {} → {}: {}", f.method, f.action.split('?').next().unwrap_or(""), f.fields.iter().map(|(t, n, _)| format!("{n}:{t}")).collect::<Vec<_>>().join(" ")));
    }
    if let Some(f) = all.iter().find(|f| f.has_password()) {
        if *password_sent {
            return Err("the university login didn't accept the user name or password".into());
        }
        *password_sent = true;
        let action = url.join(&f.action).map_err(|e| e.to_string())?;
        return Ok(Step::Post(action, f.filled(uid, password)));
    }
    // Forms that send themselves on: with hidden fields only, or empty and
    // submitted by script (`document.forms[0].submit()`).
    let submits_itself = page.contains(".submit()");
    if let Some(f) = all.iter().find(|f| f.forwards() && (!f.fields.is_empty() || submits_itself)) {
        let action = url.join(&f.action).map_err(|e| e.to_string())?;
        let fields = f.filled("", "");
        return Ok(if f.method == "post" {
            Step::Post(action, fields)
        } else {
            let mut to = action;
            to.query_pairs_mut().extend_pairs(&fields);
            Step::Get(to)
        });
    }
    if let Some(to) = page_redirect(page) {
        return Ok(Step::Get(url.join(&to).map_err(|e| e.to_string())?));
    }
    Err(format!("stuck on a login page ({}{}); run with DISCOMMAND_GW_LOG=<file> to see the steps", url.host_str().unwrap_or(""), url.path()))
}

/// A page of Microsoft's login, by what it asks for.
async fn microsoft(
    http: &reqwest::Client,
    url: &Url,
    config: &Value,
    page: &str,
    ms_user: &str,
    say: &(dyn Fn(String) + Sync),
    trace: &Trace,
) -> Result<Step, String> {
    let s = |key: &str| config[key].as_str().unwrap_or_default().to_string();
    let pgid = s("pgid");
    trace.note(format!("  microsoft page {pgid}"));
    if let Some(e) = config["strServiceExceptionMessage"].as_str().filter(|e| !e.is_empty()) {
        return Err(format!("Microsoft: {e}"));
    }
    let post_url = || url.join(&s("urlPost")).map_err(|e| e.to_string());
    match pgid.as_str() {
        // Which account: Microsoft is asked where it signs in (the
        // university's server, for a federated account).
        "ConvergedSignIn" => {
            say("Microsoft: sending the account to the university login…".into());
            let body = json!({
                "username": ms_user,
                "isOtherIdpSupported": true,
                "checkPhones": false,
                "isRemoteNGCSupported": true,
                "isCookieBannerShown": false,
                "isFidoSupported": false,
                "originalRequest": s("sCtx"),
                "flowToken": s("sFT"),
            });
            let answer = ms_api(http, url, config, "urlGetCredentialType", &body).await?;
            let fed = answer["Credentials"]["FederationRedirectUrl"].as_str().ok_or_else(|| {
                if answer["IfExistsResult"].as_i64() == Some(1) {
                    format!("Microsoft doesn't know the account {ms_user}")
                } else {
                    "Microsoft didn't send the account to the university login".to_string()
                }
            })?;
            Ok(Step::Get(url.join(fed).map_err(|e| e.to_string())?))
        }
        // The second factor.
        "ConvergedTFA" => second_factor(http, url, config, ms_user, say, trace).await.and_then(|fields| Ok(Step::Post(post_url()?, fields))),
        // "Stay signed in?" yes.
        "KmsiInterrupt" => Ok(Step::Post(
            post_url()?,
            vec![
                ("LoginOptions".into(), "1".into()),
                ("type".into(), "28".into()),
                ("ctx".into(), s("sCtx")),
                ("hpgrequestid".into(), s("sessionId")),
                ("flowToken".into(), s("sFT")),
                ("canary".into(), s("canary")),
                ("i19".into(), "1000".into()),
            ],
        )),
        "ConvergedProofUpRedirect" | "ConvergedProofUp" => {
            Err("Microsoft wants more security info set up for this account: sign in once in a browser".into())
        }
        // Pages that send themselves on: their fields are in the settings.
        _ if config["oPostParams"].is_object() && !s("urlPost").is_empty() => {
            let fields = config["oPostParams"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect();
            Ok(Step::Post(post_url()?, fields))
        }
        // Pages that only forward (with Microsoft's settings in them too).
        _ => {
            let mut sent = false;
            university(url, page, "", "", &mut sent, trace).map_err(|_| format!("Microsoft asked for something unexpected (page \"{pgid}\")"))
        }
    }
}

/// Microsoft's second-factor page: the authenticator code (TOTP), or else a
/// push to the phone. The fields for the page's form afterwards.
async fn second_factor(
    http: &reqwest::Client,
    url: &Url,
    config: &Value,
    ms_user: &str,
    say: &(dyn Fn(String) + Sync),
    trace: &Trace,
) -> Result<Vec<(String, String)>, String> {
    let s = |key: &str| config[key].as_str().unwrap_or_default().to_string();
    let proofs: Vec<String> = config["arrUserProofs"].as_array().into_iter().flatten().filter_map(|p| p["authMethodId"].as_str().map(String::from)).collect();
    trace.note(format!("  second factor offered: {}", proofs.join(", ")));
    let totp = crate::system::totp::current().is_some();
    let method = proofs
        .iter()
        .find(|m| totp && m.contains("OTP") && !m.contains("SMS"))
        .or_else(|| proofs.iter().find(|m| m.as_str() == "PhoneAppNotification"))
        .cloned()
        .ok_or_else(|| {
            if totp {
                format!("Microsoft offered no authenticator code for this account ({})", proofs.join(", "))
            } else {
                "Microsoft wants a second factor: set up discommand's authenticator first (:totp setup KEY)".to_string()
            }
        })?;
    let begin = ms_api(http, url, config, "urlBeginAuth", &json!({ "AuthMethodId": method, "Method": "BeginAuth", "ctx": s("sCtx"), "flowToken": s("sFT") })).await?;
    if begin["Success"].as_bool() != Some(true) {
        return Err(format!("Microsoft didn't start the second factor ({})", begin["Message"].as_str().unwrap_or("no reason given")));
    }
    let field = |v: &Value, k: &str| v[k].as_str().unwrap_or_default().to_string();
    let (mut ctx, mut flow, session) = (field(&begin, "Ctx"), field(&begin, "FlowToken"), field(&begin, "SessionId"));
    let mut code = String::new();
    let end = if method == "PhoneAppNotification" {
        let number = begin["Entropy"].as_i64().filter(|n| *n > 0).map(|n| format!(" (enter {n})")).unwrap_or_default();
        say(format!("approve the sign-in in the Microsoft Authenticator app on your phone{number}…"));
        let mut answer = Value::Null;
        for poll in 1..=30 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let body = json!({ "AuthMethodId": method, "Method": "EndAuth", "SessionId": session, "FlowToken": flow, "Ctx": ctx, "PollCount": poll });
            answer = ms_api(http, url, config, "urlEndAuth", &body).await?;
            if answer["Success"].as_bool() == Some(true) || answer["ResultValue"].as_str().is_some_and(|r| r != "AuthenticationPending") {
                break;
            }
        }
        answer
    } else {
        // A code about to change is waited out, so it isn't stale on arrival.
        let (mut now, left) = crate::system::totp::current().ok_or("the authenticator key is gone")?;
        if left < 4 {
            tokio::time::sleep(std::time::Duration::from_secs(left + 1)).await;
            now = crate::system::totp::current().ok_or("the authenticator key is gone")?.0;
        }
        code = now;
        say("Microsoft: entering the authenticator code…".into());
        let body = json!({ "AuthMethodId": method, "Method": "EndAuth", "SessionId": session, "FlowToken": flow, "Ctx": ctx, "AdditionalAuthData": code, "PollCount": 1 });
        ms_api(http, url, config, "urlEndAuth", &body).await?
    };
    if end["Success"].as_bool() != Some(true) {
        return Err(match end["ResultValue"].as_str() {
            Some("InvalidOTP" | "OathCodeIncorrect") => "Microsoft didn't accept the authenticator code (is the computer's clock right?)".into(),
            Some("PhoneAppDenied") => "the sign-in was denied on the phone".into(),
            Some("AuthenticationPending") => "the sign-in wasn't approved on the phone in time".into(),
            Some(r) => format!("the second factor failed ({r})"),
            None => "the second factor failed".into(),
        });
    }
    ctx = field(&end, "Ctx");
    flow = field(&end, "FlowToken");
    Ok(vec![
        ("type".into(), "19".into()),
        ("GeneralVerify".into(), "false".into()),
        ("request".into(), ctx),
        ("mfaLastPollStart".into(), String::new()),
        ("mfaLastPollEnd".into(), String::new()),
        ("mfaAuthMethod".into(), method),
        ("otc".into(), code),
        ("login".into(), ms_user.to_string()),
        ("flowToken".into(), flow),
        ("hpgrequestid".into(), s("sessionId")),
        ("sacxt".into(), String::new()),
        ("hideSmsInMfaProofs".into(), "false".into()),
        ("canary".into(), s("canary")),
        ("i19".into(), "1000".into()),
    ])
}

/// One of the JSON calls Microsoft's login page makes (its address is in the
/// page's settings under `key`).
async fn ms_api(http: &reqwest::Client, page: &Url, config: &Value, key: &str, body: &Value) -> Result<Value, String> {
    let s = |key: &str| config[key].as_str().unwrap_or_default().to_string();
    let to = page.join(&s(key)).map_err(|_| format!("Microsoft's page has no {key}"))?;
    let mut req = http.post(to).json(body);
    for (header, key) in [("canary", "apiCanary"), ("hpgrequestid", "sessionId"), ("client-request-id", "correlationId")] {
        req = req.header(header, s(key));
    }
    for (header, key) in [("hpgid", "hpgid"), ("hpgact", "hpgact")] {
        req = req.header(header, config[key].to_string());
    }
    let resp = req.send().await.map_err(|e| format!("couldn't reach Microsoft: {e}"))?;
    resp.json().await.map_err(|e| format!("Microsoft answered strangely: {e}"))
}

/// The settings Microsoft's login pages carry (`$Config={…};`).
fn ms_config(page: &str) -> Option<Value> {
    let start = page.find("$Config=")? + "$Config=".len();
    serde_json::Deserializer::from_str(&page[start..]).into_iter::<Value>().next()?.ok()
}

/// The end: `gw.uni-regensburg.de/login?code=…` swapped for the tokens.
async fn finish(http: &reqwest::Client, at: &Url, state: &str, verifier: &str, trace: &Trace) -> Result<Tokens, String> {
    let query: std::collections::HashMap<_, _> = at.query_pairs().into_owned().collect();
    if let Some(e) = query.get("error") {
        return Err(format!("the login server refused: {}", query.get("error_description").unwrap_or(e)));
    }
    if query.get("state").map(String::as_str) != Some(state) {
        return Err("the login came back for a different request".into());
    }
    let code = query.get("code").ok_or("the login came back without a code")?;
    trace.note("got the login code, asking for the token".into());
    let form = [("grant_type", "authorization_code"), ("code", code), ("redirect_uri", REDIRECT), ("client_id", CLIENT_ID), ("code_verifier", verifier)];
    let resp = http.post(TOKEN).form(&form).send().await.map_err(|e| format!("couldn't reach my.ur.de: {e}"))?;
    let status = resp.status();
    let v: Value = resp.json().await.map_err(|e| format!("the token answer was unreadable: {e}"))?;
    trace.note(format!("token answer {}: {}", status.as_u16(), v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>().join(", ")).unwrap_or_default()));
    let access = v["access_token"].as_str().ok_or_else(|| format!("no token from my.ur.de ({})", v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("no reason given")))?;
    Ok(Tokens { access: access.to_string(), refresh: v["refresh_token"].as_str().map(String::from) })
}

fn random(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let _ = getrandom::fill(&mut buf);
    b64url(&buf)
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// The trace for `DISCOMMAND_GW_LOG`: addresses (without their query, which
/// carries codes), form field names and Microsoft's page names; never values.
struct Trace(Option<Mutex<Vec<String>>>);

impl Trace {
    fn open() -> Trace {
        Trace(std::env::var_os("DISCOMMAND_GW_LOG").map(|_| Mutex::new(Vec::new())))
    }

    fn note(&self, line: String) {
        if let Some(lines) = &self.0 {
            if let Ok(mut l) = lines.lock() {
                l.push(line);
            }
        }
    }

    fn save(&self) {
        let (Some(lines), Some(path)) = (&self.0, std::env::var_os("DISCOMMAND_GW_LOG")) else { return };
        if let Ok(l) = lines.lock() {
            let _ = std::fs::write(path, l.join("\n") + "\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ms_config;

    #[test]
    fn reads_microsofts_page_settings() {
        let page = r#"<script>//<![CDATA[
$Config={"pgid":"ConvergedTFA","sFT":"flow","arrUserProofs":[{"authMethodId":"PhoneAppOTP"}],"urlPost":"/common/SAS/ProcessAuth"};
//]]></script>"#;
        let c = ms_config(page).unwrap();
        assert_eq!(c["pgid"], "ConvergedTFA");
        assert_eq!(c["arrUserProofs"][0]["authMethodId"], "PhoneAppOTP");
        assert!(ms_config("<html>no settings</html>").is_none());
    }
}

#[cfg(test)]
mod live {
    /// Walks the real login with a made-up account: up to the university's
    /// password form, which then rejects its made-up password. Contacts the
    /// real servers, so only on request: `cargo test live -- --ignored`
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn reaches_microsoft() {
        let say = |s: String| eprintln!("{s}");
        let result = super::login("zz-nobody-discommand-test", "x", &say).await;
        eprintln!("{result:?}");
    }
}
