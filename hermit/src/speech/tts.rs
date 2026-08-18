//! Streaming text-to-speech.
//!
//! Primary: Cartesia Sonic over a **persistent** websocket. Alternate: ElevenLabs
//! Flash. Offline fallback: Piper, lazy-spawned, never resident.
//!
//! # Why the socket is held open
//!
//! A cold TLS handshake is 100–400 ms (spec §5). The first-audio budget is 1.2 s
//! total. Opening a websocket per utterance would spend a third of the budget before
//! a single character of text had been sent. So one connection is established at
//! boot, kept alive, and reused for every utterance with a fresh `context_id`. A
//! dropped connection is re-established lazily on the next utterance and, failing
//! that, in the background.
//!
//! # Sample rate
//!
//! PCM is requested at exactly the card's native rate (`tts.sample_rate`), so
//! nothing resamples on the Pi. If Phase 0 shows the card only accepts 16 kHz, that
//! is what we ask for and the resampler never runs.

use crate::audio::AudioPlayer;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Result of speaking one utterance.
#[derive(Debug, Clone, Default)]
pub struct SpeakResult {
    /// Milliseconds from first text sent to first audio sample queued.
    pub ttfa_ms: Option<f64>,
    pub samples: usize,
    /// True if playback was cut short by a barge-in.
    pub interrupted: bool,
}

impl SpeakResult {
    pub fn completed(&self) -> bool {
        self.samples > 0 && !self.interrupted
    }
}

/// Text handed to the speaker, one chunk at a time from the sentence chunker.
pub type TextRx = tokio::sync::mpsc::Receiver<String>;

pub enum Tts {
    Cartesia(CartesiaTts),
    ElevenLabs(ElevenLabsTts),
    Piper(PiperTts),
    /// No provider configured — text-only operation.
    Disabled,
}

