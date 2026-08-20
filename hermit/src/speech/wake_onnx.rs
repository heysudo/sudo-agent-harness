//! "Hey Sudo" wake word — the project's own trained model.
//!
//! This is the wake engine the wider `heysudo/sudo` project already trained and
//! ships as `sudoedge/models/hey_sudo.onnx`. It is a **livekit-wakeword** /
//! openWakeWord-style classifier, NOT Porcupine, so it needs its own runtime. This
//! module is a faithful port of that Python inference path (`livekit.wakeword`'s
//! `WakeWordModel.predict`) so the Rust daemon scores bit-for-bit like the reference.
//!
//! # Pipeline (all at 16 kHz, verified against the Python reference)
//!
//! ```text
//!   2.0 s int16 window (25 × 80 ms frames)
//!     -> /32768 to f32
//!     -> melspectrogram.onnx     : (1, samples) -> (time, 32) dB mel, then x/10 + 2
//!     -> for each 76-mel window, stride 8:
//!          embedding_model.onnx  : (1, 76, 32, 1) -> (96,)
//!     -> stack the LAST 16 embeddings -> (1, 16, 96)
//!     -> hey_sudo.onnx (classifier) : (1, 16, 96) -> score in [0, 1]
//!   fire when score >= threshold (reference default 0.5)
//! ```
//!
//! The two upstream graphs (`melspectrogram.onnx`, `embedding_model.onnx`) are the
//! stock livekit-wakeword resources; only `hey_sudo.onnx` is project-trained. All
//! three are deployed to `/opt/hermit/models/` and the daemon loads them by path.
//!
//! onnxruntime is loaded dynamically (`ort` feature `load-dynamic`), so a missing
//! `libonnxruntime.so` degrades to "wake word disabled" exactly like the Porcupine
//! path — the daemon still serves text, voice-via-`/listen`, and everything else.

// # Streaming rewrite (2026-08-20)
//
// The first port recomputed the FULL 2 s pipeline (mel over 32000 samples +
// 16 embeddings + classifier) on every score: ~204 ms on the Pi 4. Even at a
// 4-frame stride that blocked the mic consumer long enough under music load
// (mpv + radio decode) that capture chunks were dropped — and a detector fed
// gapped audio scores near zero regardless of what was said. Observed live:
// score 0.005 while the user shouted the phrase at 60 cm with music playing,
// 19 "mic consumer lagging" warnings in 30 minutes, hermit at 75% CPU.
//
// This version is incremental, mirroring openWakeWord's streaming extractor:
// per 80 ms frame it computes mel for just that chunk (with 480 samples of
// context), one embedding when 8 new mel frames have accumulated, and one
// classifier pass — ~15-20 ms per frame. The consumer never starves, and
// every frame is scored instead of every 4th.

use super::wake::WakeDetector;
use anyhow::{Result, bail};
use ndarray::{Array2, Array3, Array4, ArrayD};
use ort::session::Session;
use ort::value::Tensor;
use std::path::{Path, PathBuf};

/// 80 ms at 16 kHz.
const FRAME_SAMPLES: usize = 1280;
/// 25 × 80 ms = 2.0 s window the classifier expects.
const WINDOW_FRAMES: usize = 25;
/// Mel frames per embedding, and stride between them (livekit-wakeword constants).
const EMBEDDING_WINDOW: usize = 76;
const EMBEDDING_STRIDE: usize = 8;
/// Classifier input length — the last N embeddings.
const MIN_EMBEDDINGS: usize = 16;
const EMBEDDING_DIM: usize = 96;
const MEL_BINS: usize = 32;

/// Reference default from `livekit.wakeword` / openWakeWord.
pub const DEFAULT_THRESHOLD: f32 = 0.5;

