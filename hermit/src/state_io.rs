//! Pipeline → console decoupling, borrowed from the b2-34 `sudo-console` design.
//!
//! The daemon writes small JSON files to tmpfs; the operator TUI (`tools/sudo-console`)
//! tails them. The interface is one-way by construction — the daemon never depends on
//! a console being present — with a single inbound exception: `control.json`, where
//! the console can set `mic_muted` / `speaker_muted`. A missing or unreadable control
//! file means "no overrides", so a crashed console leaves nothing muted.
//!
//! Files (under `HERMIT_STATE_DIR`, default `/run/hermit-console`):
//! - `telemetry/live.json` — 8 Hz heartbeat + meters: wake score, mic RMS, flags. Stale file
//!   (>3 s) = daemon gone; the console renders that as DEAD.
//! - `telemetry/events.jsonl` — append-only ring of structured events.
//! - `control/control.json` — console → daemon lease: mute flags + timestamp.
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
/// A dead/disconnected console must never leave either audio path muted.
const CONTROL_TTL_SECS: f64 = 3.0;

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub struct StateWriter {
    dir: PathBuf,
    live_path: PathBuf,
    events_path: PathBuf,
    control_path: PathBuf,
    live_next: Instant,
    ctl_next: Instant,
    ctl_cache: Control,
    live_fields: serde_json::Map<String, serde_json::Value>,
    event_count: usize,
    enabled: bool,
}

/// Console overrides. Default = nothing muted, no volume request.
///
/// Mutes are a LEASE: they expire with the control file's TTL so a crashed
/// console can never leave the device silent. `volume` is a COMMAND: the
/// daemon applies it once per change and it persists after the console exits
/// (snapping volume back on quit would surprise the operator).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Control {
    pub mic_muted: bool,
    pub speaker_muted: bool,
    pub volume: Option<u8>,
}

impl StateWriter {
    pub fn new() -> Self {
        let dir = std::env::var("HERMIT_STATE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/run/hermit-console"));
        let enabled = match std::fs::create_dir_all(&dir) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(dir = %dir.display(), error = %e,
                    "state dir unavailable; console telemetry disabled");
                false
            }
        };
        Self {
            live_path: dir.join("telemetry/live.json"),
            events_path: dir.join("telemetry/events.jsonl"),
            control_path: dir.join("control/control.json"),
            dir,
            live_next: Instant::now(),
            ctl_next: Instant::now(),
            ctl_cache: Control::default(),
            live_fields: serde_json::Map::new(),
            event_count: 0,
            enabled,
        }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Throttled heartbeat + meters. Safe to call every mic frame. Fields from
    /// independent producers are merged so an STT update cannot erase the meters.
    pub fn write_live(&mut self, fields: serde_json::Value) {
        if !self.enabled {
            return;
        }
        if let Some(src) = fields.as_object() {
            for (k, v) in src {
                self.live_fields.insert(k.clone(), v.clone());
            }
        }
        if Instant::now() < self.live_next {
            return;
        }
        self.live_next = Instant::now() + Duration::from_secs_f64(1.0 / LIVE_HZ);
        let mut obj = serde_json::Value::Object(self.live_fields.clone());
        obj.as_object_mut()
            .unwrap()
            .insert("ts".into(), json!(now_ts()));
        let tmp = self.live_path.with_extension("tmp");
        let _ = std::fs::write(&tmp, obj.to_string())
            .and_then(|_| std::fs::rename(&tmp, &self.live_path));
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
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.events_path)
        {
            let _ = writeln!(f, "{obj}");
        }
        self.event_count += 1;
        if self.event_count.is_multiple_of(100) {
            self.trim();
        }
    }

    fn trim(&self) {
        let Ok(text) = std::fs::read_to_string(&self.events_path) else {
            return;
        };
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
                .and_then(|v| {
                    let age = now_ts() - v.get("ts").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    (0.0..=CONTROL_TTL_SECS).contains(&age).then(|| Control {
                        mic_muted: v
                            .get("mic_muted")
                            .and_then(|x| x.as_bool())
                            .unwrap_or(false),
                        speaker_muted: v
                            .get("speaker_muted")
                            .and_then(|x| x.as_bool())
                            .unwrap_or(false),
                        volume: v
                            .get("volume")
                            .and_then(|x| x.as_u64())
                            .map(|x| x.min(100) as u8),
                    })
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
            live_fields: serde_json::Map::new(),
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
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("live.json")).unwrap())
                .unwrap();
        assert_eq!(v["ww"], 0.42);
        assert!(v["ts"].as_f64().unwrap() > 0.0);
        // Immediately again: throttled, must not rewrite.
        let before = std::fs::metadata(dir.path().join("live.json"))
            .unwrap()
            .modified()
            .unwrap();
        w.write_live(json!({"ww": 0.99}));
        let after = std::fs::metadata(dir.path().join("live.json"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            before, after,
            "second write inside the throttle window must be skipped"
        );
    }

    #[test]
    fn control_roundtrip_and_absence_means_unmuted() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer_in(dir.path());
        assert_eq!(
            w.read_control(),
            Control::default(),
            "no file => no overrides"
        );
        std::fs::write(
            dir.path().join("control.json"),
            format!(
                r#"{{"ts":{},"mic_muted":true,"speaker_muted":false}}"#,
                now_ts()
            ),
        )
        .unwrap();
        w.ctl_next = Instant::now(); // bypass throttle for the test
        assert!(w.read_control().mic_muted);
        // Volume: absent => None, present => clamped to 100.
        assert_eq!(w.read_control().volume, None, "no volume key => None");
        std::fs::write(
            dir.path().join("control.json"),
            format!(
                r#"{{"ts":{},"mic_muted":false,"speaker_muted":false,"volume":140}}"#,
                now_ts()
            ),
        )
        .unwrap();
        w.ctl_next = Instant::now();
        assert_eq!(
            w.read_control().volume,
            Some(100),
            "volume above 100 clamps"
        );
        std::fs::write(
            dir.path().join("control.json"),
            r#"{"ts":1,"mic_muted":true,"speaker_muted":true}"#,
        )
        .unwrap();
        w.ctl_next = Instant::now();
        assert_eq!(
            w.read_control(),
            Control::default(),
            "stale console => unmuted"
        );
        // Corrupt file => default, not a crash.
        std::fs::write(dir.path().join("control.json"), "{not json").unwrap();
        w.ctl_next = Instant::now();
        assert_eq!(w.read_control(), Control::default());
    }

    #[test]
    fn live_updates_merge_independent_meter_and_transcript_fields() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = writer_in(dir.path());
        w.write_live(json!({"rms": -18.5, "ww": 0.22}));
        w.live_next = Instant::now();
        w.write_live(json!({"listening": true, "transcript_interim": "hello sudo"}));
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("live.json")).unwrap())
                .unwrap();
        assert_eq!(v["rms"], -18.5);
        assert_eq!(v["ww"], 0.22);
        assert_eq!(v["listening"], true);
        assert_eq!(v["transcript_interim"], "hello sudo");
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
        assert!(
            lines.lines().last().unwrap().contains("699"),
            "newest events kept"
        );
    }
}
