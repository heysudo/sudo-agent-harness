//! Backend used on dev machines and in tests.
//!
//! Consumes PCM in real time so timing-sensitive code behaves roughly as it would
//! on hardware, but produces no sound. Selected automatically when the crate is
//! built without the `alsa-audio` feature.

use super::Backend;
use anyhow::Result;
use std::time::{Duration, Instant};

pub struct NullBackend {
    sample_rate: u32,
    /// When the notional output buffer will have drained.
    playhead: Option<Instant>,
    realtime: bool,
}

impl NullBackend {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate: sample_rate.max(1), playhead: None, realtime: false }
    }

    /// Sleep in proportion to the audio written, approximating a real device.
    pub fn realtime(mut self) -> Self {
        self.realtime = true;
        self
    }
}

impl Backend for NullBackend {
    fn write(&mut self, samples: &[i16]) -> Result<()> {
        let dur = Duration::from_secs_f64(samples.len() as f64 / self.sample_rate as f64);
        let now = Instant::now();
        let until = self.playhead.filter(|p| *p > now).unwrap_or(now) + dur;
        self.playhead = Some(until);
        // Always pace, not just in `realtime` mode: the keepalive loop relies on the
        // backend blocking for roughly the duration of the audio it accepted. A
        // backend that returns instantly would turn that loop into a busy-spin.
        // Bounded so a huge buffer cannot wedge a caller.
        let ahead = until.saturating_duration_since(now);
        std::thread::sleep(ahead.min(Duration::from_millis(250)));
        Ok(())
    }

    fn discard(&mut self) -> Result<()> {
        self.playhead = None;
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        if let Some(until) = self.playhead.take() {
            std::thread::sleep(until.saturating_duration_since(Instant::now()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_advances_and_discard_clears_the_playhead() {
        let mut b = NullBackend::new(16_000);
        b.write(&vec![0i16; 1600]).unwrap(); // 100ms
        assert!(b.playhead.is_some());
        b.discard().unwrap();
        assert!(b.playhead.is_none());
    }

    #[test]
    fn zero_sample_rate_does_not_divide_by_zero() {
        let mut b = NullBackend::new(0);
        assert!(b.write(&[0, 1, 2]).is_ok());
    }

    #[test]
    fn drain_waits_for_the_remaining_playhead() {
        let mut b = NullBackend::new(16_000);
        let started = Instant::now();
        b.write(&vec![0i16; 4800]).unwrap(); // 300 ms, write itself is capped at 250 ms
        b.drain().unwrap();
        assert!(started.elapsed() >= Duration::from_millis(280));
        assert!(b.playhead.is_none());
    }
}
