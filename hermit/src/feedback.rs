//! The self-refining feedback loop.
//!
//! After an eligible spoken turn, hermit sometimes asks "did I get that
//! right?", opens a short listen window, and records the verdict. Feedback
//! drives four levers, all bounded:
//!
//! 1. **Wake threshold** - confirmed turns whose wake score sat well above
//!    threshold let it drift up (fewer false wakes); denied turns right at
//!    threshold push it up too (that wake was probably noise); a run of
//!    confirms at low scores lets it drift down. Clamped to [0.30, 0.60].
//! 2. **Ask rate** - the loop asks often while young, then decays toward
//!    `min_ask_probability` as the confirmation rate rises: a device that is
//!    usually right learns to stop nagging. A denial bumps the rate back up.
//! 3. **Corrections as memory** - a denied turn followed by a restated
//!    command becomes a correction fact routed through the reflection
//!    firewall (never directly into the prompt), so recall can whisper
//!    "Akashvani Katak means the Cuttack station" to future turns.
//! 4. **Wake clips as data** - the 2s pre-trigger window that fired the wake
//!    is kept, labeled by the verdict. Confirmed clips are future positives;
//!    denied ones are hard negatives for the Indic retrain.
//!
//! Everything lives under `<data_dir>/feedback/`: `feedback.jsonl` (ledger),
//! `tuning.json` (learned parameters, atomically swapped), `clips/`.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

/// What the user said to "did I get that right?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Yes,
    No,
    /// Neither a yes nor a no - treated as "user ignored the ask".
    Unclear,
}

/// Classify a transcript as yes/no/unclear, tolerantly, in the three
/// languages this device serves (English / Hindi / Odia, romanized or not).
/// Denials win ties: a false "yes" poisons the tuner, a false "no" merely
/// re-asks.
pub fn classify(transcript: &str) -> Verdict {
    let t = transcript.to_lowercase();
    let words: Vec<String> = t
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| c.is_ascii_punctuation() || c == '\u{0964}')
                .to_string()
        })
        .filter(|w| !w.is_empty())
        .collect();

    const NO: &[&str] = &[
        "no",
        "nope",
        "nah",
        "wrong",
        "incorrect",
        "नहीं",
        "नही",
        "नहि",
        "गलत",
        "nahi",
        "nahin",
        "galat",
        "ନା",
        "ନାହିଁ",
        "ନୁହେଁ",
        "ଭୁଲ",
        "nuhen",
        "bhula",
    ];
    const YES: &[&str] = &[
        "yes",
        "yeah",
        "yep",
        "yup",
        "correct",
        "perfect",
        "exactly",
        "right",
        "हां",
        "हाँ",
        "जी",
        "सही",
        "ठीक",
        "haan",
        "han",
        "ji",
        "sahi",
        "theek",
        "thik",
        "ହଁ",
        "ହଉ",
        "ଠିକ",
        "ଠିକ୍",
        "hau",
        "thika",
    ];

    let hit = |set: &[&str]| words.iter().any(|w| set.contains(&w.as_str()));
    if hit(NO) {
        Verdict::No
    } else if hit(YES) {
        Verdict::Yes
    } else {
        Verdict::Unclear
    }
}

/// Learned parameters. Atomically swapped on disk so a crash mid-write can
/// never half-apply; consumed at boot and after each consolidation pass.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Tuning {
    /// Wake threshold the daemon should use (overrides wake.sensitivity when
    /// present). Clamped to [0.30, 0.60] - the tuner may drift, never leap.
    pub wake_threshold: f32,
    /// Current probability of asking for feedback after an eligible turn.
    pub ask_probability: f64,
    /// Lifetime counters, for the consolidator and for honesty in logs.
    pub confirms: u64,
    pub denials: u64,
    pub unclear: u64,
}

impl Tuning {
    pub fn bootstrap(cfg: &crate::config::Config) -> Self {
        Self {
            wake_threshold: if cfg.wake.sensitivity > 0.0 {
                cfg.wake.sensitivity
            } else {
                0.5
            },
            ask_probability: cfg.feedback.ask_probability,
            confirms: 0,
            denials: 0,
            unclear: 0,
        }
    }
}

