//! Sarvam Bulbul v3 streaming TTS — every spoken word the device produces.
//!
//! # Wire protocol (verified against the live API, not the docs)
//!
//! Two things here are not written down anywhere and cost real debugging time:
//!
//! 1. **Auth is a websocket SUBPROTOCOL, not a header.** The connection is opened
//!    with `Sec-WebSocket-Protocol: api-subscription-key.<KEY>`. Passing the key as
//!    an `api-subscription-key` header connects fine and then rejects every config
//!    message with the misleading `Input parameters has to be a valid dictionary`.
//!
//! 2. **Odia is `od-IN` here, but `or-IN` in Sarvam's STT service.** The same
//!    vendor uses different codes for the same language in adjacent products, and
//!    each rejects the other's. TTS-verified: od-IN, hi-IN, en-IN, bn-IN, ta-IN,
//!    te-IN, kn-IN, ml-IN, mr-IN, gu-IN, pa-IN.
//!
//! Message flow: `{"type":"config","data":{…}}` → `{"type":"text","data":{"text":…}}`
//! (repeatable) → `{"type":"flush"}`. Audio returns as base64 `linear16` in
//! `{"type":"audio","data":{"audio":"…"}}`.
//!
//! # Socket lifetime
//!
//! A warm socket is reused between utterances. Sarvam also closes sockets that
//! idle a while, so a stale handle is dropped and redialed once per utterance.
//! Measured first audio on a warm socket is ~450 ms for Odia.

use crate::audio::AudioPlayer;
use crate::speech::tts::{SpeakResult, TextRx};
use anyhow::{Context, Result};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub struct SarvamTts {
    url: String,
    api_key: String,
    language_code: String,
    speaker: String,
    sample_rate: u32,
    connect_timeout: Duration,
    /// Warm socket + last-use instant. Sarvam idle-closes sockets, so a
    /// stale one is discarded and redialed instead of being spoken into.
    conn: Mutex<Option<(WsStream, Instant)>>,
}
impl SarvamTts {
    pub fn new(cfg: &crate::config::Tts, api_key: String) -> Self {
        Self {
            url: cfg.sarvam_tts_url.clone(),
            api_key,
            language_code: cfg.sarvam_tts_language.clone(),
            speaker: cfg.sarvam_tts_voice.clone(),
            sample_rate: cfg.sample_rate,
            connect_timeout: Duration::from_millis(cfg.connect_timeout_ms),
            conn: Mutex::new(None),
        }
    }

    /// Build the socket URL.
    ///
    /// `send_completion_event=true` is REQUIRED. Without it Sarvam streams the
    /// audio and then goes silent forever — no completion frame, no close — so a
    /// reader waiting for end-of-utterance hangs until its timeout. Verified live:
    /// with the flag, `{"type":"event","data":{"event_type":"final"}}` arrives
    /// ~220ms after the last audio chunk.
    fn connect_url(&self) -> String {
        let base = self.url.trim_end_matches('/');
        let sep = if base.contains('?') { '&' } else { '?' };
        let mut url = base.to_string();
        if !base.contains("model=") {
            url = format!("{base}{sep}model=bulbul:v3");
        }
        if !url.contains("send_completion_event") {
            let sep = if url.contains('?') { '&' } else { '?' };
            url = format!("{url}{sep}send_completion_event=true");
        }
        url
    }

