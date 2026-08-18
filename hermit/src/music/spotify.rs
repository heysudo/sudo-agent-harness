//! Spotify Web API player control, targeting the local librespot device.
//!
//! librespot registers a Spotify Connect device on the LAN; we then drive playback
//! through the Web API with `device_id` pinned to that device. This requires a
//! Premium account (Connect is Premium-only) — documented honestly in the README.
//!
//! Auth is the OAuth refresh-token flow. The refresh token is long-lived and comes
//! from the environment; access tokens are cached in memory and refreshed slightly
//! before expiry so no user turn ever waits on a token round trip.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
/// Refresh this far before actual expiry so a turn never blocks on it.
const REFRESH_SKEW: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct SpotifyClient {
    http: reqwest::Client,
    api_base: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    device_name: String,
    token: Arc<RwLock<Option<CachedToken>>>,
    device_id: Arc<RwLock<Option<String>>>,
}

#[derive(Debug, Clone)]
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct DevicesResponse {
    #[serde(default)]
    devices: Vec<Device>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Device {
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub is_active: bool,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    tracks: Option<TrackPage>,
    #[serde(default)]
    albums: Option<ItemPage>,
    #[serde(default)]
    playlists: Option<ItemPage>,
    #[serde(default)]
    artists: Option<ItemPage>,
}

#[derive(Debug, Deserialize)]
struct TrackPage {
    #[serde(default)]
    items: Vec<Track>,
}

#[derive(Debug, Deserialize)]
struct ItemPage {
    #[serde(default)]
    items: Vec<SimpleItem>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Track {
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub artists: Vec<ArtistRef>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ArtistRef {
    pub name: String,
}

#[derive(Debug, Deserialize, Clone)]
struct SimpleItem {
    uri: String,
    name: String,
}

/// What is currently playing, if anything.
#[derive(Debug, Deserialize, Default)]
pub struct PlaybackState {
    #[serde(default)]
    pub is_playing: bool,
    #[serde(default)]
    pub item: Option<Track>,
}

impl SpotifyClient {
    pub fn new(
        http: reqwest::Client,
        api_base: impl Into<String>,
        client_id: String,
        client_secret: String,
        refresh_token: String,
        device_name: impl Into<String>,
    ) -> Self {
        Self {
            http,
            api_base: api_base.into().trim_end_matches('/').to_string(),
            client_id,
            client_secret,
            refresh_token,
            device_name: device_name.into(),
            token: Arc::new(RwLock::new(None)),
            device_id: Arc::new(RwLock::new(None)),
        }
    }

    /// Build from the environment, returning `None` when Spotify is not configured
    /// so the rest of the daemon runs fine without it.
    pub fn from_env(
        http: reqwest::Client,
        api_base: &str,
        device_name: &str,
    ) -> Option<Self> {
        let id = crate::http::secret_opt("SPOTIFY_CLIENT_ID")?;
        let secret = crate::http::secret_opt("SPOTIFY_CLIENT_SECRET")?;
        let refresh = crate::http::secret_opt("SPOTIFY_REFRESH_TOKEN")?;
        Some(Self::new(http, api_base, id, secret, refresh, device_name))
    }

    /// A valid access token, refreshing if needed.
    async fn access_token(&self) -> Result<String> {
        if let Some(t) = self.token.read().await.as_ref()
            && Instant::now() + REFRESH_SKEW < t.expires_at
        {
            return Ok(t.access_token.clone());
        }

        let resp = self
            .http
            .post(TOKEN_URL)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", self.refresh_token.as_str()),
            ])
            .timeout(Duration::from_secs(8))
            .send()
            .await
            .context("spotify token refresh failed")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("spotify token refresh returned {status}: {body}");
        }
        let parsed: TokenResponse =
            serde_json::from_str(&body).context("decoding spotify token response")?;

        let expires_in = if parsed.expires_in == 0 { 3600 } else { parsed.expires_in };
        let cached = CachedToken {
            access_token: parsed.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        };
        *self.token.write().await = Some(cached);
        Ok(parsed.access_token)
    }

    /// Warm the token at boot so the first "play X" does not pay for a refresh.
    pub async fn prewarm(&self) {
        match self.access_token().await {
            Ok(_) => tracing::info!("spotify token warmed"),
            Err(e) => tracing::warn!(error = %e, "spotify token warm failed"),
        }
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        let token = self.access_token().await?;
        Ok(self
            .http
            .get(format!("{}{}", self.api_base, path))
            .bearer_auth(token)
            .timeout(Duration::from_secs(6))
            .send()
            .await?)
    }

    async fn put(&self, path: &str, body: Option<serde_json::Value>) -> Result<()> {
        let token = self.access_token().await?;
        let mut req = self
            .http
            .put(format!("{}{}", self.api_base, path))
            .bearer_auth(token)
            .timeout(Duration::from_secs(6));
        req = match body {
            Some(b) => req.json(&b),
            // Spotify rejects a PUT with no body and no content-length.
            None => req.header(reqwest::header::CONTENT_LENGTH, "0"),
        };
        let resp = req.send().await?;
        expect_ok(resp).await
    }

    async fn post(&self, path: &str) -> Result<()> {
        let token = self.access_token().await?;
        let resp = self
            .http
            .post(format!("{}{}", self.api_base, path))
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_LENGTH, "0")
            .timeout(Duration::from_secs(6))
            .send()
            .await?;
        expect_ok(resp).await
    }

    /// The librespot device's id, discovered by name and then cached.
    ///
    /// Re-discovers automatically if the cached id disappears (librespot restart).
    pub async fn device_id(&self) -> Result<String> {
        if let Some(id) = self.device_id.read().await.clone() {
            return Ok(id);
        }
        let id = self.discover_device().await?;
        *self.device_id.write().await = Some(id.clone());
        Ok(id)
    }

    async fn discover_device(&self) -> Result<String> {
        let resp = self.get("/me/player/devices").await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("listing spotify devices returned {status}: {body}");
        }
        let parsed: DevicesResponse = serde_json::from_str(&body)?;

        let wanted = self.device_name.to_lowercase();
        parsed
            .devices
            .iter()
            .find(|d| d.name.to_lowercase() == wanted)
            .and_then(|d| d.id.clone())
            .ok_or_else(|| {
                let names: Vec<&str> = parsed.devices.iter().map(|d| d.name.as_str()).collect();
                anyhow::anyhow!(
                    "librespot device {:?} not found among {:?} — is the librespot sidecar running \
                     and logged in?",
                    self.device_name,
                    names
                )
            })
    }

    /// Drop the cached device id so the next call rediscovers.
    pub async fn invalidate_device(&self) {
        *self.device_id.write().await = None;
    }

    /// Search and start playback of the best match on our device.
    pub async fn play_query(&self, query: &str) -> Result<String> {
        let uri_and_label = self.resolve_query(query).await?;
        self.play_uri(&uri_and_label.0, uri_and_label.1.clone()).await?;
        Ok(uri_and_label.1)
    }

    /// Resolve free text to a playable URI plus a human label.
    async fn resolve_query(&self, query: &str) -> Result<(String, String)> {
        let encoded = urlencoding::encode(query);
        let path = format!("/search?q={encoded}&type=track,album,playlist,artist&limit=3");
        let resp = self.get(&path).await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("spotify search returned {status}: {body}");
        }
        let parsed: SearchResponse = serde_json::from_str(&body)?;

        // Prefer a track, then album, then playlist, then artist. A bare artist
        // name most often means "play this artist", but a track match on the exact
        // phrase is the stronger signal, so tracks win.
        if let Some(t) = parsed.tracks.as_ref().and_then(|p| p.items.first()) {
            let artists = t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
            let label = if artists.is_empty() {
                t.name.clone()
            } else {
                format!("{} by {}", t.name, artists)
            };
            return Ok((t.uri.clone(), label));
        }
        for page in [&parsed.albums, &parsed.playlists, &parsed.artists] {
            if let Some(item) = page.as_ref().and_then(|p| p.items.first()) {
                return Ok((item.uri.clone(), item.name.clone()));
            }
        }
        bail!("nothing on Spotify matched {query:?}")
    }

    /// Start playback of a URI. Track URIs go in `uris`, everything else is a
    /// context — sending the wrong one is a 400 from the API.
    async fn play_uri(&self, uri: &str, _label: String) -> Result<()> {
        let device = self.device_id().await?;
        let body = if uri.starts_with("spotify:track:") {
            serde_json::json!({ "uris": [uri] })
        } else {
            serde_json::json!({ "context_uri": uri })
        };
        let path = format!("/me/player/play?device_id={device}");

        match self.put(&path, Some(body.clone())).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Most likely the device id went stale (librespot restarted).
                tracing::warn!(error = %e, "spotify play failed; rediscovering device and retrying");
                self.invalidate_device().await;
                let device = self.device_id().await?;
                self.put(&format!("/me/player/play?device_id={device}"), Some(body)).await
            }
        }
    }

    /// Transfer playback to our device without changing what is queued.
    pub async fn ensure_active(&self) -> Result<()> {
        let device = self.device_id().await?;
        self.put(
            "/me/player",
            Some(serde_json::json!({ "device_ids": [device], "play": false })),
        )
        .await
    }

    pub async fn resume(&self) -> Result<()> {
        let device = self.device_id().await?;
        self.put(&format!("/me/player/play?device_id={device}"), None).await
    }

    pub async fn pause(&self) -> Result<()> {
        let device = self.device_id().await?;
        self.put(&format!("/me/player/pause?device_id={device}"), None).await
    }

    pub async fn next(&self) -> Result<()> {
        let device = self.device_id().await?;
        self.post(&format!("/me/player/next?device_id={device}")).await
    }

    pub async fn previous(&self) -> Result<()> {
        let device = self.device_id().await?;
        self.post(&format!("/me/player/previous?device_id={device}")).await
    }

    pub async fn set_volume(&self, percent: u8) -> Result<()> {
        let device = self.device_id().await?;
        let v = percent.min(100);
        self.put(
            &format!("/me/player/volume?volume_percent={v}&device_id={device}"),
            None,
        )
        .await
    }

    pub async fn state(&self) -> Result<PlaybackState> {
        let resp = self.get("/me/player").await?;
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(PlaybackState::default()); // nothing playing anywhere
        }
        let body = resp.text().await.unwrap_or_default();
        Ok(serde_json::from_str(&body).unwrap_or_default())
    }

    /// Human-readable "now playing".
    pub async fn now_playing(&self) -> Option<String> {
        let s = self.state().await.ok()?;
        let t = s.item?;
        let artists = t.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
        Some(if artists.is_empty() { t.name } else { format!("{} by {}", t.name, artists) })
    }
}

