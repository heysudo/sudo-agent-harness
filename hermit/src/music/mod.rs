//! Music: Spotify (via librespot + Web API) and internet radio (via mpv).
//!
//! One controller owns both backends so ducking, volume and transport commands
//! behave identically whichever is playing. Both feed the same ALSA device — the
//! XVF3800 — which is what keeps hardware AEC fed with a loopback reference.

pub mod mpv;
pub mod spotify;

pub use mpv::MpvClient;
pub use spotify::SpotifyClient;

use crate::router::DeviceCommand;
use anyhow::{Result, bail};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Which backend is currently producing sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    #[default]
    None,
    Spotify,
    Radio,
}

#[derive(Debug, Clone)]
struct State {
    source: Source,
    /// Nominal user-facing volume, 0–100, independent of ducking.
    volume: u8,
    ducked: bool,
    now_playing: Option<String>,
}

pub struct MusicController {
    mpv: MpvClient,
    spotify: Option<SpotifyClient>,
    stations: RwLock<BTreeMap<String, String>>,
    state: RwLock<State>,
    duck_db: f32,
}

/// Convert a decibel change into a linear amplitude scale factor.
/// -12 dB → 10^(-12/20) ≈ 0.251.
pub fn db_to_scale(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

impl MusicController {
    pub fn new(
        mpv: MpvClient,
        spotify: Option<SpotifyClient>,
        stations: BTreeMap<String, String>,
        default_volume: u8,
        duck_db: f32,
    ) -> Self {
        Self {
            mpv,
            spotify,
            stations: RwLock::new(stations),
            state: RwLock::new(State {
                source: Source::None,
                volume: default_volume.min(100),
                ducked: false,
                now_playing: None,
            }),
            duck_db,
        }
    }

    /// Load `stations.toml`. Format:
    /// ```toml
    /// [stations]
    /// npr = "https://npr-ice.streamguys1.com/live.mp3"
    /// ```
    pub fn load_stations(path: &Path) -> BTreeMap<String, String> {
        #[derive(serde::Deserialize, Default)]
        struct File {
            #[serde(default)]
            stations: BTreeMap<String, String>,
        }
        match std::fs::read_to_string(path) {
            Ok(text) => match toml::from_str::<File>(&text) {
                Ok(f) => {
                    tracing::info!(count = f.stations.len(), path = %path.display(), "stations loaded");
                    f.stations.into_iter().map(|(k, v)| (k.to_lowercase(), v)).collect()
                }
                Err(e) => {
                    tracing::error!(error = %e, path = %path.display(), "stations.toml is invalid");
                    BTreeMap::new()
                }
            },
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(error = %e, path = %path.display(), "could not read stations.toml");
                }
                BTreeMap::new()
            }
        }
    }

    pub async fn set_stations(&self, stations: BTreeMap<String, String>) {
        *self.stations.write().await = stations;
    }

    pub async fn station_names(&self) -> Vec<String> {
        self.stations.read().await.keys().cloned().collect()
    }

    pub async fn source(&self) -> Source {
        self.state.read().await.source
    }

    pub async fn is_playing(&self) -> bool {
        self.state.read().await.source != Source::None
    }

    fn spotify(&self) -> Result<&SpotifyClient> {
        self.spotify.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "Spotify is not configured — set SPOTIFY_CLIENT_ID, SPOTIFY_CLIENT_SECRET and \
                 SPOTIFY_REFRESH_TOKEN in /etc/hermit/hermit.env"
            )
        })
    }

    // -----------------------------------------------------------------
    // Fast-path command execution (spec §4.2, target <50 ms)
    // -----------------------------------------------------------------

    /// Execute a routed device command. Returns a short line to speak/print.
    pub async fn execute(&self, cmd: &DeviceCommand) -> Result<String> {
        match cmd {
            DeviceCommand::Pause => self.pause().await.map(|_| "Paused.".to_string()),
            DeviceCommand::Resume => self.resume().await.map(|_| "Resuming.".to_string()),
            DeviceCommand::StopMusic => self.stop().await.map(|_| "Stopped.".to_string()),
            DeviceCommand::Next => self.next().await.map(|_| "Next.".to_string()),
            DeviceCommand::Previous => self.previous().await.map(|_| "Going back.".to_string()),
            DeviceCommand::VolumeUp => {
                let v = self.nudge_volume(10).await?;
                Ok(format!("Volume {v}."))
            }
            DeviceCommand::VolumeDown => {
                let v = self.nudge_volume(-10).await?;
                Ok(format!("Volume {v}."))
            }
            DeviceCommand::VolumeSet(v) => {
                self.set_volume(*v).await?;
                Ok(format!("Volume {v}."))
            }
            DeviceCommand::Mute => {
                self.set_volume(0).await?;
                Ok("Muted.".to_string())
            }
            DeviceCommand::Unmute => {
                let v = self.state.read().await.volume.max(30);
                self.set_volume(v).await?;
                Ok("Unmuted.".to_string())
            }
            DeviceCommand::PlaySpotify(q) => {
                let label = self.play_spotify(q).await?;
                Ok(format!("Playing {label}."))
            }
            DeviceCommand::PlayStation(name) => {
                self.play_station(name).await?;
                Ok(format!("Playing {name}."))
            }
            DeviceCommand::NowPlaying => Ok(self
                .now_playing()
                .await
                .map(|t| format!("This is {t}."))
                .unwrap_or_else(|| "Nothing is playing.".to_string())),
            DeviceCommand::TimeOfDay => {
                let now = chrono::Local::now();
                Ok(format!("It's {}.", now.format("%-I:%M %p")))
            }
        }
    }

    pub async fn play_spotify(&self, query: &str) -> Result<String> {
        let sp = self.spotify()?;
        // Radio and Spotify share one output device; stop the other first.
        let _ = self.mpv.stop().await;
        let label = sp.play_query(query).await?;
        let vol = {
            let mut st = self.state.write().await;
            st.source = Source::Spotify;
            st.now_playing = Some(label.clone());
            st.ducked = false;
            st.volume
        };
        let _ = sp.set_volume(vol).await;
        Ok(label)
    }

    pub async fn play_station(&self, name: &str) -> Result<()> {
        let url = {
            let stations = self.stations.read().await;
            let key = name.trim().to_lowercase();
            stations
                .get(&key)
                .or_else(|| stations.iter().find(|(k, _)| key.starts_with(k.as_str())).map(|(_, v)| v))
                .cloned()
        };
        let Some(url) = url else {
            let known = self.station_names().await;
            bail!("no station named {name:?}. Known stations: {}", known.join(", "));
        };
        self.play_stream(&url, name).await
    }

    /// Play an arbitrary stream URL (used by "play NPR live" style news routing).
    pub async fn play_stream(&self, url: &str, label: &str) -> Result<()> {
        if let Some(sp) = &self.spotify {
            let _ = sp.pause().await;
        }
        self.mpv.loadfile(url).await?;
        let vol = {
            let mut st = self.state.write().await;
            st.source = Source::Radio;
            st.now_playing = Some(label.to_string());
            st.ducked = false;
            st.volume
        };
        self.mpv.set_volume(vol).await?;
        Ok(())
    }

    pub async fn pause(&self) -> Result<()> {
        match self.source().await {
            Source::Spotify => self.spotify()?.pause().await,
            Source::Radio => self.mpv.pause().await,
            Source::None => Ok(()), // nothing to pause; not an error
        }
    }

    pub async fn resume(&self) -> Result<()> {
        match self.source().await {
            Source::Spotify => self.spotify()?.resume().await,
            Source::Radio => self.mpv.resume().await,
            Source::None => Ok(()),
        }
    }

    pub async fn stop(&self) -> Result<()> {
        let src = self.source().await;
        match src {
            Source::Spotify => {
                let _ = self.spotify()?.pause().await;
            }
            Source::Radio => {
                let _ = self.mpv.stop().await;
            }
            Source::None => {}
        }
        let mut st = self.state.write().await;
        st.source = Source::None;
        st.now_playing = None;
        st.ducked = false;
        Ok(())
    }

    pub async fn next(&self) -> Result<()> {
        match self.source().await {
            Source::Spotify => self.spotify()?.next().await,
            // Skipping a live radio stream is meaningless; say so rather than
            // silently doing nothing.
            Source::Radio => bail!("that's a live radio stream — there's no next track"),
            Source::None => bail!("nothing is playing"),
        }
    }

    pub async fn previous(&self) -> Result<()> {
        match self.source().await {
            Source::Spotify => self.spotify()?.previous().await,
            Source::Radio => bail!("that's a live radio stream — there's no previous track"),
            Source::None => bail!("nothing is playing"),
        }
    }

    /// Set nominal volume and apply it to the active backend.
    pub async fn set_volume(&self, percent: u8) -> Result<()> {
        let v = percent.min(100);
        {
            let mut st = self.state.write().await;
            st.volume = v;
            st.ducked = false;
        }
        self.apply_volume(v).await
    }

    async fn nudge_volume(&self, delta: i16) -> Result<u8> {
        let current = self.state.read().await.volume as i16;
        let next = (current + delta).clamp(0, 100) as u8;
        self.set_volume(next).await?;
        Ok(next)
    }

    async fn apply_volume(&self, v: u8) -> Result<()> {
        match self.source().await {
            Source::Spotify => self.spotify()?.set_volume(v).await,
            Source::Radio => self.mpv.set_volume(v).await,
            Source::None => Ok(()),
        }
    }

    pub async fn volume(&self) -> u8 {
        self.state.read().await.volume
    }

    // -----------------------------------------------------------------
    // Ducking (spec §7)
    // -----------------------------------------------------------------

    /// Attenuate music while TTS speaks. Idempotent.
    pub async fn duck(&self) {
        let (should, target) = {
            let mut st = self.state.write().await;
            if st.ducked || st.source == Source::None {
                (false, 0)
            } else {
                st.ducked = true;
                let scaled = (st.volume as f32 * db_to_scale(self.duck_db)).round();
                (true, scaled.clamp(0.0, 100.0) as u8)
            }
        };
        if should
            && let Err(e) = self.apply_volume(target).await
        {
            tracing::warn!(error = %e, "ducking failed");
        }
    }

    /// Restore volume after TTS. Idempotent.
    pub async fn unduck(&self) {
        let (should, target) = {
            let mut st = self.state.write().await;
            if !st.ducked {
                (false, 0)
            } else {
                st.ducked = false;
                (true, st.volume)
            }
        };
        if should
            && let Err(e) = self.apply_volume(target).await
        {
            tracing::warn!(error = %e, "unducking failed");
        }
    }

    pub async fn now_playing(&self) -> Option<String> {
        match self.source().await {
            Source::Spotify => match &self.spotify {
                Some(sp) => sp.now_playing().await.or(self.state.read().await.now_playing.clone()),
                None => None,
            },
            Source::Radio => {
                self.mpv.now_playing().await.or(self.state.read().await.now_playing.clone())
            }
            Source::None => None,
        }
    }

    /// One-line status for the `music` tool.
    pub async fn status(&self) -> String {
        let st = self.state.read().await.clone();
        match st.source {
            Source::None => "nothing playing".to_string(),
            other => {
                let what = self.now_playing().await.unwrap_or_else(|| "unknown".into());
                let backend = if other == Source::Spotify { "spotify" } else { "radio" };
                format!("playing \"{what}\" via {backend} at volume {}", st.volume)
            }
        }
    }
}