/// Raw-audio left context carried between chunks for streaming mel extraction.
///
/// The mel transform uses a 400-sample window at a 160-sample hop. Prepending
/// 480 samples (3 hops) of the previous chunk to each new 1280-sample chunk and
/// keeping the LAST 8 mel frames yields hop-aligned, contiguous mel frames —
/// the same trick openWakeWord's streaming feature extractor uses.
const MEL_CONTEXT_SAMPLES: usize = 480;
/// Mel frames produced per 80 ms chunk (1280 samples / 160-sample hop).
const MEL_FRAMES_PER_CHUNK: usize = 8;

pub struct HeySudo {
    mel: Session,
    embed: Session,
    classifier: Session,
    threshold: f32,
    /// Software gain applied to each sample before the mel transform. The
    /// XVF3800's beamformed voice channel is quiet (~-35 dBFS speech); the
    /// training data was normal-level audio, so at native level real phrases
    /// score far below their offline scores. Same fix as stt.sarvam_gain.
    gain: f32,
    /// Last 480 raw samples of the previous chunk: left context so each
    /// chunk's mel frames are hop-aligned and contiguous with the last.
    mel_context: Vec<i16>,
    /// Rolling mel-frame buffer (newest last), capped at EMBEDDING_WINDOW.
    mel_frames: std::collections::VecDeque<[f32; MEL_BINS]>,
    /// Mel frames accumulated since the last embedding was computed.
    mel_since_embed: usize,
    /// Rolling embedding buffer (newest last), capped at MIN_EMBEDDINGS.
    embeddings: std::collections::VecDeque<Vec<f32>>,
    /// Refractory counter: suppress re-triggers for a few frames after a fire so one
    /// utterance does not produce a burst of wakes.
    cooldown: usize,
    /// Liveness/tuning telemetry: rolling max score and window count. Without this a
    /// detector that is silently starved of audio looks identical to a quiet room.
    windows_scored: u64,
    recent_max: f32,
    /// Rolling average score cost, surfaced in the heartbeat so a regression in
    /// scoring speed is visible in logs rather than as mysteriously low scores.
    avg_score_ms: f32,
    /// Most recent score, exposed via `WakeDetector::last_score` for the console.
    last_score: f32,
}

/// Where the three models live, resolved with an env override for flexibility.
fn model_dir() -> PathBuf {
    std::env::var("HERMIT_MODEL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/opt/hermit/models"))
}

impl HeySudo {
    /// Load the three graphs. `keyword_path` overrides the classifier location; the
    /// two upstream resources are always looked up in the model dir.
    pub fn load(classifier_path: Option<&Path>, threshold: f32, gain: f32) -> Result<Self> {
        let dir = model_dir();
        let mel_path = dir.join("melspectrogram.onnx");
        let embed_path = dir.join("embedding_model.onnx");
        let cls_path = classifier_path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.join("hey_sudo.onnx"));

        for (label, p) in [
            ("melspectrogram.onnx", &mel_path),
            ("embedding_model.onnx", &embed_path),
            ("hey_sudo.onnx", &cls_path),
        ] {
            if !p.exists() {
                bail!(
                    "wake model {label} not found at {}. Deploy the three ONNX files to {} \
                     (or set HERMIT_MODEL_DIR / wake.keyword_path).",
                    p.display(),
                    dir.display()
                );
            }
        }

        // One intra-op thread keeps the always-on wake detector below one core on the
        // passively cooled Pi 4. A live two-thread build held 125-130% CPU and reached
        // 78 C within minutes. One thread still finishes the score inside the 320 ms
        // stride while leaving thermal headroom for the foreground turn.
        //
        // ort's builder/error types are not Send+Sync, so `?` cannot cross a closure
        // boundary here; each session is built inline. `map_err(anyhow)` converts the
        // ort error into something `Context` can wrap.
        let session = |path: &Path, what: &str| -> Result<Session> {
            Session::builder()
                .map_err(|e| anyhow::anyhow!("ort: {e}"))?
                .with_intra_threads(1)
                .map_err(|e| anyhow::anyhow!("ort: {e}"))?
                .commit_from_file(path)
                .map_err(|e| anyhow::anyhow!("loading {what}: {e}"))
        };

