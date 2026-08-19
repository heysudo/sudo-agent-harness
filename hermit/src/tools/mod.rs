//! The tool layer — exactly five tools (spec §6). No more, ever.
//!
//! Schemas are deliberately tiny. Every extra tool and every extra parameter makes
//! selection slower and less reliable, and this device answers in under a second.

pub mod fetch;
pub mod news;
pub mod research;
pub mod search;

use crate::llm::{ToolCall, ToolDef};
use crate::music::Music;
use std::sync::Arc;
use std::time::Instant;

/// Everything a tool worker needs. Cheap to clone (all handles are `Arc`-backed).
#[derive(Clone)]
pub struct ToolContext {
    pub cfg: Arc<crate::config::Config>,
    pub search: Option<Arc<search::SearchClient>>,
    pub fetch: Option<Arc<fetch::FetchClient>>,
    pub http: reqwest::Client,
    pub llm: Arc<crate::llm::CerebrasClient>,
    pub music: Music,
    /// Submissions to the off-hot-path research worker.
    pub research: tokio::sync::mpsc::Sender<research::ResearchJob>,
    /// Style prompt for news briefings, loaded from config/prompts/.
    pub news_style: Arc<String>,
}

/// One tool's result, as handed back to the model.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub ok: bool,
}

impl ToolOutput {
    fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ok: true,
        }
    }
    /// Errors are returned to the model as text, not propagated as failures: the
    /// model can usually recover ("that page wouldn't load, here's what I know"),
    /// whereas aborting the turn always sounds broken to the user.
    fn err(msg: impl std::fmt::Display) -> Self {
        Self {
            content: format!("Tool error: {msg}"),
            ok: false,
        }
    }
}

/// The five tool schemas. This list is closed.
pub fn schemas() -> Vec<ToolDef> {
    vec![
        ToolDef::new(
            "web_search",
            "Search the web for current or factual information. Use for anything time-sensitive, \
             local, or that you are not certain of.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query." }
                },
                "required": ["query"]
            }),
        ),
        ToolDef::new(
            "fetch_page",
            "Read the full text of one specific web page. Only use when search excerpts are not \
             enough and you already have the URL.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Absolute http(s) URL." }
                },
                "required": ["url"]
            }),
        ),
        ToolDef::new(
            "news_briefing",
            "Produce a short spoken news briefing from the user's configured feeds.",
            serde_json::json!({ "type": "object", "properties": {} }),
        ),
        ToolDef::new(
            "music",
            "Control playback: play something on Spotify, play a named radio station, or \
             pause/resume/stop/skip/set volume/report status.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["play_spotify", "play_station", "pause", "resume", "stop",
                                 "next", "previous", "volume", "status"]
                    },
                    "query": {
                        "type": "string",
                        "description": "What to play, for play_spotify or play_station."
                    },
                    "volume": { "type": "integer", "description": "0-100, for the volume action." }
                },
                "required": ["action"]
            }),
        ),
        ToolDef::new(
            "background_research",
            "Hand a question to the background researcher when it needs many steps. Returns \
             immediately; the answer is delivered later. Use for 'research', 'deep dive', or any \
             question you cannot answer within two tool rounds.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The full question to research." }
                },
                "required": ["question"]
            }),
        ),
    ]
}

/// Names the model is allowed to call. Anything else is rejected.
pub const TOOL_NAMES: &[&str] = &[
    "web_search",
    "fetch_page",
    "news_briefing",
    "music",
    "background_research",
];

/// Execute every tool call of a round CONCURRENTLY (spec §4.3).
///
/// Returns results in the same order as the input calls, each with its own duration
/// for the metrics line.
pub async fn execute_all(
    ctx: &ToolContext,
    calls: &[ToolCall],
    objective: &str,
) -> Vec<(ToolCall, ToolOutput, f64)> {
    let futures = calls.iter().map(|call| {
        let ctx = ctx.clone();
        let call = call.clone();
        let objective = objective.to_string();
        async move {
            let started = Instant::now();
            let out = execute(&ctx, &call, &objective).await;
            let ms = crate::metrics::ms_since(started);
            tracing::debug!(tool = %call.name, ms, ok = out.ok, "tool complete");
            (call, out, ms)
        }
    });
    futures_util::future::join_all(futures).await
}

