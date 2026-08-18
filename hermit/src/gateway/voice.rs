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
use crate::speech::stt::{Deepgram, SttEvent, TranscriptBuilder};
use crate::speech::wake::{FrameFeeder, WakeDetector};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

    let Some(deepgram) = Deepgram::from_config(&cfg.stt) else {
        tracing::warn!(
            "DEEPGRAM_API_KEY not set; voice input disabled (text front ends still work)"
        );
        // Drain the microphone so the capture thread does not block forever.
        while mic_rx.recv().await.is_some() {}
        return;
    };
    let deepgram = Arc::new(deepgram);

    let mut feeder = FrameFeeder::new(detector);
    let listening = Arc::new(AtomicBool::new(false));

    tracing::info!("voice pipeline ready; waiting for the wake word");

    loop {
        // Either the wake word fires, or something triggers a turn manually.
        let start_turn = tokio::select! {
            frame = mic_rx.recv() => match frame {
                Some(samples) => {
                    // While a turn is being transcribed, audio is forwarded to STT by
                    // the utterance task rather than scanned for the wake word.
                    if listening.load(Ordering::Acquire) {
                        continue;
                    }
                    let hit = feeder.push(&samples).is_some();
                    if hit {
                        feeder.reset();
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
        gateway.player.flush();
        gateway.music.duck().await;

        listening.store(true, Ordering::Release);
        tracing::info!(
            flush_us = wake_at.elapsed().as_micros(),
            "listening"
        );

        let gw = gateway.clone();
        let dg = deepgram.clone();
        let flag = listening.clone();
        // 128 x 20 ms = ~2.5 s of audio, comfortably covering the Deepgram
        // handshake so early speech is not lost while the socket opens.
        let (utt_tx, utt_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(128);

        // The utterance task owns STT and the turn; the capture loop keeps feeding
        // it audio until it signals completion.
        let handle = tokio::spawn(async move {
            let result = transcribe_and_answer(gw.clone(), dg, utt_rx, wake_at).await;
            gw.music.unduck().await;
            flag.store(false, Ordering::Release);
            result
        });

        // Forward microphone audio to the utterance task until it finishes.
        forward_until_done(&mut mic_rx, utt_tx, &listening).await;
        let _ = handle.await;
    }

    tracing::info!("voice pipeline stopped");
}

/// Pump microphone audio into the utterance channel until the turn releases the
/// listening flag or the channel closes.
async fn forward_until_done(
    mic_rx: &mut MicRx,
    utt_tx: MicTx,
    listening: &AtomicBool,
) {
    while listening.load(Ordering::Acquire) {
        match tokio::time::timeout(Duration::from_millis(100), mic_rx.recv()).await {
            Ok(Some(samples)) => {
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
            Ok(None) => break, // capture ended
            Err(_) => continue, // timeout: re-check the flag
        }
    }
    drop(utt_tx); // signals end-of-audio to Deepgram
}

/// Stream to Deepgram, fire a speculative search, then answer.
async fn transcribe_and_answer(
    gateway: Arc<Gateway>,
    deepgram: Arc<Deepgram>,
    mut mic_rx: MicRx,
    wake_at: std::time::Instant,
) -> Option<String> {
    let (audio_tx, mut events) = match deepgram.start().await {
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
            SttEvent::Interim(_) | SttEvent::Final(_) => {
                builder.apply(&ev);
                // Speculative prefetch (spec §5): fire once, before the user has
                // finished speaking, when the partial looks like a lookup.
                if prefetch.is_none() {
                    let provisional = builder.provisional();
                    if provisional != prefetched_for
                        && let Some(p) =
                            crate::orchestrator::spawn_prefetch(&gateway.orch.tools, &provisional)
                    {
                        prefetched_for = provisional;
                        prefetch = Some(p);
                    }
                }
            }
            SttEvent::EndOfSpeech(_) => {
                builder.apply(&ev);
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

    let utterance = builder.finished();
    if utterance.trim().is_empty() {
        tracing::info!("no speech captured after wake word");
        if let Some(p) = prefetch {
            p.handle.abort();
        }
        return None;
    }

    // Discard a prefetch that no longer matches the finalized transcript.
    let prefetch = prefetch.filter(|p| {
        let keep = p.matches(&utterance);
        if !keep {
            p.handle.abort();
        }
        keep
    });

    tracing::info!(
        utterance = %utterance,
        wake_to_transcript_ms = wake_at.elapsed().as_millis(),
        "answering"
    );

    match gateway.handle(&utterance, true, None, prefetch).await {
        Ok(r) => Some(r.answer),
        Err(e) => {
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
            loop {
                match io.readi(&mut buf) {
                    Ok(n) if n > 0 => {
                        if tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break; // consumer gone
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