        let mel = session(&mel_path, "melspectrogram.onnx")?;
        let embed = session(&embed_path, "embedding_model.onnx")?;
        let classifier = session(&cls_path, "hey_sudo.onnx")?;

        tracing::info!(
            threshold,
            dir = %dir.display(),
            "hey-sudo wake model loaded (mel + embedding + classifier)"
        );

        Ok(Self {
            mel,
            embed,
            classifier,
            threshold,
            gain: if gain > 0.0 { gain } else { 1.0 },
            mel_context: Vec::new(),
            mel_frames: std::collections::VecDeque::with_capacity(
                EMBEDDING_WINDOW + MEL_FRAMES_PER_CHUNK,
            ),
            mel_since_embed: 0,
            embeddings: std::collections::VecDeque::with_capacity(MIN_EMBEDDINGS),
            cooldown: 0,
            windows_scored: 0,
            recent_max: 0.0,
            avg_score_ms: 0.0,
            last_score: 0.0,
        })
    }

    pub fn from_config(cfg: &crate::config::Wake) -> Result<Self> {
        let threshold = if cfg.sensitivity > 0.0 {
            cfg.sensitivity
        } else {
            DEFAULT_THRESHOLD
        };
        Self::load(cfg.keyword_path.as_deref(), threshold, cfg.gain)
    }

    /// Advance the streaming pipeline by one 80 ms chunk and score.
    ///
    /// Semantically identical to the reference full-window computation: after
    /// warm-up, embedding i covers mel frames [8i, 8i+76) exactly as the
    /// batch version's stride-8 slices do — the classifier sees the same
    /// (16, 96) sequence, it is just built incrementally. Cost per frame is
    /// one small mel + ONE embedding + one classifier (~15 ms on the Pi 4)
    /// versus mel-over-2s + SIXTEEN embeddings (~204 ms) for the batch form.
    fn step(&mut self, frame: &[i16]) -> Result<Option<f32>> {
        // ---- mel over [context | chunk], hop-aligned ---------------------
        // Gain rides the int16 -> f32 conversion, clamped to [-1, 1) exactly
        // as saturating i16 arithmetic would. Matches the STT path's boost so
        // the detector hears the same signal level the recognizer does.
        let mut audio = Vec::with_capacity(self.mel_context.len() + frame.len());
        for &s in self.mel_context.iter().chain(frame.iter()) {
            audio.push((s as f32 * self.gain / 32768.0).clamp(-1.0, 0.999_969_5));
        }
        self.mel_context = frame[frame.len() - MEL_CONTEXT_SAMPLES..].to_vec();

        let mel = self.run_mel(&audio)?;
        let n_mel = mel.shape()[0];
        if n_mel < MEL_FRAMES_PER_CHUNK {
            return Ok(None);
        }
        // Keep the LAST 8 frames: with 480 samples of context the chunk yields
        // 9 frames whose first duplicates the previous chunk's last hop.
        for i in (n_mel - MEL_FRAMES_PER_CHUNK)..n_mel {
            let mut row = [0.0f32; MEL_BINS];
            for (j, cell) in row.iter_mut().enumerate() {
                *cell = mel[[i, j]];
            }
            if self.mel_frames.len() == EMBEDDING_WINDOW + MEL_FRAMES_PER_CHUNK {
                self.mel_frames.pop_front();
            }
            self.mel_frames.push_back(row);
        }
        self.mel_since_embed += MEL_FRAMES_PER_CHUNK;

        // ---- one embedding per 8 new mel frames --------------------------
        if self.mel_frames.len() < EMBEDDING_WINDOW || self.mel_since_embed < EMBEDDING_STRIDE {
            return Ok(None);
        }
        self.mel_since_embed = 0;
        let skip = self.mel_frames.len() - EMBEDDING_WINDOW;
        let mut win = Array2::<f32>::zeros((EMBEDDING_WINDOW, MEL_BINS));
        for (i, row) in self.mel_frames.iter().skip(skip).enumerate() {
            for (j, &v) in row.iter().enumerate() {
                win[[i, j]] = v;
            }
        }
        let emb = self.run_embed(&win)?;
        if self.embeddings.len() == MIN_EMBEDDINGS {
            self.embeddings.pop_front();
        }
        self.embeddings.push_back(emb);

        // ---- classifier over the last 16 embeddings -----------------------
        if self.embeddings.len() < MIN_EMBEDDINGS {
            return Ok(None); // still warming up (~2 s from cold)
        }
        let mut seq = Array3::<f32>::zeros((1, MIN_EMBEDDINGS, EMBEDDING_DIM));
        for (i, emb) in self.embeddings.iter().enumerate() {
            for (j, &v) in emb.iter().enumerate() {
                seq[[0, i, j]] = v;
            }
        }
        self.run_classifier(seq).map(Some)
    }

    fn run_mel(&mut self, audio: &[f32]) -> Result<Array2<f32>> {
        let input = Array2::<f32>::from_shape_vec((1, audio.len()), audio.to_vec())?;
        let tensor = Tensor::from_array(input)?;
        let outputs = self.mel.run(ort::inputs![tensor])?;
        let arr = outputs[0].try_extract_array::<f32>()?.to_owned();
        // The graph emits (time, 1, 1, 32) (opset 13 export); squeeze to (time, 32).
        let flat: Vec<f32> = arr.iter().copied().collect();
        let time = flat.len() / MEL_BINS;
        let mut mel = Array2::<f32>::from_shape_vec((time, MEL_BINS), flat)?;
        // Post-processing to match openWakeWord's melspec_transform.
        mel.mapv_inplace(|x| x / 10.0 + 2.0);
        Ok(mel)
    }

    fn run_embed(&mut self, mel_window: &Array2<f32>) -> Result<Vec<f32>> {
        // (76, 32) -> (1, 76, 32, 1) channels-last, as the graph expects.
        let mut inp = Array4::<f32>::zeros((1, EMBEDDING_WINDOW, MEL_BINS, 1));
        for i in 0..EMBEDDING_WINDOW {
            for j in 0..MEL_BINS {
                inp[[0, i, j, 0]] = mel_window[[i, j]];
            }
        }
        let tensor = Tensor::from_array(inp)?;
        let outputs = self.embed.run(ort::inputs![tensor])?;
        let arr = outputs[0].try_extract_array::<f32>()?.to_owned();
        // (1, 1, 1, 96) -> 96 values.
        Ok(arr.iter().copied().collect())
    }

    fn run_classifier(&mut self, seq: Array3<f32>) -> Result<f32> {
        let tensor = Tensor::from_array(seq)?;
        let outputs = self.classifier.run(ort::inputs![tensor])?;
        let arr: ArrayD<f32> = outputs[0].try_extract_array::<f32>()?.to_owned();
        Ok(arr.iter().copied().next().unwrap_or(0.0))
    }
}

