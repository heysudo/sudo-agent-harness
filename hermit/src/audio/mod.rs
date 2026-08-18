//! Audio output: a small ring buffer in front of ALSA, with instant flush.
//!
//! Everything plays through the ONE sound card — the XVF3800 in USB mode — so the
//! chip's hardware AEC always has a loopback reference and wake-word detection keeps
//! working while music or TTS is playing (spec §2 topology, LOCKED).
//!
//! # Barge-in
//!
//! When the wake word fires mid-answer, queued speech must stop being audible
//! within ~100 ms. Two things happen together:
//!
//! 1. A monotonically increasing *generation* counter is bumped. Any PCM still
//!    sitting in the channel carries an older generation and is discarded rather
//!    than played.
//! 2. `snd_pcm_drop` throws away frames already handed to the driver.
//!
//! Without (2), audio already inside the ALSA buffer keeps playing for as long as
//! that buffer is deep — which is exactly why `audio.buffer_ms` is kept small.

#[cfg(feature = "alsa-audio")]
mod alsa_sink;
mod null_sink;

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Messages to the playback thread.
pub enum AudioMsg {
    /// Signed 16-bit mono PCM at the configured rate, tagged with the generation
    /// it was produced in.
    Pcm { generation: u64, samples: Vec<i16> },
    /// Discard everything buffered, in software and in the driver.
    Flush,
    Stop,
}

/// What a playback backend must do.
pub trait Backend: Send {
    /// Write mono PCM, blocking until the driver accepts it.
    fn write(&mut self, samples: &[i16]) -> Result<()>;
    /// Discard buffered frames immediately (barge-in).
    fn discard(&mut self) -> Result<()>;
    /// Block until buffered audio has actually played out.
    fn drain(&mut self) -> Result<()>;
}

/// Handle used from async code to play and flush audio.
#[derive(Clone)]
pub struct AudioPlayer {
    tx: tokio::sync::mpsc::Sender<AudioMsg>,
    generation: Arc<AtomicU64>,
    sample_rate: u32,
    /// Speaker mute (console `control.json`). Muted playback writes ZEROS rather
    /// than skipping writes: the XVF3800's capture clock is slaved to playback, so
    /// a mute that stopped the stream would also deafen the microphone.
    muted: Arc<std::sync::atomic::AtomicBool>,
}

