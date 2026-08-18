//! `web_search` — Parallel Search API.
//!
//! Verified against docs.parallel.ai (2026-08):
//!   POST https://api.parallel.ai/v1/search
//!   auth header: `x-api-key` (NOT bearer)
//!   body: { objective?, search_queries[], mode?, max_chars_total?, advanced_settings? }
//!   response: { search_id, results: [{ url, title, publish_date, excerpts[] }], ... }
//!
//! CRITICAL (spec §6): `mode` MUST be sent explicitly. The documented default when
//! `mode` is omitted is `advanced` — ~3 s, versus turbo's ~200 ms. Forgetting this
//! field silently costs an order of magnitude of latency, which is why
//! `Config::validate` refuses to start unless `search.mode == "turbo"`.
//!
//! Turbo supports English and Japanese only; other scripts fall back to `basic`.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Deserialize, Default)]
pub struct SearchResponse {
    #[serde(default)]
    pub results: Vec<SearchResult>,
    #[serde(default)]
    pub warnings: Option<Vec<Warning>>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SearchResult {
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub publish_date: Option<String>,
    #[serde(default)]
    pub excerpts: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Warning {
    #[serde(default)]
    pub message: String,
}

pub struct SearchClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    mode: String,
    fallback_mode: String,
    max_results: u32,
    timeout: Duration,
}

impl SearchClient {
    pub fn new(http: reqwest::Client, cfg: &crate::config::Search, api_key: String) -> Self {
        Self {
            http,
            endpoint: format!("{}/v1/search", cfg.base_url.trim_end_matches('/')),
            api_key,
            mode: cfg.mode.clone(),
            fallback_mode: cfg.fallback_mode.clone(),
            max_results: cfg.max_results,
            timeout: Duration::from_millis(cfg.timeout_ms),
        }
    }

    /// Run one search. `objective` gives the API context for ranking excerpts; we
    /// pass the user's actual question so returned excerpts are answer-grade and
    /// need no further scraping (spec §6).
    pub async fn search(&self, query: &str, objective: Option<&str>) -> Result<SearchResponse> {
        let mode = self.mode_for(query);

        let mut body = serde_json::json!({
            "search_queries": [query],
            // Explicit, always. See the module note.
            "mode": mode,
            "advanced_settings": { "max_results": self.max_results },
        });
        if let Some(obj) = objective
            && !obj.trim().is_empty()
        {
            body["objective"] = serde_json::Value::String(obj.to_string());
        }

        let resp = self
            .http
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .context("parallel search request failed")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("parallel search returned {status}: {}", crate::tools::clip(&text, 300));
        }

        let parsed: SearchResponse =
            serde_json::from_str(&text).context("decoding parallel search response")?;

        if let Some(ws) = &parsed.warnings {
            for w in ws {
                tracing::warn!(warning = %w.message, "parallel search warning");
            }
        }
        Ok(parsed)
    }

    /// Turbo covers English and Japanese. Anything in another script must use the
    /// fallback mode or it will return poor results.
    ///
    /// Limitation, stated honestly: script detection cannot distinguish English
    /// from other Latin-script languages, so a Spanish query still goes to turbo.
    /// Doing better would need a language-ID model, which the RAM budget (§11)
    /// rules out.
    fn mode_for(&self, query: &str) -> &str {
        if self.mode != "turbo" {
            return &self.mode;
        }
        if script_is_turbo_supported(query) { &self.mode } else { &self.fallback_mode }
    }
}

/// True when the text is Latin-script (assumed English) or Japanese.
pub fn script_is_turbo_supported(s: &str) -> bool {
    let mut has_kana = false;
    let mut has_unsupported = false;

    for c in s.chars() {
        let cp = c as u32;
        match cp {
            // Hiragana / Katakana / halfwidth katakana → definitely Japanese.
            0x3040..=0x30FF | 0xFF66..=0xFF9D => has_kana = true,
            // CJK ideographs: Japanese *or* Chinese. Only kana disambiguates.
            0x4E00..=0x9FFF | 0x3400..=0x4DBF => {}
            // Scripts turbo does not cover.
            0x0400..=0x04FF   // Cyrillic
            | 0x0590..=0x05FF // Hebrew
            | 0x0600..=0x06FF // Arabic
            | 0x0900..=0x097F // Devanagari
            | 0x0E00..=0x0E7F // Thai
            | 0xAC00..=0xD7AF // Hangul
            | 0x0370..=0x03FF // Greek
            => has_unsupported = true,
            _ => {}
        }
    }

    if has_unsupported {
        return false;
    }
    // Han characters with no kana are most likely Chinese → use the fallback.
    let has_han = s.chars().any(|c| matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF));
    if has_han && !has_kana {
        return false;
    }
    true
}

