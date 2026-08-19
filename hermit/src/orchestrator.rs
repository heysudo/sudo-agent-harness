//! The turn loop (spec §4.3).
//!
//! ```text
//!   recall + assemble  ->  stream from Cerebras  ->  chunker  ->  TTS
//!                              |
//!                              +-- tool calls: play a canned ack, run them ALL
//!                                  concurrently, feed results back, stream the answer
//! ```
//!
//! HARD CAP: two tool rounds. If the model still wants tools after that, the turn
//! is converted into a background_research job — one line spoken now, the real
//! answer delivered when it lands.

use crate::config::Config;
use crate::llm::{CerebrasClient, ChatMessage, ChatRequest, Effort, StreamItem, ToolCall};
use crate::memory::{Store, prompt::Layers};
use crate::metrics::TurnTimings;
use crate::speech::chunker::Chunker;
use crate::tools::{self, ToolContext};
use anyhow::Result;
use std::sync::Arc;
use std::time::Instant;

/// Everything the turn emits as it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    /// Raw text delta, for text clients.
    Token(String),
    /// A complete clause, ready to speak.
    SpeechChunk(String),
    /// Play a canned acknowledgment now — a tool round is starting.
    Ack,
    /// A tool round began (names, for logging/UI).
    ToolRound(Vec<String>),
    /// The turn finished; carries the full answer text.
    Final(String),
    Error(String),
}

pub type EventTx = tokio::sync::mpsc::UnboundedSender<TurnEvent>;

/// A speculative search fired before end-of-speech (spec §5).
pub struct Prefetch {
    pub query: String,
    pub handle: tokio::task::JoinHandle<Option<String>>,
}

impl Prefetch {
    /// Would this prefetch answer `query`?
    ///
    /// Requires high token overlap, not equality: STT finalization routinely edits
    /// the tail of a sentence ("weather in oslo" -> "weather in Oslo tomorrow"), and
    /// a strict match would throw away nearly every prefetch that was actually
    /// useful.
    pub fn matches(&self, query: &str) -> bool {
        let a = word_set(&self.query);
        let b = word_set(query);
        if a.is_empty() || b.is_empty() {
            return false;
        }
        let overlap = a.intersection(&b).count() as f64;
        overlap / (a.len().min(b.len()) as f64) >= 0.7
    }
}

fn word_set(s: &str) -> std::collections::BTreeSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(|w| w.to_lowercase())
        .collect()
}

pub struct Orchestrator {
    pub llm: Arc<CerebrasClient>,
    pub tools: ToolContext,
    pub store: Arc<Store>,
    pub layers: Arc<Layers>,
}

impl Orchestrator {
    /// Run one turn end to end.
    ///
    /// Emits events as they happen and returns the full answer text.
    pub async fn run_turn(
        &self,
        cfg: &Config,
        utterance: &str,
        events: &EventTx,
        timings: &mut TurnTimings,
        prefetch: Option<Prefetch>,
    ) -> Result<String> {
        // ---- recall + assemble (local overhead, <=15ms budget) ----------
        let (assembled, recall_ms, assemble_ms) = crate::memory::prompt::recall_and_assemble(
            &self.store,
            &self.layers,
            utterance,
            cfg.memory.recall_facts,
            cfg.memory.recall_skills,
            cfg.memory.history_turns,
            PREFIX_TOKEN_BUDGET,
        );
        timings.recall_ms = Some(recall_ms);
        timings.assemble_ms = Some(assemble_ms);

        let is_research = crate::router::is_research(utterance);
        let effort = if is_research {
            Effort::parse(&cfg.llm.reasoning_effort_research)
        } else {
            Effort::parse(&cfg.llm.reasoning_effort_default)
        };

        let mut messages = assembled.messages;
        let mut chunker = Chunker::new();
        let mut answer = String::new();
        let mut prefetch = prefetch;
        timings.prefetch_fired = prefetch.is_some();

        for round in 0..=cfg.llm.max_tool_rounds {
            let last_round = round == cfg.llm.max_tool_rounds;

            let req = ChatRequest {
                messages: messages.clone(),
                // On the final round the model must answer, not call more tools.
                tools: if last_round { vec![] } else { tools::schemas() },
                effort,
                max_tokens: cfg.llm.max_tokens,
                temperature: cfg.llm.temperature,
            };

            let calls = self
                .stream_round(req, &mut chunker, &mut answer, events, timings)
                .await?;

            if calls.is_empty() {
                break;
            }

            // The final round is offered no tools, but a model can still emit tool
            // calls anyway. Executing them would silently turn a 2-round cap into a
            // 3-round one, so they are dropped here. This is what makes the cap hard
            // rather than advisory.
            if last_round {
                tracing::warn!(
                    requested = ?calls.iter().map(|c| &c.name).collect::<Vec<_>>(),
                    "model requested tools on the final round; ignoring (2-round cap, spec §4.3)"
                );
                break;
            }

            // A tool round is starting. Speak whatever partial text the model
            // produced ("let me check that") if there is any; otherwise play a
            // canned acknowledgment so the user hears something immediately.
            match chunker.flush() {
                Some(text) => {
                    let _ = events.send(TurnEvent::SpeechChunk(text));
                }
                None => {
                    let _ = events.send(TurnEvent::Ack);
                }
            }

            let names: Vec<String> = calls.iter().map(|c| c.name.clone()).collect();
            let _ = events.send(TurnEvent::ToolRound(names));
            timings.tool_rounds = round + 1;

            let results = self
                .run_tools(&calls, utterance, &mut prefetch, timings)
                .await;

            messages.push(ChatMessage::assistant_tool_calls(&calls));
            for (call, content) in results {
                messages.push(ChatMessage::tool_result(&call.id, content));
            }
        }

        if let Some(tail) = chunker.flush() {
            let _ = events.send(TurnEvent::SpeechChunk(tail));
        }

        let final_text = answer.trim().to_string();
        let _ = events.send(TurnEvent::Final(final_text.clone()));
        Ok(final_text)
    }