/// Shared handle.
pub type Music = Arc<MusicController>;

#[cfg(test)]
mod tests {
    use super::*;

    fn controller(duck_db: f32) -> MusicController {
        let mut stations = BTreeMap::new();
        stations.insert("npr".to_string(), "https://example.invalid/npr.mp3".to_string());
        MusicController::new(
            MpvClient::new("/nonexistent/hermit-test.sock"),
            None,
            stations,
            70,
            duck_db,
        )
    }

    #[test]
    fn db_to_scale_matches_the_spec_figure() {
        // -12 dB is the ducking level called for in §7.
        assert!((db_to_scale(-12.0) - 0.2512).abs() < 0.001);
        assert!((db_to_scale(0.0) - 1.0).abs() < 1e-6);
        // The §2 speaker-protection ceiling: sqrt(3/5) ~= 0.775 ~= -2.2 dB.
        assert!((db_to_scale(-2.2) - 0.7762).abs() < 0.005);
    }

    #[tokio::test]
    async fn ducking_is_idempotent_and_restores_exactly() {
        let c = controller(-12.0);
        // Force a source so ducking engages; mpv calls fail harmlessly.
        c.state.write().await.source = Source::Radio;

        assert_eq!(c.volume().await, 70);
        c.duck().await;
        assert!(c.state.read().await.ducked);
        c.duck().await; // second duck must not compound
        assert!(c.state.read().await.ducked);
        c.unduck().await;
        assert!(!c.state.read().await.ducked);
        assert_eq!(c.volume().await, 70, "nominal volume must survive a duck cycle");
        c.unduck().await; // idempotent
        assert_eq!(c.volume().await, 70);
    }

