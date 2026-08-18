//! Four-layer memory (spec §9).
//!
//! ```text
//!   L1  identity.md   static persona + directives      always in prompt
//!   L2  core.md       distilled user model, <=600 tok  always in prompt
//!   L3  archive       facts/sessions/messages + FTS5   BM25 top-N per turn
//!   L4  skills/*.md   procedural learnings             BM25 top-N per turn
//! ```
//!
//! # The firewall (spec §9.4, LOCKED)
//!
//! Raw web pages, search excerpts and tool output are NEVER written to memory.
//! This is the defense against a poisoned web page persisting an instruction into
//! the user's long-term memory, where it would be replayed into every future
//! prompt.
//!
//! It is enforced structurally, not by convention:
//!
//! - [`Store`] exposes no public "insert this text as a fact" method. The only
//!   write path for facts is [`Store::apply_reflection`], which accepts a
//!   [`ReflectionBatch`] — a type that can only be produced by
//!   [`crate::reflect::parse_extraction`] from the reflection model's strict JSON
//!   output, or by nightly consolidation over already-stored facts.
//! - [`Store::record_message`] rejects any role other than user/assistant, so tool
//!   results never even reach the messages table (and therefore never reach the
//!   reflection prompt, which is built from that table).
//!
//! `tests/memory_firewall.rs` asserts both properties.

pub mod prompt;
pub mod schema;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

/// A fact as stored in L3.
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub id: i64,
    pub text: String,
    pub tags: String,
    pub importance: f64,
    pub pinned: bool,
    pub source: String,
}

/// A skill file (L4), indexed for retrieval.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub goal: String,
    pub body: String,
    pub path: PathBuf,
}

/// A candidate fact proposed by the reflection model.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateFact {
    pub text: String,
    pub tags: Vec<String>,
    pub importance: f64,
}

/// The ONLY thing that can write facts.
///
/// Deliberately has no public constructor taking free text — see the firewall note
/// above. Build one via [`crate::reflect::parse_extraction`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReflectionBatch {
    pub(crate) facts: Vec<CandidateFact>,
    /// `(fact_id, new_importance)` adjustments to existing facts.
    pub(crate) importance_updates: Vec<(i64, f64)>,
    /// Facts the model judged obsolete.
    pub(crate) retire: Vec<i64>,
    pub(crate) source: &'static str,
}

impl ReflectionBatch {
    pub fn facts(&self) -> &[CandidateFact] {
        &self.facts
    }
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty() && self.importance_updates.is_empty() && self.retire.is_empty()
    }
}

/// Result of a per-turn recall.
#[derive(Debug, Clone, Default)]
pub struct Recall {
    pub facts: Vec<Fact>,
    pub skills: Vec<Skill>,
    pub elapsed_ms: f64,
}

pub struct Store {
    conn: Mutex<Connection>,
    data_dir: PathBuf,
    session_id: i64,
}

impl Store {
    /// Open (creating if needed) the database at `data_dir/hermit.db`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;
        let db_path = data_dir.join("hermit.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;

        conn.execute_batch(schema::PRAGMAS).context("applying pragmas")?;
        Self::assert_fts5(&conn)?;
        Self::migrate(&conn)?;

        let now = now_ts();
        conn.execute("INSERT INTO sessions (started_at) VALUES (?1)", params![now])?;
        let session_id = conn.last_insert_rowid();

        Ok(Self { conn: Mutex::new(conn), data_dir: data_dir.to_path_buf(), session_id })
    }