    /// Stream one round, forwarding tokens. Returns any tool calls the model made.
    async fn stream_round(
        &self,
        req: ChatRequest,
        chunker: &mut Chunker,
        answer: &mut String,
        events: &EventTx,
        timings: &mut TurnTimings,
    ) -> Result<Vec<ToolCall>> {
        let started = Instant::now();
        let mut rx = self.llm.stream(req).await?;
        let mut first_token = true;

        loop {
            // Race the stream against the chunker's 250 ms first-chunk deadline, so
            // a slow generation still starts speaking on time.
            let deadline = chunker.deadline();

            let item = match deadline {
                Some(when) => {
                    tokio::select! {
                        biased;
                        item = rx.recv() => item,
                        _ = tokio::time::sleep_until(when.into()) => {
                            if let Some(chunk) = chunker.on_deadline() {
                                let _ = events.send(TurnEvent::SpeechChunk(chunk));
                            }
                            continue;
                        }
                    }
                }
                None => rx.recv().await,
            };

            let Some(item) = item else { break };

            match item? {
                StreamItem::Token(t) => {
                    if first_token {
                        timings
                            .ttft_ms
                            .get_or_insert(crate::metrics::ms_since(started));
                        first_token = false;
                    }
                    answer.push_str(&t);
                    let _ = events.send(TurnEvent::Token(t.clone()));
                    if let Some(chunk) = chunker.push(&t) {
                        let _ = events.send(TurnEvent::SpeechChunk(chunk));
                    }
                }
                StreamItem::ToolCalls(calls) => return Ok(calls),
                StreamItem::Done { .. } => break,
            }
        }
        Ok(Vec::new())
    }

