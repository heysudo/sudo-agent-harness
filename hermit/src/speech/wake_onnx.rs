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
const WINDOW_SAMPLES: usize = FRAME_SAMPLES * WINDOW_FRAMES; // 32000
/// Mel frames per embedding, and stride between them (livekit-wakeword constants).
const EMBEDDING_WINDOW: usize = 76;
const EMBEDDING_STRIDE: usize = 8;
/// Classifier input length — the last N embeddings.
const MIN_EMBEDDINGS: usize = 16;
const EMBEDDING_DIM: usize = 96;
const MEL_BINS: usize = 32;

/// Reference default from `livekit.wakeword` / openWakeWord.
pub const DEFAULT_THRESHOLD: f32 = 0.5;

/// Score every Nth 80 ms frame rather than every frame.
///
/// Measured on the Pi 4: one full-window score (mel + 9 embeddings + classifier)
/// costs ~200 ms. Scoring every 80 ms frame therefore runs ~2.5x behind real time;
/// the mic channel backs up, ALSA overruns, and the detector ends up scoring gapped
/// audio — live scores collapse to a fraction of what the same audio scores offline
/// (observed: 0.16 live vs 0.80 offline on an identical utterance). At stride 4 the
/// scoring cadence is 320 ms > cost, the pipeline keeps up, and a 2 s window still
/// overlaps the phrase several times. Worst-case added detection latency: 320 ms.
const SCORE_STRIDE_FRAMES: u32 = 4;

pub struct HeySudo {
    mel: Session,
    embed: Session,
    classifier: Session,
    threshold: f32,
    /// Rolling 2 s audio window, oldest-first, one Vec per 80 ms frame.
    window: std::collections::VecDeque<Vec<i16>>,
    /// Refractory counter: suppress re-triggers for a few frames after a fire so one
    /// utterance does not produce a burst of wakes.
    cooldown: usize,
    /// Liveness/tuning telemetry: rolling max score and window count. Without this a
    /// detector that is silently starved of audio looks identical to a quiet room.
    windows_scored: u64,
    recent_max: f32,
    /// Frames since the last score, for [`SCORE_STRIDE_FRAMES`].
    frames_since_score: u32,
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
    pub fn load(classifier_path: Option<&Path>, threshold: f32) -> Result<Self> {
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
            window: std::collections::VecDeque::with_capacity(WINDOW_FRAMES),
            cooldown: 0,
            windows_scored: 0,
            recent_max: 0.0,
            frames_since_score: 0,
            avg_score_ms: 0.0,
            last_score: 0.0,
        })
    }

    pub fn from_config(cfg: &crate::config::Wake) -> Result<Self> {
        let threshold = if cfg.sensitivity > 0.0 { cfg.sensitivity } else { DEFAULT_THRESHOLD };
        Self::load(cfg.keyword_path.as_deref(), threshold)
    }

    /// Score the current 2 s window. Returns the classifier probability in [0, 1].
    ///
    /// Mirrors `WakeWordModel.predict` exactly: mel over the whole window, then
    /// 76-frame embedding windows at stride 8, then the last 16 embeddings through
    /// the classifier.
    fn score_window(&mut self) -> Result<f32> {
        // ---- assemble the 2 s window as f32 in [-1, 1) ----------------------
        let mut audio = Vec::with_capacity(WINDOW_SAMPLES);
        for frame in &self.window {
            for &s in frame {
                audio.push(s as f32 / 32768.0);
            }
        }
        if audio.len() < WINDOW_SAMPLES {
            return Ok(0.0);
        }

        // ---- mel: (1, samples) -> (time, 32), then x/10 + 2 -----------------
        let mel = self.run_mel(&audio)?;
        let n_mel = mel.shape()[0];
        if n_mel < EMBEDDING_WINDOW {
            return Ok(0.0);
        }

        // ---- embeddings: 76-frame windows, stride 8 -------------------------
        let mut embeddings: Vec<Vec<f32>> = Vec::new();
        let mut start = 0;
        while start + EMBEDDING_WINDOW <= n_mel {
            let win = mel.slice(ndarray::s![start..start + EMBEDDING_WINDOW, ..]).to_owned();
            embeddings.push(self.run_embed(&win)?);
            start += EMBEDDING_STRIDE;
        }
        if embeddings.len() < MIN_EMBEDDINGS {
            return Ok(0.0);
        }

        // ---- classifier over the last 16 embeddings -------------------------
        let take = &embeddings[embeddings.len() - MIN_EMBEDDINGS..];
        let mut seq = Array3::<f32>::zeros((1, MIN_EMBEDDINGS, EMBEDDING_DIM));
        for (i, emb) in take.iter().enumerate() {
            for (j, &v) in emb.iter().enumerate() {
                seq[[0, i, j]] = v;
            }
        }
        self.run_classifier(seq)
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
        // Slide the 2 s window forward by one 80 ms frame.
        if self.window.len() == WINDOW_FRAMES {
            self.window.pop_front();
        }
        self.window.push_back(frame.to_vec());

        if self.window.len() < WINDOW_FRAMES {
            return None; // not enough audio yet
        }
        if self.cooldown > 0 {
            self.cooldown -= 1;
            return None;
        }
        // Stride: scoring every frame costs more than real time on the Pi and
        // corrupts the capture stream (see SCORE_STRIDE_FRAMES).
        self.frames_since_score += 1;
        if self.frames_since_score < SCORE_STRIDE_FRAMES {
            return None;
        }
        self.frames_since_score = 0;

        let scored_at = std::time::Instant::now();
        match self.score_window() {
            Ok(score) => {
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
        assert_eq!(WINDOW_SAMPLES, 32_000, "2.0 s at 16 kHz");
        assert_eq!(FRAME_SAMPLES, 1_280, "80 ms at 16 kHz");
        assert_eq!(WINDOW_FRAMES, 25);
        assert_eq!(EMBEDDING_WINDOW, 76);
        assert_eq!(EMBEDDING_STRIDE, 8);
        assert_eq!(MIN_EMBEDDINGS, 16);
    }

    #[test]
    fn model_dir_is_overridable() {
        // Verified without touching the global env of other tests.
        assert_eq!(model_dir(), PathBuf::from("/opt/hermit/models"));
    }

    #[test]
    fn missing_models_error_clearly() {
        // Session is not Debug, so match on the Err arm rather than unwrap_err().
        match HeySudo::load(Some(Path::new("/nope/hey_sudo.onnx")), 0.5) {
            Err(e) => assert!(
                e.to_string().contains("not found"),
                "error should name the missing file: {e}"
            ),
            Ok(_) => panic!("loading a nonexistent model must fail"),
        }
    }
}
