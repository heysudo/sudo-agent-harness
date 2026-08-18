//! Pipeline → console decoupling, borrowed from the b2-34 `sudo-console` design.
//!
//! The daemon writes small JSON files to tmpfs; the operator TUI (`tools/sudo-console`)
//! tails them. The interface is one-way by construction — the daemon never depends on
//! a console being present — with a single inbound exception: `control.json`, where
//! the console can set `mic_muted` / `speaker_muted`. A missing or unreadable control
//! file means "no overrides", so a crashed console leaves nothing muted.
//!
//! Files (in `HERMIT_STATE_DIR`, default `/run/hermit`):
//! - `live.json` — 8 Hz heartbeat + meters: wake score, mic RMS, flags. Stale file
//!   (>3 s) = daemon gone; the console renders that as DEAD.
//! - `events.jsonl` — append-only ring of structured events (ww_fired, turn_complete…)
//! - `control.json` — console → daemon: `{"mic_muted":bool,"speaker_muted":bool}`
//!
//! Telemetry must never disturb the hot path: every write is throttled, atomic
//! (tmp + rename), and swallows errors.

use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Telemetry rate for the console meters.
const LIVE_HZ: f64 = 8.0;
/// Keep this many events in the ring file.
const EVENT_RING: usize = 500;

fn now_ts() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

pub struct StateWriter {
    dir: PathBuf,
    live_path: PathBuf,
    events_path: PathBuf,
    control_path: PathBuf,
    live_next: Instant,
    ctl_next: Instant,
    ctl_cache: Control,
    event_count: usize,
    enabled: bool,
}

/// Console overrides. Default = nothing muted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Control {
    pub mic_muted: bool,
    pub speaker_muted: bool,
}

impl StateWriter {
    pub fn new() -> Self {
        let dir = std::env::var("HERMIT_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/run/hermit"));
        let enabled = match std::fs::create_dir_all(&dir) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e,
                    "state dir unavailable; console telemetry disabled");
                false
            }
        };
        Self {
            live_path: dir.join("live.json"),
            events_path: dir.join("events.jsonl"),
            control_path: dir.join("control.json"),
            dir,
            live_next: Instant::now(),
            ctl_next: Instant::now(),
            ctl_cache: Control::default(),
            event_count: 0,
            enabled,
        }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Throttled heartbeat + meters. Safe to call every mic frame.
    pub fn write_live(&mut self, fields: serde_json::Value) {
        if !self.enabled || Instant::now() < self.live_next {
            return;
        }
        self.live_next = Instant::now() + Duration::from_secs_f64(1.0 / LIVE_HZ);
        let mut obj = json!({ "ts": now_ts() });
        if let (Some(dst), Some(src)) = (obj.as_object_mut(), fields.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        let tmp = self.live_path.with_extension("tmp");
        let _ = std::fs::write(&tmp, obj.to_string()).and_then(|_| std::fs::rename(&tmp, &self.live_path));
    }

    /// Append a structured event to the ring.
    pub fn emit(&mut self, event_type: &str, fields: serde_json::Value) {
        if !self.enabled {
            return;
        }
        let mut obj = json!({ "ts": now_ts(), "type": event_type });
        if let (Some(dst), Some(src)) = (obj.as_object_mut(), fields.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.events_path) {
            let _ = writeln!(f, "{obj}");
        }
        self.event_count += 1;
        if self.event_count.is_multiple_of(100) {
            self.trim();
        }
    }

    fn trim(&self) {
        let Ok(text) = std::fs::read_to_string(&self.events_path) else { return };
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() > EVENT_RING {
            let tail = lines[lines.len() - EVENT_RING..].join("\n");
            let tmp = self.events_path.with_extension("tmp");
            let _ = std::fs::write(&tmp, tail + "\n")
                .and_then(|_| std::fs::rename(&tmp, &self.events_path));
        }
    }

    /// Poll the console's control file. Cached and throttled to LIVE_HZ.
    pub fn read_control(&mut self) -> Control {
        if !self.enabled {
            return Control::default();
        }
        if Instant::now() >= self.ctl_next {
            self.ctl_next = Instant::now() + Duration::from_secs_f64(1.0 / LIVE_HZ);
            self.ctl_cache = std::fs::read_to_string(&self.control_path)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .map(|v| Control {
                    mic_muted: v.get("mic_muted").and_then(|x| x.as_bool()).unwrap_or(false),
                    speaker_muted: v.get("speaker_muted").and_then(|x| x.as_bool()).unwrap_or(false),
                })
                .unwrap_or_default();
        }
        self.ctl_cache
    }
}

impl Default for StateWriter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer_in(dir: &std::path::Path) -> StateWriter {
        // Constructed directly rather than via env so tests stay parallel-safe.
        StateWriter {
            live_path: dir.join("live.json"),
            events_path: dir.join("events.jsonl"),
            control_path: dir.join("control.json"),
            dir: dir.to_path_buf(),
            live_next: Instant::now(),
            ctl_next: Instant::now(),
            ctl_cache: Control::default(),
            event_count: 0,
            enabled: true,
        }
    }

    #[test]
    fn live_json_is_written_and_throttled() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer_in(dir.path());
        w.write_live(json!({"ww": 0.42, "rms": -30.1}));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("live.json")).unwrap()).unwrap();
        assert_eq!(v["ww"], 0.42);
        assert!(v["ts"].as_f64().unwrap() > 0.0);
        // Immediately again: throttled, must not rewrite.
        let before = std::fs::metadata(dir.path().join("live.json")).unwrap().modified().unwrap();
        w.write_live(json!({"ww": 0.99}));
        let after = std::fs::metadata(dir.path().join("live.json")).unwrap().modified().unwrap();
        assert_eq!(before, after, "second write inside the throttle window must be skipped");
    }

    #[test]
    fn control_roundtrip_and_absence_means_unmuted() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer_in(dir.path());
        assert_eq!(w.read_control(), Control::default(), "no file => no overrides");
        std::fs::write(dir.path().join("control.json"), r#"{"mic_muted":true,"speaker_muted":false}"#).unwrap();
        w.ctl_next = Instant::now(); // bypass throttle for the test
        assert!(w.read_control().mic_muted);
        // Corrupt file => default, not a crash.
        std::fs::write(dir.path().join("control.json"), "{not json").unwrap();
        w.ctl_next = Instant::now();
        assert_eq!(w.read_control(), Control::default());
    }

    #[test]
    fn event_ring_trims() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer_in(dir.path());
        for i in 0..700 {
            w.emit("test", json!({"i": i}));
        }
        let lines = std::fs::read_to_string(dir.path().join("events.jsonl")).unwrap();
        let n = lines.lines().count();
        assert!(n <= EVENT_RING + 100, "ring must trim, had {n}");
        assert!(lines.lines().last().unwrap().contains("699"), "newest events kept");
    }
}