impl Tts {
    /// Build from config + environment. Falls back to Piper, then Disabled, rather
    /// than failing to boot: a device that answers in text is better than one that
    /// will not start.
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        match cfg.tts.provider.as_str() {
            "cartesia" => match crate::http::secret_opt("CARTESIA_API_KEY") {
                Some(key) => Tts::Cartesia(CartesiaTts::new(&cfg.tts, key)),
                None => {
                    tracing::warn!("CARTESIA_API_KEY not set; falling back to Piper");
                    Self::piper_or_disabled(cfg)
                }
            },
            "elevenlabs" => match crate::http::secret_opt("ELEVENLABS_API_KEY") {
                Some(key) => Tts::ElevenLabs(ElevenLabsTts::new(&cfg.tts, key)),
                None => {
                    tracing::warn!("ELEVENLABS_API_KEY not set; falling back to Piper");
                    Self::piper_or_disabled(cfg)
                }
            },
            "piper" => Self::piper_or_disabled(cfg),
            other => {
                tracing::error!(provider = other, "unknown tts.provider");
                Tts::Disabled
            }
        }
    }

    fn piper_or_disabled(cfg: &crate::config::Config) -> Self {
        if cfg.tts.piper_binary.exists() {
            Tts::Piper(PiperTts::new(&cfg.tts))
        } else {
            tracing::warn!(
                path = %cfg.tts.piper_binary.display(),
                "no TTS available; the daemon will answer in text only"
            );
            Tts::Disabled
        }
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self, Tts::Disabled)
    }

    /// Open the connection ahead of first use.
    pub async fn prewarm(&self) {
        match self {
            Tts::Cartesia(c) => c.prewarm().await,
            Tts::ElevenLabs(e) => e.prewarm().await,
            _ => {}
        }
    }

    /// Speak a stream of text chunks, writing PCM into `player`.
    ///
    /// `generation` is the audio generation at the time the turn began. If a
    /// barge-in bumps it, this returns early with `interrupted = true`.
    pub async fn speak(
        &self,
        text_rx: TextRx,
        player: &AudioPlayer,
        generation: u64,
    ) -> Result<SpeakResult> {
        match self {
            Tts::Cartesia(c) => c.speak(text_rx, player, generation).await,
            Tts::ElevenLabs(e) => e.speak(text_rx, player, generation).await,
            Tts::Piper(p) => p.speak(text_rx, player, generation).await,
            Tts::Disabled => {
                // Drain so the producer is not left blocked on a full channel.
                let mut rx = text_rx;
                while rx.recv().await.is_some() {}
                Ok(SpeakResult::default())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cartesia
// ---------------------------------------------------------------------------

pub struct CartesiaTts {
    url: String,
    api_key: String,
    model: String,
    voice_id: String,
    version: String,
    sample_rate: u32,
    max_buffer_delay_ms: u32,
    connect_timeout: Duration,
    conn: Arc<Mutex<Option<WsStream>>>,
    context_seq: std::sync::atomic::AtomicU64,
}

impl CartesiaTts {
    pub fn new(cfg: &crate::config::Tts, api_key: String) -> Self {
        Self {
            url: cfg.cartesia_url.clone(),
            api_key,
            model: cfg.cartesia_model.clone(),
            voice_id: cfg.cartesia_voice_id.clone(),
            version: cfg.cartesia_version.clone(),
            sample_rate: cfg.sample_rate,
            max_buffer_delay_ms: cfg.cartesia_max_buffer_delay_ms,
            connect_timeout: Duration::from_millis(cfg.connect_timeout_ms),
            conn: Arc::new(Mutex::new(None)),
            context_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn connect_url(&self) -> String {
        // The key travels as a query parameter because the websocket handshake in
        // tungstenite cannot carry custom headers without building the request by
        // hand; Cartesia accepts either form.
        format!(
            "{}?cartesia_version={}&api_key={}",
            self.url,
            urlencoding::encode(&self.version),
            urlencoding::encode(&self.api_key)
        )
    }

    async fn connect(&self) -> Result<WsStream> {
        let url = self.connect_url();
        let (ws, _resp) =
            tokio::time::timeout(self.connect_timeout, tokio_tungstenite::connect_async(&url))
                .await
                .map_err(|_| anyhow::anyhow!("cartesia websocket connect timed out"))?
                .context("connecting to cartesia websocket")?;
        tracing::info!("cartesia websocket connected");
        Ok(ws)
    }

    pub async fn prewarm(&self) {
        let mut guard = self.conn.lock().await;
        if guard.is_some() {
            return;
        }
        match self.connect().await {
            Ok(ws) => *guard = Some(ws),
            Err(e) => tracing::warn!(error = %e, "cartesia prewarm failed; will retry on demand"),
        }
    }

    fn request(&self, context_id: &str, transcript: &str, continue_: bool) -> serde_json::Value {
        serde_json::json!({
            "model_id": self.model,
            "transcript": transcript,
            "voice": { "mode": "id", "id": self.voice_id },
            "output_format": {
                "container": "raw",
                "encoding": "pcm_s16le",
                "sample_rate": self.sample_rate,
            },
            "language": "en",
            "context_id": context_id,
            "continue": continue_,
            // The provider default is 3000ms — see config::validate.
            "max_buffer_delay_ms": self.max_buffer_delay_ms,
        })
    }

    async fn speak(
        &self,
        mut text_rx: TextRx,
        player: &AudioPlayer,
        generation: u64,
    ) -> Result<SpeakResult> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect().await?);
        }

        let ctx_id = format!(
            "hermit-{}",
            self.context_seq
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );

        let result = self
            .stream_utterance(
                guard.as_mut().unwrap(),
                &ctx_id,
                &mut text_rx,
                player,
                generation,
            )
            .await;

        match result {
            Ok(r) => Ok(r),
            Err(e) => {
                // A failed utterance usually means a dead socket. Drop it so the
                // next call reconnects rather than failing forever.
                tracing::warn!(error = %e, "cartesia utterance failed; dropping connection");
                *guard = None;
                // Drain remaining text so the producer is not wedged.
                while text_rx.recv().await.is_some() {}
                Err(e)
            }
        }
    }

    async fn stream_utterance(
        &self,
        ws: &mut WsStream,
        ctx_id: &str,
        text_rx: &mut TextRx,
        player: &AudioPlayer,
        generation: u64,
    ) -> Result<SpeakResult> {
        let mut result = SpeakResult::default();
        let started = Instant::now();
        let mut sent_any = false;
        let mut text_done = false;

        loop {
            if player.is_stale(generation) {
                result.interrupted = true;
                break;
            }

            tokio::select! {
                biased;

                // Prefer draining audio so first-audio latency is never delayed by
                // text arriving.
                msg = ws.next() => {
                    let Some(msg) = msg else { bail!("cartesia closed the connection") };
                    match msg.context("cartesia websocket error")? {
                        Message::Text(t) => {
                            match self.handle_frame(&t, ctx_id, player, generation, &mut result, started).await {
                                FrameOutcome::Continue => {}
                                FrameOutcome::Done => break,
                                FrameOutcome::Stale => { result.interrupted = true; break }
                                FrameOutcome::Failed => bail!("cartesia returned an error frame"),
                            }
                        }
                        Message::Binary(_) => {} // Cartesia sends JSON only
                        Message::Close(_) => bail!("cartesia closed the connection"),
                        Message::Ping(p) => { ws.send(Message::Pong(p)).await.ok(); }
                        _ => {}
                    }
                }

                chunk = text_rx.recv(), if !text_done => {
                    match chunk {
                        Some(text) if !text.trim().is_empty() => {
                            let payload = self.request(ctx_id, &format!("{} ", text.trim()), true);
                            ws.send(Message::Text(payload.to_string().into())).await
                                .context("sending transcript to cartesia")?;
                            sent_any = true;
                        }
                        Some(_) => {}
                        None => {
                            text_done = true;
                            if !sent_any {
                                return Ok(result); // nothing to say
                            }
                            // Empty transcript with continue=false closes the context
                            // and tells Cartesia to flush the tail.
                            let payload = self.request(ctx_id, "", false);
                            ws.send(Message::Text(payload.to_string().into())).await
                                .context("closing cartesia context")?;
                        }
                    }
                }
            }
        }

        Ok(result)
    }

    async fn handle_frame(
        &self,
        text: &str,
        ctx_id: &str,
        player: &AudioPlayer,
        generation: u64,
        result: &mut SpeakResult,
        started: Instant,
    ) -> FrameOutcome {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
            return FrameOutcome::Continue;
        };
        // Ignore frames belonging to an older context (a previous utterance).
        if let Some(id) = v.get("context_id").and_then(|x| x.as_str())
            && id != ctx_id
        {
            return FrameOutcome::Continue;
        }

        match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "chunk" => {
                let Some(b64) = v.get("data").and_then(|x| x.as_str()) else {
                    return FrameOutcome::Continue;
                };
                let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else {
                    tracing::warn!("cartesia sent undecodable audio");
                    return FrameOutcome::Continue;
                };
                let samples = crate::audio::pcm_s16le_to_samples(&bytes);
                if samples.is_empty() {
                    return FrameOutcome::Continue;
                }
                if result.ttfa_ms.is_none() {
                    result.ttfa_ms = Some(crate::metrics::ms_since(started));
                }
                result.samples += samples.len();
                if player.is_stale(generation) {
                    return FrameOutcome::Stale;
                }
                if player.play(samples).await {
                    FrameOutcome::Continue
                } else {
                    FrameOutcome::Failed
                }
            }
            "done" => FrameOutcome::Done,
            "error" => {
                tracing::error!(frame = %text, "cartesia error frame");
                FrameOutcome::Failed
            }
            _ => FrameOutcome::Continue,
        }
    }
}

