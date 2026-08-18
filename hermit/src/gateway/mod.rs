//! Front ends and the shared turn handler.
//!
//! Every input surface — CLI, local WebSocket, voice — funnels into
//! [`Gateway::handle`], so routing, memory, tool policy and the metrics line behave
//! identically no matter how a request arrived. Telegram/Discord adapters would slot
//! in here as additional front ends; they are deliberately not built (spec §4.1).

pub mod cli;
pub mod voice;
pub mod ws;

use crate::config::Config;
use crate::memory::Store;
use crate::metrics::TurnTimings;
use crate::music::Music;
use crate::orchestrator::{Orchestrator, Prefetch, TurnEvent};
use crate::reflect::ReflectSignal;
use crate::router::{DeviceCommand, Route};
use crate::speech::acks::AckBank;
use crate::speech::tts::Tts;
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Where a turn's text output should go, in addition to speech.
pub type TextSink = tokio::sync::mpsc::UnboundedSender<String>;

/// One completed turn.
#[derive(Debug, Clone)]
pub struct TurnResult {
    pub answer: String,
    pub fast_path: bool,
}

pub struct Gateway {
    pub cfg_rx: tokio::sync::watch::Receiver<Arc<Config>>,
    pub orch: Arc<Orchestrator>,
    pub store: Arc<Store>,
    pub music: Music,
    pub tts: Arc<Tts>,
    pub player: crate::audio::AudioPlayer,
    pub acks: Arc<AckBank>,
    pub reflect_tx: tokio::sync::mpsc::Sender<ReflectSignal>,
    /// Starts a voice turn without the wake word (CLI `/listen`, or a future button).
    pub voice_trigger: std::sync::OnceLock<voice::TriggerTx>,
    turn_seq: AtomicU64,
    /// One turn at a time. The device has one speaker and one user; overlapping
    /// turns would interleave speech and corrupt the conversation history.
    turn_lock: tokio::sync::Mutex<()>,
}

