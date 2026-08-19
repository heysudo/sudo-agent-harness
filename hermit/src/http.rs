//! Shared, pre-warmed HTTP plumbing.
//!
//! Spec §5: a cold TLS handshake costs 100–400 ms and we must never pay it inside a
//! turn. One `reqwest::Client` (so one connection pool) is built at boot with a long
//! idle timeout and HTTP/2 keep-alive pings, then [`prewarm`] opens and parks a live
//! connection to every upstream before the first user request arrives. A background
//! task re-warms on a timer so the pool never goes cold during a quiet night.

use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

/// Build the process-wide HTTP client.
///
/// Notes on the settings that matter for latency:
/// - `pool_idle_timeout` far exceeds any realistic gap between turns, so the
///   TLS session survives idle periods instead of being reaped.
/// - `http2_keep_alive_interval` + `while_idle` keeps NAT/firewall state alive and
///   detects a dead path before a user is waiting on it.
/// - `tcp_nodelay` disables Nagle: our requests are small and latency-critical.
pub fn build_client() -> Result<reqwest::Client> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("hermit/", env!("CARGO_PKG_VERSION")))
        .pool_idle_timeout(Duration::from_secs(600))
        .pool_max_idle_per_host(4)
        .http2_keep_alive_interval(Duration::from_secs(30))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        .tcp_keepalive(Duration::from_secs(60))
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_secs(5))
        .build()?;
    Ok(client)
}

/// Upstreams worth holding a warm connection to. WebSocket endpoints (TTS, STT) are
/// warmed separately by their own modules since they need a different handshake.
pub fn hot_endpoints(cfg: &crate::config::Config) -> Vec<String> {
    vec![
        cfg.llm.base_url.clone(),
        cfg.search.base_url.clone(),
        cfg.fetch.base_url.clone(),
        cfg.music.spotify_api_base.clone(),
    ]
}

/// Open a connection to each upstream and throw the response away.
///
/// We deliberately use a cheap request that does not need credentials. A 401/404 is
/// a perfectly good outcome — the point is the completed TCP + TLS + HTTP/2 handshake
/// now sitting in the pool, not the response body.
pub async fn prewarm(client: &reqwest::Client, endpoints: &[String]) {
    let futures = endpoints.iter().map(|url| {
        let client = client.clone();
        let url = url.clone();
        async move {
            let started = std::time::Instant::now();
            let result = client
                .get(&url)
                .timeout(Duration::from_secs(6))
                .send()
                .await;
            let ms = started.elapsed().as_millis();
            match result {
                Ok(resp) => tracing::debug!(url = %url, status = %resp.status(), ms, "prewarmed"),
                Err(e) => tracing::warn!(url = %url, ms, error = %e, "prewarm failed (will retry on timer)"),
            }
        }
    });
    futures_util::future::join_all(futures).await;
}

/// Keep the pool hot forever. Runs every `interval`, well inside `pool_idle_timeout`.
pub async fn prewarm_loop(
    client: reqwest::Client,
    mut cfg_rx: tokio::sync::watch::Receiver<Arc<crate::config::Config>>,
    interval: Duration,
) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let cfg = cfg_rx.borrow_and_update().clone();
        prewarm(&client, &hot_endpoints(&cfg)).await;
    }
}

/// Read a secret from the environment, with a clear error naming the variable.
///
/// Secrets are never read from the config file (spec §14: no secrets in the repo);
/// systemd supplies them via `EnvironmentFile=/etc/hermit/hermit.env`.
pub fn secret(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| {
        anyhow::anyhow!(
            "missing required secret {name}; set it in /etc/hermit/hermit.env (see .env.example)"
        )
    })
}

/// Like [`secret`] but returns `None` instead of erroring, for optional providers.
pub fn secret_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds() {
        assert!(build_client().is_ok());
    }

    #[test]
    fn hot_endpoints_cover_every_http_upstream() {
        let cfg = crate::config::Config::default();
        let eps = hot_endpoints(&cfg);
        assert!(eps.iter().any(|e| e.contains("cerebras")));
        assert!(eps.iter().any(|e| e.contains("parallel")));
        assert!(eps.iter().any(|e| e.contains("firecrawl")));
    }

    #[test]
    fn secret_error_names_the_variable() {
        // A name no environment would define, so this needs no env mutation
        // (mutating one is a data race under parallel test threads).
        let name = "HERMIT_TEST_ABSENT_KEY_a41f2c7b";
        let err = secret(name).unwrap_err().to_string();
        assert!(err.contains(name));
    }
}
