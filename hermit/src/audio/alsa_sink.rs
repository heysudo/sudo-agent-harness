//! ALSA playback backend (Linux only, `--features alsa-audio`).
//!
//! Opens the named PCM from `/etc/asound.conf` — `hermit_out` by default, which is
//! the dmix → route(stereo→mono) → softvol(ceiling) chain onto the XVF3800.
//!
//! Deliberately thin. All routing, downmixing and the speaker-protection volume
//! ceiling live in `asound.conf` where an operator can inspect and adjust them
//! without a rebuild; this file only pushes mono S16_LE frames at the card's rate.
//!
//! NOTE: this is the one module that cannot be compile-checked on a non-Linux dev
//! machine (alsa-sys needs libasound + pkg-config for the target). It is verified by
//! the first `cross build --features pi` and by Phase 0 on hardware.

use super::Backend;
use alsa::pcm::{Access, Format, HwParams, PCM, State};
use alsa::{Direction, ValueOr};
use anyhow::{Context, Result};

pub struct AlsaBackend {
    pcm: PCM,
    /// Retained for the error message on reopen.
    device: String,
    sample_rate: u32,
}

impl AlsaBackend {
    pub fn open(cfg: &crate::config::Audio) -> Result<Self> {
        let pcm = Self::open_pcm(&cfg.playback_pcm, cfg)?;
        Ok(Self {
            pcm,
            device: cfg.playback_pcm.clone(),
            sample_rate: cfg.sample_rate,
        })
    }

    fn open_pcm(name: &str, cfg: &crate::config::Audio) -> Result<PCM> {
        // Blocking mode: the playback thread is dedicated and blocking writes give
        // natural backpressure without a busy loop.
        let pcm = PCM::new(name, Direction::Playback, false)
            .with_context(|| format!("opening ALSA playback PCM {name:?}"))?;

        {
            let hwp = HwParams::any(&pcm).context("querying ALSA hw params")?;
            // Mono: the FIT0502 is a single driver, and asound.conf already
            // downmixes anything stereo that other clients send.
            hwp.set_channels(1).context("setting mono")?;
            hwp.set_rate(cfg.sample_rate, ValueOr::Nearest)
                .with_context(|| format!("setting rate {}", cfg.sample_rate))?;
            hwp.set_format(Format::s16()).context("setting S16_LE")?;
            hwp.set_access(Access::RWInterleaved).context("setting access mode")?;
            hwp.set_buffer_time_near(cfg.buffer_ms * 1000, ValueOr::Nearest)
                .context("setting buffer time")?;
            hwp.set_period_time_near(cfg.period_ms * 1000, ValueOr::Nearest)
                .context("setting period time")?;
            pcm.hw_params(&hwp).context("applying ALSA hw params")?;

            let actual = hwp.get_rate().unwrap_or(0);
            if actual != cfg.sample_rate {
                // Not fatal — `plug` will resample — but it costs CPU and latency,
                // so it must be visible in the logs.
                tracing::warn!(
                    requested = cfg.sample_rate,
                    actual,
                    "card did not accept the requested rate; audio will be resampled. \
                     Set audio.sample_rate and tts.sample_rate to the card's native rate."
                );
            }
        }

        pcm.prepare().context("preparing ALSA PCM")?;
        tracing::info!(device = %name, rate = cfg.sample_rate, "ALSA playback open");
        Ok(pcm)
    }

    /// Recover from an underrun/suspend. Returns Ok if playback can continue.
    fn recover(&self, err: alsa::Error) -> Result<()> {
        // errno 32 = EPIPE (underrun), 11 = EAGAIN, ESTRPIPE = suspended.
        self.pcm
            .try_recover(err, true)
            .context("ALSA could not recover from an error")?;
        Ok(())
    }
}

impl Backend for AlsaBackend {
    fn write(&mut self, samples: &[i16]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        // After a `drop()` the device sits in Setup and must be prepared again.
        if matches!(self.pcm.state(), State::Setup | State::XRun) {
            let _ = self.pcm.prepare();
        }

        let io = self.pcm.io_i16().context("getting ALSA io handle")?;
        let mut offset = 0usize;

        while offset < samples.len() {
            match io.writei(&samples[offset..]) {
                Ok(frames) => {
                    if frames == 0 {
                        // Nothing accepted; avoid a hot spin.
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    offset += frames;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "ALSA write error; attempting recovery");
                    self.recover(e).with_context(|| {
                        format!("writing to {} after an unrecoverable error", self.device)
                    })?;
                    // Retry the same offset once recovered.
                }
            }
        }
        Ok(())
    }

    fn discard(&mut self) -> Result<()> {
        // snd_pcm_drop: throw away frames the driver already holds. This is what
        // makes barge-in immediate rather than "after the buffer plays out".
        if let Err(e) = self.pcm.drop() {
            tracing::debug!(error = %e, "pcm drop failed; falling back to reset");
            let _ = self.pcm.reset();
        }
        // Leave the device ready for the next write.
        let _ = self.pcm.prepare();
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        // snd_pcm_drain: block until buffered frames have played.
        self.pcm.drain().context("draining ALSA PCM")?;
        Ok(())
    }
}

/// Open the capture PCM (`hermit_in`: the XVF3800's processed voice channel).
///
/// The 2-channel USB firmware exposes channel 0 as processed mono voice; asound.conf
/// pins capture to it, so the daemon sees a plain mono 16 kHz stream that has
/// already been through hardware AEC, beamforming and noise suppression.
pub fn open_capture(cfg: &crate::config::Audio) -> Result<PCM> {
    let pcm = PCM::new(&cfg.capture_pcm, Direction::Capture, false)
        .with_context(|| format!("opening ALSA capture PCM {:?}", cfg.capture_pcm))?;
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_channels(1)?;
        hwp.set_rate(cfg.sample_rate, ValueOr::Nearest)?;
        hwp.set_format(Format::s16())?;
        hwp.set_access(Access::RWInterleaved)?;
        // Capture buffers stay short: wake-word latency is charged straight to the
        // user, and Porcupine consumes fixed 512-sample frames anyway.
        hwp.set_buffer_time_near(100_000, ValueOr::Nearest)?;
        hwp.set_period_time_near(20_000, ValueOr::Nearest)?;
        pcm.hw_params(&hwp)?;
    }
    pcm.prepare()?;
    tracing::info!(device = %cfg.capture_pcm, rate = cfg.sample_rate, "ALSA capture open");
    Ok(pcm)
}
