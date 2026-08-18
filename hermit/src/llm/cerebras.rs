//! Streaming client for the Cerebras OpenAI-compatible chat completions endpoint.
//!
//! Design notes:
//! - Streaming only. Tokens are forwarded the instant they arrive so the sentence
//!   chunker can start TTS while the model is still generating.
//! - Tool-call deltas arrive fragmented across many SSE frames (`id` in one, the
//!   `arguments` JSON split across a dozen more). [`ToolCallAccumulator`] reassembles
//!   them by index, which is the only correct key — `id` is absent on later deltas.
//! - `reasoning` deltas from gpt-oss are dropped: they must never reach TTS.

use super::{ChatRequest, Effort, StreamItem, ToolCall};
use anyhow::{Context, Result, bail};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::Deserialize;
use std::time::Duration;

pub struct CerebrasClient {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    timeout: Duration,
}

impl CerebrasClient {
    pub fn new(
        client: reqwest::Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            model: model.into(),
            timeout,
        }
    }

    fn body(&self, req: &ChatRequest, stream: bool) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": req.messages,
            "stream": stream,
            "max_completion_tokens": req.max_tokens,
            "temperature": req.temperature,
            "reasoning_effort": req.effort.as_str(),
        });
        if !req.tools.is_empty() {
            body["tools"] = serde_json::to_value(&req.tools).unwrap_or(serde_json::Value::Null);
            body["tool_choice"] = serde_json::Value::String("auto".into());
            // Let the model request several tools in one turn; we execute them
            // concurrently (spec §4.3).
            body["parallel_tool_calls"] = serde_json::Value::Bool(true);
        }
        body
    }

    /// Start a streaming completion. Returns a receiver of [`StreamItem`]s.
    ///
    /// The HTTP request and SSE decode run in a spawned task so the caller can
    /// start consuming immediately; dropping the receiver cancels the task and
    /// aborts the upstream request.
    pub async fn stream(
        &self,
        req: ChatRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<StreamItem>>> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.body(&req, true);

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .context("cerebras request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!("cerebras returned {status}: {}", truncate(&text, 500));
        }

        // Depth 32: enough to absorb a burst at 3,000 tok/s without the decoder
        // blocking, small enough that a slow consumer applies backpressure rather
        // than growing unboundedly on a 1GB box.
        let (tx, rx) = tokio::sync::mpsc::channel(32);

        tokio::spawn(async move {
            let mut acc = ToolCallAccumulator::default();
            let mut finish_reason = None;
            let mut stream = resp.bytes_stream().eventsource();

            while let Some(event) = stream.next().await {
                let event = match event {
                    Ok(e) => e,
                    Err(e) => {
                        let _ = tx.send(Err(anyhow::anyhow!("sse transport error: {e}"))).await;
                        return;
                    }
                };

                if event.data.trim() == "[DONE]" {
                    break;
                }

                let chunk: Chunk = match serde_json::from_str(&event.data) {
                    Ok(c) => c,
                    Err(e) => {
                        // A single malformed frame is not worth killing the turn over.
                        tracing::warn!(error = %e, data = %truncate(&event.data, 200), "skipping malformed SSE frame");
                        continue;
                    }
                };

                let Some(choice) = chunk.choices.into_iter().next() else { continue };

                if let Some(reason) = choice.finish_reason {
                    finish_reason = Some(reason);
                }

                if let Some(delta) = choice.delta {
                    // `reasoning` is intentionally ignored — never spoken, never shown.
                    if let Some(text) = delta.content
                        && !text.is_empty()
                        && tx.send(Ok(StreamItem::Token(text))).await.is_err()
                    {
                        return; // consumer dropped: cancel.
                    }
                    if let Some(calls) = delta.tool_calls {
                        acc.absorb(calls);
                    }
                }
            }

            let calls = acc.finish();
            let item = if calls.is_empty() {
                StreamItem::Done { finish_reason }
            } else {
                StreamItem::ToolCalls(calls)
            };
            let _ = tx.send(Ok(item)).await;
        });

        Ok(rx)
    }

    /// Non-streaming call, used off the hot path only (reflection extraction,
    /// news summarization, background research synthesis).
    pub async fn complete(&self, req: ChatRequest) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.body(&req, false);

        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .timeout(self.timeout)
            .json(&body)
            .send()
            .await
            .context("cerebras request failed")?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("cerebras returned {status}: {}", truncate(&text, 500));
        }

        let parsed: NonStreamResponse =
            serde_json::from_str(&text).context("decoding cerebras response")?;
        Ok(parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default())
    }

    pub fn effort_for(cfg_value: &str) -> Effort {
        Effort::parse(cfg_value)
    }
}

// ---------------------------------------------------------------------------
// Streaming tool-call reassembly
// ---------------------------------------------------------------------------

/// Reassembles fragmented tool-call deltas.
///
/// The wire protocol sends, per SSE frame, a sparse patch keyed by `index`:
/// frame 1 might carry `{index:0, id:"call_x", function:{name:"web_search", arguments:""}}`
/// and frames 2..n carry `{index:0, function:{arguments:"{\"qu"}}` etc. Only `index`
/// is reliably present on every fragment, so it is the join key.
#[derive(Default, Debug)]
pub struct ToolCallAccumulator {
    slots: Vec<Slot>,
}

