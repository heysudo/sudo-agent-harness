//! The voice pipeline (spec §7).
//!
//! ```text
//!  XVF3800 processed mono 16 kHz  ->  Porcupine  ->  Deepgram  ->  Gateway
//!         (hardware AEC/beamforming/NS)   wake        streaming      turn
//! ```
//!
//! Capture runs on its own OS thread because ALSA reads block. Everything after the
//! wake word is async.
//!
//! Barge-in: the wake word is watched *during* playback too. When it fires mid
//! answer, the audio player is flushed immediately (queued speech discarded, driver
//! buffer dropped) and music ducks — so the user can interrupt without waiting for
//! the sentence to finish.

use super::Gateway;
use crate::speech::earcons::Earcons;
use crate::speech::stt::{Deepgram, Sarvam, SttEvent, SttSession, TranscriptBuilder};
use crate::speech::wake::{FrameFeeder, WakeDetector};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// dBFS of a mono i16 frame, for the console level meter.
fn frame_dbfs(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return -99.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    let rms = (sum_sq / samples.len() as f64).sqrt() / 32768.0;
    if rms <= 0.0 {
        -99.0
    } else {
        (20.0 * rms.log10()) as f32
    }
}

/// Audio captured from the microphone, handed to the async side.
pub type MicRx = tokio::sync::mpsc::Receiver<Vec<i16>>;
pub type MicTx = tokio::sync::mpsc::Sender<Vec<i16>>;

/// Manual "start listening now" trigger, equivalent to the wake word firing.
///
/// Exists because the wake word needs a Picovoice access key, which a device may not
/// have; it also gives a physical button or GPIO somewhere to hook into later. The CLI
/// exposes it as `/listen`.
pub type TriggerRx = tokio::sync::mpsc::Receiver<()>;
pub type TriggerTx = tokio::sync::mpsc::Sender<()>;

