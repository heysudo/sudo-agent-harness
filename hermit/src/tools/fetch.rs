//! `fetch_page` — Firecrawl scrape.
//!
//! Verified against docs.firecrawl.dev (2026-08):
//!   POST https://api.firecrawl.dev/v2/scrape
//!   auth: `Authorization: Bearer <key>`
//!   body: { url, formats: ["markdown"], onlyMainContent: true }
//!   response: { success, data: { markdown, metadata: { title, ... } } }
//!
//! Only invoked when the model explicitly needs a full page — search excerpts
//! answer most questions on their own and cost a tenth as much latency.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize)]
struct ScrapeResponse {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<ScrapeData>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ScrapeData {
    #[serde(default)]
    markdown: Option<String>,
    #[serde(default)]
    metadata: Option<Metadata>,
}

#[derive(Debug, Deserialize, Default)]
struct Metadata {
    // Firecrawl returns either a string or an array here depending on the page's
    // meta tags, so accept anything and normalize.
    #[serde(default)]
    title: Option<serde_json::Value>,
}

pub struct FetchClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    timeout: Duration,
    max_tokens: usize,
}

impl FetchClient {
    pub fn new(http: reqwest::Client, cfg: &crate::config::Fetch, api_key: String) -> Self {
        Self {
            http,
            endpoint: format!("{}/v2/scrape", cfg.base_url.trim_end_matches('/')),
            api_key,
            timeout: Duration::from_millis(cfg.timeout_ms),
            max_tokens: cfg.max_tokens,
        }
    }

    pub async fn fetch(&self, url: &str) -> Result<String> {
        let url = normalize_url(url)?;

        let resp = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .timeout(self.timeout)
            .json(&serde_json::json!({
                "url": url,
                "formats": ["markdown"],
                "onlyMainContent": true,
            }))
            .send()
            .await
            .context("firecrawl request failed")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("firecrawl returned {status}: {}", crate::tools::clip(&text, 300));
        }

        let parsed: ScrapeResponse =
            serde_json::from_str(&text).context("decoding firecrawl response")?;
        if !parsed.success {
            bail!(
                "firecrawl could not scrape {url}: {}",
                parsed.error.unwrap_or_else(|| "unknown error".into())
            );
        }

        let data = parsed.data.unwrap_or_default();
        let markdown = data.markdown.unwrap_or_default();
        if markdown.trim().is_empty() {
            bail!("firecrawl returned no readable text for {url}");
        }

        let title = data
            .metadata
            .and_then(|m| m.title)
            .and_then(|v| match v {
                serde_json::Value::String(s) => Some(s),
                serde_json::Value::Array(a) => {
                    a.first().and_then(|x| x.as_str().map(String::from))
                }
                _ => None,
            })
            .unwrap_or_default();

        let body = truncate_tokens(&markdown, self.max_tokens);
        Ok(if title.is_empty() {
            format!("source: {url}\n\n{body}")
        } else {
            format!("title: {title}\nsource: {url}\n\n{body}")
        })
    }
}

/// Reject anything that is not plain http(s) before it reaches the network.
///
/// The model chooses this URL, often from search results, so it is untrusted
/// input. Blocking non-http schemes stops `file://`, `gopher://` and friends from
/// turning a scrape into a local-file read on whatever host runs the scraper.
fn normalize_url(url: &str) -> Result<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        bail!("fetch_page called with an empty url");
    }
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with("http://") || lowered.starts_with("https://") {
        return Ok(trimmed.to_string());
    }

    // Reject ANY other scheme, including the schemeless-authority forms that have
    // no "://" at all — `data:text/html,...`, `javascript:...`, `mailto:...`.
    // Matching on "://" alone would let those through to the bare-domain branch
    // below and produce `https://data:text/html,...`.
    let scheme_end = lowered
        .find(':')
        .filter(|&i| {
            i > 0
                && lowered[..i].starts_with(|c: char| c.is_ascii_alphabetic())
                && lowered[..i]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        });
    if let Some(i) = scheme_end {
        // A bare "example.com:8080/path" is a host:port, not a scheme.
        let is_port = lowered[i + 1..].starts_with(|c: char| c.is_ascii_digit());
        if !is_port {
            bail!("refusing to fetch non-http url: {trimmed}");
        }
    }

    // Bare domain: assume https.
    Ok(format!("https://{trimmed}"))
}

/// Hard-cap page text handed to the model (spec §6: 4,000 tokens).
///
/// Cuts on a line boundary so markdown structure survives, and says plainly that
/// it truncated so the model does not treat a partial page as complete.
pub fn truncate_tokens(text: &str, max_tokens: usize) -> String {
    if crate::memory::approx_tokens(text) <= max_tokens {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for line in text.lines() {
        let cost = crate::memory::approx_tokens(line) + 1;
        if used + cost > max_tokens {
            break;
        }
        out.push_str(line);
        out.push('\n');
        used += cost;
    }
    out.push_str("\n[truncated: page exceeded the size limit]");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_uses_v2_and_is_slash_safe() {
        let cfg = crate::config::Fetch {
            base_url: "https://api.firecrawl.dev/".into(),
            ..Default::default()
        };
        let c = FetchClient::new(reqwest::Client::new(), &cfg, "k".into());
        assert_eq!(c.endpoint, "https://api.firecrawl.dev/v2/scrape");
    }

    #[test]
    fn rejects_non_http_schemes() {
        for bad in [
            "file:///etc/passwd",
            "gopher://x",
            "ftp://host/f",
            "data:text/html,<script>alert(1)</script>",
            "javascript:alert(1)",
            "mailto:a@b.c",
        ] {
            assert!(normalize_url(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn accepts_http_and_bare_domains() {
        assert_eq!(normalize_url("https://x.com/a").unwrap(), "https://x.com/a");
        assert_eq!(normalize_url("http://x.com").unwrap(), "http://x.com");
        assert_eq!(normalize_url("example.com/page").unwrap(), "https://example.com/page");
        // host:port must not be mistaken for a scheme
        assert_eq!(normalize_url("example.com:8080/p").unwrap(), "https://example.com:8080/p");
    }

    #[test]
    fn empty_url_is_rejected() {
        assert!(normalize_url("   ").is_err());
    }

    #[test]
    fn truncation_respects_the_cap_and_announces_itself() {
        let long = (0..5000).map(|i| format!("line number {i}\n")).collect::<String>();
        let out = truncate_tokens(&long, 4000);
        assert!(crate::memory::approx_tokens(&out) <= 4100, "cap must hold");
        assert!(out.contains("[truncated"));
        assert!(out.starts_with("line number 0"));
    }

    #[test]
    fn short_pages_pass_through_untouched() {
        let s = "# Title\n\nA short page.";
        assert_eq!(truncate_tokens(s, 4000), s);
    }

    #[test]
    fn title_may_arrive_as_an_array() {
        let json = r#"{"success":true,"data":{"markdown":"body","metadata":{"title":["First","Second"]}}}"#;
        let r: ScrapeResponse = serde_json::from_str(json).unwrap();
        let t = r.data.unwrap().metadata.unwrap().title.unwrap();
        assert!(t.is_array());
    }
}
