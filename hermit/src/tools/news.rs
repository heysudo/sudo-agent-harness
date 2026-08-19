//! `news_briefing` — fetch RSS feeds concurrently, then one Cerebras pass to turn
//! them into a spoken-style briefing (spec §6).
//!
//! Feeds are fetched in parallel and a slow or dead feed is dropped rather than
//! delaying the whole briefing. The summarization prompt asks for prose meant to be
//! heard, not read: no bullet points, no URLs, no markdown.

use crate::config::{FeedSpec, News};
use crate::llm::{ChatMessage, ChatRequest, Effort};
use anyhow::{Result, bail};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Headline {
    pub source: String,
    pub title: String,
    pub summary: String,
}

/// Fetch and parse every configured feed concurrently.
pub async fn gather(http: &reqwest::Client, cfg: &News) -> Vec<Headline> {
    let timeout = Duration::from_millis(cfg.timeout_ms);
    let futures = cfg
        .feeds
        .iter()
        .map(|f| fetch_feed(http, f, cfg.items_per_feed, timeout));
    let per_feed = futures_util::future::join_all(futures).await;

    let mut out = Vec::new();
    for (spec, result) in cfg.feeds.iter().zip(per_feed) {
        match result {
            Ok(items) => out.extend(items),
            Err(e) => tracing::warn!(feed = %spec.name, error = %e, "skipping unreachable feed"),
        }
    }
    out
}

async fn fetch_feed(
    http: &reqwest::Client,
    spec: &FeedSpec,
    limit: usize,
    timeout: Duration,
) -> Result<Vec<Headline>> {
    let resp = http.get(&spec.url).timeout(timeout).send().await?;
    if !resp.status().is_success() {
        bail!("{} returned {}", spec.url, resp.status());
    }
    let bytes = resp.bytes().await?;
    let feed = feed_rs::parser::parse(&bytes[..])?;

    Ok(feed
        .entries
        .into_iter()
        .take(limit)
        .map(|e| {
            let title = e.title.map(|t| t.content).unwrap_or_default();
            let summary = e
                .summary
                .map(|t| t.content)
                .or_else(|| e.content.and_then(|c| c.body))
                .unwrap_or_default();
            Headline {
                source: spec.name.clone(),
                title: strip_html(&title),
                summary: clip_words(&strip_html(&summary), 60),
            }
        })
        .filter(|h| !h.title.trim().is_empty())
        .collect())
}

/// Summarize headlines into a spoken briefing.
pub async fn summarize(
    llm: &crate::llm::CerebrasClient,
    headlines: &[Headline],
    target: (usize, usize),
    style_prompt: &str,
) -> Result<String> {
    if headlines.is_empty() {
        bail!("no news feeds were reachable");
    }

    let mut material = String::new();
    for h in headlines {
        material.push_str(&format!("[{}] {}\n{}\n\n", h.source, h.title, h.summary));
    }

    let system = if style_prompt.trim().is_empty() {
        default_style(target)
    } else {
        style_prompt
            .replace("{min_words}", &target.0.to_string())
            .replace("{max_words}", &target.1.to_string())
    };

    let req = ChatRequest {
        messages: vec![
            ChatMessage::system(system),
            ChatMessage::user(format!("Today's headlines:\n\n{material}")),
        ],
        tools: vec![],
        // A briefing is a summarization pass, not a reasoning problem.
        effort: Effort::Low,
        max_tokens: 700,
        temperature: 0.4,
    };

    let text = llm.complete(req).await?;
    if text.trim().is_empty() {
        bail!("summarizer returned nothing");
    }
    Ok(text.trim().to_string())
}

fn default_style((min_w, max_w): (usize, usize)) -> String {
    format!(
        "You write short news briefings that will be READ ALOUD by a speaker in someone's kitchen.\n\
         Rules:\n\
         - {min_w}-{max_w} words, flowing prose in 3-5 short paragraphs.\n\
         - No bullet points, no headings, no markdown, no URLs, no source citations.\n\
         - Group related items. Lead with what matters most.\n\
         - Plain spoken English. Expand abbreviations the ear cannot parse.\n\
         - Never invent detail that is not in the material. If coverage is thin, say so briefly.\n\
         - Do not open with a greeting or close with a sign-off."
    )
}

/// Strip tags and decode the handful of entities that actually show up in RSS.
///
/// A full HTML parser would be the right tool if this text were rendered, but it
/// only ever becomes speech, so tag removal plus common entities is sufficient and
/// costs no dependency.
pub fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn clip_words(s: &str, max_words: usize) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() <= max_words {
        s.to_string()
    } else {
        format!("{}…", words[..max_words].join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_entities() {
        assert_eq!(
            strip_html("<p>Rates &amp; prices <b>rose</b></p>"),
            "Rates & prices rose"
        );
        assert_eq!(strip_html("a&nbsp;&nbsp;b"), "a b");
        assert_eq!(strip_html("<img src=\"x\"/>caption"), "caption");
    }

    #[test]
    fn strip_html_collapses_whitespace() {
        assert_eq!(strip_html("line\n\n  one\ttwo"), "line one two");
    }

    #[test]
    fn clip_words_adds_an_ellipsis_only_when_clipping() {
        assert_eq!(clip_words("a b c", 5), "a b c");
        assert_eq!(clip_words("a b c d e f", 3), "a b c…");
    }

    #[test]
    fn default_style_carries_the_word_target_and_forbids_markdown() {
        let s = default_style((150, 250));
        assert!(s.contains("150-250 words"));
        assert!(s.contains("No bullet points"));
        assert!(s.contains("READ ALOUD"));
    }

    #[test]
    fn custom_style_placeholders_are_substituted() {
        let out = summarize_style_for_test("Write {min_words} to {max_words} words.", (150, 250));
        assert_eq!(out, "Write 150 to 250 words.");
    }

    fn summarize_style_for_test(style: &str, target: (usize, usize)) -> String {
        style
            .replace("{min_words}", &target.0.to_string())
            .replace("{max_words}", &target.1.to_string())
    }

    #[tokio::test]
    async fn gather_tolerates_a_dead_feed() {
        let cfg = News {
            feeds: vec![FeedSpec {
                name: "Dead".into(),
                url: "http://127.0.0.1:1/never".into(),
            }],
            items_per_feed: 3,
            target_words: (150, 250),
            timeout_ms: 300,
        };
        // Must return empty rather than propagating an error.
        let out = gather(&reqwest::Client::new(), &cfg).await;
        assert!(out.is_empty());
    }

    #[test]
    fn parses_a_real_rss_document() {
        let xml = r#"<?xml version="1.0"?>
        <rss version="2.0"><channel><title>T</title>
        <item><title>First &amp; foremost</title><description>&lt;p&gt;Body text&lt;/p&gt;</description></item>
        <item><title>Second</title><description>More</description></item>
        </channel></rss>"#;
        let feed = feed_rs::parser::parse(xml.as_bytes()).unwrap();
        assert_eq!(feed.entries.len(), 2);
        let t = strip_html(&feed.entries[0].title.as_ref().unwrap().content);
        assert_eq!(t, "First & foremost");
    }
}