    /// Execute a round's tool calls concurrently, using the prefetch when it fits.
    async fn run_tools(
        &self,
        calls: &[ToolCall],
        objective: &str,
        prefetch: &mut Option<Prefetch>,
        timings: &mut TurnTimings,
    ) -> Vec<(ToolCall, String)> {
        // If the model's search matches the speculative one already in flight, take
        // that result and drop the call from the concurrent batch.
        let mut prefetched: Option<(ToolCall, String)> = None;
        let mut remaining: Vec<ToolCall> = Vec::with_capacity(calls.len());

        for call in calls {
            let is_search = call.name == "web_search";
            let query = call
                .args()
                .get("query")
                .and_then(|q| q.as_str())
                .unwrap_or("")
                .to_string();

            if is_search
                && prefetched.is_none()
                && let Some(p) = prefetch.as_ref()
                && p.matches(&query)
            {
                let p = prefetch.take().expect("checked above");
                match p.handle.await {
                    Ok(Some(content)) => {
                        tracing::debug!(query = %query, "speculative prefetch hit");
                        timings.prefetch_hit = true;
                        timings.tool_ms.push(("web_search(prefetched)".into(), 0.0));
                        prefetched = Some((call.clone(), content));
                        continue;
                    }
                    Ok(None) | Err(_) => {
                        tracing::debug!("prefetch produced nothing; running the search normally");
                    }
                }
            }
            remaining.push(call.clone());
        }

        // Anything still in flight and unused is cancelled — it cost $0.001.
        if let Some(p) = prefetch.take() {
            p.handle.abort();
        }

        let mut out: Vec<(ToolCall, String)> = Vec::with_capacity(calls.len());
        if let Some(hit) = prefetched {
            out.push(hit);
        }

        if !remaining.is_empty() {
            let results = tools::execute_all(&self.tools, &remaining, objective).await;
            for (call, output, ms) in results {
                timings.tool_ms.push((call.name.clone(), ms));
                out.push((call, output.content));
            }
        }

        // Restore the model's original call order so tool_call_ids line up
        // predictably in the transcript.
        out.sort_by_key(|(c, _)| {
            calls
                .iter()
                .position(|x| x.id == c.id)
                .unwrap_or(usize::MAX)
        });
        out
    }
}

/// Budget for the stable prompt prefix (spec §5).
pub const PREFIX_TOKEN_BUDGET: usize = 1_200;

/// Fire a speculative search for an in-progress transcript.
///
/// Returns `None` when the transcript does not look like a lookup. The returned
/// task can be aborted for free if the final transcript turns out different.
pub fn spawn_prefetch(ctx: &ToolContext, interim: &str) -> Option<Prefetch> {
    if !crate::router::should_prefetch(interim) {
        return None;
    }
    let client = ctx.search.clone()?;
    let query = interim.trim().to_string();
    let q = query.clone();
    let max_results = ctx.cfg.search.max_results as usize;

    let handle = tokio::spawn(async move {
        match client.search(&q, Some(&q)).await {
            Ok(resp) => Some(tools::search::format_for_model(&resp, max_results)),
            Err(e) => {
                tracing::debug!(error = %e, "speculative prefetch failed");
                None
            }
        }
    });
    tracing::debug!(query = %query, "speculative prefetch fired");
    Some(Prefetch { query, handle })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefetch_for(query: &str) -> Prefetch {
        Prefetch {
            query: query.to_string(),
            handle: tokio::spawn(async { None }),
        }
    }

    #[tokio::test]
    async fn prefetch_matches_a_lightly_edited_transcript() {
        let p = prefetch_for("what is the weather in oslo");
        assert!(
            p.matches("what is the weather in Oslo"),
            "casing must not matter"
        );
        assert!(
            p.matches("what is the weather in oslo tomorrow"),
            "a trailing word is normal"
        );
    }

    #[tokio::test]
    async fn prefetch_rejects_an_unrelated_query() {
        let p = prefetch_for("what is the weather in oslo");
        assert!(!p.matches("who won the cup final"));
        assert!(!p.matches("price of copper today"));
    }

    #[tokio::test]
    async fn prefetch_rejects_empty_queries() {
        let p = prefetch_for("what is the weather in oslo");
        assert!(!p.matches(""));
        assert!(!p.matches("a of"));
    }

    #[test]
    fn word_set_drops_short_filler() {
        let s = word_set("what is the price of it");
        assert!(s.contains("what"));
        assert!(s.contains("price"));
        assert!(!s.contains("is"), "two-letter words carry no signal");
        assert!(!s.contains("of"));
    }

    #[test]
    fn prefix_budget_matches_the_spec() {
        assert_eq!(PREFIX_TOKEN_BUDGET, 1_200);
    }

    #[test]
    fn research_queries_select_medium_effort() {
        let cfg = Config::default();
        let effort = if crate::router::is_research("do a deep dive on tidal power") {
            Effort::parse(&cfg.llm.reasoning_effort_research)
        } else {
            Effort::parse(&cfg.llm.reasoning_effort_default)
        };
        assert_eq!(effort, Effort::Medium);

        let effort = if crate::router::is_research("what's the capital of Peru") {
            Effort::parse(&cfg.llm.reasoning_effort_research)
        } else {
            Effort::parse(&cfg.llm.reasoning_effort_default)
        };
        assert_eq!(
            effort,
            Effort::Low,
            "ordinary chat must stay on the fast path"
        );
    }
}