enum FrameOutcome {
    Continue,
    Done,
    Stale,
    Failed,
}

// ---------------------------------------------------------------------------
// ElevenLabs Flash
// ---------------------------------------------------------------------------

pub struct ElevenLabsTts {
    base_url: String,
    api_key: String,
    model: String,
    voice_id: String,
    sample_rate: u32,
    connect_timeout: Duration,
    conn: Arc<Mutex<Option<WsStream>>>,
}

impl ElevenLabsTts {
    pub fn new(cfg: &crate::config::Tts, api_key: String) -> Self {
        Self {
            base_url: cfg.elevenlabs_url.clone(),
            api_key,
            model: cfg.elevenlabs_model.clone(),
            voice_id: cfg.elevenlabs_voice_id.clone(),
            sample_rate: cfg.sample_rate,
            connect_timeout: Duration::from_millis(cfg.connect_timeout_ms),
            conn: Arc::new(Mutex::new(None)),
        }
    }

    /// ElevenLabs supports pcm_16000/22050/24000/44100. Anything else must be
    /// resampled, which we avoid by asking Phase 0 for the card's native rate.
    fn output_format(&self) -> String {
        format!("pcm_{}", self.sample_rate)
    }

    fn connect_url(&self) -> String {
        format!(
            "{}/{}/stream-input?model_id={}&output_format={}&auto_mode=true",
            self.base_url.trim_end_matches('/'),
            self.voice_id,
            urlencoding::encode(&self.model),
            self.output_format()
        )
    }

