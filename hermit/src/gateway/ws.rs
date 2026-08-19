//! Local WebSocket front end for text clients.
//!
//! Bound to loopback by default. The protocol is deliberately trivial — a client
//! sends a plain-text utterance, and receives JSON events:
//!
//! ```json
//! {"type":"token","text":"..."}
//! {"type":"final","text":"..."}
//! {"type":"error","text":"..."}
//! ```
//!
//! # Security posture
//!
//! This socket is a control surface: an utterance is standing permission to
//! spend LLM and tool budget, and `/listen` (when enabled) arms the microphone.
//! Accordingly:
//!
//! - **Auth.** If `HERMIT_WS_TOKEN` is set, every connection must present it —
//!   either as an `Authorization: Bearer <token>` header on the HTTP upgrade,
//!   or as a first message `/auth <token>`. Unauthenticated messages get one
//!   `error` event and the socket closes. Config validation refuses a
//!   non-loopback bind without this token, so "exposed and open" cannot be
//!   configured by accident.
//! - **`/listen` is opt-in.** Remotely arming the mic is a control-plane action;
//!   it requires `server.ws_allow_listen = true` (default false) in addition to
//!   whatever auth applies.
//! - **Connection cap.** At most `server.ws_max_connections` concurrent clients
//!   (default 4); excess connections are refused before the WS upgrade.
//! - **Frame cap.** Incoming messages are limited to 64 KiB — an utterance is a
//!   sentence, not a payload — replacing tungstenite's 64 MiB default.

use super::Gateway;
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Incoming frames are capped here instead of tungstenite's 64 MiB default.
/// The largest legitimate client message is one spoken-length utterance.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

pub async fn run(gateway: Arc<Gateway>, bind: String) -> Result<()> {
    let listener = TcpListener::bind(&bind).await?;
    let token = crate::http::secret_opt("HERMIT_WS_TOKEN");
    let max_conns = gateway.config().server.ws_max_connections.max(1);
    let permits = Arc::new(tokio::sync::Semaphore::new(max_conns));
    tracing::info!(
        bind = %bind,
        auth = if token.is_some() { "token" } else { "none (loopback)" },
        max_connections = max_conns,
        "websocket gateway listening"
    );

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        // Refuse over-cap connections before the WS upgrade: each connection
        // is standing permission to queue turns against the turn lock.
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            tracing::warn!(%peer, "websocket connection refused: at capacity");
            continue;
        };
        let gateway = gateway.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let _permit = permit; // held for the connection's lifetime
            if let Err(e) = serve(gateway, stream, token).await {
                tracing::debug!(%peer, error = %e, "websocket client ended");
            }
        });
    }
}

async fn serve(gateway: Arc<Gateway>, stream: TcpStream, token: Option<String>) -> Result<()> {
    // If a token is required, accept it on the upgrade request's Authorization
    // header; otherwise fall back to the first-message `/auth` handshake below.
    let mut header_authed = false;
    // The callback's signature carries tungstenite's large ErrorResponse; we
    // never construct it, so the lint's copy-cost concern does not apply.
    #[allow(clippy::result_large_err)]
    let callback = |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
        if let (Some(expected), Some(got)) = (token.as_deref(), req.headers().get("authorization"))
            && let Ok(got) = got.to_str()
            && let Some(bearer) = got.strip_prefix("Bearer ")
            && constant_time_eq(bearer.trim(), expected)
        {
            header_authed = true;
        }
        Ok(resp)
    };
    let ws = tokio_tungstenite::accept_hdr_async_with_config(
        stream,
        callback,
        Some(WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE_BYTES))),
    )
    .await?;
    let (mut sink, mut source) = ws.split();
    let mut authed = token.is_none() || header_authed;

    while let Some(msg) = source.next().await {
        let text = match msg? {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            Message::Ping(p) => {
                sink.send(Message::Pong(p)).await?;
                continue;
            }
            _ => continue,
        };

        let utterance = text.trim().to_string();
        if utterance.is_empty() {
            continue;
        }

        // First-message auth for clients that cannot set headers (browsers).
        if !authed {
            if let Some(rest) = utterance.strip_prefix("/auth ")
                && constant_time_eq(rest.trim(), token.as_deref().unwrap_or(""))
            {
                authed = true;
                sink.send(Message::Text(event("final", "authenticated").into()))
                    .await?;
                continue;
            }
            // One refusal, then hang up: no oracle for guessing.
            sink.send(Message::Text(event("error", "unauthorized").into()))
                .await?;
            break;
        }

        // Remote voice-turn trigger: identical to typing /listen at the CLI.
        // Gated behind an explicit operator opt-in — arming the microphone is a
        // control-plane action, not something network reachability should buy.
        if utterance == "/listen" {
            if !gateway.config().server.ws_allow_listen {
                sink.send(Message::Text(
                    event(
                        "error",
                        "remote /listen is disabled (server.ws_allow_listen)",
                    )
                    .into(),
                ))
                .await?;
                continue;
            }
            let ok = gateway.trigger_listen();
            let kind = if ok { "final" } else { "error" };
            let msg = if ok {
                "listening... speak now"
            } else {
                "voice pipeline is not running"
            };
            sink.send(Message::Text(event(kind, msg).into())).await?;
            continue;
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let gw = gateway.clone();
        let turn = tokio::spawn(async move {
            // Text clients do not drive the speaker: a phone in another room
            // should not make the kitchen talk.
            gw.handle(&utterance, false, Some(tx), None).await
        });

        while let Some(tok) = rx.recv().await {
            sink.send(Message::Text(event("token", &tok).into()))
                .await?;
        }

        match turn.await {
            Ok(Ok(result)) => {
                sink.send(Message::Text(event("final", &result.answer).into()))
                    .await?;
            }
            Ok(Err(e)) => {
                sink.send(Message::Text(event("error", &e.to_string()).into()))
                    .await?;
            }
            Err(e) => {
                sink.send(Message::Text(event("error", &e.to_string()).into()))
                    .await?;
            }
        }
    }
    Ok(())
}

fn event(kind: &str, text: &str) -> String {
    serde_json::json!({ "type": kind, "text": text }).to_string()
}

/// Length-independent-ish comparison for short tokens: XOR-accumulate over the
/// longer input so equality never short-circuits on the first differing byte.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_well_formed_json() {
        let e = event("token", "hello \"world\"\n");
        let v: serde_json::Value = serde_json::from_str(&e).unwrap();
        assert_eq!(v["type"], "token");
        assert_eq!(v["text"], "hello \"world\"\n");
    }

    #[test]
    fn token_comparison_is_exact() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secre"));
        assert!(!constant_time_eq("secret", "secreT"));
        assert!(!constant_time_eq("", "secret"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn frame_cap_is_a_sentence_not_a_payload() {
        // Guards the constant against a "helpful" bump back toward the
        // tungstenite default (64 MiB): review finding, loopback peers could
        // push huge frames.
        const { assert!(MAX_MESSAGE_BYTES <= 64 * 1024) };
    }
}