impl AudioPlayer {
    /// Spawn the playback thread for the configured backend.
    ///
    /// ALSA calls block, so they live on a dedicated OS thread rather than a tokio
    /// worker — a blocked worker would stall unrelated tasks on a 4-core box.
    /// Falls back to a silent backend if the audio device cannot be opened, rather
    /// than refusing to start.
    ///
    /// This matters on a real device: the named PCM lives in `/etc/asound.conf`, so
    /// a not-yet-provisioned box, a typo'd PCM name, or the reSpeaker being
    /// unplugged would otherwise take down the whole daemon — including the text
    /// and WebSocket front ends, which need no audio at all. Same policy as TTS and
    /// the wake word: degrade loudly, keep serving. The error is logged at ERROR so
    /// it cannot be mistaken for working audio.
    pub fn spawn(cfg: &crate::config::Audio) -> Result<Self> {
        let backend = match make_backend(cfg) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(
                    error = ?e,
                    pcm = %cfg.playback_pcm,
                    "could not open the audio device; continuing with audio DISABLED. \
                     Text and WebSocket still work. Check /etc/asound.conf and that the \
                     reSpeaker is connected."
                );
                Box::new(null_sink::NullBackend::new(cfg.sample_rate))
            }
        };
        // One period of silence per idle tick. See spawn_keepalive: without this the
        // XVF3800's microphone returns EIO and the wake word never fires.
        let keepalive_frames = if cfg.keepalive_silence {
            ((cfg.sample_rate as usize / 1000) * cfg.period_ms as usize).max(64)
        } else {
            0
        };
        Self::spawn_keepalive(backend, cfg.sample_rate, keepalive_frames)
    }

    pub fn spawn_with(backend: Box<dyn Backend>, sample_rate: u32) -> Result<Self> {
        Self::spawn_inner(backend, sample_rate, 0)
    }

    /// Spawn with a **keepalive silence** stream of `keepalive_frames` per idle tick.
    ///
    /// # Why this exists (measured on the XVF3800, 2026-08-18)
    ///
    /// The reSpeaker's USB capture endpoint is SYNC type and its capture clock is
    /// slaved to the playback stream. Measured on hardware:
    ///
    /// | condition                      | result                       |
    /// |--------------------------------|------------------------------|
    /// | capture alone                  | `EIO`, zero frames           |
    /// | capture while playback streams  | works, full-length audio     |
    /// | capture after playback stops    | `EIO` again, immediately     |
    ///
    /// So the microphone only produces data while something is being played. A
    /// voice assistant that only opens playback when it wants to speak would have a
    /// **permanently deaf microphone** — no wake word, ever.
    ///
    /// The fix is to never let the playback stream go idle: when there is nothing
    /// to play, write silence. At 16 kHz mono that is 32 kB/s of zeroes, which
    /// costs nothing and has a second benefit — the XVF3800's AEC reference path
    /// stays continuously fed, which is exactly what the LOCKED topology wants.
    ///
    /// Pass `keepalive_frames = 0` to disable (tests, and non-ALSA dev builds).
    pub fn spawn_keepalive(
        backend: Box<dyn Backend>,
        sample_rate: u32,
        keepalive_frames: usize,
    ) -> Result<Self> {
        Self::spawn_inner(backend, sample_rate, keepalive_frames)
    }

    fn spawn_inner(
        mut backend: Box<dyn Backend>,
        sample_rate: u32,
        keepalive_frames: usize,
    ) -> Result<Self> {
        // Depth 64: ~1.3 s of 20 ms chunks. Deep enough that TTS never stalls on a
        // slow write, shallow enough to bound RAM and flush latency.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AudioMsg>(64);
        let generation = Arc::new(AtomicU64::new(0));
        let gen_for_thread = generation.clone();

        std::thread::Builder::new()
            .name("audio-out".into())
            .spawn(move || {
                let silence = vec![0i16; keepalive_frames];
                // How long `silence` should take to play; used to pace the idle loop
                // when the backend does not block (e.g. the null backend on a dev
                // machine), so this never becomes a busy-spin.
                let silence_dur = if sample_rate > 0 {
                    std::time::Duration::from_secs_f64(
                        keepalive_frames as f64 / sample_rate as f64,
                    )
                } else {
                    std::time::Duration::from_millis(20)
                };

                loop {
                    let msg = if keepalive_frames == 0 {
                        // No keepalive: plain blocking wait.
                        match rx.blocking_recv() {
                            Some(m) => m,
                            None => break,
                        }
                    } else {
                        match rx.try_recv() {
                            Ok(m) => m,
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                                // Nothing to play: keep the device clocked.
                                //
                                // Do NOT sleep here. A blocking ALSA write returns as
                                // soon as the frames are ACCEPTED into the buffer, not
                                // when they are played — so on an empty buffer it
                                // returns instantly. Sleeping on that "fast" return
                                // starves the buffer and produces a continuous stream
                                // of EPIPE underruns (observed on hardware: one every
                                // ~20 ms). Writing straight back fills the buffer until
                                // ALSA blocks, which is the correct pacing. Backends
                                // that never block (the null/test sinks) pace
                                // themselves inside write().
                                if let Err(e) = backend.write(&silence) {
                                    tracing::warn!(error = %e, "keepalive write failed");
                                    std::thread::sleep(silence_dur);
                                }
                                continue;
                            }
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                        }
                    };

                    match msg {
                        AudioMsg::Pcm { generation, samples } => {
                            // Stale audio from before a barge-in: drop it silently.
                            if generation < gen_for_thread.load(Ordering::Acquire) {
                                continue;
                            }
                            if let Err(e) = backend.write(&samples) {
                                tracing::warn!(error = %e, "audio write failed");
                            }
                        }
                        AudioMsg::Flush => {
                            if let Err(e) = backend.discard() {
                                tracing::warn!(error = %e, "audio discard failed");
                            }
                        }
                        AudioMsg::Stop => break,
                    }
                }
                let _ = backend.drain();
                tracing::debug!("audio thread exiting");
            })?;

        Ok(Self { tx, generation, sample_rate, muted: Arc::new(std::sync::atomic::AtomicBool::new(false)) })
    }

    /// Speaker mute. Audio keeps flowing as silence so capture stays clocked.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Queue PCM for playback. Returns false if the player has shut down.
    pub async fn play(&self, mut samples: Vec<i16>) -> bool {
        if self.is_muted() {
            samples.iter_mut().for_each(|s| *s = 0);
        }
        let generation = self.generation.load(Ordering::Acquire);
        self.tx.send(AudioMsg::Pcm { generation, samples }).await.is_ok()
    }

    /// Queue PCM without awaiting; drops the chunk if the buffer is full.
    ///
    /// Used from contexts that must not block. A dropped chunk is a click; a
    /// blocked hot path is a hang, so this is the right trade there.
    pub fn try_play(&self, mut samples: Vec<i16>) -> bool {
        if self.is_muted() {
            samples.iter_mut().for_each(|s| *s = 0);
        }
        let generation = self.generation.load(Ordering::Acquire);
        match self.tx.try_send(AudioMsg::Pcm { generation, samples }) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("dropping audio chunk: {e}");
                false
            }
        }
    }

    /// Barge-in. Bumps the generation so queued chunks are discarded, then tells
    /// the driver to drop what it already has.
    ///
    /// Ordering matters: bump FIRST so nothing produced before this instant can
    /// still be written after the discard.
    pub fn flush(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        if let Err(e) = self.tx.try_send(AudioMsg::Flush) {
            tracing::warn!("could not signal audio flush: {e}");
        }
    }

    /// Current generation — callers can check whether their stream is still current.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Has a flush happened since `generation` was taken?
    pub fn is_stale(&self, generation: u64) -> bool {
        self.generation.load(Ordering::Acquire) > generation
    }

    pub async fn stop(&self) {
        let _ = self.tx.send(AudioMsg::Stop).await;
    }
}

