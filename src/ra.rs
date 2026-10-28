//! Discord QR-code login ("remote auth"), as in the desktop app's
//! "Log in with QR Code": the phone app scans the code and confirms, and the
//! token arrives encrypted with a key generated here. No password, captcha
//! or 2FA prompt in the terminal.

use std::time::Duration;

use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use rsa::pkcs8::EncodePublicKey;
use rsa::rand_core::OsRng;
use rsa::{Oaep, RsaPrivateKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use crate::api::user_agent;
use crate::qr;

const GATEWAY: &str = "wss://remote-auth-gateway.discord.gg/?v=2";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn decrypt(key: &RsaPrivateKey, b64: &str) -> Result<Vec<u8>, String> {
    let data = B64.decode(b64).map_err(|e| e.to_string())?;
    key.decrypt(Oaep::new::<Sha256>(), &data).map_err(|e| format!("decrypt: {e}"))
}

enum Outcome {
    Token(String),
    /// The code expired before it was scanned: show a new one.
    Expired,
}

/// Shows QR codes until one is scanned and confirmed; returns the token.
pub async fn login() -> Result<String, String> {
    println!("Preparing Discord login…");
    // Generating the key takes a moment on slow devices; do it once.
    let key = tokio::task::spawn_blocking(|| RsaPrivateKey::new(&mut OsRng, 2048))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    for _ in 0..5 {
        match attempt(&key).await? {
            Outcome::Token(t) => return Ok(t),
            Outcome::Expired => continue,
        }
    }
    Err("the QR code expired too often".into())
}

async fn attempt(key: &RsaPrivateKey) -> Result<Outcome, String> {
    let mut req = GATEWAY.into_client_request().map_err(|e| e.to_string())?;
    // The gateway only accepts connections that look like they come from discord.com.
    req.headers_mut().insert("Origin", HeaderValue::from_static("https://discord.com"));
    req.headers_mut().insert("User-Agent", HeaderValue::from_str(&user_agent()).map_err(|e| e.to_string())?);
    let (ws, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| format!("Discord login: {e}"))?;
    let (mut sink, mut stream) = ws.split();
    let send = |v: Value| WsMsg::Text(v.to_string());

    let mut heartbeat = tokio::time::interval(Duration::from_secs(40));
    heartbeat.tick().await;
    loop {
        let msg = tokio::select! {
            m = stream.next() => m,
            _ = heartbeat.tick() => {
                sink.send(send(json!({"op": "heartbeat"}))).await.map_err(|e| e.to_string())?;
                continue;
            }
        };
        let text = match msg {
            Some(Ok(WsMsg::Text(t))) => t,
            Some(Ok(WsMsg::Close(_))) | None => return Ok(Outcome::Expired),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(format!("Discord login: {e}")),
        };
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        match v["op"].as_str().unwrap_or("") {
            "hello" => {
                let ms = v["heartbeat_interval"].as_u64().unwrap_or(41_250);
                heartbeat = tokio::time::interval(Duration::from_millis(ms));
                heartbeat.tick().await;
                let der = key.to_public_key().to_public_key_der().map_err(|e| e.to_string())?;
                sink.send(send(json!({"op": "init", "encoded_public_key": B64.encode(der.as_bytes())})))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            "nonce_proof" => {
                let nonce = decrypt(key, v["encrypted_nonce"].as_str().unwrap_or(""))?;
                let proof = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&nonce));
                sink.send(send(json!({"op": "nonce_proof", "proof": proof}))).await.map_err(|e| e.to_string())?;
            }
            "pending_remote_init" => {
                let fp = v["fingerprint"].as_str().unwrap_or("");
                qr::show(
                    &[
                        "Log in to Discord: open the Discord app on your phone →",
                        "your profile picture / Settings → \"Scan QR Code\", and scan this.",
                        "(Or start with --discord-token to paste a token instead.)",
                    ],
                    &format!("https://discord.com/ra/{fp}"),
                );
            }
            "pending_ticket" => {
                // "id:discriminator:avatar:username"
                let user = decrypt(key, v["encrypted_user_payload"].as_str().unwrap_or(""))?;
                let user = String::from_utf8_lossy(&user);
                let name = user.rsplit(':').next().unwrap_or("");
                println!("Scanned{}. Confirm the login on your phone…", if name.is_empty() { String::new() } else { format!(" as {name}") });
            }
            "pending_login" => {
                let ticket = v["ticket"].as_str().unwrap_or("");
                return redeem(key, ticket).await.map(Outcome::Token);
            }
            // Older protocol versions sent the token directly.
            "finish" => {
                let token = decrypt(key, v["encrypted_token"].as_str().unwrap_or(""))?;
                return String::from_utf8(token).map(Outcome::Token).map_err(|e| e.to_string());
            }
            "cancel" => return Err("the login was cancelled on the phone".into()),
            _ => {}
        }
    }
}

/// Exchanges the login ticket for the (encrypted) token.
async fn redeem(key: &RsaPrivateKey, ticket: &str) -> Result<String, String> {
    let v: Value = reqwest::Client::builder()
        .user_agent(user_agent())
        .build()
        .map_err(|e| e.to_string())?
        .post("https://discord.com/api/v9/users/@me/remote-auth/login")
        .json(&json!({ "ticket": ticket }))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    let Some(enc) = v["encrypted_token"].as_str() else {
        return Err(format!("Discord refused the login: {}", v["message"].as_str().unwrap_or("no token")));
    };
    String::from_utf8(decrypt(key, enc)?).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    /// Talks to Discord: the handshake must get as far as showing a QR code
    /// (then it waits for a scan, so the timeout is the success case).
    #[test]
    #[ignore]
    fn handshake_reaches_qr_code() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let r = rt.block_on(async { tokio::time::timeout(std::time::Duration::from_secs(25), super::login()).await });
        assert!(r.is_err(), "login ended early: {:?}", r.unwrap());
    }
}
