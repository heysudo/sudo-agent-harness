//! Canned acknowledgments (spec §5).
//!
//! Six short clips are synthesized once and kept as raw PCM in tmpfs. Whenever a
//! turn is about to run a tool round, one plays instantly — so the user hears
//! something within milliseconds instead of waiting out a search.
//!
//! Synthesis happens at boot (or is loaded from tmpfs if a previous boot already
//! did it), never inside a turn.

use crate::audio::AudioPlayer;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const PHRASES: &[&str] = &[
    "ଠିକ୍ ଅଛି।",
    "ଦେଖୁଛି।",
    "ଏକ ମୁହୂର୍ତ୍ତ — ଖୋଜୁଛି।",
    "ମୁଁ ଦେଖୁଛି।",
    "ଅପେକ୍ଷା କରନ୍ତୁ।",
    "ଦେଖିବାକୁ ଚାହୁଁଛି।",
];

pub struct AckBank {
    clips: Vec<Arc<Vec<i16>>>,
    cursor: AtomicUsize,
}

impl AckBank {
    pub fn empty() -> Self {
        Self { clips: Vec::new(), cursor: AtomicUsize::new(0) }
    }

    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    pub fn len(&self) -> usize {
        self.clips.len()
    }

    /// Load cached clips from `dir`, synthesizing any that are missing.
    ///
    /// Failure is never fatal: a device that answers without an acknowledgment
    /// sound is fine, one that will not boot is not.
    pub async fn load_or_build(
        dir: &Path,
        tts: &super::tts::Tts,
        player: &AudioPlayer,
    ) -> Self {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "cannot create ack dir");
            return Self::empty();
        }

        let mut clips = Vec::new();
        for (i, phrase) in PHRASES.iter().enumerate() {
            let path = clip_path(dir, i);
            match load_clip(&path) {
                Ok(pcm) if !pcm.is_empty() => {
                    clips.push(Arc::new(pcm));
                    continue;
                }
                _ => {}
            }
            if !tts.is_enabled() {
                continue;
            }
            // Bound each synthesis. A provider that accepts the connection and then
            // stalls must not hold the whole device in `activating` — an ack is a
            // nicety, booting is not. (Sarvam will sit on an idle socket for ~60s
            // before erroring, which is 6 minutes across the bank.)
            let built = tokio::time::timeout(
                std::time::Duration::from_secs(8),
                synthesize(tts, player, phrase),
            )
            .await;
            match built {
                Ok(Ok(pcm)) if !pcm.is_empty() => {
                    if let Err(e) = save_clip(&path, &pcm) {
                        tracing::warn!(error = %e, "could not cache ack clip");
                    }
                    clips.push(Arc::new(pcm));
                }
                Ok(Ok(_)) => tracing::warn!(phrase, "ack synthesis produced no audio"),
                Ok(Err(e)) => tracing::warn!(phrase, error = %e, "ack synthesis failed"),
                Err(_) => tracing::warn!(phrase, "ack synthesis timed out; skipping"),
            }
        }

        tracing::info!(count = clips.len(), dir = %dir.display(), "acknowledgment clips ready");
        Self { clips, cursor: AtomicUsize::new(0) }
    }

    /// Build directly from PCM, for tests.
    pub fn from_clips(clips: Vec<Vec<i16>>) -> Self {
        Self {
            clips: clips.into_iter().map(Arc::new).collect(),
            cursor: AtomicUsize::new(0),
        }
    }

    /// Play the next acknowledgment. Round-robins so the device does not sound
    /// like a recording.
    pub fn play(&self, player: &AudioPlayer) -> bool {
        if self.clips.is_empty() {
            return false;
        }
        let i = self.cursor.fetch_add(1, Ordering::Relaxed) % self.clips.len();
        player.try_play(self.clips[i].as_ref().clone())
    }
}

fn clip_path(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("ack{index}.pcm"))
}

fn load_clip(path: &Path) -> Result<Vec<i16>> {
    let bytes = std::fs::read(path)?;
    Ok(crate::audio::pcm_s16le_to_samples(&bytes))
}

fn save_clip(path: &Path, pcm: &[i16]) -> Result<()> {
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Synthesize one phrase by capturing what the TTS engine would have played.
///
/// Uses a throwaway player wired to a capturing backend so the clip is never
/// audible during boot.
async fn synthesize(
    tts: &super::tts::Tts,
    _live_player: &AudioPlayer,
    phrase: &str,
) -> Result<Vec<i16>> {
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    let backend = CaptureBackend { out: captured.clone() };
    let capture_player = AudioPlayer::spawn_with(Box::new(backend), 16_000)?;

    let (tx, rx) = tokio::sync::mpsc::channel(2);
    tx.send(phrase.to_string()).await.ok();
    drop(tx);

    tts.speak(rx, &capture_player, capture_player.generation(), None).await?;

    // Let the playback thread finish consuming before reading the buffer.
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let len = captured.lock().unwrap().len();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if len > 0 && len == captured.lock().unwrap().len() {
            break;
        }
    }
    capture_player.stop().await;

    let pcm = captured.lock().unwrap().clone();
    Ok(pcm)
}

struct CaptureBackend {
    out: Arc<std::sync::Mutex<Vec<i16>>>,
}

impl crate::audio::Backend for CaptureBackend {
    fn write(&mut self, samples: &[i16]) -> Result<()> {
        self.out.lock().unwrap().extend_from_slice(samples);
        Ok(())
    }
    fn discard(&mut self) -> Result<()> {
        Ok(())
    }
    fn drain(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn six_phrases_are_defined_and_short() {
        assert_eq!(PHRASES.len(), 6);
        for p in PHRASES {
            assert!(p.split_whitespace().count() <= 6, "{p:?} is too long to be instant");
        }
    }

    #[test]
    fn empty_bank_plays_nothing_without_panicking() {
        let bank = AckBank::empty();
        let p = AudioPlayer::spawn_with(
            Box::new(crate::audio::NullBackend::new(16_000)),
            16_000,
        )
        .unwrap();
        assert!(!bank.play(&p));
        assert!(bank.is_empty());
    }

    #[test]
    fn clips_round_robin() {
        let bank = AckBank::from_clips(vec![vec![1], vec![2], vec![3]]);
        assert_eq!(bank.len(), 3);
        let p = AudioPlayer::spawn_with(
            Box::new(crate::audio::NullBackend::new(16_000)),
            16_000,
        )
        .unwrap();
        for _ in 0..7 {
            assert!(bank.play(&p));
        }
        // cursor wrapped without panicking
        assert!(bank.cursor.load(Ordering::Relaxed) >= 7);
    }

    #[test]
    fn clips_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = clip_path(dir.path(), 0);
        let pcm = vec![-1i16, 0, 1, 32767, -32768];
        save_clip(&path, &pcm).unwrap();
        assert_eq!(load_clip(&path).unwrap(), pcm);
    }

    #[tokio::test]
    async fn missing_dir_and_no_tts_yields_an_empty_bank_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = AudioPlayer::spawn_with(
            Box::new(crate::audio::NullBackend::new(16_000)),
            16_000,
        )
        .unwrap();
        let bank =
            AckBank::load_or_build(&dir.path().join("nested"), &super::super::tts::Tts::Disabled, &p)
                .await;
        assert!(bank.is_empty());
    }
}
