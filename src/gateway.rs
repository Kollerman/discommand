//! Gateway websocket: zlib-stream compressed, auto-resuming, and filtering
//! out events the UI doesn't use before they are parsed.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use flate2::{Decompress, FlushDecompress};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::json;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::{interval_at, Instant, Interval, MissedTickBehavior};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use crate::api::super_properties;
use crate::app::AppEvent;

const GATEWAY: &str = "wss://gateway.discord.gg";

/// Dispatch events the app handles; everything else (presences, typing,
/// member lists, ...) is dropped without being deserialized.
const WANTED: &[&str] = &[
    "READY",
    "GUILD_CREATE",
    "GUILD_UPDATE",
    "GUILD_DELETE",
    "CHANNEL_CREATE",
    "CHANNEL_UPDATE",
    "CHANNEL_DELETE",
    "MESSAGE_CREATE",
    "MESSAGE_UPDATE",
    "MESSAGE_DELETE",
    "MESSAGE_ACK",
    "MESSAGE_REACTION_ADD",
    "MESSAGE_REACTION_REMOVE",
    "MESSAGE_REACTION_REMOVE_ALL",
    "MESSAGE_REACTION_REMOVE_EMOJI",
];

pub enum GwEvent {
    Dispatch(String, Box<RawValue>),
    Status(String),
    Fatal(String),
}

#[derive(Deserialize)]
struct Payload<'a> {
    op: u8,
    #[serde(borrow, default)]
    d: Option<&'a RawValue>,
    #[serde(default)]
    s: Option<u64>,
    #[serde(default)]
    t: Option<&'a str>,
}

#[derive(Deserialize)]
struct ReadyMeta {
    session_id: String,
    resume_gateway_url: String,
}

#[derive(Deserialize)]
struct Hello {
    heartbeat_interval: u64,
}

struct Session {
    id: String,
    resume_url: String,
    seq: Option<u64>,
}

enum Exit {
    Resume,
    Fresh,
    Fatal(String),
    Error(String),
}

/// Accumulates zlib-stream frames and inflates each complete payload.
struct Inflater {
    z: Decompress,
    buf: Vec<u8>,
}

impl Inflater {
    fn new() -> Self {
        Self { z: Decompress::new(true), buf: Vec::new() }
    }

    fn push(&mut self, data: &[u8]) -> Result<Option<String>, String> {
        self.buf.extend_from_slice(data);
        if !self.buf.ends_with(&[0, 0, 0xff, 0xff]) {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(self.buf.len() * 4);
        let mut input = &self.buf[..];
        loop {
            if out.capacity() - out.len() < 16 * 1024 {
                out.reserve(64 * 1024);
            }
            let (before_in, before_out) = (self.z.total_in(), self.z.total_out());
            self.z
                .decompress_vec(input, &mut out, FlushDecompress::Sync)
                .map_err(|e| format!("inflate: {e}"))?;
            let consumed = (self.z.total_in() - before_in) as usize;
            let produced = self.z.total_out() - before_out;
            input = &input[consumed..];
            if input.is_empty() && out.len() < out.capacity() {
                break;
            }
            if consumed == 0 && produced == 0 {
                break;
            }
        }
        self.buf.clear();
        if self.buf.capacity() > 1 << 20 {
            self.buf.shrink_to(64 * 1024);
        }
        String::from_utf8(out).map(Some).map_err(|e| e.to_string())
    }
}

async fn tick(hb: &mut Option<Interval>) {
    match hb {
        Some(i) => {
            i.tick().await;
        }
        None => std::future::pending().await,
    }
}

fn jitter(max_ms: u64) -> u64 {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos() as u64).unwrap_or(0);
    n % max_ms.max(1)
}

pub async fn run(token: String, tx: UnboundedSender<AppEvent>, mut cmds: UnboundedReceiver<String>) {
    let mut session: Option<Session> = None;
    let mut backoff = 1u64;
    loop {
        let status = if session.is_some() { "resuming…" } else { "connecting…" };
        let _ = tx.send(AppEvent::Gateway(GwEvent::Status(status.into())));
        match connect_once(&token, &tx, &mut cmds, &mut session).await {
            Exit::Resume => backoff = 1,
            Exit::Fresh => {
                session = None;
                backoff = 1;
            }
            Exit::Fatal(msg) => {
                let _ = tx.send(AppEvent::Gateway(GwEvent::Fatal(msg)));
                return;
            }
            Exit::Error(msg) => {
                if tx.is_closed() {
                    return;
                }
                let _ = tx.send(AppEvent::Gateway(GwEvent::Status(format!("offline ({msg}), retry in {backoff}s"))));
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(60);
            }
        }
    }
}

