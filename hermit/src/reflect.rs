//! The self-learning loop (spec §9).
//!
//! Three jobs, all off the hot path and all lowest priority:
//!
//! 1. **Nudges** — every 6 turns or 60 s idle, send the recent user/assistant turns
//!    to Cerebras with an extraction prompt and store what comes back as facts.
//! 2. **Skill distillation** — after a successful multi-step tool run, write a
//!    procedural DRAFT to `<data_dir>/skills-pending/` for operator review
//!    (quarantined; see [`ReflectionWorker::distill_skill`]).
//! 3. **Nightly consolidation** — summarize sessions, rewrite `core.md`, decay
//!    importance, prune. Keeps the prompt and the index bounded forever.
//!
//! # Firewall (spec §9.4, LOCKED)
//!
//! [`parse_extraction`] is the ONLY function in the crate that constructs a
//! [`ReflectionBatch`], and [`Store::apply_reflection`] is the only thing that
//! writes facts. Its input is the reflection model's own JSON, derived from a
//! transcript that structurally cannot contain tool output (see
//! [`Store::record_message`]). Raw web text therefore has no path into memory.

use crate::config::Config;
use crate::llm::{CerebrasClient, ChatMessage, ChatRequest, Effort};
use crate::memory::{CandidateFact, ReflectionBatch, Store, prompt::Layers};
use anyhow::{Context, Result, bail};
use std::sync::Arc;
use std::time::Duration;

/// Signals into the reflection worker.
#[derive(Debug, Clone)]
pub enum ReflectSignal {
    /// A user/assistant exchange completed.
    TurnCompleted,
    /// A multi-step tool run succeeded and is worth distilling into a skill.
    SkillCandidate {
        goal: String,
        tools_used: Vec<String>,
        answer: String,
    },
    /// Run consolidation now (the nightly timer, or `hermit consolidate`).
    Consolidate,
}

pub struct ReflectionWorker {
    pub llm: Arc<CerebrasClient>,
    pub store: Arc<Store>,
    pub layers: Arc<Layers>,
    pub cfg_rx: tokio::sync::watch::Receiver<Arc<Config>>,
    pub extract_prompt: Arc<String>,
    pub skill_prompt: Arc<String>,
    pub consolidate_prompt: Arc<String>,
}

impl ReflectionWorker {
    /// Main loop. Never blocks the hot path: every unit of work is awaited on its
    /// own and failures are logged, not propagated.
    pub async fn run(self, mut signals: tokio::sync::mpsc::Receiver<ReflectSignal>) {
        let cfg = self.cfg_rx.borrow().clone();
        let mut turns_since_nudge = 0usize;
        let mut last_reflected_id = self.store.max_message_id();
        let idle = Duration::from_secs(cfg.reflect.idle_secs.max(5));

        loop {
            let signal = tokio::time::timeout(idle, signals.recv()).await;

            let cfg = self.cfg_rx.borrow().clone();
            if !cfg.reflect.enabled {
                // Still drain, so the channel never backs up.
                if matches!(signal, Ok(None)) {
                    return;
                }
                continue;
            }

            match signal {
                Ok(None) => return, // senders gone
                Ok(Some(ReflectSignal::TurnCompleted)) => {
                    turns_since_nudge += 1;
                    if turns_since_nudge >= cfg.reflect.turns_per_nudge {
                        turns_since_nudge = 0;
                        last_reflected_id = self.nudge(&cfg, last_reflected_id).await;
                    }
                }
                Ok(Some(ReflectSignal::SkillCandidate {
                    goal,
                    tools_used,
                    answer,
                })) => {
                    if cfg.reflect.skill_creation
                        && let Err(e) = self.distill_skill(&cfg, &goal, &tools_used, &answer).await
                    {
                        tracing::warn!(error = ?e, "skill distillation failed");
                    }
                }
                Ok(Some(ReflectSignal::Consolidate)) => {
                    if let Err(e) = self.consolidate(&cfg).await {
                        tracing::error!(error = ?e, "consolidation failed");
                    }
                }
                Err(_) => {
                    // Idle timeout: reflect on whatever has accumulated.
                    if turns_since_nudge > 0 {
                        turns_since_nudge = 0;
                        last_reflected_id = self.nudge(&cfg, last_reflected_id).await;
                    }
                }
            }

            // Be a good citizen on a 4-core box: let anything else run first.
            tokio::task::yield_now().await;
        }
    }