impl WakeDetector for HeySudo {
    fn frame_length(&self) -> usize {
        FRAME_SAMPLES
    }

    fn last_score(&self) -> Option<(f32, f32)> {
        Some((self.last_score, self.threshold))
    }

    fn process(&mut self, frame: &[i16]) -> Option<usize> {
        if frame.len() != FRAME_SAMPLES {
            return None;
        }
        if self.cooldown > 0 {
            self.cooldown -= 1;
            // Keep the streaming state advancing through the refractory period
            // so post-wake audio does not arrive as a discontinuity.
            let _ = self.step(frame);
            return None;
        }

        let scored_at = std::time::Instant::now();
        match self.step(frame) {
            Ok(None) => None, // warming up
            Ok(Some(score)) => {
                self.last_score = score;
                // Heartbeat every ~4 s of audio: proves the detector is being fed and
                // gives a real floor/ceiling for tuning `wake.sensitivity`.
                self.windows_scored += 1;
                self.recent_max = self.recent_max.max(score);
                let cost = scored_at.elapsed().as_secs_f32() * 1000.0;
                self.avg_score_ms = if self.avg_score_ms == 0.0 {
                    cost
                } else {
                    self.avg_score_ms * 0.9 + cost * 0.1
                };
                if self.windows_scored.is_multiple_of(50) {
                    tracing::info!(
                        windows = self.windows_scored,
                        max_score = self.recent_max,
                        score_ms = self.avg_score_ms,
                        threshold = self.threshold,
                        "hey-sudo listening"
                    );
                    self.recent_max = 0.0;
                }
                if score >= self.threshold {
                    tracing::info!(score, threshold = self.threshold, "hey-sudo wake");
                    // ~1 s refractory so one "hey sudo" fires once.
                    self.cooldown = WINDOW_FRAMES / 2;
                    Some(0)
                } else {
                    if score >= 0.15 {
                        tracing::debug!(score, "hey-sudo near-miss");
                    }
                    None
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "hey-sudo scoring failed");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_match_the_reference_pipeline() {
        assert_eq!(FRAME_SAMPLES, 1_280, "80 ms at 16 kHz");
        assert_eq!(WINDOW_FRAMES, 25);
        assert_eq!(EMBEDDING_WINDOW, 76);
        assert_eq!(EMBEDDING_STRIDE, 8);
        assert_eq!(MIN_EMBEDDINGS, 16);
        // Streaming alignment invariants: 8 mel frames per 80 ms chunk at a
        // 160-sample hop, with 480 samples (3 hops) of left context so chunk
        // boundaries are hop-aligned (1280 + 480 = 11 hops -> 9 frames, keep 8).
        assert_eq!(FRAME_SAMPLES % 160, 0);
        assert_eq!(MEL_FRAMES_PER_CHUNK, FRAME_SAMPLES / 160);
        assert_eq!(MEL_CONTEXT_SAMPLES % 160, 0);
        // One embedding per classifier hop: stride 8 == frames per chunk, so
        // exactly one embedding is produced per 80 ms once warm.
        assert_eq!(EMBEDDING_STRIDE, MEL_FRAMES_PER_CHUNK);
    }

    #[test]
    fn gain_boosts_and_clamps_like_saturating_i16() {
        // The conversion must saturate, not wrap: a loud frame times 8 stays at
        // full-scale, exactly like the STT path's clamped i16 boost.
        let g = 8.0f32;
        let quiet = (1000_f32 * g / 32768.0).clamp(-1.0, 0.999_969_5);
        assert!(
            (quiet - 0.244).abs() < 0.01,
            "quiet samples scale linearly: {quiet}"
        );
        let loud = (i16::MAX as f32 * g / 32768.0).clamp(-1.0, 0.999_969_5);
        assert!(loud <= 0.999_969_5, "must clamp, not exceed full scale");
        let neg = (i16::MIN as f32 * g / 32768.0).clamp(-1.0, 0.999_969_5);
        assert_eq!(neg, -1.0, "negative rail clamps to -1");
    }

    #[test]
    fn model_dir_is_overridable() {
        // Verified without touching the global env of other tests.
        assert_eq!(model_dir(), PathBuf::from("/opt/hermit/models"));
    }

    #[test]
    fn missing_models_error_clearly() {
        // Session is not Debug, so match on the Err arm rather than unwrap_err().
        match HeySudo::load(Some(Path::new("/nope/hey_sudo.onnx")), 0.5, 8.0) {
            Err(e) => assert!(
                e.to_string().contains("not found"),
                "error should name the missing file: {e}"
            ),
            Ok(_) => panic!("loading a nonexistent model must fail"),
        }
    }
}