/// Execute a single tool call.
pub async fn execute(ctx: &ToolContext, call: &ToolCall, objective: &str) -> ToolOutput {
    let args = call.args();

    match call.name.as_str() {
        "web_search" => {
            let Some(q) = args.get("query").and_then(|v| v.as_str()) else {
                return ToolOutput::err("web_search requires a 'query' string");
            };
            let Some(client) = &ctx.search else {
                return ToolOutput::err("web search is not configured (PARALLEL_API_KEY missing)");
            };
            match client.search(q, Some(objective)).await {
                Ok(resp) => ToolOutput::ok(search::format_for_model(
                    &resp,
                    ctx.cfg.search.max_results as usize,
                )),
                Err(e) => ToolOutput::err(e),
            }
        }

        "fetch_page" => {
            let Some(url) = args.get("url").and_then(|v| v.as_str()) else {
                return ToolOutput::err("fetch_page requires a 'url' string");
            };
            let Some(client) = &ctx.fetch else {
                return ToolOutput::err(
                    "page fetching is not configured (FIRECRAWL_API_KEY missing)",
                );
            };
            match client.fetch(url).await {
                Ok(text) => ToolOutput::ok(text),
                Err(e) => ToolOutput::err(e),
            }
        }

        "news_briefing" => {
            let headlines = news::gather(&ctx.http, &ctx.cfg.news).await;
            match news::summarize(
                &ctx.llm,
                &headlines,
                ctx.cfg.news.target_words,
                &ctx.news_style,
            )
            .await
            {
                Ok(text) => ToolOutput::ok(text),
                Err(e) => ToolOutput::err(e),
            }
        }

        "music" => music_action(ctx, &args).await,

        "background_research" => {
            let Some(question) = args.get("question").and_then(|v| v.as_str()) else {
                return ToolOutput::err("background_research requires a 'question' string");
            };
            let job = research::ResearchJob {
                question: question.to_string(),
            };
            match ctx.research.try_send(job) {
                Ok(()) => ToolOutput::ok(
                    "Research started in the background. Tell the user you're looking into it and \
                     will report back — do not attempt to answer the question yourself.",
                ),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    ToolOutput::err("the research queue is full; try again shortly")
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    ToolOutput::err("the research worker is not running")
                }
            }
        }

        other => ToolOutput::err(format!(
            "unknown tool {other:?}; available tools are {}",
            TOOL_NAMES.join(", ")
        )),
    }
}

async fn music_action(ctx: &ToolContext, args: &serde_json::Value) -> ToolOutput {
    let action = args
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("status");
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    let result: anyhow::Result<String> = match action {
        "play_spotify" => {
            if query.is_empty() {
                Err(anyhow::anyhow!("play_spotify needs a 'query'"))
            } else {
                ctx.music
                    .play_spotify(query)
                    .await
                    .map(|l| format!("playing {l}"))
            }
        }
        "play_station" => {
            if query.is_empty() {
                Err(anyhow::anyhow!("play_station needs a 'query'"))
            } else {
                ctx.music
                    .play_station(query)
                    .await
                    .map(|_| format!("playing {query}"))
            }
        }
        "pause" => ctx.music.pause().await.map(|_| "paused".into()),
        "resume" => ctx.music.resume().await.map(|_| "resumed".into()),
        "stop" => ctx.music.stop().await.map(|_| "stopped".into()),
        "next" => ctx.music.next().await.map(|_| "skipped".into()),
        "previous" => ctx.music.previous().await.map(|_| "went back".into()),
        "volume" => {
            let v = args.get("volume").and_then(|v| v.as_i64()).unwrap_or(-1);
            if !(0..=100).contains(&v) {
                Err(anyhow::anyhow!("volume must be 0-100"))
            } else {
                ctx.music
                    .set_volume(v as u8)
                    .await
                    .map(|_| format!("volume {v}"))
            }
        }
        "status" => Ok(ctx.music.status().await),
        other => Err(anyhow::anyhow!("unknown music action {other:?}")),
    };

    match result {
        Ok(s) => ToolOutput::ok(s),
        Err(e) => ToolOutput::err(e),
    }
}

/// UTF-8-safe truncation for error bodies.
pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|i| *i <= max)
        .last()
        .unwrap_or(0);
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_five_tools_are_exposed() {
        let s = schemas();
        assert_eq!(s.len(), 5, "spec §6 fixes the tool count at five");
        let names: Vec<&str> = s.iter().map(|t| t.function.name.as_str()).collect();
        assert_eq!(names, TOOL_NAMES);
    }

    #[test]
    fn schemas_stay_small() {
        // Big schemas slow and degrade tool selection (§6). Keep them honest.
        for t in schemas() {
            let json = serde_json::to_string(&t.function.parameters).unwrap();
            assert!(
                json.len() < 600,
                "{} schema is {} bytes — too large",
                t.function.name,
                json.len()
            );
            assert!(
                t.function.description.len() < 260,
                "{} description is too long",
                t.function.name
            );
        }
    }

    #[test]
    fn every_schema_is_a_valid_object_with_declared_properties() {
        for t in schemas() {
            assert_eq!(t.function.parameters["type"], "object");
            assert!(t.function.parameters.get("properties").is_some());
        }
    }

    #[test]
    fn clip_is_utf8_safe() {
        let s = "αβγδε".repeat(50);
        let out = clip(&s, 7);
        assert!(out.ends_with('…'));
        assert!(out.len() <= 16);
    }

    #[test]
    fn clip_leaves_short_strings_alone() {
        assert_eq!(clip("short", 100), "short");
    }

    #[test]
    fn tool_output_error_is_still_content_for_the_model() {
        let o = ToolOutput::err("boom");
        assert!(!o.ok);
        assert!(o.content.contains("boom"));
    }
}