/// One ledger entry - everything the tuner and the retrain need to know
/// about one asked-and-answered feedback exchange.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub ts: u64,
    pub utterance: String,
    pub answer_head: String,
    pub verdict: String,
    pub wake_score: f32,
    pub wake_threshold: f32,
    /// Set when the wake clip was persisted, relative to clips/.
    pub clip: Option<String>,
    /// The restated command after a denial, when one arrived.
    pub correction: Option<String>,
    pub language: Option<String>,
}

/// Disk layout + the bounded tuning rules. One instance owned by the voice
/// loop; consolidation reads the same files offline.
pub struct FeedbackStore {
    dir: PathBuf,
}

impl FeedbackStore {
    pub fn open(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("feedback");
        std::fs::create_dir_all(dir.join("clips")).context("creating feedback dirs")?;
        Ok(Self { dir })
    }

    pub fn clips_dir(&self) -> PathBuf {
        self.dir.join("clips")
    }

    pub fn load_tuning(&self, cfg: &crate::config::Config) -> Tuning {
        let path = self.dir.join("tuning.json");
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "tuning.json unreadable; bootstrapping");
                Tuning::bootstrap(cfg)
            }),
            Err(_) => Tuning::bootstrap(cfg),
        }
    }

    pub fn save_tuning(&self, t: &Tuning) -> Result<()> {
        let tmp = self.dir.join("tuning.json.tmp");
        let fin = self.dir.join("tuning.json");
        std::fs::write(&tmp, serde_json::to_vec_pretty(t)?)?;
        std::fs::rename(&tmp, &fin)?; // atomic on the same filesystem
        Ok(())
    }

    pub fn append(&self, e: &Entry) -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("feedback.jsonl"))?;
        writeln!(f, "{}", serde_json::to_string(e)?)?;
        Ok(())
    }

    /// Read the whole ledger (it is small: one line per ask, not per turn).
    pub fn entries(&self) -> Vec<Entry> {
        let Ok(text) = std::fs::read_to_string(self.dir.join("feedback.jsonl")) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Keep the newest `max` clips; delete the rest. Called opportunistically.
    pub fn prune_clips(&self, max: u64) {
        let Ok(rd) = std::fs::read_dir(self.clips_dir()) else {
            return;
        };
        let mut files: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        if files.len() as u64 <= max {
            return;
        }
        files.sort(); // names start with the unix timestamp, so oldest first
        let excess = files.len() - max as usize;
        for p in files.into_iter().take(excess) {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Apply one verdict to the tuning state. Pure, bounded, and pinned by tests:
/// this is the ONLY place feedback mutates behavior.
pub fn apply_verdict(t: &mut Tuning, verdict: Verdict, wake_score: f32, min_ask: f64) {
    const THRESH_LO: f32 = 0.30;
    const THRESH_HI: f32 = 0.60;
    const STEP: f32 = 0.01;

    match verdict {
        Verdict::Yes => {
            t.confirms += 1;
            // Confirmed wake at a score comfortably above threshold: the gate
            // can afford to rise a hair (false-wake resistance). Confirmed
            // BARELY above threshold: the gate is well placed; drift down a
            // hair so borderline true wakes stop getting missed.
            if wake_score > t.wake_threshold + 0.15 {
                t.wake_threshold = (t.wake_threshold + STEP).min(THRESH_HI);
            } else if wake_score > 0.0 {
                t.wake_threshold = (t.wake_threshold - STEP).max(THRESH_LO);
            }
            // Confidence earned: ask less often, decaying by a tenth of the
            // distance to the floor per confirm.
            t.ask_probability =
                (t.ask_probability - (t.ask_probability - min_ask) * 0.1).max(min_ask);
        }
        Verdict::No => {
            t.denials += 1;
            // A denial right at the threshold suggests a noise-triggered wake;
            // push the gate up more firmly than a confirm moves it.
            if wake_score > 0.0 && wake_score < t.wake_threshold + 0.10 {
                t.wake_threshold = (t.wake_threshold + 2.0 * STEP).min(THRESH_HI);
            }
            // Trust lost: resume asking.
            t.ask_probability = (t.ask_probability + 0.25).min(1.0);
        }
        Verdict::Unclear => {
            t.unclear += 1;
            // The user could not be bothered - that IS signal: nag less.
            t.ask_probability =
                (t.ask_probability - (t.ask_probability - min_ask) * 0.05).max(min_ask);
        }
    }
}

/// The spoken ask, in the language of the turn (TTS codes).
pub fn ask_phrase(lang: Option<&str>) -> &'static str {
    match lang {
        Some("od-IN") => "ମୁଁ ଠିକ୍ ବୁଝିଲି ତ?",
        Some("hi-IN") => "क्या मैंने सही समझा?",
        _ => "Did I get that right?",
    }
}

/// The re-ask after a denial, inviting a restated command.
pub fn retry_phrase(lang: Option<&str>) -> &'static str {
    match lang {
        Some("od-IN") => "ଦୟାକରି ପୁଣି ଥରେ କୁହନ୍ତୁ।",
        Some("hi-IN") => "कृपया फिर से बताइए।",
        _ => "Sorry - please say that again?",
    }
}

pub fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_no_classification_across_languages() {
        for s in [
            "yes",
            "yeah that is right",
            "correct",
            "perfect thanks",
            "haan",
            "sahi hai",
            "ठीक है",
            "जी हाँ",
            "hau",
            "ଠିକ୍ ଅଛି",
            "ହଁ",
        ] {
            assert_eq!(classify(s), Verdict::Yes, "{s:?} should be Yes");
        }
        for s in [
            "no",
            "nope wrong song",
            "that is incorrect",
            "nahi",
            "galat hai",
            "नहीं",
            "ନା",
            "ଭୁଲ",
            "no no play the other one",
        ] {
            assert_eq!(classify(s), Verdict::No, "{s:?} should be No");
        }
        for s in [
            "play something else entirely",
            "hmm",
            "",
            "what is the weather",
        ] {
            assert_eq!(classify(s), Verdict::Unclear, "{s:?} should be Unclear");
        }
    }

    #[test]
    fn denial_wins_over_agreement_in_one_reply() {
        // "no no, right one is Cuttack" - denial words present, must be No.
        assert_eq!(classify("no no the right one is cuttack"), Verdict::No);
    }

    #[test]
    fn tuner_is_bounded_and_directionally_sane() {
        let cfg = crate::config::Config::default();
        let mut t = Tuning::bootstrap(&cfg);
        t.wake_threshold = 0.38;
        let min = cfg.feedback.min_ask_probability;

        // A denial near threshold pushes the gate UP and re-arms asking.
        t.ask_probability = 0.2;
        apply_verdict(&mut t, Verdict::No, 0.40, min);
        assert!(t.wake_threshold > 0.38);
        assert!(t.ask_probability > 0.2);

        // Confirms decay the ask rate toward (never below) the floor.
        for _ in 0..200 {
            apply_verdict(&mut t, Verdict::Yes, 0.39, min);
        }
        assert!(t.ask_probability >= min);
        assert!((t.ask_probability - min).abs() < 0.05);

        // No amount of feedback can drive the threshold out of its clamp.
        for _ in 0..200 {
            let s = t.wake_threshold + 0.05;
            apply_verdict(&mut t, Verdict::No, s, min);
        }
        assert!(t.wake_threshold <= 0.60);
        for _ in 0..500 {
            apply_verdict(&mut t, Verdict::Yes, 0.31, min);
        }
        assert!(t.wake_threshold >= 0.30);
    }

    #[test]
    fn tuning_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = FeedbackStore::open(dir.path()).unwrap();
        let cfg = crate::config::Config::default();
        let mut t = store.load_tuning(&cfg);
        t.confirms = 7;
        t.wake_threshold = 0.42;
        store.save_tuning(&t).unwrap();
        assert_eq!(store.load_tuning(&cfg), t);
    }

    #[test]
    fn ledger_appends_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = FeedbackStore::open(dir.path()).unwrap();
        store
            .append(&Entry {
                ts: 1,
                utterance: "play akashvani cuttack".into(),
                answer_head: "Playing akashvani.".into(),
                verdict: "yes".into(),
                wake_score: 0.41,
                wake_threshold: 0.38,
                clip: None,
                correction: None,
                language: Some("or-IN".into()),
            })
            .unwrap();
        let all = store.entries();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].verdict, "yes");
    }

    #[test]
    fn clip_pruning_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let store = FeedbackStore::open(dir.path()).unwrap();
        for i in 0..6 {
            std::fs::write(store.clips_dir().join(format!("{i:04}_x.wav")), b"x").unwrap();
        }
        store.prune_clips(2);
        let left: Vec<_> = std::fs::read_dir(store.clips_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left.len(), 2);
        assert!(left.contains(&"0004_x.wav".to_string()));
        assert!(left.contains(&"0005_x.wav".to_string()));
    }
}