    #[tokio::test]
    async fn ducking_does_nothing_when_silent() {
        let c = controller(-12.0);
        c.duck().await;
        assert!(!c.state.read().await.ducked, "no source means nothing to duck");
    }

    #[tokio::test]
    async fn volume_nudges_clamp_at_the_ends() {
        let c = controller(-12.0);
        c.set_volume(95).await.unwrap();
        assert_eq!(c.nudge_volume(10).await.unwrap(), 100);
        c.set_volume(5).await.unwrap();
        assert_eq!(c.nudge_volume(-10).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn unknown_station_lists_the_known_ones() {
        let c = controller(-12.0);
        let err = c.play_station("classic fm").await.unwrap_err().to_string();
        assert!(err.contains("npr"), "error should help the user: {err}");
    }

    #[tokio::test]
    async fn transport_on_radio_explains_itself() {
        let c = controller(-12.0);
        c.state.write().await.source = Source::Radio;
        let err = c.next().await.unwrap_err().to_string();
        assert!(err.contains("live radio"));
    }

    #[tokio::test]
    async fn pause_when_idle_is_not_an_error() {
        let c = controller(-12.0);
        assert!(c.pause().await.is_ok(), "pausing silence should be a no-op, not a failure");
    }

    #[tokio::test]
    async fn spotify_absent_gives_an_actionable_error() {
        let c = controller(-12.0);
        let err = c.play_spotify("miles davis").await.unwrap_err().to_string();
        assert!(err.contains("SPOTIFY_CLIENT_ID"));
    }

    #[tokio::test]
    async fn time_of_day_needs_no_backend() {
        let c = controller(-12.0);
        let out = c.execute(&DeviceCommand::TimeOfDay).await.unwrap();
        assert!(out.starts_with("It's "));
    }

    #[test]
    fn stations_file_parses_and_lowercases_keys() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("stations.toml");
        std::fs::write(&p, "[stations]\nNPR = \"https://x/npr.mp3\"\n\"BBC World Service\" = \"https://x/bbc\"\n").unwrap();
        let s = MusicController::load_stations(&p);
        assert_eq!(s.get("npr").unwrap(), "https://x/npr.mp3");
        assert!(s.contains_key("bbc world service"));
    }

    #[test]
    fn missing_stations_file_is_empty_not_fatal() {
        assert!(MusicController::load_stations(Path::new("/nonexistent/stations.toml")).is_empty());
    }
}