    async fn connect(&self) -> Result<WsStream> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = self.connect_url().into_client_request()?;
        req.headers_mut().insert(
            "xi-api-key",
            self.api_key.parse().context("invalid ELEVENLABS_API_KEY")?,
        );
        let (ws, _) =
            tokio::time::timeout(self.connect_timeout, tokio_tungstenite::connect_async(req))
                .await
                .map_err(|_| anyhow::anyhow!("elevenlabs websocket connect timed out"))?
                .context("connecting to elevenlabs websocket")?;
        tracing::info!("elevenlabs websocket connected");
        Ok(ws)
    }

    pub async fn prewarm(&self) {
        let mut guard = self.conn.lock().await;
        if guard.is_some() {
            return;
        }
        match self.connect().await {
            Ok(ws) => *guard = Some(ws),
            Err(e) => tracing::warn!(error = %e, "elevenlabs prewarm failed"),
        }
    }

    async fn speak(
        &self,
        mut text_rx: TextRx,
        player: &AudioPlayer,
        generation: u64,
    ) -> Result<SpeakResult> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect().await?);
        }
        let ws = guard.as_mut().unwrap();

        let mut result = SpeakResult::default();
        let started = Instant::now();

        // Protocol: an initial message opens the stream, then text chunks, then an
        // empty string closes it.
        ws.send(Message::Text(
            serde_json::json!({
                "text": " ",
                "voice_settings": { "stability": 0.5, "similarity_boost": 0.8 },
            })
            .to_string()
            .into(),
        ))
        .await?;

        let mut text_done = false;
        loop {
            if player.is_stale(generation) {
                result.interrupted = true;
                break;
            }
            tokio::select! {
                biased;
                msg = ws.next() => {
                    let Some(msg) = msg else { break };
                    match msg? {
                        Message::Text(t) => {
                            let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                            if let Some(b64) = v.get("audio").and_then(|x| x.as_str())
                                && !b64.is_empty()
                                && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64)
                            {
                                let samples = crate::audio::pcm_s16le_to_samples(&bytes);
                                if !samples.is_empty() {
                                    if result.ttfa_ms.is_none() {
                                        result.ttfa_ms = Some(crate::metrics::ms_since(started));
                                    }
                                    result.samples += samples.len();
                                    if !player.play(samples).await {
                                        result.interrupted = true;
                                        break;
                                    }
                                }
                            }
                            if v.get("isFinal").and_then(|x| x.as_bool()).unwrap_or(false) {
                                break;
                            }
                        }
                        Message::Close(_) => break,
                        Message::Ping(p) => { ws.send(Message::Pong(p)).await.ok(); }
                        _ => {}
                    }
                }
                chunk = text_rx.recv(), if !text_done => {
                    match chunk {
                        Some(text) if !text.trim().is_empty() => {
                            ws.send(Message::Text(
                                serde_json::json!({ "text": format!("{} ", text.trim()), "flush": true })
                                    .to_string().into(),
                            )).await?;
                        }
                        Some(_) => {}
                        None => {
                            text_done = true;
                            ws.send(Message::Text(serde_json::json!({ "text": "" }).to_string().into())).await?;
                        }
                    }
                }
            }
        }

        // ElevenLabs ties a connection to one generation; reopen for the next.
        *guard = None;
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Piper (offline fallback)
// ---------------------------------------------------------------------------

/// Lazy-spawned local TTS for when the network is down.
///
/// Never resident: the process is started per utterance and exits with it, so it
/// costs nothing against the RAM budget when the network is healthy (spec §3).
pub struct PiperTts {
    binary: std::path::PathBuf,
    voice: std::path::PathBuf,
}

impl PiperTts {
    pub fn new(cfg: &crate::config::Tts) -> Self {
        Self {
            binary: cfg.piper_binary.clone(),
            voice: cfg.piper_voice.clone(),
        }
    }

    async fn speak(
        &self,
        mut text_rx: TextRx,
        player: &AudioPlayer,
        generation: u64,
    ) -> Result<SpeakResult> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut result = SpeakResult::default();
        let started = Instant::now();

        // Collect the whole utterance: Piper is not a streaming engine, so there is
        // no first-clause advantage to be had here. This path is the degraded one.
        let mut text = String::new();
        while let Some(chunk) = text_rx.recv().await {
            text.push_str(chunk.trim());
            text.push(' ');
        }
        if text.trim().is_empty() {
            return Ok(result);
        }

