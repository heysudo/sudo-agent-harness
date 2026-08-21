//! Configuration: every tunable lives in `config/hermit.toml` and every prompt in
//! `config/prompts/*.md`. Nothing here requires a rebuild to change — the daemon
//! watches the config directory with `notify` and swaps a new `Arc<Config>` in on
//! write (see [`watch`]).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Root config, deserialized from `hermit.toml`. Every field has a default so a
/// partial file is legal and an upgrade never breaks an existing deployment.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub paths: Paths,
    pub llm: Llm,
    pub search: Search,
    pub fetch: Fetch,
    pub news: News,
    pub music: Music,
    pub tts: Tts,
    pub stt: Stt,
    pub wake: Wake,
    pub feedback: Feedback,
    pub audio: Audio,
    pub memory: Memory,
    pub reflect: Reflect,
    pub research: Research,

    /// Absolute path this config was loaded from. Filled in by [`load`], not parsed.
    #[serde(skip)]
    pub source_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    /// Local WebSocket text-client bind address. Loopback only by default.
    /// Binding beyond loopback requires HERMIT_WS_TOKEN to be set — the daemon
    /// refuses to start otherwise (see [`Config::validate`]).
    pub ws_bind: String,
    /// Read text turns from stdin and write answers to stdout.
    pub cli: bool,
    /// Allow WebSocket clients to send `/listen` and remotely arm the
    /// microphone. Off by default: arming the mic is a control-plane action
    /// and needs an explicit operator decision, not just network reachability.
    pub ws_allow_listen: bool,
    /// Maximum concurrent WebSocket connections. Turns queue serially against
    /// the turn lock, so each connection is standing permission to spend LLM
    /// and tool budget — keep this small.
    pub ws_max_connections: usize,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            ws_bind: "127.0.0.1:8765".into(),
            cli: true,
            ws_allow_listen: false,
            ws_max_connections: 4,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Paths {
    /// SQLite database + mutable markdown (core.md) live here.
    pub data_dir: PathBuf,
    /// Read-only-ish config tree: prompts/, skills/, identity.md, stations.toml.
    pub config_dir: PathBuf,
    /// tmpfs directory for pre-synthesized acknowledgment WAVs.
    pub ack_dir: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            data_dir: "/var/lib/hermit".into(),
            config_dir: "/opt/hermit/config".into(),
            ack_dir: "/var/lib/hermit/acks".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Llm {
    pub base_url: String,
    pub model: String,
    /// `reasoning_effort` for chat and device-adjacent turns. Lowest latency.
    pub reasoning_effort_default: String,
    /// `reasoning_effort` for research-classified queries only.
    pub reasoning_effort_research: String,
    pub max_tokens: u32,
    pub temperature: f32,
    /// HARD CAP on interactive tool rounds (spec §4.3). Anything more becomes
    /// background_research.
    pub max_tool_rounds: usize,
    pub request_timeout_ms: u64,
}

impl Default for Llm {
    fn default() -> Self {
        Self {
            base_url: "https://api.cerebras.ai/v1".into(),
            model: "gpt-oss-120b".into(),
            reasoning_effort_default: "low".into(),
            reasoning_effort_research: "medium".into(),
            max_tokens: 1024,
            temperature: 0.6,
            max_tool_rounds: 2,
            request_timeout_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Search {
    pub base_url: String,
    /// MUST be "turbo". The Parallel API defaults to "advanced" when unset, which
    /// is an order of magnitude slower. We always send this explicitly.
    pub mode: String,
    /// Turbo is English + Japanese only; other languages fall back to this mode.
    pub fallback_mode: String,
    pub max_results: u32,
    pub timeout_ms: u64,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            base_url: "https://api.parallel.ai".into(),
            mode: "turbo".into(),
            fallback_mode: "base".into(),
            max_results: 5,
            timeout_ms: 4_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Fetch {
    pub base_url: String,
    pub timeout_ms: u64,
    /// Hard cap on page text handed to the model (spec §6).
    pub max_tokens: usize,
}

impl Default for Fetch {
    fn default() -> Self {
        Self {
            base_url: "https://api.firecrawl.dev".into(),
            timeout_ms: 15_000,
            max_tokens: 4_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct News {
    pub feeds: Vec<FeedSpec>,
    pub items_per_feed: usize,
    pub target_words: (usize, usize),
    pub timeout_ms: u64,
}

impl Default for News {
    fn default() -> Self {
        Self {
            feeds: vec![
                FeedSpec {
                    name: "BBC".into(),
                    url: "https://feeds.bbci.co.uk/news/world/rss.xml".into(),
                },
                FeedSpec {
                    name: "Reuters".into(),
                    url: "https://www.reutersagency.com/feed/?best-topics=top-news&post_type=best"
                        .into(),
                },
                FeedSpec {
                    name: "NPR".into(),
                    url: "https://feeds.npr.org/1001/rss.xml".into(),
                },
            ],
            items_per_feed: 6,
            target_words: (150, 250),
            timeout_ms: 5_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedSpec {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Music {
    /// Unix socket for `mpv --input-ipc-server`.
    pub mpv_socket: PathBuf,
    /// Spotify Connect device name librespot advertises.
    pub librespot_device_name: String,
    pub spotify_api_base: String,
    /// Named internet-radio stations, loaded from `stations.toml`.
    pub stations_file: PathBuf,
    /// Volume (0-100) restored after ducking.
    pub default_volume: u8,
}

impl Default for Music {
    fn default() -> Self {
        Self {
            mpv_socket: "/run/hermit/mpv.sock".into(),
            librespot_device_name: "Hermit".into(),
            spotify_api_base: "https://api.spotify.com/v1".into(),
            stations_file: "stations.toml".into(),
            default_volume: 70,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Tts {
    /// "cartesia" | "elevenlabs" | "piper" (offline fallback only).
    pub provider: String,
    pub cartesia_url: String,
    pub cartesia_model: String,
    pub cartesia_voice_id: String,
    pub cartesia_version: String,
    /// Cartesia buffers this long before it starts generating. The API DEFAULT IS
    /// 3000 ms, which on its own blows the 1.2 s first-audio gate — always send 0.
    pub cartesia_max_buffer_delay_ms: u32,
    pub elevenlabs_url: String,
    pub elevenlabs_model: String,
    pub elevenlabs_voice_id: String,
    /// Ask the provider for PCM at exactly the card's native rate so nothing
    /// resamples on the Pi. Set from Phase 0 `--dump-hw-params` output.
    pub sample_rate: u32,
    pub piper_binary: PathBuf,
    pub piper_voice: PathBuf,
    pub connect_timeout_ms: u64,
    /// Sarvam Bulbul v3 TTS for Hindi / Odia / Indian-English.
    pub sarvam_tts_url: String,
    /// TTS language code. NOTE: Bulbul v3 uses "od-IN" for Odia while the STT
    /// service uses "or-IN" for the same language — Sarvam is inconsistent between
    /// its own services and each rejects the other's code. Both verified live.
    /// TTS-supported: od-IN, hi-IN, en-IN, bn-IN, ta-IN, te-IN, kn-IN, ml-IN,
    /// mr-IN, gu-IN, pa-IN. (as-IN is NOT supported by TTS.)
    pub sarvam_tts_language: String,
    /// Bulbul v3 speaker name, lowercase (e.g. "shubh").
    pub sarvam_tts_voice: String,
}

impl Default for Tts {
    fn default() -> Self {
        Self {
            provider: "cartesia".into(),
            cartesia_url: "wss://api.cartesia.ai/tts/websocket".into(),
            cartesia_model: "sonic-3".into(),
            cartesia_voice_id: "".into(),
            cartesia_version: "2026-08-14".into(),
            cartesia_max_buffer_delay_ms: 0,
            elevenlabs_url: "wss://api.elevenlabs.io/v1/text-to-speech".into(),
            elevenlabs_model: "eleven_flash_v2_5".into(),
            elevenlabs_voice_id: "".into(),
            sample_rate: 16_000,
            piper_binary: "/usr/local/bin/piper".into(),
            piper_voice: "/opt/hermit/config/voices/en_US-lessac-low.onnx".into(),
            connect_timeout_ms: 3_000,
            sarvam_tts_url: "wss://api.sarvam.ai/text-to-speech/ws".into(),
            sarvam_tts_language: "od-IN".into(),
            sarvam_tts_voice: "shubh".into(),
        }
    }
}

/// The turn-feedback loop: after an answer, sometimes ask "did I get that
/// right?", listen briefly, and learn from the reply. `ask_probability`
/// decays as confirmation rate rises (handled in tuning state, not here).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Feedback {
    /// Master switch for the ask-confirm-learn loop.
    pub enabled: bool,
    /// Seconds of the answer/music the user hears before the ask. 0 = ask
    /// immediately after speech completes.
    pub settle_secs: u64,
    /// How long the yes/no listen window stays open (ms).
    pub listen_ms: u64,
    /// Starting probability of asking after an eligible turn (0..1).
    pub ask_probability: f64,
    /// Hard floor/ceiling the tuner may move ask_probability within.
    pub min_ask_probability: f64,
    /// Wake clips: keep at most this many confirmed/denied clips on disk.
    pub max_clips: u64,
}

impl Default for Feedback {
    fn default() -> Self {
        Self {
            enabled: true,
            settle_secs: 20,
            listen_ms: 6000,
            ask_probability: 1.0,
            min_ask_probability: 0.15,
            max_clips: 400,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Stt {
    pub url: String,
    pub model: String,
    pub language: String,
    /// Deepgram endpointing in milliseconds of trailing silence.
    pub endpointing_ms: u32,
    pub sample_rate: u32,
    /// Hard cap on how long the mic stays open after the wake word. When reached,
    /// the turn finalizes with whatever was transcribed (interim included) rather
    /// than listening forever on a noisy line that never trips endpointing.
    pub max_utterance_ms: u64,

    /// Which STT backend to use. "deepgram" (English-only) or "sarvam" (Hindi /
    /// Odia / Indian-English). Default "deepgram" to keep existing behavior.
    pub provider: String,
    /// Sarvam realtime websocket endpoint.
    pub sarvam_url: String,
    /// Sarvam language code, or "auto" for per-utterance detection.
    /// Verified live: auto, hi-IN, or-IN (Odia), en-IN, bn-IN, ta-IN, te-IN,
    /// kn-IN, ml-IN, mr-IN, gu-IN, pa-IN, as-IN, ur-IN, + 10 more. NOT "od-IN".
    pub sarvam_language: String,
    /// Latency/accuracy tradeoff: "fast" | "balanced" | "simulated".
    pub sarvam_stream_type: String,
    /// Server-side VAD silence before finalizing an utterance (ms).
    pub sarvam_silence_ms: u32,
    /// Server-side VAD sensitivity (0..1, lower = more sensitive). The
    /// default 0.3 misses quiet beamformed mic audio; 0.15 catches it.
    pub sarvam_vad_threshold: f32,
    /// Software gain on mic samples before upload (XVF3800 output is quiet).
    pub sarvam_gain: f32,
    /// Follow-up conversation window: after a spoken answer the mic re-opens
    /// for this long, and a command spoken within it needs no second wake
    /// word. Only a non-empty transcript arms the turn (music residue and
    /// background chatter produce empty partials and lapse silently); music
    /// stays ducked while the window is open. 0 disables follow-ups.
    pub followup_window_ms: u64,
}

impl Default for Stt {
    fn default() -> Self {
        Self {
            url: "wss://api.deepgram.com/v1/listen".into(),
            model: "nova-3".into(),
            language: "en-US".into(),
            endpointing_ms: 300,
            sample_rate: 16_000,
            max_utterance_ms: 8_000,

            provider: "deepgram".into(),
            sarvam_url: "wss://api.sarvam.ai/speech-to-text-realtime/ws".into(),
            sarvam_language: "auto".into(),
            sarvam_stream_type: "fast".into(),
            sarvam_silence_ms: 500,
            sarvam_vad_threshold: 0.15,
            sarvam_gain: 8.0,
            followup_window_ms: 3_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Wake {
    pub enabled: bool,
    /// Built-in Porcupine keyword name, e.g. "computer", "jarvis".
    pub keyword: String,
    /// Optional path to a custom .ppn keyword file; overrides `keyword`.
    pub keyword_path: Option<PathBuf>,
    pub sensitivity: f32,
    /// Software gain applied to mic samples before wake scoring. The XVF3800's
    /// beamformed output is quiet (~-35 dBFS speech); the same boost that fixed
    /// STT (stt.sarvam_gain) is needed here or music/noise masks the phrase.
    pub gain: f32,
}

impl Default for Wake {
    fn default() -> Self {
        Self {
            enabled: true,
            keyword: "computer".into(),
            keyword_path: None,
            sensitivity: 0.5,
            gain: 8.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    /// ALSA PCM names defined in /etc/asound.conf.
    pub playback_pcm: String,
    pub capture_pcm: String,
    pub sample_rate: u32,
    /// Ring buffer depth in milliseconds. Small = fast barge-in flush.
    pub buffer_ms: u32,
    pub period_ms: u32,
    /// Music attenuation while TTS is speaking.
    pub duck_db: f32,
    /// Keep the playback stream alive with silence when nothing is playing.
    ///
    /// REQUIRED on the reSpeaker XVF3800: its USB capture clock is slaved to the
    /// playback stream, so the microphone returns EIO whenever playback is idle.
    /// Measured on hardware 2026-08-18. Turning this off makes the device deaf.
    pub keepalive_silence: bool,
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            playback_pcm: "hermit_out".into(),
            capture_pcm: "hermit_in".into(),
            sample_rate: 16_000,
            buffer_ms: 200,
            period_ms: 20,
            duck_db: -12.0,
            keepalive_silence: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Memory {
    /// HARD CAP on core.md (spec §9 L2).
    pub core_token_cap: usize,
    /// BM25 top-N injected per turn.
    pub recall_facts: usize,
    pub recall_skills: usize,
    /// Nightly decay multiplier and prune floor.
    pub importance_decay: f64,
    pub prune_below: f64,
    /// Conversation turns kept in the live context window.
    pub history_turns: usize,
    /// Token-set containment above which a candidate fact is treated as a
    /// duplicate of one already stored. 1.0 = every content word already present.
    pub dedupe_similarity: f64,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            core_token_cap: 600,
            recall_facts: 5,
            recall_skills: 2,
            importance_decay: 0.98,
            prune_below: 0.15,
            history_turns: 12,
            dedupe_similarity: 0.8,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reflect {
    pub enabled: bool,
    /// Nudge after this many turns...
    pub turns_per_nudge: usize,
    /// ...or this many seconds idle, whichever comes first.
    pub idle_secs: u64,
    /// Distill a skill file after a successful multi-step tool run.
    pub skill_creation: bool,
}

impl Default for Reflect {
    fn default() -> Self {
        Self {
            enabled: true,
            turns_per_nudge: 6,
            idle_secs: 60,
            skill_creation: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Research {
    /// Off-hot-path round cap for background_research.
    pub max_rounds: usize,
    pub timeout_secs: u64,
    /// Speak the result when it lands, not just store it.
    pub speak_on_complete: bool,
}

impl Default for Research {
    fn default() -> Self {
        Self {
            max_rounds: 8,
            timeout_secs: 300,
            speak_on_complete: true,
        }
    }
}

impl Config {
    pub fn llm_timeout(&self) -> Duration {
        Duration::from_millis(self.llm.request_timeout_ms)
    }

    /// Resolve a path that may be relative to the config directory.
    pub fn resolve(&self, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.config_dir().join(p)
        }
    }

    pub fn config_dir(&self) -> PathBuf {
        if self.paths.config_dir.as_os_str().is_empty() {
            self.source_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."))
        } else {
            self.paths.config_dir.clone()
        }
    }

    /// Sanity checks that would otherwise surface as confusing runtime failures.
    ///
    /// Reads the environment (for `HERMIT_WS_TOKEN`); the pure rule lives in
    /// [`Self::validate_with`] so tests never touch process-global state.
    pub fn validate(&self) -> Result<()> {
        self.validate_with(crate::http::secret_opt("HERMIT_WS_TOKEN").is_some())
    }

    /// Pure form of [`Self::validate`]: `ws_token_present` is supplied rather
    /// than read from the environment.
    ///
    /// This split exists for a real bug: the env-reading version can only be
    /// tested by mutating a process-global, and `cargo test` runs tests in
    /// parallel threads — concurrent `setenv`/`getenv` is a data race (which is
    /// why it is `unsafe` in edition 2024). It passed locally and aborted on
    /// CI, where the core count and scheduling differ.
    pub fn validate_with(&self, ws_token_present: bool) -> Result<()> {
        anyhow::ensure!(
            self.llm.max_tool_rounds >= 1 && self.llm.max_tool_rounds <= 2,
            "llm.max_tool_rounds must be 1 or 2 — the interactive path is capped at 2 rounds by design (spec §4.3)"
        );
        anyhow::ensure!(
            self.search.mode == "turbo",
            "search.mode must be \"turbo\"; the Parallel API silently defaults to the much slower \"advanced\" mode"
        );
        anyhow::ensure!(
            self.memory.core_token_cap <= 600,
            "memory.core_token_cap is hard-capped at 600 tokens"
        );
        anyhow::ensure!(
            self.feedback.settle_secs <= 60,
            "feedback.settle_secs is capped at 60 - the ask must arrive while the turn is still fresh"
        );
        anyhow::ensure!(
            self.feedback.listen_ms >= 2_000 && self.feedback.listen_ms <= 15_000,
            "feedback.listen_ms must be within 2000..=15000"
        );
        anyhow::ensure!(
            (0.0..=1.0).contains(&self.feedback.ask_probability)
                && (0.0..=1.0).contains(&self.feedback.min_ask_probability)
                && self.feedback.min_ask_probability <= self.feedback.ask_probability,
            "feedback probabilities must be in 0..=1 with min <= start"
        );
        anyhow::ensure!(
            self.stt.followup_window_ms <= 10_000,
            "stt.followup_window_ms of {}ms holds the mic open too long after every \
             answer; keep the follow-up window at 10s or less (0 disables it)",
            self.stt.followup_window_ms
        );
        anyhow::ensure!(
            self.audio.buffer_ms >= self.audio.period_ms,
            "audio.buffer_ms must be >= period_ms"
        );
        anyhow::ensure!(
            self.tts.cartesia_max_buffer_delay_ms == 0,
            "tts.cartesia_max_buffer_delay_ms must be 0; the provider default of 3000ms alone \
             exceeds the 1.2s first-audio budget"
        );
        anyhow::ensure!(
            self.server.ws_max_connections >= 1 && self.server.ws_max_connections <= 64,
            "server.ws_max_connections must be between 1 and 64"
        );
        // A WS gateway reachable beyond loopback is a remote agent controller:
        // it can spend LLM/tool budget and (if ws_allow_listen) arm the mic.
        // Refuse to expose that without a bearer token.
        if !self.server.ws_bind.is_empty()
            && !ws_bind_is_loopback(&self.server.ws_bind)
            && !ws_token_present
        {
            anyhow::bail!(
                "server.ws_bind = \"{}\" is reachable beyond loopback but HERMIT_WS_TOKEN is not \
                 set. Set the token in /etc/hermit/hermit.env (clients authenticate with an \
                 `Authorization: Bearer <token>` header or a first-message `/auth <token>`), or \
                 bind to 127.0.0.1.",
                self.server.ws_bind
            );
        }
        Ok(())
    }
}

/// True when a bind address can only be reached from this machine.
pub fn ws_bind_is_loopback(bind: &str) -> bool {
    use std::net::SocketAddr;
    match bind.parse::<SocketAddr>() {
        Ok(addr) => addr.ip().is_loopback(),
        // Unparseable (e.g. a bare hostname): treat as NOT loopback so the
        // token requirement fails closed rather than open.
        Err(_) => false,
    }
}

/// Load and validate a config file.
pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    let mut cfg: Config =
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
    cfg.source_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    cfg.validate()?;
    Ok(cfg)
}

/// Watch the config directory and publish a fresh `Arc<Config>` on every write.
///
/// Hot reload is deliberately best-effort: a broken edit logs an error and keeps
/// the previous config live rather than taking the daemon down mid-conversation.
pub fn watch(path: PathBuf, tx: tokio::sync::watch::Sender<Arc<Config>>) -> Result<()> {
    use notify::{EventKind, RecursiveMode, Watcher};

    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let (raw_tx, raw_rx) = std::sync::mpsc::channel();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && matches!(
                ev.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            )
        {
            let _ = raw_tx.send(());
        }
    })?;
    watcher.watch(&dir, RecursiveMode::Recursive)?;

    // Watcher must outlive the thread; move it in and park here forever.
    std::thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            let _watcher = watcher;
            loop {
                if raw_rx.recv().is_err() {
                    break;
                }
                // Coalesce editor write storms (write + rename + chmod) into one reload.
                while raw_rx.recv_timeout(Duration::from_millis(250)).is_ok() {}
                match load(&path) {
                    Ok(cfg) => {
                        tracing::info!(path = %path.display(), "config reloaded");
                        let _ = tx.send(Arc::new(cfg));
                    }
                    Err(e) => {
                        tracing::error!(error = ?e, "config reload failed; keeping previous config")
                    }
                }
            }
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        let cfg = Config::default();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.search.mode, "turbo");
        assert_eq!(cfg.llm.max_tool_rounds, 2);
    }

    #[test]
    fn rejects_non_turbo_search_mode() {
        let mut cfg = Config::default();
        cfg.search.mode = "advanced".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_more_than_two_tool_rounds() {
        let mut cfg = Config::default();
        cfg.llm.max_tool_rounds = 5;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn followup_window_is_bounded_and_defaults_sane() {
        let cfg = Config::default();
        assert_eq!(
            cfg.stt.followup_window_ms, 3_000,
            "3 s follow-up by default"
        );
        assert!(cfg.validate().is_ok());

        // A window that long stops being a follow-up and starts being an
        // always-open mic; the validator refuses it.
        let mut cfg = Config::default();
        cfg.stt.followup_window_ms = 30_000;
        assert!(cfg.validate().is_err());

        // 0 = feature off, and must stay valid.
        let mut cfg = Config::default();
        cfg.stt.followup_window_ms = 0;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_oversized_core_cap() {
        let mut cfg = Config::default();
        cfg.memory.core_token_cap = 2000;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn ws_gateway_defaults_are_closed() {
        // Review finding #1: the WS gateway is a control surface. The safe
        // posture must be the default posture.
        let cfg = Config::default();
        assert!(super::ws_bind_is_loopback(&cfg.server.ws_bind));
        assert!(
            !cfg.server.ws_allow_listen,
            "remote mic arming must be opt-in"
        );
        assert!(cfg.server.ws_max_connections <= 8);
    }

    #[test]
    fn non_loopback_ws_bind_requires_a_token() {
        // Pure: no process-global env mutation, so this is safe under the
        // parallel test threads that made the previous version CI-flaky.
        let mut cfg = Config::default();
        for bind in [
            "0.0.0.0:8765",
            "192.168.1.10:8765",
            "[::]:8765",
            "myhost:8765",
        ] {
            cfg.server.ws_bind = bind.into();
            let err = cfg.validate_with(false).unwrap_err().to_string();
            assert!(err.contains("HERMIT_WS_TOKEN"), "bind {bind}: {err}");
            assert!(cfg.validate_with(true).is_ok(), "a token unlocks {bind}");
        }
        // Loopback never needs a token.
        cfg.server.ws_bind = "127.0.0.1:8765".into();
        assert!(cfg.validate_with(false).is_ok());
    }

    #[test]
    fn loopback_binds_are_recognized() {
        assert!(super::ws_bind_is_loopback("127.0.0.1:8765"));
        assert!(super::ws_bind_is_loopback("[::1]:8765"));
        assert!(!super::ws_bind_is_loopback("0.0.0.0:8765"));
        // Fail closed on anything unparseable.
        assert!(!super::ws_bind_is_loopback("localhost:8765"));
        assert!(!super::ws_bind_is_loopback(""));
    }
}