    /// Extract facts from recent turns. Returns the new high-water message id.
    async fn nudge(&self, cfg: &Config, after_id: i64) -> i64 {
        let messages = self.store.messages_since(after_id, 40);
        if messages.is_empty() {
            return after_id;
        }
        let high_water = messages.last().map(|(id, _, _)| *id).unwrap_or(after_id);

        let transcript = messages
            .iter()
            .map(|(_, role, content)| format!("{role}: {content}"))
            .collect::<Vec<_>>()
            .join("\n");

        match self.extract(cfg, &transcript).await {
            Ok(batch) => {
                if batch.is_empty() {
                    tracing::debug!("reflection found nothing worth storing");
                } else {
                    match self
                        .store
                        .apply_reflection(&batch, cfg.memory.dedupe_similarity)
                    {
                        Ok(n) => tracing::info!(
                            proposed = batch.facts().len(),
                            stored = n,
                            "reflection complete"
                        ),
                        Err(e) => tracing::warn!(error = ?e, "storing reflection failed"),
                    }
                }
            }
            Err(e) => tracing::warn!(error = ?e, "fact extraction failed"),
        }
        high_water
    }

    async fn extract(&self, cfg: &Config, transcript: &str) -> Result<ReflectionBatch> {
        let req = ChatRequest {
            messages: vec![
                ChatMessage::system(self.extract_prompt.as_str()),
                ChatMessage::user(format!("Conversation:\n\n{transcript}")),
            ],
            tools: vec![],
            effort: Effort::Low,
            max_tokens: 800,
            temperature: 0.2,
        };
        let _ = cfg;
        let raw = self.llm.complete(req).await?;
        parse_extraction(&raw)
    }

    /// Write a procedural note describing a multi-step run that worked.
    ///
    /// # Quarantine (security)
    ///
    /// The draft transits the model's own output, which may echo instructions
    /// from a poisoned web page ("second-order prompt injection"). The memory
    /// firewall cannot see that path, so drafts are **quarantined**: written to
    /// `<data_dir>/skills-pending/`, which is NEVER indexed into recall. A
    /// human promotes a reviewed draft into `<config_dir>/skills/` (root-owned
    /// on a provisioned device) before it can ever enter a system prompt.
    /// `tests/skill_quarantine.rs` is the acceptance gate for this property.
    ///
    /// This also fixes a deploy mismatch: `<config_dir>` is deliberately
    /// read-only to the daemon on provisioned hardware, so writing drafts
    /// there silently failed. `<data_dir>` is the daemon's writable state dir.
    async fn distill_skill(
        &self,
        cfg: &Config,
        goal: &str,
        tools_used: &[String],
        answer: &str,
    ) -> Result<()> {
        let req = ChatRequest {
            messages: vec![
                ChatMessage::system(self.skill_prompt.as_str()),
                ChatMessage::user(format!(
                    "Goal: {goal}\nTools used, in order: {}\nOutcome: {}",
                    tools_used.join(", "),
                    crate::tools::clip(answer, 1200)
                )),
            ],
            tools: vec![],
            effort: Effort::Low,
            max_tokens: 500,
            temperature: 0.3,
        };
        let body = self.llm.complete(req).await?;
        if body.trim().is_empty() {
            bail!("skill distillation produced nothing");
        }

        let path = write_skill_draft(cfg, goal, &body)?;
        tracing::info!(
            path = %path.display(),
            "distilled a skill DRAFT (quarantined; review and move into {} to activate)",
            cfg.config_dir().join("skills").display()
        );

        // Deliberately NO reindex here: only operator-promoted skills in the
        // config dir are indexed (at boot and on file-watch). Model output must
        // never reach the system prompt without a human in the loop.
        Ok(())
    }

