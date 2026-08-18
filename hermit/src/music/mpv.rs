//! mpv control over its JSON IPC unix socket.
//!
//! mpv runs as a sidecar (`mpv --no-video --idle --input-ipc-server=...`) and plays
//! internet radio / HLS / Icecast streams. We talk to it with one line of JSON per
//! command.
//!
//! A fresh connection is opened per command rather than held open. On a unix socket
//! that costs tens of microseconds — irrelevant against the 50 ms fast-path budget —
//! and it means an mpv crash-and-restart heals automatically instead of leaving us
//! holding a dead socket.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Commands are expected to complete far inside this; it exists so a wedged mpv
/// cannot hang the hot path.
const IPC_TIMEOUT: Duration = Duration::from_millis(400);

#[derive(Clone)]
pub struct MpvClient {
    socket: PathBuf,
}

impl MpvClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self { socket: socket.into() }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Is the sidecar reachable?
    pub async fn is_available(&self) -> bool {
        tokio::time::timeout(IPC_TIMEOUT, UnixStream::connect(&self.socket))
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
    }

    /// Send one command and return its `data` field.
    async fn command(&self, args: Value) -> Result<Value> {
        let fut = async {
            let stream = UnixStream::connect(&self.socket)
                .await
                .with_context(|| format!("connecting to mpv at {}", self.socket.display()))?;

            let (read_half, mut write_half) = stream.into_split();
            let payload = json!({ "command": args, "request_id": 1 });
            let mut line = serde_json::to_string(&payload)?;
            line.push('\n');
            write_half.write_all(line.as_bytes()).await?;
            write_half.flush().await?;

            // mpv interleaves async event lines with command replies; skip events
            // and take the first line carrying our request_id.
            let mut reader = BufReader::new(read_half);
            let mut buf = String::new();
            loop {
                buf.clear();
                let n = reader.read_line(&mut buf).await?;
                if n == 0 {
                    bail!("mpv closed the connection without replying");
                }
                let Ok(v) = serde_json::from_str::<Value>(&buf) else { continue };
                if v.get("request_id").and_then(Value::as_i64) != Some(1) {
                    continue; // an event, not our reply
                }
                let error = v.get("error").and_then(Value::as_str).unwrap_or("success");
                if error != "success" {
                    bail!("mpv error: {error}");
                }
                return Ok(v.get("data").cloned().unwrap_or(Value::Null));
            }
        };

        tokio::time::timeout(IPC_TIMEOUT, fut)
            .await
            .map_err(|_| anyhow::anyhow!("mpv IPC timed out after {IPC_TIMEOUT:?}"))?
    }

    pub async fn get_property(&self, name: &str) -> Result<Value> {
        self.command(json!(["get_property", name])).await
    }

    pub async fn set_property(&self, name: &str, value: Value) -> Result<()> {
        self.command(json!(["set_property", name, value])).await.map(|_| ())
    }

    /// Replace whatever is playing with `url`.
    pub async fn loadfile(&self, url: &str) -> Result<()> {
        self.command(json!(["loadfile", url, "replace"])).await.map(|_| ())
    }

    pub async fn pause(&self) -> Result<()> {
        self.set_property("pause", json!(true)).await
    }

    pub async fn resume(&self) -> Result<()> {
        self.set_property("pause", json!(false)).await
    }

    /// Stop playback and clear the playlist.
    pub async fn stop(&self) -> Result<()> {
        self.command(json!(["stop"])).await.map(|_| ())
    }

    /// mpv volume is 0–100 (and may exceed 100; we never do).
    pub async fn set_volume(&self, percent: u8) -> Result<()> {
        self.set_property("volume", json!(percent.min(100) as i64)).await
    }

    pub async fn volume(&self) -> Result<u8> {
        let v = self.get_property("volume").await?;
        Ok(v.as_f64().unwrap_or(0.0).round().clamp(0.0, 100.0) as u8)
    }

    pub async fn is_paused(&self) -> Result<bool> {
        Ok(self.get_property("pause").await?.as_bool().unwrap_or(false))
    }

    /// Whether anything is loaded at all.
    pub async fn is_idle(&self) -> Result<bool> {
        Ok(self.get_property("idle-active").await?.as_bool().unwrap_or(true))
    }

    /// Best-effort "what's playing": stream title from ICY metadata, else the
    /// media title, else the filename.
    pub async fn now_playing(&self) -> Option<String> {
        for prop in ["media-title", "filtered-metadata/icy-title", "filename"] {
            if let Ok(v) = self.get_property(prop).await
                && let Some(s) = v.as_str()
                && !s.trim().is_empty()
            {
                return Some(s.to_string());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    /// Stand up a fake mpv that speaks the real IPC dialect.
    async fn fake_mpv(
        responder: impl Fn(&Value) -> Value + Send + Sync + 'static,
    ) -> (MpvClient, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mpv.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let responder = std::sync::Arc::new(responder);

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { return };
                let responder = responder.clone();
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut reader = BufReader::new(r);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let req: Value = serde_json::from_str(&line).unwrap();
                    // Emit an unsolicited event first — the client must skip it.
                    let _ = w.write_all(b"{\"event\":\"playback-restart\"}\n").await;
                    let mut resp = responder(&req["command"]);
                    resp["request_id"] = json!(1);
                    let mut out = serde_json::to_string(&resp).unwrap();
                    out.push('\n');
                    let _ = w.write_all(out.as_bytes()).await;
                    let _ = w.flush().await;
                });
            }
        });
        (MpvClient::new(path), dir)
    }

    #[tokio::test]
    async fn skips_async_events_and_reads_the_reply() {
        let (c, _d) = fake_mpv(|_| json!({"error":"success","data":42})).await;
        assert_eq!(c.get_property("volume").await.unwrap(), json!(42));
    }

    #[tokio::test]
    async fn surfaces_mpv_errors() {
        let (c, _d) = fake_mpv(|_| json!({"error":"property not found"})).await;
        let err = c.get_property("nope").await.unwrap_err().to_string();
        assert!(err.contains("property not found"));
    }

    #[tokio::test]
    async fn volume_is_clamped_into_range() {
        let (c, _d) = fake_mpv(|cmd| {
            // Assert the client never sends >100.
            assert!(cmd[2].as_i64().unwrap() <= 100);
            json!({"error":"success"})
        })
        .await;
        c.set_volume(255).await.unwrap();
    }

    #[tokio::test]
    async fn missing_socket_is_an_error_not_a_hang() {
        let c = MpvClient::new("/nonexistent/hermit-test.sock");
        assert!(!c.is_available().await);
        assert!(c.pause().await.is_err());
    }

    #[tokio::test]
    async fn now_playing_falls_back_through_properties() {
        let (c, _d) = fake_mpv(|cmd| {
            if cmd[1] == json!("media-title") {
                json!({"error":"success","data":null})
            } else {
                json!({"error":"success","data":"Some Station - Track"})
            }
        })
        .await;
        assert_eq!(c.now_playing().await.as_deref(), Some("Some Station - Track"));
    }
}