    /// In-memory store for tests.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        // WAL is meaningless for :memory:; the rest still applies.
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA temp_store = MEMORY;")?;
        Self::assert_fts5(&conn)?;
        Self::migrate(&conn)?;
        conn.execute("INSERT INTO sessions (started_at) VALUES (?1)", params![now_ts()])?;
        let session_id = conn.last_insert_rowid();
        Ok(Self {
            conn: Mutex::new(conn),
            data_dir: std::env::temp_dir(),
            session_id,
        })
    }

    /// Fail loudly at boot rather than mysteriously at first recall.
    fn assert_fts5(conn: &Connection) -> Result<()> {
        conn.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS _fts5_probe USING fts5(x); DROP TABLE _fts5_probe;")
            .context(
                "SQLite was built without FTS5. Rebuild with the rusqlite `bundled` feature \
                 (which enables SQLITE_ENABLE_FTS5) — recall depends on it.",
            )?;
        Ok(())
    }

    fn migrate(conn: &Connection) -> Result<()> {
        let version: i64 =
            conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        for (i, sql) in schema::MIGRATIONS.iter().enumerate() {
            let target = i as i64 + 1;
            if version < target {
                conn.execute_batch(sql)
                    .with_context(|| format!("applying migration {target}"))?;
                conn.pragma_update(None, "user_version", target)?;
                tracing::info!(version = target, "applied schema migration");
            }
        }
        Ok(())
    }

    pub fn session_id(&self) -> i64 {
        self.session_id
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    // -----------------------------------------------------------------
    // Recall (hot path — target <5 ms)
    // -----------------------------------------------------------------

    /// BM25 top-N facts and skills for the given utterance.
    ///
    /// Synchronous on purpose: at this corpus size the query costs a fraction of a
    /// millisecond and a `spawn_blocking` hop would cost more than the work. The
    /// lock is never held across an await because this function is not async.
    pub fn recall(&self, utterance: &str, n_facts: usize, n_skills: usize) -> Recall {
        let started = Instant::now();
        let Some(query) = fts_query(utterance) else {
            return Recall { elapsed_ms: crate::metrics::ms_since(started), ..Default::default() };
        };

        let conn = match self.conn.lock() {
            Ok(c) => c,
            Err(poisoned) => poisoned.into_inner(),
        };

        let facts = query_facts(&conn, &query, n_facts).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "fact recall failed");
            Vec::new()
        });
        let skills = query_skills(&conn, &query, n_skills).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "skill recall failed");
            Vec::new()
        });

        // Touch last_used_at so consolidation can decay unused facts faster.
        if !facts.is_empty() {
            let ids: Vec<String> = facts.iter().map(|f| f.id.to_string()).collect();
            let sql = format!(
                "UPDATE facts SET last_used_at = ?1 WHERE id IN ({})",
                ids.join(",")
            );
            let _ = conn.execute(&sql, params![now_ts()]);
        }

        Recall { facts, skills, elapsed_ms: crate::metrics::ms_since(started) }
    }

    // -----------------------------------------------------------------
    // Conversation history
    // -----------------------------------------------------------------

    /// Persist one conversational turn.
    ///
    /// FIREWALL: rejects any role but user/assistant. Tool output must not be
    /// stored, because the reflection prompt is built from this table and storing
    /// tool output would route untrusted web text into the fact extractor.
    pub fn record_message(&self, role: &str, content: &str) -> Result<()> {
        anyhow::ensure!(
            matches!(role, "user" | "assistant"),
            "refusing to persist role {role:?}: only user/assistant turns are stored (memory firewall, spec §9.4)"
        );
        if content.trim().is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "INSERT INTO messages (session_id, role, content, ts) VALUES (?1, ?2, ?3, ?4)",
            params![self.session_id, role, content, now_ts()],
        )?;
        Ok(())
    }

    /// Recent turns for the live context window, oldest first.
    pub fn recent_messages(&self, limit: usize) -> Vec<(String, String)> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let mut stmt = match conn.prepare(
            "SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "recent_messages prepare failed");
                return Vec::new();
            }
        };
        let rows = stmt
            .query_map(params![self.session_id, limit as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .and_then(|it| it.collect::<rusqlite::Result<Vec<_>>>())
            .unwrap_or_default();
        rows.into_iter().rev().collect()
    }

    /// Turns since the last reflection nudge, for the extraction prompt.
    /// Tool output is structurally absent (see `record_message`).
    pub fn messages_since(&self, after_id: i64, limit: usize) -> Vec<(i64, String, String)> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, role, content FROM messages WHERE id > ?1 ORDER BY id ASC LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![after_id, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })
        .and_then(|it| it.collect::<rusqlite::Result<Vec<_>>>())
        .unwrap_or_default()
    }

    pub fn max_message_id(&self) -> i64 {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |r| r.get(0))
            .unwrap_or(0)
    }

    // -----------------------------------------------------------------
    // Writes — reflection channel only
    // -----------------------------------------------------------------

    /// Apply a reflection batch. The single write path for facts.
    ///
    /// Returns the number of facts actually inserted (duplicates are dropped).
    pub fn apply_reflection(&self, batch: &ReflectionBatch, dedupe_similarity: f64) -> Result<usize> {
        if batch.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn.transaction()?;
        let now = now_ts();
        let mut inserted = 0usize;

        for cand in &batch.facts {
            let text = cand.text.trim();
            if text.is_empty() {
                continue;
            }
            if is_duplicate(&tx, text, dedupe_similarity)? {
                tracing::debug!(fact = %text, "reflection produced a near-duplicate; skipping");
                continue;
            }
            tx.execute(
                "INSERT INTO facts (text, tags, created_at, last_used_at, importance, pinned, source)
                 VALUES (?1, ?2, ?3, ?3, ?4, 0, ?5)",
                params![
                    text,
                    cand.tags.join(","),
                    now,
                    cand.importance.clamp(0.0, 1.0),
                    batch.source,
                ],
            )?;
            inserted += 1;
        }

        for (id, importance) in &batch.importance_updates {
            tx.execute(
                "UPDATE facts SET importance = ?1 WHERE id = ?2",
                params![importance.clamp(0.0, 1.0), id],
            )?;
        }
        for id in &batch.retire {
            tx.execute("DELETE FROM facts WHERE id = ?1 AND pinned = 0", params![id])?;
        }

        tx.commit()?;
        Ok(inserted)
    }

    // -----------------------------------------------------------------
    // Skills (L4)
    // -----------------------------------------------------------------

    /// Rebuild the skills index from `dir`. Cheap; run at boot and on file change.
    pub fn reindex_skills(&self, dir: &Path) -> Result<usize> {
        let mut conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM skills_fts", [])?;

        let mut count = 0usize;
        if dir.is_dir() {
            for entry in std::fs::read_dir(dir)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("md") {
                    continue;
                }
                let body = match std::fs::read_to_string(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "unreadable skill file");
                        continue;
                    }
                };
                let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("skill").to_string();
                let goal = extract_goal(&body);
                tx.execute(
                    "INSERT INTO skills_fts (name, goal, body, path) VALUES (?1, ?2, ?3, ?4)",
                    params![name, goal, body, path.to_string_lossy()],
                )?;
                count += 1;
            }
        }
        tx.commit()?;
        tracing::info!(count, dir = %dir.display(), "skills reindexed");
        Ok(count)
    }

    // -----------------------------------------------------------------
    // Consolidation (nightly)
    // -----------------------------------------------------------------

    /// Top facts by importance, used to rewrite core.md.
    pub fn top_facts(&self, limit: usize) -> Vec<Fact> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, text, tags, importance, pinned, source FROM facts
             ORDER BY pinned DESC, importance DESC, last_used_at DESC LIMIT ?1",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![limit as i64], row_to_fact)
            .and_then(|it| it.collect::<rusqlite::Result<Vec<_>>>())
            .unwrap_or_default()
    }

    /// Decay importance and prune. Returns `(decayed, pruned)`.
    pub fn decay_and_prune(&self, decay: f64, floor: f64) -> Result<(usize, usize)> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let decayed = conn.execute(
            "UPDATE facts SET importance = importance * ?1 WHERE pinned = 0",
            params![decay],
        )?;
        let pruned = conn.execute(
            "DELETE FROM facts WHERE pinned = 0 AND importance < ?1",
            params![floor],
        )?;
        Ok((decayed, pruned))
    }

    pub fn fact_count(&self) -> i64 {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.query_row("SELECT COUNT(*) FROM facts", [], |r| r.get(0)).unwrap_or(0)
    }

    /// Sessions that have not been summarized yet.
    pub fn unsummarized_sessions(&self, limit: usize) -> Vec<(i64, i64)> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let Ok(mut stmt) = conn.prepare(
            "SELECT id, started_at FROM sessions WHERE summary IS NULL AND id != ?1 ORDER BY id ASC LIMIT ?2",
        ) else {
            return Vec::new();
        };
        stmt.query_map(params![self.session_id, limit as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })
        .and_then(|it| it.collect::<rusqlite::Result<Vec<_>>>())
        .unwrap_or_default()
    }

    pub fn session_messages(&self, session_id: i64) -> Vec<(String, String)> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let Ok(mut stmt) = conn
            .prepare("SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY id ASC")
        else {
            return Vec::new();
        };
        stmt.query_map(params![session_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .and_then(|it| it.collect::<rusqlite::Result<Vec<_>>>())
        .unwrap_or_default()
    }

    pub fn set_session_summary(&self, session_id: i64, summary: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "UPDATE sessions SET summary = ?1, ended_at = ?2 WHERE id = ?3",
            params![summary, now_ts(), session_id],
        )?;
        Ok(())
    }

    /// Reclaim SD-card space after a prune. Cheap on a database this size.
    pub fn vacuum(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        Ok(())
    }

    /// Pin a fact so decay and pruning never touch it.
    pub fn set_pinned(&self, id: i64, pinned: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "UPDATE facts SET pinned = ?1 WHERE id = ?2",
            params![pinned as i64, id],
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Query helpers
// ---------------------------------------------------------------------------

fn row_to_fact(r: &rusqlite::Row<'_>) -> rusqlite::Result<Fact> {
    Ok(Fact {
        id: r.get(0)?,
        text: r.get(1)?,
        tags: r.get(2)?,
        importance: r.get(3)?,
        pinned: r.get::<_, i64>(4)? != 0,
        source: r.get(5)?,
    })
}

fn query_facts(conn: &Connection, query: &str, n: usize) -> Result<Vec<Fact>> {
    // bm25() returns a negative score where more negative is a better match.
    // Pinned facts sort first; importance breaks ties among similar matches so a
    // durable preference outranks an incidental mention.
    let mut stmt = conn.prepare_cached(
        "SELECT f.id, f.text, f.tags, f.importance, f.pinned, f.source
         FROM facts_fts ft
         JOIN facts f ON f.id = ft.rowid
         WHERE facts_fts MATCH ?1
         ORDER BY f.pinned DESC, (bm25(facts_fts) - f.importance) ASC
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![query, n as i64], row_to_fact)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn query_skills(conn: &Connection, query: &str, n: usize) -> Result<Vec<Skill>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name, goal, body, path FROM skills_fts
         WHERE skills_fts MATCH ?1
         ORDER BY bm25(skills_fts) ASC LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![query, n as i64], |r| {
            Ok(Skill {
                name: r.get(0)?,
                goal: r.get(1)?,
                body: r.get(2)?,
                path: PathBuf::from(r.get::<_, String>(3)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Is this candidate fact already in the archive?
///
/// Two stages, because neither alone is right:
///
/// 1. FTS5 narrows thousands of facts to a handful of candidates cheaply.
/// 2. Token-set **containment** decides. A raw bm25() threshold cannot be used here:
///    bm25 scores depend on corpus size and term rarity, so the same pair of
///    sentences scores near 0 in a 1-fact archive and -12 in a 5,000-fact one.
///    Any fixed cutoff would therefore dedupe nothing on a fresh device and
///    everything on a mature one. Containment is corpus-independent and reads as a
///    plain fraction.
///
/// Containment (`|A∩B| / min(|A|,|B|)`) rather than Jaccard, so that a terse restatement
/// of a longer stored fact still counts as a duplicate.
fn is_duplicate(conn: &Connection, text: &str, threshold: f64) -> Result<bool> {
    let Some(query) = fts_query(text) else {
        return Ok(false);
    };
    let candidate = token_set(text);
    if candidate.is_empty() {
        return Ok(false);
    }

    let mut stmt = conn.prepare_cached(
        "SELECT f.text FROM facts_fts ft JOIN facts f ON f.id = ft.rowid
         WHERE facts_fts MATCH ?1 ORDER BY bm25(facts_fts) ASC LIMIT 8",
    )?;
    let rows = stmt
        .query_map(params![query], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    for existing in rows {
        if containment(&candidate, &token_set(&existing)) >= threshold {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Content tokens of a string, using the same normalization as [`fts_query`].
fn token_set(s: &str) -> std::collections::BTreeSet<String> {
    match fts_query(s) {
        Some(q) => q
            .split(" OR ")
            .map(|t| t.trim_matches('"').to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        None => Default::default(),
    }
}

fn containment(
    a: &std::collections::BTreeSet<String>,
    b: &std::collections::BTreeSet<String>,
) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let overlap = a.intersection(b).count() as f64;
    overlap / (a.len().min(b.len()) as f64)
}

/// Turn arbitrary user text into a safe FTS5 MATCH expression.
///
/// This matters: FTS5 treats `"`, `*`, `:`, `^`, `-`, `AND`/`OR`/`NOT` as syntax, so
/// passing a raw utterance straight to MATCH throws a parse error on perfectly
/// ordinary questions ("what's the *best* option?"). Each token is extracted as
/// alphanumerics only and double-quoted, then OR-ed.
///
/// Returns `None` when nothing usable remains, so the caller can skip the query.
pub fn fts_query(input: &str) -> Option<String> {
    const STOP: &[&str] = &[
        "the", "a", "an", "and", "or", "but", "is", "are", "was", "were", "be", "been",
        "to", "of", "in", "on", "at", "for", "with", "my", "me", "i", "you", "it", "its",
        "that", "this", "do", "does", "did", "what", "whats", "how", "can", "could",
        "please", "would", "should", "am", "if", "so", "as", "by", "from",
    ];

    let mut tokens: Vec<String> = Vec::new();
    for raw in input.split(|c: char| !c.is_alphanumeric()) {
        let t = raw.trim().to_lowercase();
        if t.len() < 2 || STOP.contains(&t.as_str()) {
            continue;
        }
        // FTS5 tokens must not start with a digit-only form that looks like a column
        // filter; quoting handles all of it.
        if !tokens.contains(&t) {
            tokens.push(t);
        }
        if tokens.len() >= 24 {
            break; // bound the query cost
        }
    }
    if tokens.is_empty() {
        return None;
    }
    Some(
        tokens
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

/// Pull the `goal:` line (or first heading) out of a skill markdown file.
fn extract_goal(body: &str) -> String {
    for line in body.lines().take(20) {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("goal:").or_else(|| l.strip_prefix("Goal:")) {
            return rest.trim().to_string();
        }
        if let Some(rest) = l.strip_prefix("# ") {
            return rest.trim().to_string();
        }
    }
    body.lines().next().unwrap_or("").trim().to_string()
}

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Rough token estimate. Used only to enforce the core.md and prefix caps, where
/// being approximately right and free beats being exact and expensive.
///
/// Calibrated slightly conservative (over-estimates) so a cap is never blown.
pub fn approx_tokens(s: &str) -> usize {
    let chars = s.chars().count();
    let words = s.split_whitespace().count();
    // English prose lands near 4 chars/token; word count is a good floor for
    // punctuation-heavy text. Take the larger so we never under-count.
    chars.div_ceil(4).max(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(facts: Vec<(&str, f64)>) -> ReflectionBatch {
        ReflectionBatch {
            facts: facts
                .into_iter()
                .map(|(t, i)| CandidateFact {
                    text: t.to_string(),
                    tags: vec!["test".into()],
                    importance: i,
                })
                .collect(),
            importance_updates: vec![],
            retire: vec![],
            source: "reflection",
        }
    }

    #[test]
    fn fts_query_strips_operators_and_stopwords() {
        let q = fts_query("what's the *best* option for AND/OR queries?").unwrap();
        assert!(!q.contains('*'));
        assert!(q.contains("\"best\""));
        assert!(q.contains("\"option\""));
        // "what"/"the"/"for" are stopwords
        assert!(!q.contains("\"the\""));
    }

    #[test]
    fn fts_query_returns_none_for_pure_noise() {
        assert!(fts_query("?!  ... ").is_none());
        assert!(fts_query("").is_none());
        assert!(fts_query("a the of").is_none());
    }

    #[test]
    fn hostile_input_does_not_break_recall() {
        let s = Store::open_in_memory().unwrap();
        s.apply_reflection(&batch(vec![("user likes strong coffee", 0.8)]), 0.8).unwrap();
        // Every one of these would be an FTS5 syntax error if passed raw.
        for hostile in [
            "\" OR 1=1 --",
            "NEAR/2 foo",
            "col:value AND *",
            "^anchor",
            "'; DROP TABLE facts; --",
        ] {
            let r = s.recall(hostile, 5, 2);
            assert!(r.elapsed_ms >= 0.0);
        }
        assert_eq!(s.fact_count(), 1, "facts table must be intact");
    }

    #[test]
    fn recall_finds_relevant_facts() {
        let s = Store::open_in_memory().unwrap();
        s.apply_reflection(
            &batch(vec![
                ("the user's dog is named Ada", 0.9),
                ("the user dislikes cilantro", 0.7),
                ("the user works as a structural engineer", 0.6),
            ]),
            0.8,
        )
        .unwrap();

        let r = s.recall("what is my dog called", 5, 2);
        assert!(!r.facts.is_empty());
        assert!(r.facts[0].text.contains("Ada"), "got {:?}", r.facts[0].text);
    }

    #[test]
    fn recall_is_under_five_milliseconds() {
        let s = Store::open_in_memory().unwrap();
        // Seed a corpus far larger than a real device would accumulate.
        let facts: Vec<(String, f64)> = (0..2000)
            .map(|i| (format!("fact number {i} about topic {} and subject {}", i % 37, i % 11), 0.5))
            .collect();
        let b = ReflectionBatch {
            facts: facts
                .iter()
                .map(|(t, i)| CandidateFact { text: t.clone(), tags: vec![], importance: *i })
                .collect(),
            importance_updates: vec![],
            retire: vec![],
            source: "reflection",
        };
        // dedupe effectively off (containment can never exceed 1.0) so all 2000 land
        s.apply_reflection(&b, 2.0).unwrap();
        assert!(s.fact_count() > 1500);

        // Warm the prepared-statement cache, then measure.
        let _ = s.recall("topic 12 subject 3", 5, 2);
        let mut samples: Vec<f64> = (0..50)
            .map(|_| s.recall("tell me about topic 12 and subject 3", 5, 2).elapsed_ms)
            .collect();
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = samples[samples.len() / 2];
        let p95 = samples[samples.len() * 95 / 100];

        // The spec gate is on typical recall. Asserting on the worst single sample
        // makes this flaky on a loaded build machine (an OS scheduler hiccup is not
        // a regression), so gate on p50 and keep a loose ceiling to catch genuine
        // blowups.
        assert!(p50 < 5.0, "p50 recall was {p50:.2}ms over a 2000-fact corpus (gate: 5ms)");
        assert!(p95 < 25.0, "p95 recall was {p95:.2}ms — that is a real regression");
    }

    #[test]
    fn dedupe_blocks_near_duplicates() {
        let s = Store::open_in_memory().unwrap();
        s.apply_reflection(&batch(vec![("the user's dog is named Ada", 0.9)]), 0.8).unwrap();
        let inserted = s
            .apply_reflection(&batch(vec![("user dog named Ada", 0.9)]), 0.8)
            .unwrap();
        assert_eq!(inserted, 0, "near-duplicate should not be stored twice");
    }

    #[test]
    fn tool_role_messages_are_refused() {
        let s = Store::open_in_memory().unwrap();
        let err = s.record_message("tool", "<injected instructions from a web page>").unwrap_err();
        assert!(err.to_string().contains("firewall"));
        assert!(s.recent_messages(10).is_empty());
    }

    #[test]
    fn user_and_assistant_messages_round_trip() {
        let s = Store::open_in_memory().unwrap();
        s.record_message("user", "hello").unwrap();
        s.record_message("assistant", "hi there").unwrap();
        let msgs = s.recent_messages(10);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], ("user".to_string(), "hello".to_string()));
    }

    #[test]
    fn decay_and_prune_respects_pinning() {
        let s = Store::open_in_memory().unwrap();
        s.apply_reflection(&batch(vec![("low value trivia", 0.16), ("pinned truth", 0.16)]), 2.0)
            .unwrap();
        let pinned_id = s.top_facts(10).iter().find(|f| f.text == "pinned truth").unwrap().id;
        s.set_pinned(pinned_id, true).unwrap();

        // 0.16 * 0.98 = 0.1568 -> above 0.15; run enough rounds to cross the floor.
        for _ in 0..5 {
            s.decay_and_prune(0.98, 0.15).unwrap();
        }
        let remaining: Vec<String> = s.top_facts(10).into_iter().map(|f| f.text).collect();
        assert!(remaining.contains(&"pinned truth".to_string()));
        assert!(!remaining.contains(&"low value trivia".to_string()));
    }

    #[test]
    fn importance_clamps_to_unit_range() {
        let s = Store::open_in_memory().unwrap();
        s.apply_reflection(&batch(vec![("wild importance", 9.9)]), 2.0).unwrap();
        assert!(s.top_facts(1)[0].importance <= 1.0);
    }

    #[test]
    fn approx_tokens_never_undercounts_badly() {
        assert!(approx_tokens("hello world") >= 2);
        assert!(approx_tokens(&"word ".repeat(100)) >= 100);
        assert_eq!(approx_tokens(""), 0);
    }

    #[test]
    fn skills_reindex_and_recall() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("check-flight.md"),
            "# Check a flight status\ngoal: look up whether a flight is delayed\nsteps:\n1. web_search the flight number\n",
        )
        .unwrap();
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.reindex_skills(dir.path()).unwrap(), 1);
        let r = s.recall("is my flight delayed", 5, 2);
        assert_eq!(r.skills.len(), 1);
        assert_eq!(r.skills[0].name, "check-flight");
    }
}