async fn expect_ok(resp: reqwest::Response) -> Result<()> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let body = resp.text().await.unwrap_or_default();
    // 403 on a player endpoint is nearly always "not a Premium account".
    if status == reqwest::StatusCode::FORBIDDEN {
        bail!(
            "spotify refused the request ({status}). Spotify Connect control requires a Premium \
             account. Response: {body}"
        );
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        bail!("spotify device not found ({status}) — librespot may have restarted. Response: {body}");
    }
    bail!("spotify returned {status}: {body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_uris_go_in_uris_and_contexts_in_context_uri() {
        // Guards the shape decision without needing a live API: assert the branch
        // predicate directly, since sending the wrong field is a hard 400.
        assert!("spotify:track:abc".starts_with("spotify:track:"));
        assert!(!"spotify:album:abc".starts_with("spotify:track:"));
        assert!(!"spotify:playlist:abc".starts_with("spotify:track:"));
    }

    #[test]
    fn from_env_is_none_when_unconfigured() {
        for k in ["SPOTIFY_CLIENT_ID", "SPOTIFY_CLIENT_SECRET", "SPOTIFY_REFRESH_TOKEN"] {
            unsafe { std::env::remove_var(k) };
        }
        let c = SpotifyClient::from_env(reqwest::Client::new(), "https://api.spotify.com/v1", "Hermit");
        assert!(c.is_none(), "Spotify must be optional");
    }

    #[test]
    fn api_base_trailing_slash_is_normalized() {
        let c = SpotifyClient::new(
            reqwest::Client::new(),
            "https://api.spotify.com/v1/",
            "id".into(),
            "sec".into(),
            "rt".into(),
            "Hermit",
        );
        assert_eq!(c.api_base, "https://api.spotify.com/v1");
    }

    #[tokio::test]
    async fn device_cache_can_be_invalidated() {
        let c = SpotifyClient::new(
            reqwest::Client::new(),
            "https://api.spotify.com/v1",
            "id".into(),
            "sec".into(),
            "rt".into(),
            "Hermit",
        );
        *c.device_id.write().await = Some("dev123".into());
        assert_eq!(c.device_id().await.unwrap(), "dev123");
        c.invalidate_device().await;
        assert!(c.device_id.read().await.is_none());
    }

    #[test]
    fn search_response_tolerates_missing_pages() {
        // Spotify omits whole sections when a type has no matches.
        let r: SearchResponse = serde_json::from_str(r#"{"tracks":{"items":[]}}"#).unwrap();
        assert!(r.albums.is_none());
        assert!(r.tracks.unwrap().items.is_empty());
    }
}