impl Gateway {
    // Eight arguments, and clippy is right to notice. A builder would be the usual
    // answer, but this is called exactly once from main() and every field is a
    // distinct subsystem handle — grouping them into a struct just to unpack it
    // again would add a type without removing anything.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg_rx: tokio::sync::watch::Receiver<Arc<Config>>,
        orch: Arc<Orchestrator>,
        store: Arc<Store>,
        music: Music,
        tts: Arc<Tts>,
        player: crate::audio::AudioPlayer,
        acks: Arc<AckBank>,
        reflect_tx: tokio::sync::mpsc::Sender<ReflectSignal>,
    ) -> Self {
        Self {
            cfg_rx,
            orch,
            store,
            music,
            tts,
            player,
            acks,
            reflect_tx,
            voice_trigger: std::sync::OnceLock::new(),
            turn_seq: AtomicU64::new(0),
            turn_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Ask the voice pipeline to start listening now. Returns false if voice is not
    /// running (no microphone, or built without ALSA).
    pub fn trigger_listen(&self) -> bool {
        match self.voice_trigger.get() {
            Some(tx) => tx.try_send(()).is_ok(),
            None => false,
        }
    }

    pub fn config(&self) -> Arc<Config> {
        self.cfg_rx.borrow().clone()
    }

    /// Handle one utterance end to end.
    ///
    /// `speak` controls whether the answer goes to the speaker; `text` receives the
    /// streamed answer for a connected text client.
    pub async fn handle(
        &self,
        utterance: &str,
        speak: bool,
        text: Option<TextSink>,
        prefetch: Option<Prefetch>,
    ) -> Result<TurnResult> {
        let _guard = self.turn_lock.lock().await;
        let cfg = self.config();
        let turn_id = self.turn_seq.fetch_add(1, Ordering::Relaxed);
        let mut timings = TurnTimings::new(turn_id);

        let utterance = utterance.trim();
        if utterance.is_empty() {
            return Ok(TurnResult { answer: String::new(), fast_path: true });
        }

        // ---- fast path (spec §4.2) --------------------------------------
        let route_started = Instant::now();
        let stations = self.music.station_names().await;
        let route = crate::router::route(utterance, &stations);
        timings.route_ms = Some(crate::metrics::ms_since(route_started));

        if let Route::Device(cmd) = route {
            let answer = self.run_device_command(&cmd).await;
            timings.fast_path = true;
            if let Some(tx) = &text {
                let _ = tx.send(answer.clone());
            }
            if speak {
                self.speak_once(&answer).await;
            }
            // Device commands are conversational too — record them so "turn it up"
            // followed by "actually, back down" has context.
            let _ = self.store.record_message("user", utterance);
            let _ = self.store.record_message("assistant", &answer);
            timings.first_audio_ms = timings.total_ms;
            timings.finish();
            timings.emit();
            return Ok(TurnResult { answer, fast_path: true });
        }

        // ---- agent path -------------------------------------------------
        let generation = self.player.generation();
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<TurnEvent>();

        // TTS consumer: chunks arrive as the model produces them.
        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<String>(16);
        let speaking = speak && self.tts.is_enabled();

        let tts_task = if speaking {
            self.music.duck().await;
            let tts = self.tts.clone();
            let player = self.player.clone();
            Some(tokio::spawn(async move {
                tts.speak(chunk_rx, &player, generation).await
            }))
        } else {
            drop(chunk_rx);
            None
        };

        // Event pump: forwards tokens to the text client and chunks to TTS.
        let acks = self.acks.clone();
        let player = self.player.clone();
        let text_sink = text.clone();
        let turn_started = timings.started.unwrap_or_else(Instant::now);
        let pump = tokio::spawn(async move {
            let mut ack_at: Option<f64> = None;
            let mut tool_names: Vec<String> = Vec::new();
            while let Some(ev) = events_rx.recv().await {
                match ev {
                    TurnEvent::Token(t) => {
                        if let Some(tx) = &text_sink {
                            let _ = tx.send(t);
                        }
                    }
                    TurnEvent::SpeechChunk(c) => {
                        if chunk_tx.send(c).await.is_err() {
                            // TTS gone; keep draining so the turn still completes.
                        }
                    }
                    TurnEvent::Ack => {
                        if acks.play(&player) && ack_at.is_none() {
                            ack_at = Some(crate::metrics::ms_since(turn_started));
                        }
                    }
                    TurnEvent::ToolRound(names) => tool_names.extend(names),
                    TurnEvent::Final(_) => {}
                    TurnEvent::Error(e) => {
                        tracing::warn!(error = %e, "turn error event");
                    }
                }
            }
            (ack_at, tool_names)
        });

        let result = self
            .orch
            .run_turn(&cfg, utterance, &events_tx, &mut timings, prefetch)
            .await;
        drop(events_tx);

        let (ack_at, tool_names) = pump.await.unwrap_or((None, Vec::new()));

        if let Some(task) = tts_task {
            match task.await {
                Ok(Ok(sr)) => {
                    timings.tts_ttfa_ms = sr.ttfa_ms;
                    // First audio is whichever the user actually heard first: the
                    // canned acknowledgment, or the model's own first clause.
                    let spoken_at = sr
                        .ttfa_ms
                        .map(|_| crate::metrics::ms_since(turn_started));
                    timings.first_audio_ms = match (ack_at, spoken_at) {
                        (Some(a), Some(s)) => Some(a.min(s)),
                        (Some(a), None) => Some(a),
                        (None, s) => s,
                    };
                }
                Ok(Err(e)) => tracing::warn!(error = %e, "tts failed"),
                Err(e) => tracing::warn!(error = %e, "tts task panicked"),
            }
            self.music.unduck().await;
        }

        let answer = match result {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = ?e, "turn failed");
                let msg = "Sorry — something went wrong handling that.".to_string();
                if let Some(tx) = &text {
                    let _ = tx.send(msg.clone());
                }
                if speak {
                    self.speak_once(&msg).await;
                }
                msg
            }
        };

        // ---- memory + reflection ----------------------------------------
        let _ = self.store.record_message("user", utterance);
        if !answer.is_empty() {
            let _ = self.store.record_message("assistant", &answer);
        }
        let _ = self.reflect_tx.try_send(ReflectSignal::TurnCompleted);

        // A successful multi-step run is worth distilling into a skill (§9.2).
        if tool_names.len() >= 2 && !answer.is_empty() {
            let _ = self.reflect_tx.try_send(ReflectSignal::SkillCandidate {
                goal: utterance.to_string(),
                tools_used: tool_names,
                answer: answer.clone(),
            });
        }

        timings.finish();
        timings.emit();
        Ok(TurnResult { answer, fast_path: false })
    }

    async fn run_device_command(&self, cmd: &DeviceCommand) -> String {
        match self.music.execute(cmd).await {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!(error = %e, ?cmd, "device command failed");
                // Speak the reason — "there's no next track on live radio" is a
                // useful answer, not an error to swallow.
                format!("{e}")
            }
        }
    }

    /// Speak a single fixed string (device replies, research announcements).
    pub async fn speak_once(&self, text: &str) {
        if !self.tts.is_enabled() || text.trim().is_empty() {
            return;
        }
        let generation = self.player.generation();
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let _ = tx.send(text.to_string()).await;
        drop(tx);
        self.music.duck().await;
        if let Err(e) = self.tts.speak(rx, &self.player, generation).await {
            tracing::warn!(error = %e, "speaking failed");
        }
        self.music.unduck().await;
    }

    /// Deliver a finished background-research result (spec §4.3).
    ///
    /// The answer is recorded as an ordinary assistant message so facts are
    /// extracted through the normal reflection channel, never written directly.
    pub async fn announce(&self, announcement: crate::tools::research::Announcement) {
        let cfg = self.config();
        tracing::info!(about = %announcement.about, "delivering background research");

        let _ = self.store.record_message(
            "user",
            &format!("(background research request) {}", announcement.about),
        );
        let _ = self.store.record_message("assistant", &announcement.text);
        let _ = self.reflect_tx.try_send(ReflectSignal::TurnCompleted);

        if cfg.research.speak_on_complete {
            let _guard = self.turn_lock.lock().await;
            self.speak_once(&format!(
                "About {} — {}",
                announcement.about, announcement.text
            ))
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_route_is_detected_without_touching_the_llm() {
        // The routing decision itself is what keeps the LLM out of the fast path.
        let stations = vec!["npr".to_string()];
        assert!(matches!(
            crate::router::route("pause", &stations),
            Route::Device(DeviceCommand::Pause)
        ));
        assert!(matches!(crate::router::route("why is the sky blue", &stations), Route::Agent));
    }
}
