//! Earcons — the short UI sounds, borrowed from the b2-34 project.
//!
//! Two are wired (both b2-34 semantics, both explicit user requests there and here):
//! - `trigger_ack` plays the instant the wake word fires: "heard you".
//! - `thinking` plays at end of speech: "stopped listening, working on it".
//!
//! They are mono s16le 16 kHz WAVs — hermit's native format — so they are decoded
//! once at boot and enqueued as raw PCM with zero conversion. A missing file logs
//! once and stays silent; earcons must never gate boot or a turn.

use crate::audio::AudioPlayer;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

fn asset_dir() -> PathBuf {
    std::env::var("HERMIT_ASSET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/opt/hermit/assets"))
}

/// The decoded earcon set. Missing files decode to `None` (silent).
pub struct Earcons {
    trigger_ack: Option<Vec<i16>>,
    thinking: Option<Vec<i16>>,
}

impl Earcons {
    pub fn load() -> Self {
        let dir = asset_dir().join("earcons");
        let load = |name: &str| -> Option<Vec<i16>> {
            let path = dir.join(format!("{name}.wav"));
            match load_wav_mono_s16(&path) {
                Ok(pcm) => Some(pcm),
                Err(e) => {
                    tracing::warn!(earcon = name, path = %path.display(), error = %e,
                        "earcon unavailable; that cue will be silent");
                    None
                }
            }
        };
        let e = Self { trigger_ack: load("trigger_ack"), thinking: load("thinking") };
        tracing::info!(
            trigger_ack = e.trigger_ack.is_some(),
            thinking = e.thinking.is_some(),
            dir = %dir.display(),
            "earcons loaded"
        );
        e
    }

    /// Empty set for tests / dev machines without assets.
    pub fn none() -> Self {
        Self { trigger_ack: None, thinking: None }
    }

    /// "Heard the wake word." Play IMMEDIATELY after the barge-in flush — the flush
    /// bumps the player generation, so enqueue after it or the chirp is discarded
    /// as stale.
    pub fn play_trigger_ack(&self, player: &AudioPlayer) {
        if let Some(pcm) = &self.trigger_ack {
            player.try_play(pcm.clone());
        }
    }

    /// "Stopped listening; working on it."
    pub fn play_thinking(&self, player: &AudioPlayer) {
        if let Some(pcm) = &self.thinking {
            player.try_play(pcm.clone());
        }
    }
}

/// Minimal RIFF/WAVE reader for the earcon files: PCM, mono, 16-bit, 16 kHz.
///
/// A real chunk walk (not a blind 44-byte skip): WAVs written by different tools
/// carry LIST/INFO chunks before `data`, and a blind skip would play metadata as
/// a click. Anything that is not exactly our audio format is rejected loudly —
/// resampling an earcon at boot would be silly; regenerate the asset instead.
pub fn load_wav_mono_s16(path: &Path) -> Result<Vec<i16>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("not a RIFF/WAVE file");
    }

    let mut pos = 12;
    let mut fmt_ok = false;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        match id {
            b"fmt " => {
                if body + 16 > bytes.len() {
                    bail!("truncated fmt chunk");
                }
                let audio_format = u16::from_le_bytes(bytes[body..body + 2].try_into().unwrap());
                let channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().unwrap());
                let rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
                let bits = u16::from_le_bytes(bytes[body + 14..body + 16].try_into().unwrap());
                if audio_format != 1 || channels != 1 || rate != 16_000 || bits != 16 {
                    bail!(
                        "earcon must be PCM mono 16-bit 16kHz, got format={audio_format} \
                         ch={channels} rate={rate} bits={bits} — regenerate with make_earcons.py"
                    );
                }
                fmt_ok = true;
            }
            b"data" => {
                if !fmt_ok {
                    bail!("data chunk before fmt chunk");
                }
                let end = (body + size).min(bytes.len());
                return Ok(bytes[body..end]
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect());
            }
            _ => {} // skip LIST/INFO/etc.
        }
        // Chunks are word-aligned.
        pos = body + size + (size & 1);
    }
    bail!("no data chunk found")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, bits: u16, extra_chunk: bool, samples: &[i16]) -> Vec<u8> {
        let mut data = Vec::new();
        for s in samples {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&0u32.to_le_bytes()); // size patched later
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * channels as u32 * bits as u32 / 8).to_le_bytes());
        out.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        if extra_chunk {
            out.extend_from_slice(b"LIST");
            out.extend_from_slice(&5u32.to_le_bytes());
            out.extend_from_slice(b"INFOx"); // odd size: exercises word alignment
            out.push(0); // pad byte
        }
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        let total = out.len() as u32 - 8;
        out[4..8].copy_from_slice(&total.to_le_bytes());
        out
    }

    #[test]
    fn parses_canonical_wav() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.wav");
        std::fs::write(&p, wav(16_000, 1, 16, false, &[1, -2, 300])).unwrap();
        assert_eq!(load_wav_mono_s16(&p).unwrap(), vec![1, -2, 300]);
    }

    #[test]
    fn skips_extra_chunks_before_data() {
        // The blind-44-byte-skip approach would fail exactly here.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.wav");
        std::fs::write(&p, wav(16_000, 1, 16, true, &[7, 8])).unwrap();
        assert_eq!(load_wav_mono_s16(&p).unwrap(), vec![7, 8]);
    }

    #[test]
    fn rejects_wrong_format_loudly() {
        let dir = tempfile::tempdir().unwrap();
        for (rate, ch, name) in [(44_100u32, 1u16, "rate"), (16_000, 2, "stereo")] {
            let p = dir.path().join(format!("{name}.wav"));
            std::fs::write(&p, wav(rate, ch, 16, false, &[0])).unwrap();
            let e = load_wav_mono_s16(&p).unwrap_err().to_string();
            assert!(e.contains("must be PCM mono"), "{name}: {e}");
        }
    }

    #[test]
    fn rejects_non_wav_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("junk.wav");
        std::fs::write(&p, b"definitely not a wav").unwrap();
        assert!(load_wav_mono_s16(&p).is_err());
        assert!(load_wav_mono_s16(&dir.path().join("absent.wav")).is_err());
    }

    #[test]
    fn missing_assets_mean_silence_not_failure() {
        let e = Earcons::none();
        let p = AudioPlayer::spawn_with(
            Box::new(crate::audio::NullBackend::new(16_000)),
            16_000,
        )
        .unwrap();
        e.play_trigger_ack(&p); // must not panic or error
        e.play_thinking(&p);
    }

    #[test]
    fn shipped_earcons_decode() {
        // Guard the actual assets in the repo: format drift breaks the cue silently.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/earcons");
        for name in ["trigger_ack", "thinking"] {
            let pcm = load_wav_mono_s16(&dir.join(format!("{name}.wav")))
                .unwrap_or_else(|e| panic!("{name}.wav failed to decode: {e}"));
            let ms = pcm.len() as f32 / 16.0;
            assert!(ms > 30.0 && ms < 500.0, "{name} is {ms:.0} ms — expected a short chirp");
        }
    }
}