/// Run the voice loop until shutdown.
///
/// `mic_rx` yields mono PCM at `audio.sample_rate` from the capture thread.
/// `trigger_rx` starts a turn without the wake word.
pub async fn run(
    gateway: Arc<Gateway>,
    mut mic_rx: MicRx,
    detector: Box<dyn WakeDetector>,
    mut trigger_rx: TriggerRx,
) {
    let cfg = gateway.config();

    let stt: Arc<dyn SttSession> = match cfg.stt.provider.as_str() {
        "sarvam" => {
            tracing::info!(language = %cfg.stt.sarvam_language, "stt provider: sarvam");
            match Sarvam::from_config(&cfg.stt) {
                Some(s) => Arc::new(s),
                None => {
                    tracing::warn!(
                        "SARVAM_API_KEY not set; voice input disabled (text front ends still work)"
                    );
                    while mic_rx.recv().await.is_some() {}
                    return;
                }
            }
        }
        _ => {
            tracing::info!(model = %cfg.stt.model, "stt provider: deepgram");
            match Deepgram::from_config(&cfg.stt) {
                Some(d) => Arc::new(d),
                None => {
                    tracing::warn!(
                        "DEEPGRAM_API_KEY not set; voice input disabled (text front ends still work)"
                    );
                    while mic_rx.recv().await.is_some() {}
                    return;
                }
            }
        }
    };

    let mut feeder = FrameFeeder::new(detector);
    let listening = Arc::new(AtomicBool::new(false));
    let speaker_muted = Arc::new(AtomicBool::new(false));
    // Console telemetry + control (sudo-console). All throttled internally.
    let state = Arc::new(Mutex::new(crate::state_io::StateWriter::new()));
    // UI sounds (b2-34 earcons): instant wake ack + end-of-speech "working on it".
    let earcons = Arc::new(Earcons::load());

    tracing::info!("voice pipeline ready; waiting for the wake word");

    loop {
        // Either the wake word fires, or something triggers a turn manually.
        let start_turn = tokio::select! {
            frame = mic_rx.recv() => match frame {
                Some(samples) => {
                    // Console control: mic mute discards frames BEFORE the wake word
                    // and STT — nothing is detected, nothing leaves the device.
                    let ctl = state
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .read_control();
                    apply_console_control(&gateway, ctl, &speaker_muted).await;
                    let mut state_guard = state.lock().unwrap_or_else(|e| e.into_inner());
                    if ctl.mic_muted {
                        state_guard.write_live(serde_json::json!({
                            "ww": null, "rms": -99.0, "listening": false,
                            "mic_muted": true, "speaker_muted": ctl.speaker_muted,
                        }));
                        continue;
                    }
                    // While a turn is being transcribed, audio is forwarded to STT by
                    // the utterance task rather than scanned for the wake word.
                    if listening.load(Ordering::Acquire) {
                        continue;
                    }
                    let hit = feeder.push(&samples).is_some();
                    let (score, threshold) = feeder.last_score().unwrap_or((0.0, 0.0));
                    state_guard.write_live(serde_json::json!({
                        "ww": score, "ww_threshold": threshold,
                        "rms": frame_dbfs(&samples), "listening": false,
                        "mic_muted": false, "speaker_muted": ctl.speaker_muted,
                    }));
                    if hit {
                        feeder.reset();
                        state_guard.emit("ww_fired", serde_json::json!({"score": score}));
                        tracing::info!("wake word detected");
                    }
                    hit
                }
                None => break, // capture ended
            },
            trig = trigger_rx.recv() => match trig {
                Some(()) => {
                    if listening.load(Ordering::Acquire) {
                        continue;
                    }
                    tracing::info!("manual listen trigger");
                    state.lock().unwrap_or_else(|e| e.into_inner())
                        .emit("manual_trigger", serde_json::json!({}));
                    feeder.reset();
                    true
                }
                // Sender dropped: no more manual triggers, but the wake word still works.
                None => continue,
            },
        };
        if !start_turn {
            continue;
        }

        // --- wake fired (or manual trigger) --------------------------------
        let wake_at = std::time::Instant::now();

        // Barge-in: kill anything currently being spoken, immediately.
        gateway.player.flush().await;
        // Instant "heard you" chirp — AFTER the flush, which bumps the player
        // generation; enqueued before it, the chirp itself would be discarded
        // as stale audio.
        earcons.play_trigger_ack(&gateway.player).await;
        gateway.music.duck().await;

        listening.store(true, Ordering::Release);
        tracing::info!(flush_us = wake_at.elapsed().as_micros(), "listening");

        let gw = gateway.clone();
        let stt = stt.clone();
        let flag = listening.clone();
        let ec = earcons.clone();
        let turn_state = state.clone();
        let turn_speaker_muted = speaker_muted.clone();
        // 128 x 20 ms = ~2.5 s of audio, comfortably covering the Deepgram
        // handshake so early speech is not lost while the socket opens.
        let (utt_tx, utt_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(128);

        // The utterance task owns STT and the turn; the capture loop keeps feeding
        // it audio until it signals completion.
        let handle = tokio::spawn(async move {
            let result =
                transcribe_and_answer(gw.clone(), stt, utt_rx, wake_at, ec, turn_state).await;
            gw.music.unduck().await;
            flag.store(false, Ordering::Release);
            result
        });

        // Forward microphone audio to the utterance task until it finishes.
        forward_until_done(
            &mut mic_rx,
            utt_tx,
            &listening,
            &state,
            &gateway,
            &turn_speaker_muted,
        )
        .await;
        let _ = handle.await;
    }

    tracing::info!("voice pipeline stopped");
}

/// Pump microphone audio into the utterance channel until the turn releases the
/// listening flag or the channel closes.
async fn apply_console_control(
    gateway: &Gateway,
    ctl: crate::state_io::Control,
    applied_speaker_mute: &AtomicBool,
) {
    gateway.player.set_muted(ctl.speaker_muted);
    let previous = applied_speaker_mute.swap(ctl.speaker_muted, Ordering::AcqRel);
    if previous != ctl.speaker_muted {
        if ctl.speaker_muted {
            let _ = gateway.player.flush().await;
        }
        gateway.music.set_muted(ctl.speaker_muted).await;
    }
}

async fn forward_until_done(
    mic_rx: &mut MicRx,
    utt_tx: MicTx,
    listening: &AtomicBool,
    state: &Arc<Mutex<crate::state_io::StateWriter>>,
    gateway: &Gateway,
    applied_speaker_mute: &AtomicBool,
) {
    while listening.load(Ordering::Acquire) {
        match tokio::time::timeout(Duration::from_millis(100), mic_rx.recv()).await {
            Ok(Some(samples)) => {
                let ctl = {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    let ctl = state.read_control();
                    state.write_live(serde_json::json!({
                        "rms": if ctl.mic_muted { -99.0 } else { frame_dbfs(&samples) },
                        "listening": !ctl.mic_muted,
                        "mic_muted": ctl.mic_muted,
                        "speaker_muted": ctl.speaker_muted,
                    }));
                    ctl
                };
                apply_console_control(gateway, ctl, applied_speaker_mute).await;
                if ctl.mic_muted {
                    continue;
                }
                match utt_tx.try_send(samples) {
                    Ok(()) => {}
                    // The utterance channel is momentarily full — this happens
                    // routinely during the ~1 s Deepgram handshake, when audio
                    // arrives faster than the (not yet open) socket drains it.
                    // Dropping this one 20 ms chunk is harmless; treating it as
                    // "STT is finished" and hanging up is NOT. That mistake made
                    // every utterance end after exactly 32 frames (the channel
                    // depth) with a clean close code and an empty transcript.
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        tracing::trace!("utterance channel full; dropping a mic chunk");
                    }
                    // Receiver gone: STT genuinely finished. Stop forwarding.
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
            Ok(None) => break,  // capture ended
            Err(_) => continue, // timeout: re-check the flag
        }
    }
    drop(utt_tx); // signals end-of-audio to Deepgram
}

/// Stream to Deepgram, fire a speculative search, then answer.
async fn transcribe_and_answer(
    gateway: Arc<Gateway>,
    stt: Arc<dyn SttSession>,
    mut mic_rx: MicRx,
    wake_at: std::time::Instant,
    earcons: Arc<Earcons>,
    state: Arc<Mutex<crate::state_io::StateWriter>>,
) -> Option<String> {
    let (audio_tx, mut events) = match stt.start().await {
        Ok(x) => x,
        Err(e) => {
            tracing::error!(error = %e, "could not open speech-to-text");
            return None;
        }
    };
    tracing::debug!(
        listening_ms = wake_at.elapsed().as_millis(),
        "stt session open"
    );

    // Feed the microphone into Deepgram.
    let pump = tokio::spawn(async move {
        while let Some(samples) = mic_rx.recv().await {
            if audio_tx.send(samples).await.is_err() {
                break;
            }
        }
        // Dropping audio_tx tells the session to finalize.
    });

    let mut builder = TranscriptBuilder::default();
    let mut prefetch: Option<crate::orchestrator::Prefetch> = None;
    let mut prefetched_for = String::new();

    while let Some(ev) = events.recv().await {
        match &ev {
            SttEvent::Language(lang) => {
                builder.apply(&ev);
                tracing::info!(language = %lang, "stt detected language");
            }
            SttEvent::Interim(_) | SttEvent::Final(_) => {
                builder.apply(&ev);
                let provisional = builder.provisional();
                state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .write_live(serde_json::json!({
                        "transcript_interim": provisional,
                    }));
                // Speculative prefetch (spec §5): fire once, before the user has
                // finished speaking, when the partial looks like a lookup.
                if prefetch.is_none()
                    && provisional != prefetched_for
                    && let Some(p) =
                        crate::orchestrator::spawn_prefetch(&gateway.orch.tools, &provisional)
                {
                    prefetched_for = provisional;
                    prefetch = Some(p);
                }
            }
            SttEvent::EndOfSpeech(_) => {
                builder.apply(&ev);
                // Sarvam sends vad.speech_end BEFORE transcript.final (~160ms gap,
                // measured live). Wait briefly for the polished final — it has the
                // corrected text and the confident language tag — instead of
                // answering off the rougher interim.
                let deadline = tokio::time::Instant::now() + Duration::from_millis(600);
                while let Ok(Some(late)) =
                    tokio::time::timeout_at(deadline, events.recv()).await
                {
                    let is_final = matches!(late, SttEvent::Final(_));
                    builder.apply(&late);
                    if is_final {
                        break;
                    }
                }
                break;
            }
            SttEvent::Closed(reason) => {
                if let Some(r) = reason {
                    tracing::warn!(reason = %r, "stt closed early");
                }
                break;
            }
        }
    }

    pump.abort();
    // Wait for cancellation so the utterance receiver is dropped immediately. This
    // stops the capture forwarder at end-of-speech; the turn-level flag remains set
    // until the answer finishes, but the TUI no longer says LISTENING or overwrites
    // the finalized transcript with live meter updates.
    let _ = pump.await;

    let utterance = builder.best_utterance();
    if utterance.trim().is_empty() {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        state.write_live(serde_json::json!({
            "transcript_interim": "", "listening": false,
        }));
        state.emit("no_speech", serde_json::json!({}));
        tracing::info!("no speech captured after wake word");
        if let Some(p) = prefetch {
            p.handle.abort();
        }
        return None;
    }

    {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        state.write_live(serde_json::json!({
            "transcript_interim": "", "last_user": utterance,
            "listening": false,
        }));
        state.emit(
            "transcript",
            serde_json::json!({"role": "user", "text": utterance}),
        );
    }

    // Discard a prefetch that no longer matches the finalized transcript.
    let prefetch = prefetch.filter(|p| {
        let keep = p.matches(&utterance);
        if !keep {
            p.handle.abort();
        }
        keep
    });

    // "Stopped listening, working on it" — the mic-stop acknowledgement. Played
    // the moment the utterance is finalized, before the LLM round trip starts, so
    // the silence while the answer streams is never mistaken for a missed command.
    earcons.play_thinking(&gateway.player).await;

    tracing::info!(
        utterance = %utterance,
        language = builder.language().unwrap_or("unknown"),
        wake_to_transcript_ms = wake_at.elapsed().as_millis(),
        "answering"
    );

    // Reply in the language the user spoke this turn. STT and TTS use different
    // code sets for the same language (Odia: or-IN vs od-IN), so map here; an
    // unmapped or missing detection falls back to the configured default voice.
    let reply_lang = builder
        .language()
        .and_then(crate::speech::stt::stt_to_tts_lang)
        .map(String::from);

    match gateway
        .handle_in_language(&utterance, true, None, prefetch, reply_lang)
        .await
    {
        Ok(r) => {
            if r.speech_completed {
                // Every response sample has already played. Queue and drain the final
                // cue before releasing the outer music duck.
                earcons.play_response_complete(&gateway.player).await;
                if !gateway.player.drain().await {
                    tracing::warn!("response-complete cue could not be drained");
                }
            }
            {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.write_live(serde_json::json!({"last_answer": r.answer}));
                state.emit(
                    "transcript",
                    serde_json::json!({"role": "assistant", "text": r.answer}),
                );
                state.emit(
                    "turn_complete",
                    serde_json::json!({
                        "utterance": utterance,
                        "answer": r.answer,
                        "speech_completed": r.speech_completed,
                    }),
                );
            }
            Some(r.answer)
        }
        Err(e) => {
            state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .emit(
                    "turn_error",
                    serde_json::json!({"utterance": utterance, "error": e.to_string()}),
                );
            tracing::error!(error = ?e, "voice turn failed");
            None
        }
    }
}

/// Spawn the blocking ALSA capture thread.
///
/// Returns a receiver of mono PCM. On builds without ALSA this yields nothing, so
/// the voice loop simply never wakes.
#[cfg(feature = "alsa-audio")]
pub fn spawn_capture(cfg: &crate::config::Audio) -> anyhow::Result<MicRx> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<i16>>(32);
    let pcm = crate::audio::alsa_capture(cfg)?;
    let frames = (cfg.sample_rate as usize / 1000) * cfg.period_ms as usize;

    std::thread::Builder::new()
        .name("audio-in".into())
        .spawn(move || {
            let io = match pcm.io_i16() {
                Ok(io) => io,
                Err(e) => {
                    tracing::error!(error = %e, "cannot get capture io handle");
                    return;
                }
            };
            let mut buf = vec![0i16; frames.max(256)];
            let mut dropped: u64 = 0;
            loop {
                match io.readi(&mut buf) {
                    Ok(n) if n > 0 => {
                        // try_send, never block: if the consumer lags, dropping a
                        // 20 ms chunk here is strictly better than blocking, which
                        // backs up into the ALSA ring, overruns it, and hands the
                        // wake detector gapped audio for MINUTES afterwards. A
                        // dropped chunk costs one frame; a blocked read costs the
                        // whole stream's continuity.
                        match tx.try_send(buf[..n].to_vec()) {
                            Ok(()) => {}
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                dropped += 1;
                                if dropped.is_power_of_two() {
                                    tracing::warn!(
                                        dropped,
                                        "mic consumer lagging; dropping capture chunks"
                                    );
                                }
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "capture read error; recovering");
                        if pcm.try_recover(e, true).is_err() {
                            tracing::error!("capture is unrecoverable; stopping");
                            break;
                        }
                    }
                }
            }
            tracing::info!("capture thread exiting");
        })?;

    Ok(rx)
}

#[cfg(not(feature = "alsa-audio"))]
pub fn spawn_capture(_cfg: &crate::config::Audio) -> anyhow::Result<MicRx> {
    tracing::warn!("built without alsa-audio: no microphone capture");
    let (_tx, rx) = tokio::sync::mpsc::channel::<Vec<i16>>(1);
    Ok(rx)
}