#[cfg(feature = "alsa-audio")]
fn make_backend(cfg: &crate::config::Audio) -> Result<Box<dyn Backend>> {
    Ok(Box::new(alsa_sink::AlsaBackend::open(cfg)?))
}

#[cfg(not(feature = "alsa-audio"))]
fn make_backend(cfg: &crate::config::Audio) -> Result<Box<dyn Backend>> {
    tracing::warn!(
        "built without the alsa-audio feature: audio will be discarded. \
         Build with --features pi for the Raspberry Pi."
    );
    Ok(Box::new(null_sink::NullBackend::new(cfg.sample_rate)))
}

/// Convert interleaved stereo to mono by averaging.
///
/// The speaker is a single 3W driver (§2), so everything is downmixed. Averaging in
/// i32 avoids the wraparound that `(a + b) / 2` produces in i16 on loud material.
pub fn stereo_to_mono(input: &[i16]) -> Vec<i16> {
    input
        .chunks_exact(2)
        .map(|p| ((p[0] as i32 + p[1] as i32) / 2) as i16)
        .collect()
}

/// Decode little-endian signed 16-bit PCM bytes into samples.
pub fn pcm_s16le_to_samples(bytes: &[u8]) -> Vec<i16> {
    bytes
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

/// Scale samples by a linear factor, saturating rather than wrapping.
pub fn apply_gain(samples: &mut [i16], gain: f32) {
    if (gain - 1.0).abs() < f32::EPSILON {
        return;
    }
    for s in samples.iter_mut() {
        *s = (*s as f32 * gain).clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
}

pub use null_sink::NullBackend;

/// Open the microphone PCM (ALSA builds only).
#[cfg(feature = "alsa-audio")]
pub use alsa_sink::open_capture as alsa_capture;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Backend that records what it was asked to do.
    struct RecordingBackend {
        written: Arc<Mutex<Vec<i16>>>,
        discards: Arc<AtomicU64>,
    }

    impl Backend for RecordingBackend {
        fn write(&mut self, samples: &[i16]) -> Result<()> {
            self.written.lock().unwrap().extend_from_slice(samples);
            // Model a real device: consume at roughly realtime so the keepalive loop
            // paces instead of spinning. 16 kHz assumed for test sizing.
            std::thread::sleep(std::time::Duration::from_secs_f64(
                samples.len() as f64 / 16_000.0,
            ));
            Ok(())
        }
        fn discard(&mut self) -> Result<()> {
            self.discards.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn drain(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stereo_downmix_averages_without_overflow() {
        // i16::MAX in both channels must stay at i16::MAX, not wrap negative.
        assert_eq!(stereo_to_mono(&[i16::MAX, i16::MAX]), vec![i16::MAX]);
        assert_eq!(stereo_to_mono(&[i16::MIN, i16::MIN]), vec![i16::MIN]);
        assert_eq!(stereo_to_mono(&[100, 200, -50, 50]), vec![150, 0]);
    }

    #[test]
    fn downmix_ignores_a_trailing_orphan_sample() {
        assert_eq!(stereo_to_mono(&[10, 20, 30]), vec![15]);
    }

    #[test]
    fn pcm_decode_is_little_endian() {
        // 0x0100 LE = 1, 0xFFFF LE = -1
        assert_eq!(pcm_s16le_to_samples(&[0x01, 0x00, 0xFF, 0xFF]), vec![1, -1]);
        // odd trailing byte is ignored rather than panicking
        assert_eq!(pcm_s16le_to_samples(&[0x01, 0x00, 0x7F]), vec![1]);
    }

    #[test]
    fn gain_saturates_instead_of_wrapping() {
        let mut s = [i16::MAX, i16::MIN, 100];
        apply_gain(&mut s, 4.0);
        assert_eq!(s[0], i16::MAX);
        assert_eq!(s[1], i16::MIN);
        assert_eq!(s[2], 400);
    }

    #[test]
    fn unity_gain_is_a_no_op() {
        let mut s = [1, 2, 3];
        apply_gain(&mut s, 1.0);
        assert_eq!(s, [1, 2, 3]);
    }

    #[tokio::test]
    async fn audio_reaches_the_backend() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let discards = Arc::new(AtomicU64::new(0));
        let p = AudioPlayer::spawn_with(
            Box::new(RecordingBackend { written: written.clone(), discards: discards.clone() }),
            16_000,
        )
        .unwrap();

        assert!(p.play(vec![1, 2, 3]).await);
        // Give the thread a moment to consume.
        for _ in 0..50 {
            if written.lock().unwrap().len() == 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(*written.lock().unwrap(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn flush_bumps_generation_and_discards_stale_audio() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let discards = Arc::new(AtomicU64::new(0));
        let p = AudioPlayer::spawn_with(
            Box::new(RecordingBackend { written: written.clone(), discards: discards.clone() }),
            16_000,
        )
        .unwrap();

        let before = p.generation();
        // Capture a generation, then flush, then try to play with the old one.
        p.flush();
        assert!(p.is_stale(before), "generation must advance on flush");
        assert_eq!(p.generation(), before + 1);

        // Directly enqueue a stale chunk the way a mid-flight TTS stream would.
        p.tx
            .send(AudioMsg::Pcm { generation: before, samples: vec![9, 9, 9] })
            .await
            .unwrap();
        // And a current one.
        p.play(vec![7]).await;

        for _ in 0..50 {
            if !written.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let got = written.lock().unwrap().clone();
        assert!(!got.contains(&9), "stale audio must never be written: {got:?}");
        assert_eq!(got, vec![7]);
        assert!(discards.load(Ordering::SeqCst) >= 1, "driver buffer must be dropped too");
    }

    #[test]
    fn a_missing_audio_device_does_not_prevent_startup() {
        // Regression: on an unprovisioned Pi the named PCM does not exist yet, and
        // the daemon used to exit rather than serve text.
        let cfg = crate::config::Audio {
            playback_pcm: "definitely-not-a-real-pcm".into(),
            ..Default::default()
        };
        assert!(
            AudioPlayer::spawn(&cfg).is_ok(),
            "a missing audio device must degrade to silence, not abort startup"
        );
    }

    #[tokio::test]
    async fn keepalive_writes_silence_while_idle() {
        // The XVF3800 stops delivering capture the moment playback goes idle, so an
        // idle player MUST keep writing. Regression guard for a deaf microphone.
        let written = Arc::new(Mutex::new(Vec::new()));
        let discards = Arc::new(AtomicU64::new(0));
        let p = AudioPlayer::spawn_keepalive(
            Box::new(RecordingBackend { written: written.clone(), discards }),
            16_000,
            160, // 10 ms
        )
        .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let n = written.lock().unwrap().len();
        assert!(n >= 160, "expected keepalive silence while idle, got {n} samples");
        assert!(
            written.lock().unwrap().iter().all(|&s| s == 0),
            "keepalive must be silence, not noise"
        );
        p.stop().await;
    }

    #[tokio::test]
    async fn keepalive_disabled_writes_nothing_while_idle() {
        let written = Arc::new(Mutex::new(Vec::new()));
        let discards = Arc::new(AtomicU64::new(0));
        let _p = AudioPlayer::spawn_keepalive(
            Box::new(RecordingBackend { written: written.clone(), discards }),
            16_000,
            0,
        )
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        assert!(written.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn flush_is_cheap_enough_for_barge_in() {
        let p = AudioPlayer::spawn_with(Box::new(NullBackend::new(16_000)), 16_000).unwrap();
        let start = std::time::Instant::now();
        for _ in 0..100 {
            p.flush();
        }
        let per = start.elapsed().as_micros() as f64 / 100.0;
        // Flush must be effectively free; the 100ms barge-in budget is dominated by
        // the driver drop, not by this bookkeeping.
        assert!(per < 2000.0, "flush took {per}us; barge-in budget is 100ms total");
    }
}