/// Render results for the model. Excerpts are passed through as the API returns
/// them; we do not scrape on top (spec §6).
pub fn format_for_model(resp: &SearchResponse, max_results: usize) -> String {
    if resp.results.is_empty() {
        return "No results.".to_string();
    }
    let mut out = String::new();
    for (i, r) in resp.results.iter().take(max_results).enumerate() {
        out.push_str(&format!("[{}] {}\n", i + 1, r.title.as_deref().unwrap_or(&r.url)));
        out.push_str(&format!("url: {}\n", r.url));
        if let Some(d) = &r.publish_date {
            out.push_str(&format!("date: {d}\n"));
        }
        for ex in r.excerpts.iter().take(3) {
            out.push_str(ex.trim());
            out.push('\n');
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(mode: &str, fallback: &str) -> SearchClient {
        let cfg = crate::config::Search {
            mode: mode.into(),
            fallback_mode: fallback.into(),
            ..Default::default()
        };
        SearchClient::new(reqwest::Client::new(), &cfg, "test-key".into())
    }

    #[test]
    fn endpoint_is_versioned_and_slash_safe() {
        let cfg = crate::config::Search {
            base_url: "https://api.parallel.ai/".into(),
            ..Default::default()
        };
        let c = SearchClient::new(reqwest::Client::new(), &cfg, "k".into());
        assert_eq!(c.endpoint, "https://api.parallel.ai/v1/search");
    }

    #[test]
    fn english_and_japanese_use_turbo() {
        let c = client("turbo", "basic");
        assert_eq!(c.mode_for("what is the tide in Bergen"), "turbo");
        assert_eq!(c.mode_for("東京の天気はどうですか"), "turbo", "kana marks Japanese");
        assert_eq!(c.mode_for("ソニックの最新ニュース"), "turbo");
    }

    #[test]
    fn unsupported_scripts_fall_back() {
        let c = client("turbo", "basic");
        assert_eq!(c.mode_for("какая погода в Москве"), "basic", "Cyrillic");
        assert_eq!(c.mode_for("서울 날씨"), "basic", "Hangul");
        assert_eq!(c.mode_for("طقس اليوم"), "basic", "Arabic");
        assert_eq!(c.mode_for("北京的天气"), "basic", "Han without kana => Chinese");
    }

    #[test]
    fn mixed_japanese_with_latin_still_turbo() {
        assert!(script_is_turbo_supported("iPhone の価格はいくらですか"));
    }

    #[test]
    fn formatting_includes_url_and_excerpts() {
        let resp = SearchResponse {
            results: vec![SearchResult {
                url: "https://example.com/a".into(),
                title: Some("Tide tables".into()),
                publish_date: Some("2026-08-01".into()),
                excerpts: vec!["High tide at 14:20.".into()],
            }],
            warnings: None,
        };
        let s = format_for_model(&resp, 5);
        assert!(s.contains("Tide tables"));
        assert!(s.contains("https://example.com/a"));
        assert!(s.contains("High tide at 14:20."));
        assert!(s.contains("2026-08-01"));
    }

    #[test]
    fn empty_results_say_so_rather_than_returning_blank() {
        assert_eq!(format_for_model(&SearchResponse::default(), 5), "No results.");
    }

    #[test]
    fn response_decodes_with_missing_optional_fields() {
        let json = r#"{"search_id":"s1","results":[{"url":"https://x","excerpts":[]}],"session_id":"abc"}"#;
        let r: SearchResponse = serde_json::from_str(json).unwrap();
        assert_eq!(r.results.len(), 1);
        assert!(r.results[0].title.is_none());
    }
}
