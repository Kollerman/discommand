//! Minimal Discord REST client.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::{Client, Method};
use serde_json::{json, Value};

use crate::model::{Message, User};

const BASE: &str = "https://discord.com/api/v10";
const CHROME_VERSION: &str = "130.0.0.0";

pub fn os_name() -> &'static str {
    if cfg!(windows) {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "Mac OS X"
    } else {
        "Linux"
    }
}

pub fn user_agent() -> String {
    let platform = if cfg!(windows) {
        "Windows NT 10.0; Win64; x64"
    } else if cfg!(target_os = "macos") {
        "Macintosh; Intel Mac OS X 10_15_7"
    } else {
        "X11; Linux x86_64"
    };
    format!(
        "Mozilla/5.0 ({platform}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{CHROME_VERSION} Safari/537.36"
    )
}

/// Client fingerprint sent in the gateway IDENTIFY and as X-Super-Properties.
pub fn super_properties() -> Value {
    json!({
        "os": os_name(),
        "browser": "Chrome",
        "device": "",
        "system_locale": "en-US",
        "browser_user_agent": user_agent(),
        "browser_version": CHROME_VERSION,
        "os_version": "",
        "referrer": "",
        "referring_domain": "",
        "referrer_current": "",
        "referring_domain_current": "",
        "release_channel": "stable",
        "client_event_source": null,
    })
}

fn nonce() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    (ms.saturating_sub(1_420_070_400_000) << 22).to_string()
}

#[derive(Clone)]
pub struct Api {
    http: Client,
}

impl Api {
    pub fn new(token: &str) -> Result<Self, String> {
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(token).map_err(|_| "token contains invalid characters")?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        let props = base64::engine::general_purpose::STANDARD.encode(super_properties().to_string());
        headers.insert("X-Super-Properties", HeaderValue::from_str(&props).unwrap());
        headers.insert("X-Discord-Locale", HeaderValue::from_static("en-US"));
        let http = Client::builder()
            .user_agent(user_agent())
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self { http })
    }

    async fn request(&self, method: Method, path: &str, body: Option<&Value>) -> Result<String, String> {
        for _ in 0..3 {
            let mut req = self.http.request(method.clone(), format!("{BASE}{path}"));
            if let Some(b) = body {
                req = req.json(b);
            } else if method == Method::PUT {
                // Discord rejects a PUT without a Content-Length.
                req = req.body(Vec::<u8>::new());
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            if status.as_u16() == 429 {
                let v: Value = resp.json().await.unwrap_or_default();
                let secs = v["retry_after"].as_f64().unwrap_or(1.0).clamp(0.1, 30.0);
                tokio::time::sleep(Duration::from_secs_f64(secs)).await;
                continue;
            }
            let text = resp.text().await.map_err(|e| e.to_string())?;
            if status.as_u16() == 401 {
                return Err("401 unauthorized: token is invalid or expired".into());
            }
            if !status.is_success() {
                let msg = serde_json::from_str::<Value>(&text)
                    .ok()
                    .and_then(|v| v["message"].as_str().map(String::from))
                    .unwrap_or(text);
                return Err(format!("{}: {}", status.as_u16(), msg));
            }
            return Ok(text);
        }
        Err("rate limited, try again later".into())
    }

    async fn request_json<T: serde::de::DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<T, String> {
        let text = self.request(method, path, body).await?;
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    pub async fn me(&self) -> Result<User, String> {
        self.request_json(Method::GET, "/users/@me", None).await
    }

    pub async fn messages(&self, channel: &str, before: Option<&str>) -> Result<Vec<Message>, String> {
        let mut path = format!("/channels/{channel}/messages?limit=50");
        if let Some(b) = before {
            path.push_str("&before=");
            path.push_str(b);
        }
        self.request_json(Method::GET, &path, None).await
    }

    pub async fn send(&self, channel: &str, content: &str, reply: Option<(&str, bool)>) -> Result<Message, String> {
        let mut body = json!({ "content": content, "nonce": nonce(), "tts": false });
        if let Some((id, ping)) = reply {
            body["message_reference"] = json!({ "message_id": id, "channel_id": channel });
            body["allowed_mentions"] = json!({ "parse": ["users", "roles", "everyone"], "replied_user": ping });
        }
        self.request_json(Method::POST, &format!("/channels/{channel}/messages"), Some(&body))
            .await
    }

    pub async fn edit(&self, channel: &str, id: &str, content: &str) -> Result<Message, String> {
        let body = json!({ "content": content });
        self.request_json(Method::PATCH, &format!("/channels/{channel}/messages/{id}"), Some(&body))
            .await
    }

    pub async fn delete(&self, channel: &str, id: &str) -> Result<(), String> {
        self.request(Method::DELETE, &format!("/channels/{channel}/messages/{id}"), None)
            .await
            .map(|_| ())
    }

    /// Adds (or removes) our reaction with a unicode emoji.
    pub async fn react(&self, channel: &str, id: &str, emoji: &str, add: bool) -> Result<(), String> {
        let encoded: String = emoji.bytes().map(|b| format!("%{b:02X}")).collect();
        let path = format!("/channels/{channel}/messages/{id}/reactions/{encoded}/@me");
        self.request(if add { Method::PUT } else { Method::DELETE }, &path, None).await.map(|_| ())
    }

    /// Ends this login on Discord's side: the token stops working.
    pub async fn logout(&self) -> Result<(), String> {
        let body = json!({ "provider": null, "voip_provider": null });
        self.request(Method::POST, "/auth/logout", Some(&body)).await.map(|_| ())
    }

    pub async fn ack(&self, channel: &str, id: &str) -> Result<(), String> {
        let body = json!({ "token": null });
        self.request(Method::POST, &format!("/channels/{channel}/messages/{id}/ack"), Some(&body))
            .await
            .map(|_| ())
    }
}
