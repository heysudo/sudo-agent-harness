//! Deepgram streaming speech-to-text.
//!
//! Audio is streamed ONLY after the wake word fires and only while the XVF3800's
//! VAD says someone is talking (spec §7). Streaming continuously would cost money
//! and, more importantly, would mean an always-open microphone feed to a third
//! party — which this device should not do.
//!
//! Interim results drive the speculative-prefetch path in the orchestrator: a
//! partial transcript that looks like a lookup fires a provisional search before the
//! user has finished the sentence.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttEvent {
    /// Unstable partial transcript. Safe to act on speculatively, never to answer.
    Interim(String),
    /// Deepgram will not revise this segment.
    Final(String),
    /// End of utterance — the user stopped talking.
    EndOfSpeech(String),
    /// Upstream closed or errored.
    Closed(Option<String>),
}

pub struct Deepgram {
    url: String,
    api_key: String,
    model: String,
    language: String,
    endpointing_ms: u32,
    sample_rate: u32,
    max_utterance: Duration,
}

impl Deepgram {
    pub fn from_config(cfg: &crate::config::Stt) -> Option<Self> {
        let api_key = crate::http::secret_opt("DEEPGRAM_API_KEY")?;
        Some(Self {
            url: cfg.url.clone(),
            api_key,
            model: cfg.model.clone(),
            language: cfg.language.clone(),
            endpointing_ms: cfg.endpointing_ms,
            sample_rate: cfg.sample_rate,
            max_utterance: Duration::from_millis(cfg.max_utterance_ms),
        })
    }

    fn connect_url(&self) -> String {
        format!(
            "{}?model={}&language={}&encoding=linear16&sample_rate={}&channels=1\
             &interim_results=true&endpointing={}&vad_events=true&punctuate=true&smart_format=true\
             &no_delay=true",
            self.url.trim_end_matches('/'),
            urlencoding::encode(&self.model),
            urlencoding::encode(&self.language),
            self.sample_rate,
            self.endpointing_ms,
        )
    }

    /// Open a transcription session.
    ///
    /// Returns a sender for raw mono PCM and a receiver of transcript events. Drop
    /// the sender to signal end of audio; the session finalizes and closes.
    pub async fn start(
        &self,
    ) -> Result<(
        tokio::sync::mpsc::Sender<Vec<i16>>,
        tokio::sync::mpsc::Receiver<SttEvent>,
    )> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let mut req = self.connect_url().into_client_request()?;
        req.headers_mut().insert(
            "Authorization",
            format!("Token {}", self.api_key)
                .parse()
                .context("invalid DEEPGRAM_API_KEY")?,
        );

        let (ws, _) = tokio::time::timeout(
            Duration::from_secs(4),
            tokio_tungstenite::connect_async(req),
        )
        .await
        .map_err(|_| anyhow::anyhow!("deepgram connect timed out"))?
        .context("connecting to deepgram")?;

        let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(32);
        let (event_tx, event_rx) = tokio::sync::mpsc::channel::<SttEvent>(32);
        let max_utterance = self.max_utterance;