    async fn connect(&self) -> Result<WsStream> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let mut req = self.connect_url().into_client_request()?;
        // Auth rides the subprotocol header. See the module docs: a plain
        // `api-subscription-key` header is accepted at handshake and then fails
        // every subsequent config message.
        req.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            format!("api-subscription-key.{}", self.api_key)
                .parse()
                .context("invalid SARVAM_API_KEY")?,
        );

        let (ws, _) = tokio::time::timeout(
            self.connect_timeout,
            tokio_tungstenite::connect_async(req),
        )
        .await
        .map_err(|_| anyhow::anyhow!("sarvam tts connect timed out"))?
        .context("connecting to sarvam tts websocket")?;
        tracing::info!(language = %self.language_code, speaker = %self.speaker, "sarvam tts connected");
        Ok(ws)
    }

    fn config_frame(&self, language_code: &str) -> String {
        serde_json::json!({
            "type": "config",
            "data": {
                "language_code": language_code,
                "speaker": self.speaker,
                "output_audio_codec": "linear16",
                "speech_sample_rate": self.sample_rate,
                // Small buffer: start generating after ~30 chars rather than
                // waiting for a full sentence, which keeps first-audio low.
                "min_buffer_size": 30,
                "max_chunk_length": 150,
            }
        })
        .to_string()
    }

    pub async fn prewarm(&self) {
        let mut guard = self.conn.lock().await;
        if guard.is_some() {
            return;
        }
        match self.connect().await {
            Ok(ws) => *guard = Some((ws, Instant::now())),
            Err(e) => {
                tracing::warn!(error = %e, "sarvam tts prewarm failed; will retry on demand")
            }
        }
    }

    pub async fn speak(
        &self,
        text_rx: TextRx,
        player: &AudioPlayer,
        generation: u64,
        lang_override: Option<&str>,
    ) -> Result<SpeakResult> {
        // Per-turn language matching: reply in the language the user spoke
        // (already mapped to TTS codes by the caller); fall back to the
        // configured default voice language.
        let lang = lang_override.unwrap_or(&self.language_code);
        let mut text_rx = text_rx;
        let mut guard = self.conn.lock().await;
        // Reuse a warm socket only if it was used moments ago; Sarvam
        // idle-closes them and a dead socket downgrades the turn to silence.
        let mut ws = match guard.take() {
            Some((ws, last)) if last.elapsed() < Duration::from_secs(30) => ws,
            _ => self.connect().await?,
        };
        drop(guard);

        // Everything sent is recorded so a dead socket costs a redial, not the
        // turn. Two failure shapes, both observed live within one minute:
        //   1. Sarvam closed the socket but TCP writes still "succeed" locally —
        //      config/text/flush all send, the first read returns Close, and the
        //      utterance ends Ok with ZERO audio (the silent turn).
        //   2. The write itself fails ("sending sarvam tts config") because the
        //      corpse was re-warmed by an earlier silent turn.
        let mut spoken: Vec<String> = Vec::new();
        let result = self
            .stream_utterance(&mut ws, &mut text_rx, player, generation, lang, &[], &mut spoken)
            .await;

        let silent_close = matches!(
            &result,
            Ok(r) if r.samples == 0 && !r.interrupted && !spoken.is_empty()
        );
        if result.is_ok() && !silent_close {
            let r = result.unwrap();
            // Re-warm ONLY a socket that proved itself: it either produced audio
            // or had nothing to say. A zero-audio socket is a corpse; caching it
            // converts one failure into two.
            *self.conn.lock().await = Some((ws, Instant::now()));
            return Ok(r);
        }
        if let Err(e) = &result {
            tracing::warn!(error = %e, "sarvam tts utterance failed; redialing and replaying");
        } else {
            tracing::warn!("sarvam tts closed without audio; redialing and replaying the utterance");
        }

        // One retry on a guaranteed-fresh socket, replaying what was already
        // consumed from the channel, then continuing with whatever remains.
        let retry = async {
            let mut ws2 = self.connect().await?;
            let mut replay_log: Vec<String> = Vec::new();
            let replay = std::mem::take(&mut spoken);
            let r = self
                .stream_utterance(&mut ws2, &mut text_rx, player, generation, lang, &replay, &mut replay_log)
                .await?;
            if r.samples > 0 {
                *self.conn.lock().await = Some((ws2, Instant::now()));
            }
            Ok::<SpeakResult, anyhow::Error>(r)
        }
        .await;

        match retry {
            Ok(r) => Ok(r),
            Err(e) => {
                // Unwedge the producer before surfacing the failure.
                while text_rx.recv().await.is_some() {}
                Err(e)
            }
        }
    }

    /// Stream one utterance.
    ///
    /// Text is pushed as it arrives and audio is drained concurrently. The two
    /// must not be serialised: Sarvam closes a socket that receives `config` and
    /// then nothing ("Websocket was left open without any messages for too long"),
    /// and a `biased` select that always polls the socket first starves the text
    /// channel into exactly that state.
    ///
    /// `replay` is text already consumed from the channel by a previous failed
    /// attempt; it is sent first. Every text chunk sent is appended to `sent_log`
    /// so the caller can replay it if THIS attempt dies too.
    #[allow(clippy::too_many_arguments)]
    async fn stream_utterance(
        &self,
        ws: &mut WsStream,
        text_rx: &mut TextRx,
        player: &AudioPlayer,
        generation: u64,
        language_code: &str,
        replay: &[String],
        sent_log: &mut Vec<String>,
    ) -> Result<SpeakResult> {
        let mut result = SpeakResult::default();
        let started = Instant::now();
        let mut text_done = false;
        let mut sent_any_text = false;

        ws.send(Message::Text(self.config_frame(language_code).into()))
            .await
            .context("sending sarvam tts config")?;

        for text in replay {
            let frame = serde_json::json!({
                "type": "text",
                "data": { "text": text },
            })
            .to_string();
            ws.send(Message::Text(frame.into()))
                .await
                .context("replaying sarvam tts text")?;
            sent_log.push(text.clone());
            sent_any_text = true;
        }

        loop {
            if player.is_stale(generation) {
                result.interrupted = true;
                break;
            }

            tokio::select! {
                // NOT biased: text must get a fair turn or the socket times out.

                chunk = text_rx.recv(), if !text_done => {
                    match chunk {
                        Some(text) if !text.trim().is_empty() => {
                            let frame = serde_json::json!({
                                "type": "text",
                                "data": { "text": text },
                            })
                            .to_string();
                            ws.send(Message::Text(frame.into()))
                                .await
                                .context("sending sarvam tts text")?;
                            sent_log.push(text);
                            sent_any_text = true;
                        }
                        Some(_) => {}
                        None => {
                            text_done = true;
                            if sent_any_text {
                                ws.send(Message::Text(r#"{"type":"flush"}"#.to_string().into()))
                                    .await
                                    .context("flushing sarvam tts")?;
                            } else {
                                break; // nothing to say
                            }
                        }
                    }
                }

                incoming = ws.next() => {
                    let Some(frame) = incoming else {
                        break; // socket closed
                    };
                    match frame.context("sarvam tts stream error")? {
                        Message::Text(raw) => {
                            match parse_tts_frame(&raw) {
                                TtsFrame::Audio(samples) => {
                                    if !samples.is_empty() {
                                        result.samples += samples.len();
                                        if result.ttfa_ms.is_none() {
                                            result.ttfa_ms =
                                                Some(started.elapsed().as_secs_f64() * 1000.0);
                                        }
                                        if !player.try_play(samples) {
                                            result.interrupted = true;
                                            break;
                                        }
                                    }
                                }
                                TtsFrame::Done => {
                                    result.finished = result.samples > 0;
                                    break;
                                }
                                TtsFrame::Error(msg) => {
                                    anyhow::bail!("sarvam tts error: {msg}");
                                }
                                TtsFrame::Other => {}
                            }
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
            }
        }

        // Drain any remaining text ONLY on success or interruption; a failed
        // attempt leaves the channel intact so the caller's retry can finish
        // the utterance instead of losing the tail.
        if result.finished || result.interrupted || result.samples > 0 || sent_log.is_empty() {
            while text_rx.recv().await.is_some() {}
        }
        Ok(result)
    }
}

/// One decoded frame from the TTS socket.
#[derive(Debug, PartialEq)]
pub enum TtsFrame {
    Audio(Vec<i16>),
    Done,
    Error(String),
    Other,
}

/// Decode one Sarvam TTS frame.
///
/// Audio arrives as base64 `linear16` (little-endian i16) under
/// `{"type":"audio","data":{"audio":"…"}}`.
pub fn parse_tts_frame(raw: &str) -> TtsFrame {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return TtsFrame::Other;
    };
    let kind = v
        .get("type")
        .or_else(|| v.get("event"))
        .and_then(|t| t.as_str())
        .unwrap_or("");

    if let Some(b64) = v
        .get("data")
        .and_then(|d| d.get("audio"))
        .or_else(|| v.get("audio"))
        .and_then(|a| a.as_str())
        && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64)
    {
        return TtsFrame::Audio(
            bytes
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect(),
        );
    }

    match kind {
        "error" => TtsFrame::Error(
            v.get("data")
                .and_then(|d| d.get("message"))
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown sarvam tts error")
                .to_string(),
        ),
        // The real completion frame is `{"type":"event","data":{"event_type":"final"}}`.
        // The other spellings are defensive.
        "event"
            if v.get("data")
                .and_then(|d| d.get("event_type"))
                .and_then(|e| e.as_str())
                == Some("final") =>
        {
            TtsFrame::Done
        }
        "flush" | "done" | "complete" | "audio.done" => TtsFrame::Done,
        _ => TtsFrame::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tts(url: &str) -> SarvamTts {
        SarvamTts {
            url: url.into(),
            api_key: "k".into(),
            language_code: "od-IN".into(),
            speaker: "shubh".into(),
            sample_rate: 16_000,
            connect_timeout: Duration::from_millis(3_000),
            conn: Mutex::new(None),
        }
    }

    #[test]
    fn url_always_requests_the_completion_event() {
        // Without send_completion_event=true the socket streams audio and then
        // goes silent forever: no completion frame, no close. Every utterance
        // then hangs until its timeout. Verified against the live API.
        for base in [
            "wss://api.sarvam.ai/text-to-speech/ws",
            "wss://api.sarvam.ai/text-to-speech/ws?model=bulbul:v3",
        ] {
            let url = tts(base).connect_url();
            assert!(url.contains("send_completion_event=true"), "got {url}");
            assert!(url.contains("model=bulbul:v3"), "got {url}");
            assert_eq!(url.matches('?').count(), 1, "malformed query string: {url}");
        }
    }

    #[test]
    fn final_event_frame_ends_the_utterance() {
        // The live completion frame — not "done"/"complete", which the docs imply.
        let raw = r#"{"type":"event","data":{"event_type":"final"}}"#;
        assert!(matches!(parse_tts_frame(raw), TtsFrame::Done), "got {:?}", parse_tts_frame(raw));
    }

    #[test]
    fn non_final_events_do_not_end_the_utterance() {
        let raw = r#"{"type":"event","data":{"event_type":"start"}}"#;
        assert!(matches!(parse_tts_frame(raw), TtsFrame::Other));
    }

    #[test]
    fn decodes_base64_linear16_audio() {
        // Two samples: 1 and -1, little-endian.
        let pcm: Vec<u8> = vec![0x01, 0x00, 0xFF, 0xFF];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&pcm);
        let raw = format!(r#"{{"type":"audio","data":{{"audio":"{b64}"}}}}"#);
        assert_eq!(parse_tts_frame(&raw), TtsFrame::Audio(vec![1, -1]));
    }

    #[test]
    fn surfaces_the_error_message_rather_than_a_generic_failure() {
        // The exact payload the live API returns for an unsupported language.
        let raw = r#"{"type":"error","data":{"message":"Input parameters has to be a valid dictionary","code":422}}"#;
        match parse_tts_frame(raw) {
            TtsFrame::Error(m) => assert!(m.contains("valid dictionary"), "got {m}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn flush_acknowledgement_ends_the_utterance() {
        assert_eq!(parse_tts_frame(r#"{"type":"flush"}"#), TtsFrame::Done);
    }

    #[test]
    fn unknown_frames_are_ignored_not_fatal() {
        assert_eq!(parse_tts_frame(r#"{"type":"pong"}"#), TtsFrame::Other);
        assert_eq!(parse_tts_frame("not json"), TtsFrame::Other);
    }
}