#[derive(Default, Debug, Clone)]
struct Slot {
    id: String,
    name: String,
    arguments: String,
}

impl ToolCallAccumulator {
    pub fn absorb(&mut self, deltas: Vec<ToolCallDelta>) {
        for d in deltas {
            let idx = d.index.unwrap_or(0) as usize;
            if self.slots.len() <= idx {
                self.slots.resize(idx + 1, Slot::default());
            }
            let slot = &mut self.slots[idx];
            if let Some(id) = d.id
                && !id.is_empty()
            {
                slot.id = id;
            }
            if let Some(f) = d.function {
                if let Some(name) = f.name
                    && !name.is_empty()
                {
                    slot.name = name;
                }
                if let Some(args) = f.arguments {
                    slot.arguments.push_str(&args);
                }
            }
        }
    }

    /// Drop slots that never received a function name — those are protocol noise,
    /// not callable tools.
    pub fn finish(self) -> Vec<ToolCall> {
        self.slots
            .into_iter()
            .enumerate()
            .filter(|(_, s)| !s.name.is_empty())
            .map(|(i, s)| ToolCall {
                id: if s.id.is_empty() { format!("call_{i}") } else { s.id },
                name: s.name,
                arguments: s.arguments,
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCallDelta {
    #[serde(default)]
    pub index: Option<u32>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub struct FunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NonStreamResponse {
    #[serde(default)]
    choices: Vec<NonStreamChoice>,
}

#[derive(Debug, Deserialize)]
struct NonStreamChoice {
    message: NonStreamMessage,
}

#[derive(Debug, Deserialize)]
struct NonStreamMessage {
    #[serde(default)]
    content: Option<String>,
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Respect char boundaries so this never panics on UTF-8 payloads.
        let end = s.char_indices().map(|(i, _)| i).take_while(|i| *i <= max).last().unwrap_or(0);
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(index: u32, id: Option<&str>, name: Option<&str>, args: Option<&str>) -> ToolCallDelta {
        ToolCallDelta {
            index: Some(index),
            id: id.map(String::from),
            function: Some(FunctionDelta {
                name: name.map(String::from),
                arguments: args.map(String::from),
            }),
        }
    }

    #[test]
    fn reassembles_arguments_split_across_frames() {
        let mut acc = ToolCallAccumulator::default();
        acc.absorb(vec![delta(0, Some("call_a"), Some("web_search"), Some(""))]);
        acc.absorb(vec![delta(0, None, None, Some(r#"{"que"#))]);
        acc.absorb(vec![delta(0, None, None, Some(r#"ry":"tide"}"#))]);
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].args()["query"], "tide");
    }

    #[test]
    fn keys_on_index_not_arrival_order() {
        // Interleaved parallel tool calls: index is the only reliable join key.
        let mut acc = ToolCallAccumulator::default();
        acc.absorb(vec![
            delta(0, Some("a"), Some("web_search"), Some(r#"{"query":"#)),
            delta(1, Some("b"), Some("fetch_page"), Some(r#"{"url":"#)),
        ]);
        acc.absorb(vec![delta(1, None, None, Some(r#""http://x"}"#))]);
        acc.absorb(vec![delta(0, None, None, Some(r#""y"}"#))]);
        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].args()["query"], "y");
        assert_eq!(calls[1].name, "fetch_page");
        assert_eq!(calls[1].args()["url"], "http://x");
    }

    #[test]
    fn drops_slots_with_no_function_name() {
        let mut acc = ToolCallAccumulator::default();
        acc.absorb(vec![ToolCallDelta { index: Some(0), id: Some("x".into()), function: None }]);
        assert!(acc.finish().is_empty());
    }

    #[test]
    fn synthesizes_an_id_when_upstream_omits_one() {
        let mut acc = ToolCallAccumulator::default();
        acc.absorb(vec![delta(0, None, Some("news_briefing"), Some("{}"))]);
        let calls = acc.finish();
        assert_eq!(calls[0].id, "call_0");
    }

    #[test]
    fn truncate_is_utf8_safe() {
        let s = "日本語のテキストです".repeat(20);
        let t = truncate(&s, 10);
        assert!(t.len() <= 20);
    }

    #[test]
    fn body_always_sets_reasoning_effort_and_stream() {
        let client = CerebrasClient::new(
            reqwest::Client::new(),
            "https://api.cerebras.ai/v1/",
            "k",
            "gpt-oss-120b",
            Duration::from_secs(1),
        );
        let req = ChatRequest {
            messages: vec![super::super::ChatMessage::user("hi")],
            tools: vec![],
            effort: Effort::Low,
            max_tokens: 64,
            temperature: 0.5,
        };
        let b = client.body(&req, true);
        assert_eq!(b["reasoning_effort"], "low");
        assert_eq!(b["stream"], true);
        assert_eq!(b["model"], "gpt-oss-120b");
        // trailing slash on base_url must not produce a double slash
        assert_eq!(client.base_url, "https://api.cerebras.ai/v1");
        // no tools => no tool_choice key at all
        assert!(b.get("tool_choice").is_none());
    }
}
