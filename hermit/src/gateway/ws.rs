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

use super::Gateway;
use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

pub async fn run(gateway: Arc<Gateway>, bind: String) -> Result<()> {
    let listener = TcpListener::bind(&bind).await?;
    tracing::info!(bind = %bind, "websocket gateway listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        let gateway = gateway.clone();
        tokio::spawn(async move {
            if let Err(e) = serve(gateway, stream).await {
                tracing::debug!(%peer, error = %e, "websocket client ended");
            }
        });
    }
}

async fn serve(gateway: Arc<Gateway>, stream: TcpStream) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut source) = ws.split();

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

        // Remote voice-turn trigger: identical to typing /listen at the CLI. Lets
        // an operator (or a test harness) exercise the REAL wake→listen→answer
        // pipeline over the gateway without standing at the device.
        if utterance == "/listen" {
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
            sink.send(Message::Text(event("token", &tok).into())).await?;
        }

        match turn.await {
            Ok(Ok(result)) => {
                sink.send(Message::Text(event("final", &result.answer).into())).await?;
            }
            Ok(Err(e)) => {
                sink.send(Message::Text(event("error", &e.to_string()).into())).await?;
            }
            Err(e) => {
                sink.send(Message::Text(event("error", &e.to_string()).into())).await?;
            }
        }
    }
    Ok(())
}

fn event(kind: &str, text: &str) -> String {
    serde_json::json!({ "type": kind, "text": text }).to_string()
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
}