        let mut child = tokio::process::Command::new(&self.binary)
            .arg("--model")
            .arg(&self.voice)
            .arg("--output-raw")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("spawning piper at {}", self.binary.display()))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let mut stdout = child.stdout.take().context("piper produced no stdout")?;
        let mut buf = vec![0u8; 4096];
        loop {
            if player.is_stale(generation) {
                result.interrupted = true;
                let _ = child.kill().await;
                break;
            }
            let n = stdout.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            let samples = crate::audio::pcm_s16le_to_samples(&buf[..n]);
            if !samples.is_empty() {
                if result.ttfa_ms.is_none() {
                    result.ttfa_ms = Some(crate::metrics::ms_since(started));
                }
                result.samples += samples.len();
                if !player.play(samples).await {
                    result.interrupted = true;
                    break;
                }
            }
        }
        let _ = child.wait().await;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tts_cfg() -> crate::config::Tts {
        crate::config::Tts {
            cartesia_voice_id: "voice-abc".into(),
            elevenlabs_voice_id: "el-voice".into(),
            ..Default::default()
        }
    }

    #[test]
    fn cartesia_request_pins_rate_encoding_and_zero_buffer_delay() {
        let t = CartesiaTts::new(&tts_cfg(), "key".into());
        let r = t.request("ctx1", "hello", true);
        assert_eq!(r["output_format"]["encoding"], "pcm_s16le");
        assert_eq!(r["output_format"]["container"], "raw");
        assert_eq!(r["output_format"]["sample_rate"], 16000);
        assert_eq!(
            r["max_buffer_delay_ms"], 0,
            "the provider default of 3000ms would blow the first-audio budget on its own"
        );
        assert_eq!(r["continue"], true);
        assert_eq!(r["context_id"], "ctx1");
        assert_eq!(r["voice"]["id"], "voice-abc");
    }

    #[test]
    fn cartesia_final_frame_closes_the_context() {
        let t = CartesiaTts::new(&tts_cfg(), "key".into());
        let r = t.request("ctx1", "", false);
        assert_eq!(r["continue"], false);
        assert_eq!(r["transcript"], "");
    }

    #[test]
    fn cartesia_url_encodes_credentials() {
        let mut c = tts_cfg();
        c.cartesia_url = "wss://api.cartesia.ai/tts/websocket".into();
        let t = CartesiaTts::new(&c, "key/with+chars".into());
        let u = t.connect_url();
        assert!(u.contains("cartesia_version=2026-08-14"));
        assert!(
            u.contains("key%2Fwith%2Bchars"),
            "credentials must be percent-encoded: {u}"
        );
    }

    #[test]
    fn elevenlabs_url_carries_voice_model_and_format() {
        let t = ElevenLabsTts::new(&tts_cfg(), "k".into());
        let u = t.connect_url();
        assert!(u.contains("/el-voice/stream-input"));
        assert!(u.contains("output_format=pcm_16000"));
        assert!(u.contains("eleven_flash_v2_5"));
    }

    #[test]
    fn output_format_follows_the_configured_rate() {
        let mut c = tts_cfg();
        c.sample_rate = 48_000;
        let t = ElevenLabsTts::new(&c, "k".into());
        assert_eq!(t.output_format(), "pcm_48000");
    }

    #[tokio::test]
    async fn disabled_tts_drains_text_instead_of_deadlocking_the_producer() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let player =
            AudioPlayer::spawn_with(Box::new(crate::audio::NullBackend::new(16_000)), 16_000)
                .unwrap();

        let producer = tokio::spawn(async move {
            for i in 0..20 {
                if tx.send(format!("chunk {i}")).await.is_err() {
                    return false;
                }
            }
            true
        });

        let r = Tts::Disabled.speak(rx, &player, 0).await.unwrap();
        assert_eq!(r.samples, 0);
        assert!(producer.await.unwrap(), "producer must not be left blocked");
    }

    #[test]
    fn provider_selection_falls_back_rather_than_failing() {
        unsafe {
            std::env::remove_var("CARTESIA_API_KEY");
            std::env::remove_var("ELEVENLABS_API_KEY");
        }
        let mut cfg = crate::config::Config::default();
        cfg.tts.piper_binary = "/definitely/not/here/piper".into();
        let t = Tts::from_config(&cfg);
        assert!(
            !t.is_enabled(),
            "no keys and no piper => text-only, but still boots"
        );
    }
}