        tokio::spawn(async move {
            let (mut sink, mut stream) = ws.split();
            let deadline = tokio::time::sleep(max_utterance);
            tokio::pin!(deadline);

            let mut audio_open = true;
            let mut frames_sent = 0usize;
            loop {
                tokio::select! {
                    biased;

                    incoming = stream.next() => {
                        let Some(incoming) = incoming else {
                            let _ = event_tx.send(SttEvent::Closed(None)).await;
                            return;
                        };
                        match incoming {
                            Ok(Message::Text(t)) => {
                                if let Some(ev) = parse_event(&t)
                                    && event_tx.send(ev.clone()).await.is_err() {
                                        return;
                                    }
                            }
                            Ok(Message::Close(frame)) => {
                                // Surface the close code/reason: Deepgram uses it to
                                // report bad query parameters and auth problems, and
                                // without it a rejected connection is indistinguishable
                                // from "the user said nothing".
                                let detail = frame.map(|f| format!("code={} reason={}", f.code, f.reason));
                                if let Some(d) = &detail {
                                    tracing::warn!(close = %d, "deepgram closed the stream");
                                }
                                let _ = event_tx.send(SttEvent::Closed(detail)).await;
                                return;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                let _ = event_tx.send(SttEvent::Closed(Some(e.to_string()))).await;
                                return;
                            }
                        }
                    }

                    chunk = audio_rx.recv(), if audio_open => {
                        match chunk {
                            Some(samples) => {
                                let mut bytes = Vec::with_capacity(samples.len() * 2);
                                for s in samples {
                                    bytes.extend_from_slice(&s.to_le_bytes());
                                }
                                frames_sent += 1;
                                if sink.send(Message::Binary(bytes.into())).await.is_err() {
                                    let _ = event_tx.send(SttEvent::Closed(Some("send failed".into()))).await;
                                    return;
                                }
                            }
                            None => {
                                // End of audio: ask Deepgram to finalize.
                                tracing::debug!(frames_sent, "microphone stream ended; finalizing");
                                audio_open = false;
                                let _ = sink.send(Message::Text(
                                    r#"{"type":"CloseStream"}"#.to_string().into()
                                )).await;
                            }
                        }
                    }

                    _ = &mut deadline => {
                        tracing::warn!(?max_utterance, "stt session hit its ceiling; closing");
                        let _ = sink.send(Message::Text(r#"{"type":"CloseStream"}"#.to_string().into())).await;
                        let _ = event_tx.send(SttEvent::Closed(Some("max utterance exceeded".into()))).await;
                        return;
                    }
                }
            }
        });

        Ok((audio_tx, event_rx))
    }
}

/// Decode one Deepgram frame into an event.
///
/// Shape: `{"type":"Results","channel":{"alternatives":[{"transcript":"..."}]},
/// "is_final":bool,"speech_final":bool}`. Empty transcripts (silence) are dropped.
pub fn parse_event(raw: &str) -> Option<SttEvent> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;

    match v.get("type").and_then(|t| t.as_str()) {
        Some("Results") | None => {}
        Some("UtteranceEnd") => return None, // covered by speech_final
        Some("SpeechStarted") => return None,
        Some("Error") => {
            let msg = v.get("description").and_then(|d| d.as_str()).unwrap_or("deepgram error");
            return Some(SttEvent::Closed(Some(msg.to_string())));
        }
        Some(_) => return None,
    }

    let transcript = v
        .get("channel")?
        .get("alternatives")?
        .as_array()?
        .first()?
        .get("transcript")?
        .as_str()?
        .trim()
        .to_string();

    if transcript.is_empty() {
        return None;
    }

    let is_final = v.get("is_final").and_then(|x| x.as_bool()).unwrap_or(false);
    let speech_final = v.get("speech_final").and_then(|x| x.as_bool()).unwrap_or(false);

    Some(if speech_final {
        SttEvent::EndOfSpeech(transcript)
    } else if is_final {
        SttEvent::Final(transcript)
    } else {
        SttEvent::Interim(transcript)
    })
}

/// Accumulates finalized segments into the complete utterance.
///
/// Deepgram emits an utterance as a series of finalized segments; the last one is
/// not the whole thing. Joining them is what turns "what's the" + "weather in Oslo"
/// into a usable question.
#[derive(Debug, Default)]
pub struct TranscriptBuilder {
    finalized: Vec<String>,
    interim: String,
}

impl TranscriptBuilder {
    pub fn apply(&mut self, ev: &SttEvent) {
        match ev {
            SttEvent::Interim(t) => self.interim = t.clone(),
            SttEvent::Final(t) | SttEvent::EndOfSpeech(t) => {
                self.finalized.push(t.clone());
                self.interim.clear();
            }
            SttEvent::Closed(_) => {}
        }
    }

    /// Best guess right now, including the unstable tail — for prefetch only.
    pub fn provisional(&self) -> String {
        let mut s = self.finalized.join(" ");
        if !self.interim.is_empty() {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(&self.interim);
        }
        s.trim().to_string()
    }