async fn connect_once(
    token: &str,
    tx: &UnboundedSender<AppEvent>,
    cmds: &mut UnboundedReceiver<String>,
    session: &mut Option<Session>,
) -> Exit {
    let base = session.as_ref().map(|s| s.resume_url.as_str()).unwrap_or(GATEWAY);
    let url = format!("{}/?v=10&encoding=json&compress=zlib-stream", base.trim_end_matches('/'));
    let ws = match connect_async(url).await {
        Ok((ws, _)) => ws,
        Err(e) => return Exit::Error(e.to_string()),
    };
    let (mut sink, mut stream) = ws.split();
    let mut inflater = Inflater::new();
    let mut heartbeat: Option<Interval> = None;
    let mut acked = true;
    let mut seq = session.as_ref().and_then(|s| s.seq);

    loop {
        tokio::select! {
            msg = stream.next() => {
                let msg = match msg {
                    None => return Exit::Error("connection closed".into()),
                    Some(Err(e)) => return Exit::Error(e.to_string()),
                    Some(Ok(m)) => m,
                };
                let text = match msg {
                    WsMsg::Binary(b) => match inflater.push(&b) {
                        Ok(Some(t)) => t,
                        Ok(None) => continue,
                        Err(e) => return Exit::Error(e),
                    },
                    WsMsg::Text(t) => t,
                    WsMsg::Close(frame) => {
                        let code = frame.map(|f| u16::from(f.code)).unwrap_or(0);
                        return match code {
                            4004 => Exit::Fatal("authentication failed: token is invalid (run with --logout)".into()),
                            4010..=4014 => Exit::Fatal(format!("gateway refused connection (code {code})")),
                            4007 | 4009 => Exit::Fresh,
                            _ => Exit::Error(format!("closed with code {code}")),
                        };
                    }
                    _ => continue,
                };
                let p: Payload = match serde_json::from_str(&text) {
                    Ok(p) => p,
                    Err(e) => return Exit::Error(format!("bad payload: {e}")),
                };
                if p.s.is_some() {
                    seq = p.s;
                    if let Some(s) = session.as_mut() {
                        s.seq = seq;
                    }
                }
                let out = match p.op {
                    10 => {
                        let ms = p.d.and_then(|d| serde_json::from_str::<Hello>(d.get()).ok())
                            .map(|h| h.heartbeat_interval)
                            .unwrap_or(41_250);
                        let first = Instant::now() + Duration::from_millis(jitter(ms));
                        let mut i = interval_at(first, Duration::from_millis(ms));
                        i.set_missed_tick_behavior(MissedTickBehavior::Delay);
                        heartbeat = Some(i);
                        let payload = match session.as_ref() {
                            Some(s) => json!({"op": 6, "d": {"token": token, "session_id": s.id, "seq": s.seq}}),
                            None => json!({"op": 2, "d": {
                                "token": token,
                                "capabilities": 0,
                                "properties": super_properties(),
                                "compress": false,
                            }}),
                        };
                        Some(payload.to_string())
                    }
                    11 => {
                        acked = true;
                        None
                    }
                    1 => Some(json!({"op": 1, "d": seq}).to_string()),
                    7 => return Exit::Resume,
                    9 => {
                        let resumable = p.d.map(|d| d.get() == "true").unwrap_or(false);
                        tokio::time::sleep(Duration::from_millis(1000 + jitter(4000))).await;
                        return if resumable { Exit::Resume } else { Exit::Fresh };
                    }
                    0 => {
                        let t = p.t.unwrap_or("");
                        if t == "READY" {
                            if let Some(meta) = p.d.and_then(|d| serde_json::from_str::<ReadyMeta>(d.get()).ok()) {
                                *session = Some(Session { id: meta.session_id, resume_url: meta.resume_gateway_url, seq });
                            }
                            let _ = tx.send(AppEvent::Gateway(GwEvent::Status("connected".into())));
                        } else if t == "RESUMED" {
                            let _ = tx.send(AppEvent::Gateway(GwEvent::Status("connected".into())));
                        }
                        if WANTED.contains(&t) {
                            if let Some(d) = p.d {
                                if tx.send(AppEvent::Gateway(GwEvent::Dispatch(t.to_string(), d.to_owned()))).is_err() {
                                    return Exit::Fatal("app closed".into());
                                }
                            }
                        }
                        None
                    }
                    _ => None,
                };
                if let Some(out) = out {
                    if let Err(e) = sink.send(WsMsg::Text(out)).await {
                        return Exit::Error(e.to_string());
                    }
                }
            }
            _ = tick(&mut heartbeat) => {
                if !acked {
                    return Exit::Error("heartbeat timed out".into());
                }
                acked = false;
                if let Err(e) = sink.send(WsMsg::Text(json!({"op": 1, "d": seq}).to_string())).await {
                    return Exit::Error(e.to_string());
                }
            }
            Some(cmd) = cmds.recv() => {
                if let Err(e) = sink.send(WsMsg::Text(cmd)).await {
                    return Exit::Error(e.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    #[test]
    fn inflates_split_zlib_stream() {
        let mut z = Compress::new(Compression::default(), true);
        let mut inflater = Inflater::new();
        for msg in [r#"{"op":10,"d":{"heartbeat_interval":41250}}"#, r#"{"op":11}"#] {
            let mut out = Vec::with_capacity(1024);
            z.compress_vec(msg.as_bytes(), &mut out, FlushCompress::Sync).unwrap();
            assert!(out.ends_with(&[0, 0, 0xff, 0xff]));
            let (a, b) = out.split_at(out.len() / 2);
            assert_eq!(inflater.push(a).unwrap(), None);
            assert_eq!(inflater.push(b).unwrap().as_deref(), Some(msg));
        }
    }
}
