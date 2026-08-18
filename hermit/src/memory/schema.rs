//! SQLite schema for the L3 archive.
//!
//! One file, WAL mode, FTS5 for retrieval. No embedding model and no vector index:
//! BM25 over FTS5 is what Hermes-Agent uses, it costs no RAM, and on a corpus this
//! size it retrieves at least as well as a small embedding model would.

/// Applied in order, each wrapped in a transaction. `user_version` tracks which
/// migrations have run so upgrades are safe on a live device.
pub const MIGRATIONS: &[&str] = &[
    // ---- v1: base tables ------------------------------------------------
    r#"
    CREATE TABLE IF NOT EXISTS facts (
        id           INTEGER PRIMARY KEY,
        text         TEXT    NOT NULL,
        tags         TEXT    NOT NULL DEFAULT '',
        created_at   INTEGER NOT NULL,
        last_used_at INTEGER NOT NULL,
        importance   REAL    NOT NULL DEFAULT 0.5,
        pinned       INTEGER NOT NULL DEFAULT 0,
        -- Always 'reflection' or 'consolidation'. Raw tool/web content must never
        -- appear here; see the firewall note in memory/mod.rs.
        source       TEXT    NOT NULL DEFAULT 'reflection'
    );

    CREATE TABLE IF NOT EXISTS sessions (
        id         INTEGER PRIMARY KEY,
        started_at INTEGER NOT NULL,
        ended_at   INTEGER,
        summary    TEXT
    );

    CREATE TABLE IF NOT EXISTS messages (
        id         INTEGER PRIMARY KEY,
        session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        -- 'user' or 'assistant' only. Tool results are deliberately not persisted.
        role       TEXT    NOT NULL,
        content    TEXT    NOT NULL,
        ts         INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);
    CREATE INDEX IF NOT EXISTS idx_facts_importance ON facts(importance DESC);

    -- External-content FTS: the index stores only the terms, the rows live in
    -- `facts`. Keeps the database small on an SD card.
    CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(
        text, tags, content='facts', content_rowid='id', tokenize='porter unicode61'
    );

    CREATE TRIGGER IF NOT EXISTS facts_ai AFTER INSERT ON facts BEGIN
        INSERT INTO facts_fts(rowid, text, tags) VALUES (new.id, new.text, new.tags);
    END;
    CREATE TRIGGER IF NOT EXISTS facts_ad AFTER DELETE ON facts BEGIN
        INSERT INTO facts_fts(facts_fts, rowid, text, tags) VALUES('delete', old.id, old.text, old.tags);
    END;
    CREATE TRIGGER IF NOT EXISTS facts_au AFTER UPDATE ON facts BEGIN
        INSERT INTO facts_fts(facts_fts, rowid, text, tags) VALUES('delete', old.id, old.text, old.tags);
        INSERT INTO facts_fts(rowid, text, tags) VALUES (new.id, new.text, new.tags);
    END;

    CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
        content, content='messages', content_rowid='id', tokenize='porter unicode61'
    );
    CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
        INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
    END;
    CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
        INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
    END;

    -- Skills live as markdown files on disk (L4). This table is a rebuildable
    -- index over them, not the source of truth, so it is standalone FTS5.
    CREATE VIRTUAL TABLE IF NOT EXISTS skills_fts USING fts5(
        name, goal, body, path UNINDEXED, tokenize='porter unicode61'
    );
    "#,
];

/// Pragmas applied to every connection. WAL is what lets the reflection worker
/// write while the hot path reads without blocking.
pub const PRAGMAS: &str = r#"
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA foreign_keys = ON;
    PRAGMA temp_store = MEMORY;
    -- ~8MB page cache. Plenty for this corpus, trivial against the RAM budget.
    PRAGMA cache_size = -8000;
    PRAGMA busy_timeout = 3000;
"#;