    /// The finalized utterance — what actually gets answered.
    pub fn finished(&self) -> String {
        self.finalized.join(" ").trim().to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.finalized.is_empty() && self.interim.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results(transcript: &str, is_final: bool, speech_final: bool) -> String {
        serde_json::json!({
            "type": "Results",
            "channel": { "alternatives": [ { "transcript": transcript } ] },
            "is_final": is_final,
            "speech_final": speech_final,
        })
        .to_string()
    }

    #[test]
    fn url_pins_the_wire_format_deepgram_needs() {
        let d = Deepgram {
            url: "wss://api.deepgram.com/v1/listen".into(),
            api_key: "k".into(),
            model: "nova-3".into(),
            language: "en-US".into(),
            endpointing_ms: 300,
            sample_rate: 16_000,
            max_utterance: Duration::from_secs(20),
        };
        let u = d.connect_url();
        assert!(u.contains("encoding=linear16"));
        assert!(u.contains("sample_rate=16000"));
        assert!(u.contains("channels=1"));
        assert!(u.contains("interim_results=true"));
        assert!(u.contains("endpointing=300"));
        assert!(u.contains("model=nova-3"));
    }

    #[test]
    fn classifies_interim_final_and_end_of_speech() {
        assert_eq!(
            parse_event(&results("what's the", false, false)),
            Some(SttEvent::Interim("what's the".into()))
        );
        assert_eq!(
            parse_event(&results("what's the weather", true, false)),
            Some(SttEvent::Final("what's the weather".into()))
        );
        assert_eq!(
            parse_event(&results("in Oslo", true, true)),
            Some(SttEvent::EndOfSpeech("in Oslo".into()))
        );
    }

    #[test]
    fn silence_produces_no_event() {
        assert_eq!(parse_event(&results("", true, false)), None);
        assert_eq!(parse_event(&results("   ", false, false)), None);
    }

    #[test]
    fn malformed_frames_are_ignored_not_fatal() {
        assert_eq!(parse_event("not json"), None);
        assert_eq!(parse_event("{}"), None);
        assert_eq!(parse_event(r#"{"type":"Results","channel":{}}"#), None);
    }

    #[test]
    fn error_frames_close_the_session() {
        let e = parse_event(r#"{"type":"Error","description":"bad audio"}"#);
        assert_eq!(e, Some(SttEvent::Closed(Some("bad audio".into()))));
    }

    #[test]
    fn metadata_frames_are_skipped() {
        assert_eq!(parse_event(r#"{"type":"Metadata","request_id":"x"}"#), None);
        assert_eq!(parse_event(r#"{"type":"SpeechStarted"}"#), None);
    }

    #[test]
    fn builder_joins_segments_into_one_utterance() {
        let mut b = TranscriptBuilder::default();
        b.apply(&SttEvent::Interim("what's".into()));
        b.apply(&SttEvent::Final("what's the weather".into()));
        b.apply(&SttEvent::Interim("in".into()));
        assert_eq!(b.provisional(), "what's the weather in");
        assert_eq!(b.finished(), "what's the weather", "interim must not reach the answer path");

        b.apply(&SttEvent::EndOfSpeech("in Oslo tomorrow".into()));
        assert_eq!(b.finished(), "what's the weather in Oslo tomorrow");
        assert_eq!(b.provisional(), "what's the weather in Oslo tomorrow");
    }

    #[test]
    fn builder_starts_empty() {
        let b = TranscriptBuilder::default();
        assert!(b.is_empty());
        assert_eq!(b.finished(), "");
    }

    #[test]
    fn provisional_transcript_can_trigger_prefetch_before_end_of_speech() {
        let mut b = TranscriptBuilder::default();
        b.apply(&SttEvent::Interim("what is the current price of".into()));
        assert!(
            crate::router::should_prefetch(&b.provisional()),
            "this is exactly the case speculative prefetch exists for"
        );
        assert!(b.finished().is_empty(), "and nothing is final yet");
    }
}
