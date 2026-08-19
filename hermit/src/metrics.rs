//! Per-turn stage timing. Spec §5 requires one structured timing line per request
//! with route/recall/assemble/ttft/tool/tts stages so `scripts/bench.sh` can compute
//! p50/p95 without parsing prose.

use std::time::Instant;

/// Milliseconds elapsed since `start`, as f64 for sub-ms resolution on the fast path.
#[inline]
pub fn ms_since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// One turn's worth of stage timings. Fields are `Option` because not every turn
/// runs every stage (a fast-path device command has no TTFT).
#[derive(Debug, Default, Clone)]
pub struct TurnTimings {
    pub turn_id: u64,
    /// Wall clock start of the turn (end of user prompt).
    pub started: Option<Instant>,

    pub route_ms: Option<f64>,
    pub recall_ms: Option<f64>,
    pub assemble_ms: Option<f64>,
    pub ttft_ms: Option<f64>,
    /// One entry per tool invocation: (tool name, duration).
    pub tool_ms: Vec<(String, f64)>,
    pub tool_rounds: usize,
    pub tts_ttfa_ms: Option<f64>,
    pub first_audio_ms: Option<f64>,
    pub total_ms: Option<f64>,

    /// True when the turn was answered without any LLM call (§4.2 fast path).
    pub fast_path: bool,
    /// True when a speculative prefetch was actually used (not discarded).
    pub prefetch_hit: bool,
    pub prefetch_fired: bool,
}

impl TurnTimings {
    pub fn new(turn_id: u64) -> Self {
        Self {
            turn_id,
            started: Some(Instant::now()),
            ..Default::default()
        }
    }

    /// Local harness overhead — the part we actually control on-device.
    /// Spec gate: <= 15 ms.
    pub fn local_overhead_ms(&self) -> f64 {
        self.route_ms.unwrap_or(0.0)
            + self.recall_ms.unwrap_or(0.0)
            + self.assemble_ms.unwrap_or(0.0)
    }

    pub fn finish(&mut self) {
        if let Some(s) = self.started {
            self.total_ms = Some(ms_since(s));
        }
    }

    /// Emit the machine-parseable timing line. `bench.sh` greps for `hermit_timing`.
    pub fn emit(&self) {
        let tools: Vec<String> = self
            .tool_ms
            .iter()
            .map(|(n, d)| format!("{n}={d:.1}"))
            .collect();
        tracing::info!(
            target: "hermit_timing",
            turn = self.turn_id,
            fast_path = self.fast_path,
            route_ms = fmt_opt(self.route_ms),
            recall_ms = fmt_opt(self.recall_ms),
            assemble_ms = fmt_opt(self.assemble_ms),
            local_overhead_ms = format!("{:.2}", self.local_overhead_ms()),
            ttft_ms = fmt_opt(self.ttft_ms),
            tool_rounds = self.tool_rounds,
            tool_ms = tools.join(","),
            tts_ttfa_ms = fmt_opt(self.tts_ttfa_ms),
            first_audio_ms = fmt_opt(self.first_audio_ms),
            total_ms = fmt_opt(self.total_ms),
            prefetch_fired = self.prefetch_fired,
            prefetch_hit = self.prefetch_hit,
            "turn complete"
        );

        // Loud, actionable warnings when a spec gate is missed on real traffic.
        let overhead = self.local_overhead_ms();
        if overhead > 15.0 {
            tracing::warn!(target: "hermit_timing", overhead_ms = overhead, "local harness overhead exceeded 15ms gate");
        }
        if let Some(t) = self.ttft_ms
            && self.tool_rounds == 0
            && t > 700.0
        {
            tracing::warn!(target: "hermit_timing", ttft_ms = t, "text TTFT exceeded 700ms gate");
        }
        if let Some(a) = self.first_audio_ms
            && self.tool_rounds == 0
            && a > 1200.0
        {
            tracing::warn!(target: "hermit_timing", first_audio_ms = a, "voice first-audio exceeded 1.2s gate");
        }
        if let Some(a) = self.first_audio_ms
            && self.tool_rounds > 0
            && a > 2000.0
        {
            tracing::warn!(target: "hermit_timing", first_audio_ms = a, "voice-with-search first-audio exceeded 2.0s gate");
        }
    }
}

fn fmt_opt(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "-".into())
}

/// Log RSS once at boot and periodically, so the §11 budget (<=100MB) is observable.
#[cfg(target_os = "linux")]
pub fn rss_mb() -> Option<f64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: f64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096.0 / 1_048_576.0)
}

#[cfg(not(target_os = "linux"))]
pub fn rss_mb() -> Option<f64> {
    None
}

/// Background task: warn if the daemon drifts past its RAM budget.
pub async fn rss_watchdog(limit_mb: f64) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tick.tick().await;
        if let Some(mb) = rss_mb() {
            if mb > limit_mb {
                tracing::warn!(rss_mb = mb, limit_mb, "daemon RSS above budget");
            } else {
                tracing::debug!(rss_mb = mb, "rss");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_overhead_sums_only_local_stages() {
        let mut t = TurnTimings::new(1);
        t.route_ms = Some(1.0);
        t.recall_ms = Some(3.5);
        t.assemble_ms = Some(2.0);
        t.ttft_ms = Some(600.0); // network — must not count
        assert!((t.local_overhead_ms() - 6.5).abs() < 1e-9);
    }

    #[test]
    fn missing_stages_are_zero() {
        let t = TurnTimings::new(2);
        assert_eq!(t.local_overhead_ms(), 0.0);
    }
}
