//! `background_research` — the escape hatch from the 2-round interactive cap.
//!
//! When the model wants more than two tool rounds, or the user says "research this",
//! the orchestrator acknowledges in one line and hands the question here. This
//! worker runs up to 8 rounds entirely off the hot path, then speaks and records the
//! result.
//!
//! Two deliberate constraints:
//!
//! - The researcher gets only `web_search` and `fetch_page`. It cannot call
//!   `background_research`, so a job can never spawn more jobs.
//! - The finished answer is recorded as an ordinary **assistant message**, not
//!   written to memory directly. Facts from it are extracted later by the normal
//!   reflection pass. That keeps the §9.4 firewall intact: research output is
//!   ultimately derived from untrusted web text, so it must not take a shortcut
//!   into long-term memory.

use crate::llm::{ChatMessage, ChatRequest, Effort, StreamItem, ToolCall};
use crate::tools::{ToolContext, ToolOutput};
use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ResearchJob {
    pub question: String,
}

/// Something to say to the user out of band.
#[derive(Debug, Clone)]
pub struct Announcement {
    pub text: String,
    /// The question this answers, for context when speaking.
    pub about: String,
}

/// Tools available to the researcher — deliberately a subset.
fn research_schemas() -> Vec<crate::llm::ToolDef> {
    crate::tools::schemas()
        .into_iter()
        .filter(|t| matches!(t.function.name.as_str(), "web_search" | "fetch_page"))
        .collect()
}

pub struct ResearchWorker {
    ctx: ToolContext,
    system_prompt: Arc<String>,
    max_rounds: usize,
    timeout: Duration,
}

impl ResearchWorker {
    pub fn new(
        ctx: ToolContext,
        system_prompt: Arc<String>,
        max_rounds: usize,
        timeout_secs: u64,
    ) -> Self {
        Self {
            ctx,
            system_prompt,
            max_rounds,
            timeout: Duration::from_secs(timeout_secs),
        }
    }

    /// Drain the job queue forever. One job at a time: this is the lowest-priority
    /// work on a 1GB box and must never compete with a live turn.
    pub async fn run(
        self,
        mut jobs: tokio::sync::mpsc::Receiver<ResearchJob>,
        announce: tokio::sync::mpsc::Sender<Announcement>,
    ) {
        while let Some(job) = jobs.recv().await {
            let started = Instant::now();
            tracing::info!(question = %job.question, "background research started");

            let result = tokio::time::timeout(self.timeout, self.investigate(&job.question)).await;

            let text = match result {
                Ok(Ok(answer)) => answer,
                Ok(Err(e)) => {
                    tracing::warn!(error = ?e, "background research failed");
                    format!("I couldn't finish researching that: {e}")
                }
                Err(_) => {
                    tracing::warn!(timeout = ?self.timeout, "background research timed out");
                    "I ran out of time researching that one.".to_string()
                }
            };

            tracing::info!(
                ms = started.elapsed().as_millis(),
                "background research complete"
            );
            if announce
                .send(Announcement {
                    text,
                    about: job.question,
                })
                .await
                .is_err()
            {
                return; // gateway gone; shut down
            }
        }
    }

    /// Run the multi-round loop and return the synthesized answer.
    async fn investigate(&self, question: &str) -> Result<String> {
        let mut messages = vec![
            ChatMessage::system(self.system_prompt.as_str()),
            ChatMessage::user(question.to_string()),
        ];
        let tools = research_schemas();

        for round in 0..self.max_rounds {
            let req = ChatRequest {
                messages: messages.clone(),
                tools: tools.clone(),
                // Research is the one place the spec allows medium effort.
                effort: Effort::Medium,
                max_tokens: 2048,
                temperature: 0.5,
            };

            let (text, calls) = self.one_round(req).await?;

            if calls.is_empty() {
                if text.trim().is_empty() {
                    anyhow::bail!("the researcher returned an empty answer");
                }
                return Ok(text.trim().to_string());
            }

            tracing::debug!(round, calls = calls.len(), "research tool round");
            messages.push(ChatMessage::assistant_tool_calls(&calls));

            let results = crate::tools::execute_all(&self.ctx, &calls, question).await;
            for (call, out, _ms) in results {
                messages.push(ChatMessage::tool_result(&call.id, out.content));
            }
        }

        // Out of rounds: force a synthesis pass with no tools available.
        messages.push(ChatMessage::user(
            "You are out of research rounds. Answer now, in spoken prose, using only what you \
             have gathered. Say plainly if something remains unresolved.",
        ));
        let final_req = ChatRequest {
            messages,
            tools: vec![],
            effort: Effort::Medium,
            max_tokens: 1024,
            temperature: 0.5,
        };
        let answer = self.ctx.llm.complete(final_req).await?;
        Ok(answer.trim().to_string())
    }

    /// One streamed round, collected into (text, tool_calls).
    ///
    /// Streaming rather than `complete()` even off the hot path, so the round shares
    /// exactly one code path with the interactive orchestrator — including tool-call
    /// reassembly, which is the fiddly part.
    async fn one_round(&self, req: ChatRequest) -> Result<(String, Vec<ToolCall>)> {
        let mut rx = self.ctx.llm.stream(req).await?;
        let mut text = String::new();
        let mut calls = Vec::new();

        while let Some(item) = rx.recv().await {
            match item? {
                StreamItem::Token(t) => text.push_str(&t),
                StreamItem::ToolCalls(c) => {
                    calls = c;
                    break;
                }
                StreamItem::Done { .. } => break,
            }
        }
        Ok((text, calls))
    }
}

/// Keep the queue shallow: research is expensive and a backlog means the user asked
/// for more than the device should be chewing on at once.
pub const QUEUE_DEPTH: usize = 4;

/// Default acknowledgment spoken the moment a job is accepted.
pub fn acknowledgment(question: &str) -> ToolOutput {
    let _ = question;
    ToolOutput {
        content: "Research queued.".to_string(),
        ok: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn researcher_cannot_recurse() {
        let names: Vec<String> = research_schemas()
            .into_iter()
            .map(|t| t.function.name)
            .collect();
        assert_eq!(names, vec!["web_search", "fetch_page"]);
        assert!(
            !names.contains(&"background_research".to_string()),
            "a research job must never be able to spawn another"
        );
        assert!(
            !names.contains(&"music".to_string()),
            "researcher has no business changing playback"
        );
    }

    #[test]
    fn queue_is_shallow() {
        // Research is expensive; a deep backlog means the device is chewing on more
        // than it should. Guards against someone raising this casually.
        const _: () = assert!(QUEUE_DEPTH <= 8 && QUEUE_DEPTH >= 1);
    }
}