    /// Nightly: summarize sessions, rewrite core.md, decay, prune, vacuum.
    pub async fn consolidate(&self, cfg: &Config) -> Result<()> {
        tracing::info!("consolidation starting");

        // 1. Summarize any finished sessions.
        for (session_id, _started) in self.store.unsummarized_sessions(20) {
            let msgs = self.store.session_messages(session_id);
            if msgs.is_empty() {
                self.store.set_session_summary(session_id, "")?;
                continue;
            }
            let transcript = msgs
                .iter()
                .map(|(r, c)| format!("{r}: {c}"))
                .collect::<Vec<_>>()
                .join("\n");
            let req = ChatRequest {
                messages: vec![
                    ChatMessage::system(
                        "Summarize this conversation in at most three sentences. Record what the \
                         user wanted and what was decided. No preamble.",
                    ),
                    ChatMessage::user(crate::tools::clip(&transcript, 8000)),
                ],
                tools: vec![],
                effort: Effort::Low,
                max_tokens: 200,
                temperature: 0.3,
            };
            match self.llm.complete(req).await {
                Ok(s) => self.store.set_session_summary(session_id, s.trim())?,
                Err(e) => tracing::warn!(session_id, error = %e, "session summary failed"),
            }
        }

        // 2. Rewrite core.md from the highest-importance facts.
        let top = self.store.top_facts(60);
        if !top.is_empty() {
            let listing = top
                .iter()
                .map(|f| format!("- ({:.2}) {}", f.importance, f.text))
                .collect::<Vec<_>>()
                .join("\n");
            let req = ChatRequest {
                messages: vec![
                    ChatMessage::system(
                        self.consolidate_prompt
                            .replace("{token_cap}", &cfg.memory.core_token_cap.to_string()),
                    ),
                    ChatMessage::user(format!("Facts, with importance scores:\n{listing}")),
                ],
                tools: vec![],
                effort: Effort::Low,
                max_tokens: 900,
                temperature: 0.3,
            };
            match self.llm.complete(req).await {
                Ok(core) if !core.trim().is_empty() => {
                    // Layers::write_core enforces the 600-token cap regardless of
                    // what the model produced.
                    self.layers.write_core(self.store.data_dir(), core.trim())?;
                    tracing::info!(
                        tokens = crate::memory::approx_tokens(&self.layers.core()),
                        "core memory rewritten"
                    );
                }
                Ok(_) => tracing::warn!(
                    "consolidation returned an empty core memory; keeping the old one"
                ),
                Err(e) => tracing::warn!(error = %e, "core rewrite failed; keeping the old one"),
            }
        }

        // 3. Decay and prune.
        let (decayed, pruned) = self
            .store
            .decay_and_prune(cfg.memory.importance_decay, cfg.memory.prune_below)?;
        tracing::info!(
            decayed,
            pruned,
            remaining = self.store.fact_count(),
            "decay complete"
        );

        // 4. Reclaim space; the SD card is the scarcest resource here.
        if pruned > 0 {
            let _ = self.store.vacuum();
        }

        tracing::info!("consolidation finished");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Extraction parsing — the sole producer of ReflectionBatch
// ---------------------------------------------------------------------------

/// Parse the extraction model's strict-JSON output into a [`ReflectionBatch`].
///
/// Expected shape:
/// ```json
/// { "facts": [ { "text": "...", "tags": ["..."], "importance": 0.8 } ],
///   "updates": [ { "id": 12, "importance": 0.9 } ],
///   "retire": [ 7 ] }
/// ```
///
/// Tolerant of the two things models actually do wrong — wrapping JSON in a
/// markdown fence, and emitting prose around it — but not of arbitrary content:
/// anything that is not a well-formed object is rejected rather than guessed at.
pub fn parse_extraction(raw: &str) -> Result<ReflectionBatch> {
    let json = extract_json_object(raw)
        .ok_or_else(|| anyhow::anyhow!("no JSON object in extraction output"))?;
    let v: serde_json::Value =
        serde_json::from_str(&json).context("extraction output was not valid JSON")?;

    let mut facts = Vec::new();
    if let Some(arr) = v.get("facts").and_then(|f| f.as_array()) {
        for item in arr {
            let Some(text) = item.get("text").and_then(|t| t.as_str()) else {
                continue;
            };
            let text = text.trim();
            // Bound length: a "fact" the size of a web page is a sign something
            // went wrong upstream, and it would poison the prompt budget.
            if text.is_empty() || text.len() > 400 {
                continue;
            }
            let tags = item
                .get("tags")
                .and_then(|t| t.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .map(|s| s.trim().to_lowercase())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let importance = item
                .get("importance")
                .and_then(|i| i.as_f64())
                .unwrap_or(0.5)
                .clamp(0.0, 1.0);
            facts.push(CandidateFact {
                text: text.to_string(),
                tags,
                importance,
            });
        }
    }

    let importance_updates = v
        .get("updates")
        .and_then(|u| u.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|item| {
                    let id = item.get("id")?.as_i64()?;
                    let imp = item.get("importance")?.as_f64()?.clamp(0.0, 1.0);
                    Some((id, imp))
                })
                .collect()
        })
        .unwrap_or_default();

    let retire = v
        .get("retire")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default();

    Ok(ReflectionBatch {
        facts,
        importance_updates,
        retire,
        source: "reflection",
    })
}

/// Build a batch during consolidation, where the inputs are already-stored facts.
pub fn consolidation_batch(
    importance_updates: Vec<(i64, f64)>,
    retire: Vec<i64>,
) -> ReflectionBatch {
    ReflectionBatch {
        facts: Vec::new(),
        importance_updates,
        retire,
        source: "consolidation",
    }
}

/// Pull the first balanced `{...}` out of a string, ignoring braces inside strings.
fn extract_json_object(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let start = raw.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for i in start..bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(raw[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Where model-drafted skills wait for human review. Under the daemon's
/// writable data dir — never under config, which stays read-only to the
/// daemon on provisioned hardware, and never indexed into recall.
pub fn skill_quarantine_dir(cfg: &Config) -> std::path::PathBuf {
    cfg.paths.data_dir.join("skills-pending")
}

/// Persist a model-drafted skill into quarantine. The single write path for
/// drafts: everything it produces stays outside the recall index until an
/// operator moves it into `<config_dir>/skills/` by hand.
pub fn write_skill_draft(cfg: &Config, goal: &str, body: &str) -> Result<std::path::PathBuf> {
    let dir = skill_quarantine_dir(cfg);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", slugify(goal)));
    std::fs::write(&path, body.trim())
        .with_context(|| format!("writing skill draft {}", path.display()))?;
    Ok(path)
}

fn slugify(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    let trimmed = out.trim_matches('-').to_string();
    let slug = if trimmed.is_empty() {
        "skill".to_string()
    } else {
        trimmed
    };
    slug.chars().take(60).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_clean_extraction() {
        let raw =
            r#"{"facts":[{"text":"user's dog is named Ada","tags":["pets"],"importance":0.9}]}"#;
        let b = parse_extraction(raw).unwrap();
        assert_eq!(b.facts().len(), 1);
        assert_eq!(b.facts()[0].text, "user's dog is named Ada");
        assert_eq!(b.facts()[0].tags, vec!["pets"]);
        assert!((b.facts()[0].importance - 0.9).abs() < 1e-9);
    }

    #[test]
    fn tolerates_markdown_fences_and_surrounding_prose() {
        let raw = "Here is what I found:\n```json\n{\"facts\":[{\"text\":\"user drinks tea\"}]}\n```\nHope that helps!";
        let b = parse_extraction(raw).unwrap();
        assert_eq!(b.facts().len(), 1);
        assert_eq!(b.facts()[0].text, "user drinks tea");
        assert!(
            (b.facts()[0].importance - 0.5).abs() < 1e-9,
            "missing importance defaults to 0.5"
        );
    }

    #[test]
    fn handles_braces_inside_strings() {
        let raw = r#"{"facts":[{"text":"user uses the {braces} convention"}]}"#;
        let b = parse_extraction(raw).unwrap();
        assert_eq!(b.facts()[0].text, "user uses the {braces} convention");
    }

    #[test]
    fn rejects_non_json_output() {
        assert!(parse_extraction("I could not find anything.").is_err());
        assert!(parse_extraction("").is_err());
    }

    #[test]
    fn unbalanced_json_is_rejected_not_guessed() {
        assert!(parse_extraction(r#"{"facts":[{"text":"truncated"#).is_err());
    }

    #[test]
    fn empty_facts_array_is_a_valid_empty_batch() {
        let b = parse_extraction(r#"{"facts":[]}"#).unwrap();
        assert!(b.is_empty());
    }

    #[test]
    fn oversized_and_empty_facts_are_dropped() {
        let huge = "x".repeat(500);
        let raw = format!(
            r#"{{"facts":[{{"text":"{huge}"}},{{"text":"   "}},{{"text":"a real fact"}}]}}"#
        );
        let b = parse_extraction(&raw).unwrap();
        assert_eq!(b.facts().len(), 1);
        assert_eq!(b.facts()[0].text, "a real fact");
    }

    #[test]
    fn importance_is_clamped() {
        let raw = r#"{"facts":[{"text":"a","importance":7.5},{"text":"b","importance":-3}]}"#;
        let b = parse_extraction(raw).unwrap();
        assert!(
            b.facts()
                .iter()
                .all(|f| (0.0..=1.0).contains(&f.importance))
        );
    }

    #[test]
    fn updates_and_retire_are_parsed() {
        let raw = r#"{"facts":[],"updates":[{"id":3,"importance":0.95}],"retire":[7,9]}"#;
        let b = parse_extraction(raw).unwrap();
        assert_eq!(b.importance_updates, vec![(3, 0.95)]);
        assert_eq!(b.retire, vec![7, 9]);
        assert!(!b.is_empty());
    }

    #[test]
    fn malformed_entries_are_skipped_not_fatal() {
        let raw = r#"{"facts":[{"nottext":1},{"text":"kept"}],"updates":[{"id":"x"}]}"#;
        let b = parse_extraction(raw).unwrap();
        assert_eq!(b.facts().len(), 1);
        assert!(b.importance_updates.is_empty());
    }

    #[test]
    fn batches_are_tagged_with_their_provenance() {
        let b = parse_extraction(r#"{"facts":[{"text":"x"}]}"#).unwrap();
        assert_eq!(b.source, "reflection");
        assert_eq!(consolidation_batch(vec![], vec![]).source, "consolidation");
    }

    #[test]
    fn slugify_produces_safe_filenames() {
        assert_eq!(
            slugify("Check a flight's status!"),
            "check-a-flight-s-status"
        );
        assert_eq!(slugify("   "), "skill");
        assert_eq!(slugify("///"), "skill");
        assert!(slugify(&"long ".repeat(50)).len() <= 60);
    }
}
