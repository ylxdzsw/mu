use std::path::{Path, PathBuf};

use fs2::FileExt;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::provider::{
    Attachment, ContentPart, ImageDetail, Message, ToolArtifact, ToolCall, Usage, UserContent,
    approx_tokens,
};
use crate::tools::BashRisk;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub title: Option<String>,
    pub last_total_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub cwd: String,
    pub title: Option<String>,
    pub last_total_tokens: u64,
    pub message_count: u64,
    pub turn_count: u64,
}

/// Synthesized result content for a tool call that has no persisted result
/// when an interrupted turn is normalized. We deliberately do not distinguish
/// "never started" from "started but killed" (the window between persisting the
/// request and spawning is sub-millisecond), so every result-less call gets the
/// same conservative note and the agent is asked to verify state on resume.
pub const INTERRUPTED_TOOL_RESULT: &str = "error: interrupted — this command may have started and not completed; its effects are unknown. Verify the resulting state before relying on it.";

#[derive(Debug, Clone)]
pub struct MessageRecord {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_calls_json: Option<String>,
    pub seq: i64,
}

pub struct ToolCallRecord<'a> {
    pub message_id: i64,
    pub call_id: &'a str,
    pub tool: &'a str,
    pub args: &'a str,
    pub risk: Option<&'a str>,
    pub status: &'a str,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
}

pub struct TurnAttemptCompletion<'a> {
    pub outcome: &'a str,
    pub error_class: Option<&'a str>,
    pub error: Option<&'a str>,
    pub partial_output: Option<&'a str>,
    pub provider_request_count: u32,
    pub iteration_count: u32,
    pub retry_count: u32,
    pub duration_ms: u64,
    pub context_tokens: u64,
}

pub struct ReviewRecord<'a> {
    pub session_id: &'a str,
    pub tool_call_id: Option<&'a str>,
    pub action_json: &'a str,
    pub risk_level: &'a str,
    pub user_auth_level: &'a str,
    pub outcome: &'a str,
    pub reason: Option<&'a str>,
}

pub struct Store {
    conn: Connection,
    lock_dir: PathBuf,
}

#[derive(Debug)]
pub struct SessionBusy;

impl std::fmt::Display for SessionBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session busy")
    }
}

impl std::error::Error for SessionBusy {}

const CURRENT_SCHEMA_VERSION: i32 = 8;
const COMPATIBLE_SCHEMA_BASELINE: i32 = 6;

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        cwd: row.get(1)?,
        model: row.get(2)?,
        title: row.get(3)?,
        last_total_tokens: row.get::<_, i64>(4)? as u64,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let state_dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("session database path must have a parent directory"))?;
        std::fs::create_dir_all(state_dir)?;
        let conn = Connection::open(path).context("opening SQLite database")?;
        configure_connection(&conn)?;
        let store = Self {
            conn,
            lock_dir: state_dir.join("locks"),
        };
        store.ensure_schema()?;
        store.enable_foreign_keys()?;
        Ok(store)
    }

    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory SQLite database")?;
        configure_connection(&conn)?;
        let store = Self {
            conn,
            lock_dir: std::env::temp_dir().join(format!("mu-memory-locks-{}", Uuid::new_v4())),
        };
        store.ensure_schema()?;
        store.enable_foreign_keys()?;
        Ok(store)
    }

    /// Foreign-key enforcement is switched on only after `ensure_schema` so
    /// migrations can rebuild tables without fighting the checker (the pragma
    /// is a no-op inside a transaction anyway, so it cannot be part of
    /// `configure_connection` and take effect before migrations commit).
    fn enable_foreign_keys(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA foreign_keys=ON;")
            .context("enabling foreign key enforcement")
    }

    fn ensure_schema(&self) -> Result<()> {
        let mut version: i32 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .context("reading SQLite schema version")?;
        if version == 0 {
            if !self.has_application_tables()? {
                return self.create_schema();
            }
            anyhow::bail!(
                "unsupported pre-release session database schema; remove sessions.db to create a fresh release database"
            );
        }
        if version > CURRENT_SCHEMA_VERSION {
            anyhow::bail!(
                "session database schema version {version} is newer than this mu supports (maximum {CURRENT_SCHEMA_VERSION}); upgrade mu"
            );
        }
        if version < COMPATIBLE_SCHEMA_BASELINE {
            anyhow::bail!(
                "session database schema version {version} predates the compatibility baseline {COMPATIBLE_SCHEMA_BASELINE}; upgrade through a compatible mu release"
            );
        }
        while version < CURRENT_SCHEMA_VERSION {
            match version {
                6 => self.migrate_schema_v6_to_v7()?,
                7 => self.migrate_schema_v7_to_v8()?,
                other => anyhow::bail!("no migration path from schema version {other}"),
            }
            version += 1;
        }
        Ok(())
    }

    fn has_application_tables(&self) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn create_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                cwd TEXT NOT NULL,
                model TEXT NOT NULL,
                title TEXT,
                last_total_tokens INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS message (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                reasoning_content TEXT,
                native_replay_json TEXT,
                user_content_json TEXT,
                tool_content_json TEXT,
                tool_call_id TEXT,
                tool_calls_json TEXT,
                seq INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id)
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_message_session_seq ON message(session_id, seq);
            CREATE TABLE IF NOT EXISTS tool_call (
                id INTEGER PRIMARY KEY,
                call_id TEXT NOT NULL,
                message_id INTEGER NOT NULL,
                tool TEXT NOT NULL,
                args TEXT NOT NULL,
                risk TEXT,
                status TEXT NOT NULL,
                exit_code INTEGER,
                duration_ms INTEGER,
                UNIQUE(message_id, call_id),
                FOREIGN KEY(message_id) REFERENCES message(id)
            );
            CREATE TABLE IF NOT EXISTS turn_attempt (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                model TEXT NOT NULL,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                outcome TEXT NOT NULL,
                error_class TEXT,
                error TEXT,
                partial_output TEXT,
                provider_request_count INTEGER NOT NULL DEFAULT 0,
                iteration_count INTEGER NOT NULL DEFAULT 0,
                retry_count INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER,
                context_tokens INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY(session_id) REFERENCES session(id)
            );
            CREATE INDEX IF NOT EXISTS idx_turn_attempt_session_id ON turn_attempt(session_id);
            CREATE TABLE IF NOT EXISTS turn_usage (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
                total_tokens INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id)
            );
            CREATE INDEX IF NOT EXISTS idx_turn_usage_session_id ON turn_usage(session_id);
            CREATE TABLE IF NOT EXISTS attachment_blob (
                id TEXT PRIMARY KEY,
                data BLOB NOT NULL,
                size INTEGER NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS review (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                tool_call_id TEXT,
                action_json TEXT NOT NULL,
                risk_level TEXT NOT NULL,
                user_auth_level TEXT NOT NULL,
                outcome TEXT NOT NULL,
                reason TEXT,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id)
            );
            PRAGMA user_version = 8;",
        )?;
        Ok(())
    }

    fn migrate_schema_v6_to_v7(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(
            "ALTER TABLE tool_call ADD COLUMN exit_code INTEGER;
             ALTER TABLE tool_call ADD COLUMN duration_ms INTEGER;
             CREATE TABLE turn_attempt (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                model TEXT NOT NULL,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                outcome TEXT NOT NULL,
                error_class TEXT,
                error TEXT,
                partial_output TEXT,
                provider_request_count INTEGER NOT NULL DEFAULT 0,
                iteration_count INTEGER NOT NULL DEFAULT 0,
                retry_count INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER,
                context_tokens INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY(session_id) REFERENCES session(id)
             );
             CREATE INDEX idx_turn_attempt_session_id ON turn_attempt(session_id);
             PRAGMA user_version = 7;",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// v8 drops AUTOINCREMENT from `message`, `turn_attempt`, `turn_usage`, and
    /// `review` (it only prevents rowid reuse after deleting the max row —
    /// irrelevant for append-only tables, and costs a `sqlite_sequence` write
    /// per insert). Also rebuilds `tool_call` around a surrogate key: provider
    /// call ids are only unique within a single response (many OpenAI-compatible
    /// backends emit `call_0`, `call_1`, …), so using them as the table's
    /// primary key let later turns silently overwrite earlier audit rows. The
    /// result text also stops being duplicated here — it lives on the `tool`
    /// message row, joinable via `call_id`. `message` gains a UNIQUE ordering
    /// index (the replay contract), and `review` gains the session foreign key.
    fn migrate_schema_v7_to_v8(&self) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TABLE message_v8 (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                reasoning_content TEXT,
                native_replay_json TEXT,
                user_content_json TEXT,
                tool_content_json TEXT,
                tool_call_id TEXT,
                tool_calls_json TEXT,
                seq INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id)
             );
             INSERT INTO message_v8 (
                id, session_id, role, content, reasoning_content, native_replay_json,
                user_content_json, tool_content_json, tool_call_id, tool_calls_json,
                seq, created_at
             ) SELECT id, session_id, role, content, reasoning_content, native_replay_json,
                user_content_json, tool_content_json, tool_call_id, tool_calls_json,
                seq, created_at FROM message;
             DROP TABLE message;
             ALTER TABLE message_v8 RENAME TO message;
             CREATE UNIQUE INDEX idx_message_session_seq ON message(session_id, seq);
             CREATE TABLE turn_attempt_v8 (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                model TEXT NOT NULL,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                outcome TEXT NOT NULL,
                error_class TEXT,
                error TEXT,
                partial_output TEXT,
                provider_request_count INTEGER NOT NULL DEFAULT 0,
                iteration_count INTEGER NOT NULL DEFAULT 0,
                retry_count INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER,
                context_tokens INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY(session_id) REFERENCES session(id)
             );
             INSERT INTO turn_attempt_v8 SELECT * FROM turn_attempt;
             DROP TABLE turn_attempt;
             ALTER TABLE turn_attempt_v8 RENAME TO turn_attempt;
             CREATE INDEX idx_turn_attempt_session_id ON turn_attempt(session_id);
             CREATE TABLE turn_usage_v8 (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
                total_tokens INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id)
             );
             INSERT INTO turn_usage_v8 SELECT * FROM turn_usage;
             DROP TABLE turn_usage;
             ALTER TABLE turn_usage_v8 RENAME TO turn_usage;
             CREATE INDEX idx_turn_usage_session_id ON turn_usage(session_id);
             CREATE TABLE tool_call_v8 (
                id INTEGER PRIMARY KEY,
                call_id TEXT NOT NULL,
                message_id INTEGER NOT NULL,
                tool TEXT NOT NULL,
                args TEXT NOT NULL,
                risk TEXT,
                status TEXT NOT NULL,
                exit_code INTEGER,
                duration_ms INTEGER,
                UNIQUE(message_id, call_id),
                FOREIGN KEY(message_id) REFERENCES message(id)
             );
             INSERT INTO tool_call_v8 (
                call_id, message_id, tool, args, risk, status, exit_code, duration_ms
             ) SELECT id, message_id, tool, args, risk, status, exit_code, duration_ms
               FROM tool_call;
             DROP TABLE tool_call;
             ALTER TABLE tool_call_v8 RENAME TO tool_call;
             CREATE TABLE review_v8 (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                tool_call_id TEXT,
                action_json TEXT NOT NULL,
                risk_level TEXT NOT NULL,
                user_auth_level TEXT NOT NULL,
                outcome TEXT NOT NULL,
                reason TEXT,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id)
             );
             INSERT INTO review_v8 (
                id, session_id, tool_call_id, action_json, risk_level,
                user_auth_level, outcome, reason, created_at
             ) SELECT id, session_id, tool_call_id, action_json, risk_level,
                user_auth_level, outcome, reason, created_at
               FROM review;
             DROP TABLE review;
             ALTER TABLE review_v8 RENAME TO review;
             DELETE FROM sqlite_sequence;
             PRAGMA user_version = 8;",
        )
        .context(
            "migrating session database to schema v8 (a duplicate-ordering failure here means the message log was already corrupt)",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Test-only: production sessions are created via `create_session_seeded`
    /// so the session row never exists without its system prompt.
    #[cfg(test)]
    pub fn create_session(&self, cwd: &str, model: &str) -> Result<Session> {
        let id = Uuid::new_v4().to_string();
        let now = now_rfc3339();
        self.conn.execute(
            "INSERT INTO session (id, created_at, updated_at, cwd, model, title, last_total_tokens)
             VALUES (?1, ?2, ?2, ?3, ?4, NULL, 0)",
            params![id, now, cwd, model],
        )?;
        Ok(Session {
            id,
            cwd: cwd.into(),
            model: model.into(),
            title: None,
            last_total_tokens: 0,
        })
    }

    /// Create the session row, its system prompt, and the environment seed in
    /// one transaction. A crash can therefore never leave a session that fails
    /// to load with "missing persisted system prompt".
    pub fn create_session_seeded(
        &self,
        cwd: &str,
        model: &str,
        system_prompt: &str,
        environment_seed: &str,
    ) -> Result<Session> {
        let id = Uuid::new_v4().to_string();
        let now = now_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO session (id, created_at, updated_at, cwd, model, title, last_total_tokens)
             VALUES (?1, ?2, ?2, ?3, ?4, NULL, 0)",
            params![id, now, cwd, model],
        )?;
        insert_message_in(
            &tx,
            &id,
            &Message::System {
                content: system_prompt.to_string(),
            },
            &now,
        )?;
        insert_message_in(
            &tx,
            &id,
            &Message::User {
                content: UserContent::Text(environment_seed.to_string()),
            },
            &now,
        )?;
        tx.commit()?;
        Ok(Session {
            id,
            cwd: cwd.into(),
            model: model.into(),
            title: None,
            last_total_tokens: 0,
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_total_tokens
             FROM session WHERE id = ?1",
        )?;
        let row = stmt.query_row(params![id], session_from_row).optional()?;
        Ok(row)
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<(Session, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_total_tokens, updated_at
             FROM session
             ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((session_from_row(row)?, row.get::<_, String>(5)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("listing sessions")
    }

    pub fn session_summary(&self, id: &str) -> Result<Option<SessionSummary>> {
        let mut stmt = self.conn.prepare(
            // turn_count = completed assistant replies (role='assistant' with no
            // tool_calls_json, meaning the model finished without requesting a
            // further tool). This is robust across both provider adapters:
            //   - Chat Completions: completes with role=assistant, no tool_calls
            //   - Responses: same persisted shape
            // Counting user rows would be wrong: a session has the environment
            // seed, the actual prompts, AND any cwd-change reminders appended
            // mid-session — all as role='user'. Interrupted turns (assistant
            // row still carrying tool_calls_json) are intentionally not counted.
            "SELECT
                s.id, s.created_at, s.updated_at, s.cwd, s.title,
                s.last_total_tokens,
                COUNT(m.id) AS message_count,
                COALESCE(SUM(CASE
                    WHEN m.role = 'assistant'
                     AND (m.tool_calls_json IS NULL OR m.tool_calls_json = '[]')
                    THEN 1 ELSE 0 END), 0) AS turn_count
             FROM session s
             LEFT JOIN message m ON m.session_id = s.id
             WHERE s.id = ?1
             GROUP BY s.id",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                Ok(SessionSummary {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    cwd: row.get(3)?,
                    title: row.get(4)?,
                    last_total_tokens: row.get::<_, i64>(5)? as u64,
                    message_count: row.get::<_, i64>(6)? as u64,
                    turn_count: row.get::<_, i64>(7)? as u64,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn latest_session(&self) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_total_tokens
             FROM session ORDER BY updated_at DESC LIMIT 1",
        )?;
        let row = stmt.query_row([], session_from_row).optional()?;
        Ok(row)
    }

    pub fn update_session(
        &self,
        id: &str,
        usage: &Usage,
        context_tokens: u64,
        title: Option<&str>,
        model: &str,
    ) -> Result<()> {
        let now = now_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        if let Some(t) = title {
            tx.execute(
                "UPDATE session SET updated_at = ?1, last_total_tokens = ?2,
                 title = COALESCE(title, ?3), model = ?4 WHERE id = ?5",
                params![now, context_tokens as i64, t, model, id],
            )?;
        } else {
            tx.execute(
                "UPDATE session SET updated_at = ?1, last_total_tokens = ?2,
                 model = ?3 WHERE id = ?4",
                params![now, context_tokens as i64, model, id],
            )?;
        }
        tx.execute(
            "INSERT INTO turn_usage (
                session_id, model, input_tokens, cache_read_input_tokens,
                cache_write_input_tokens, output_tokens, reasoning_output_tokens,
                total_tokens, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                model,
                usage.input_tokens as i64,
                usage.cache_read_input_tokens as i64,
                usage.cache_write_input_tokens.unwrap_or(0) as i64,
                usage.output_tokens as i64,
                usage.reasoning_output_tokens as i64,
                usage.total_tokens as i64,
                now,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn update_session_cwd(&self, id: &str, cwd: &str) -> Result<()> {
        let now = now_rfc3339();
        self.conn.execute(
            "UPDATE session SET updated_at = ?1, cwd = ?2 WHERE id = ?3",
            params![now, cwd, id],
        )?;
        Ok(())
    }

    pub fn start_turn_attempt(&self, session_id: &str, kind: &str, model: &str) -> Result<i64> {
        let now = now_rfc3339();
        self.conn.execute(
            "INSERT INTO turn_attempt (session_id, kind, model, started_at, outcome)
             VALUES (?1, ?2, ?3, ?4, 'running')",
            params![session_id, kind, model, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn finish_turn_attempt(
        &self,
        attempt_id: i64,
        completion: TurnAttemptCompletion<'_>,
    ) -> Result<()> {
        let now = now_rfc3339();
        self.conn.execute(
            "UPDATE turn_attempt SET
                completed_at = ?1,
                outcome = ?2,
                error_class = ?3,
                error = ?4,
                partial_output = ?5,
                provider_request_count = ?6,
                iteration_count = ?7,
                retry_count = ?8,
                duration_ms = ?9,
                context_tokens = ?10
             WHERE id = ?11",
            params![
                now,
                completion.outcome,
                completion.error_class,
                completion.error,
                completion.partial_output,
                completion.provider_request_count as i64,
                completion.iteration_count as i64,
                completion.retry_count as i64,
                u64_to_i64(completion.duration_ms),
                u64_to_i64(completion.context_tokens),
                attempt_id,
            ],
        )?;
        Ok(())
    }

    /// A session is "clean" when its last turn finished — the last message is a
    /// completed assistant reply with no `tool_calls`. A trailing user prompt,
    /// tool result, or assistant message still carrying `tool_calls` means the
    /// turn was interrupted. A session whose only message is the synthetic
    /// environment seed (a lone leading user message) is also clean, since no
    /// real turn has run yet. Derived purely from the log so it can never drift
    /// out of sync with the messages (unlike a stored flag).
    pub fn is_session_clean(&self, session_id: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare(
            "SELECT role, tool_calls_json FROM message
             WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
        )?;
        let last: Option<(String, Option<String>)> = stmt
            .query_row(params![session_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let Some((role, tool_calls_json)) = last else {
            return Ok(true);
        };
        match role.as_str() {
            "assistant" => Ok(!has_tool_calls(tool_calls_json.as_deref())),
            "summary" => Ok(true),
            "user" => {
                let count: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM message WHERE session_id = ?1 AND role != 'system'",
                    params![session_id],
                    |row| row.get(0),
                )?;
                Ok(count <= 1)
            }
            "system" => Ok(true),
            _ => Ok(false),
        }
    }

    /// Make an interrupted turn's history API-valid: every `tool_call` in the
    /// most recent assistant tool-call message must be followed by a `tool`
    /// result. Calls that finished keep their real result; result-less calls get
    /// a synthesized interrupted result (see `INTERRUPTED_TOOL_RESULT`).
    /// Idempotent — a no-op once the latest tool-call message is fully answered,
    /// so it is safe to call before every turn/retry. Returns the number of
    /// results synthesized.
    pub fn normalize_interrupted_tail(&self, session_id: &str) -> Result<usize> {
        let records = self.message_records_from_seq(session_id, 0)?;
        let Some(idx) = records.iter().rposition(|record| {
            record.role == "assistant"
                && parse_tool_calls(record.tool_calls_json.as_deref()).is_some()
        }) else {
            return Ok(0);
        };
        let assistant_id = records[idx].id;
        let calls = parse_tool_calls(records[idx].tool_calls_json.as_deref()).unwrap_or_default();
        let answered: std::collections::HashSet<&str> = records[idx + 1..]
            .iter()
            .filter(|record| record.role == "tool")
            .filter_map(|record| record.tool_call_id.as_deref())
            .collect();

        let mut synthesized = 0;
        for call in &calls {
            if answered.contains(call.id.as_str()) {
                continue;
            }
            let risk = BashRisk::from_args_json(&call.function.arguments);
            self.persist_tool_result(
                session_id,
                ToolCallRecord {
                    message_id: assistant_id,
                    call_id: &call.id,
                    tool: &call.function.name,
                    args: &call.function.arguments,
                    risk: risk.as_ref().map(|risk| risk.as_str()),
                    status: "interrupted",
                    exit_code: None,
                    duration_ms: None,
                },
                INTERRUPTED_TOOL_RESULT,
                &[],
            )?;
            synthesized += 1;
        }
        Ok(synthesized)
    }

    pub fn message_records_from_seq(
        &self,
        session_id: &str,
        start_seq: i64,
    ) -> Result<Vec<MessageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, role, content, user_content_json, tool_call_id, tool_calls_json, seq, created_at
             FROM message
             WHERE session_id = ?1 AND seq >= ?2
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id, start_seq], load_message_record_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("loading message records")
    }

    pub fn load_context_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let summary_seq = self.latest_summary_seq(session_id)?;
        let start_seq = summary_seq.unwrap_or(-1);
        let mut stmt = self.conn.prepare(
            "SELECT role, content, reasoning_content, user_content_json, tool_content_json, tool_call_id, tool_calls_json, native_replay_json FROM message
             WHERE session_id = ?1 AND seq > ?2 AND role NOT IN ('system', 'summary')
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id, start_seq], load_context_row)?;

        let mut messages = vec![Message::System {
            content: self.system_prompt(session_id)?,
        }];
        if let Some(seq) = summary_seq
            && let Some(summary_content) = self.message_at_seq(session_id, seq)?
        {
            messages.push(Message::User {
                content: UserContent::Text(format!(
                    "[summary of earlier conversation]\n{}",
                    summary_content
                )),
            });
        }

        for row in rows {
            let (
                role,
                content,
                reasoning_content,
                user_content_json,
                tool_content_json,
                tool_call_id,
                tool_calls_json,
                native_replay_json,
            ) = row?;
            match role.as_str() {
                "user" => messages.push(Message::User {
                    content: load_user_content(&self.conn, content, user_content_json)?,
                }),
                "assistant" => {
                    let tool_calls = tool_calls_json
                        .as_ref()
                        .and_then(|j| serde_json::from_str(j).ok());
                    messages.push(Message::Assistant {
                        content: if content.is_empty() {
                            None
                        } else {
                            Some(content)
                        },
                        reasoning_content,
                        tool_calls,
                        native_replay: native_replay_json
                            .as_deref()
                            .and_then(|json| serde_json::from_str(json).ok()),
                    });
                }
                "tool" => messages.push(Message::Tool {
                    content,
                    artifacts: load_tool_artifacts(&self.conn, tool_content_json)?,
                    tool_call_id: tool_call_id.unwrap_or_default(),
                }),
                other => messages.push(Message::User {
                    content: UserContent::Text(format!("[{other}] {content}")),
                }),
            }
        }
        Ok(messages)
    }

    pub fn system_prompt(&self, session_id: &str) -> Result<String> {
        let mut stmt = self.conn.prepare(
            "SELECT content FROM message
             WHERE session_id = ?1 AND role = 'system'
             ORDER BY seq ASC LIMIT 1",
        )?;
        stmt.query_row(params![session_id], |row| row.get(0))
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("session is missing persisted system prompt"))
    }

    fn latest_summary_seq(&self, session_id: &str) -> Result<Option<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq FROM message WHERE session_id = ?1 AND role = 'summary' ORDER BY seq DESC LIMIT 1",
        )?;
        let row = stmt
            .query_row(params![session_id], |row| row.get(0))
            .optional()?;
        Ok(row)
    }

    fn message_at_seq(&self, session_id: &str, seq: i64) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT content FROM message
             WHERE session_id = ?1 AND seq = ?2",
        )?;
        let content = stmt
            .query_row(params![session_id, seq], |row| row.get(0))
            .optional()?;
        Ok(content)
    }

    pub fn append_message(&self, session_id: &str, message: &Message) -> Result<i64> {
        let now = now_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        let id = insert_message_in(&tx, session_id, message, &now)?;
        tx.commit()?;
        Ok(id)
    }

    pub fn insert_summary_before(
        &self,
        session_id: &str,
        content: &str,
        before_seq: i64,
    ) -> Result<()> {
        let now = now_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        // Shift in two passes through negative values: UNIQUE(session_id, seq)
        // is checked per updated row, so a direct `seq = seq + 1` would collide
        // with the not-yet-shifted neighbor. seq is never negative at rest, so
        // the intermediate range is free.
        tx.execute(
            "UPDATE message SET seq = -(seq + 1) WHERE session_id = ?1 AND seq >= ?2",
            params![session_id, before_seq],
        )?;
        tx.execute(
            "UPDATE message SET seq = -seq WHERE session_id = ?1 AND seq < 0",
            params![session_id],
        )?;
        tx.execute(
            "INSERT INTO message (session_id, role, content, seq, created_at) VALUES (?1, 'summary', ?2, ?3, ?4)",
            params![session_id, content, before_seq, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn append_summary(&self, session_id: &str, content: &str) -> Result<()> {
        let now = now_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        let seq = next_seq_in(&tx, session_id)?;
        tx.execute(
            "INSERT INTO message (session_id, role, content, seq, created_at) VALUES (?1, 'summary', ?2, ?3, ?4)",
            params![session_id, content, seq, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_tool_call(&self, record: ToolCallRecord<'_>) -> Result<()> {
        // OR REPLACE is scoped by UNIQUE(message_id, call_id): a re-persisted
        // result for the same call replaces its own audit row, while provider
        // call-id reuse across messages or sessions can never clobber history.
        self.conn.execute(
            "INSERT OR REPLACE INTO tool_call (
                call_id, message_id, tool, args, risk, status, exit_code, duration_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                record.call_id,
                record.message_id,
                record.tool,
                record.args,
                record.risk,
                record.status,
                record.exit_code,
                record.duration_ms.map(u64_to_i64),
            ],
        )?;
        Ok(())
    }

    pub fn persist_tool_result(
        &self,
        session_id: &str,
        record: ToolCallRecord<'_>,
        content: &str,
        artifacts: &[ToolArtifact],
    ) -> Result<i64> {
        let now = now_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO tool_call (
                call_id, message_id, tool, args, risk, status, exit_code, duration_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                record.call_id,
                record.message_id,
                record.tool,
                record.args,
                record.risk,
                record.status,
                record.exit_code,
                record.duration_ms.map(u64_to_i64),
            ],
        )?;
        let message = Message::Tool {
            content: content.to_string(),
            artifacts: artifacts.to_vec(),
            tool_call_id: record.call_id.to_string(),
        };
        let message_id = insert_message_in(&tx, session_id, &message, &now)?;
        tx.execute(
            "UPDATE session SET updated_at = ?1 WHERE id = ?2",
            params![now, session_id],
        )?;
        tx.commit()?;
        Ok(message_id)
    }

    pub fn record_review(&self, record: ReviewRecord<'_>) -> Result<()> {
        let now = now_rfc3339();
        self.conn.execute(
            "INSERT INTO review (session_id, tool_call_id, action_json, risk_level, user_auth_level, outcome, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                record.session_id,
                record.tool_call_id,
                record.action_json,
                record.risk_level,
                record.user_auth_level,
                record.outcome,
                record.reason,
                now
            ],
        )?;
        Ok(())
    }

    pub fn latest_summary_sequence(&self, session_id: &str) -> Result<Option<i64>> {
        self.latest_summary_seq(session_id)
    }

    pub fn estimate_context_tokens(&self, session_id: &str) -> u64 {
        self.load_context_messages(session_id)
            .map(|msgs| {
                msgs.iter()
                    .map(|m| match m {
                        Message::User { content } => approx_tokens(&content.text()),
                        Message::Assistant {
                            content,
                            reasoning_content,
                            tool_calls,
                            native_replay,
                        } => {
                            approx_tokens(content.as_deref().unwrap_or(""))
                                + approx_tokens(reasoning_content.as_deref().unwrap_or(""))
                                + tool_calls
                                    .as_ref()
                                    .map(|t| {
                                        approx_tokens(&serde_json::to_string(t).unwrap_or_default())
                                    })
                                    .unwrap_or(0)
                                + native_replay
                                    .as_ref()
                                    .map(|native| {
                                        approx_tokens(
                                            &serde_json::to_string(native).unwrap_or_default(),
                                        )
                                    })
                                    .unwrap_or(0)
                        }
                        Message::Tool { content, .. } => approx_tokens(content),
                        Message::System { content } => approx_tokens(content),
                    })
                    .sum()
            })
            .unwrap_or(0)
    }

    pub fn acquire_session_lock(&self, session_id: &str) -> Result<SessionLock> {
        crate::paths::ensure_dir(&self.lock_dir)?;
        let lock_path = self.session_lock_path(session_id);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .context("opening session lock file")?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(SessionLock { _file: file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Err(anyhow::Error::new(SessionBusy))
            }
            Err(error) => Err(error).context("acquiring session lock"),
        }
    }

    pub fn is_session_busy(&self, session_id: &str) -> Result<bool> {
        let lock_path = self.session_lock_path(session_id);
        let file = match std::fs::OpenOptions::new().write(true).open(&lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("opening session lock file"),
        };
        match file.try_lock_exclusive() {
            Ok(()) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error).context("checking session lock"),
        }
    }

    fn session_lock_path(&self, session_id: &str) -> PathBuf {
        self.lock_dir.join(format!("{session_id}.lock"))
    }
}

#[derive(Debug)]
pub struct SessionLock {
    _file: std::fs::File,
}

/// Connection-level tuning, applied before the schema check.
///
/// - WAL + `synchronous=NORMAL`: in WAL mode NORMAL fsyncs only at
///   checkpoints, so the per-message commits inside a turn stop paying an
///   fsync each. An application crash loses nothing; an OS crash or power
///   loss may lose the most recent commits but cannot corrupt the database —
///   the right trade for a conversation log.
/// - `busy_timeout`: rides out the rare cross-session write overlap under WAL
///   (same-session writers are serialized by the per-session flock).
/// - `trusted_schema=OFF`: a project-scope sessions.db can arrive in a cloned
///   repository, so do not run functions embedded in a crafted schema. mu's
///   own schema uses no views, triggers, or expression indexes.
/// - `journal_size_limit`: attachment blobs can push the WAL file to tens of
///   megabytes; without a limit it stays at its high-water mark forever.
///   8 MiB caps the steady-state size while keeping normal turns unaffected.
///
/// `foreign_keys=ON` is deliberately not here — see `enable_foreign_keys`.
fn configure_connection(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;
         PRAGMA trusted_schema=OFF;
         PRAGMA journal_size_limit=8388608;",
    )
    .context("configuring SQLite connection")
}

/// UTC timestamp in RFC3339 format for all database writes.
///
/// RFC3339 strings sort lexicographically only when all values use the same
/// UTC offset — chrono's `to_rfc3339()` always emits `+00:00`, so ORDER BY on
/// any timestamp column is safe and correct. Do not substitute a local-time
/// formatter here; it would silently break session ordering.
fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

type ContextRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub fn write_session_id(path: &Path, id: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, id)?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum PersistedUserContent {
    Text(String),
    Parts(Vec<PersistedContentPart>),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PersistedContentPart {
    Text { text: String },
    Attachment { attachment: PersistedAttachment },
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedAttachment {
    blob_id: String,
    filename: String,
    media_type: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedToolContent {
    artifacts: Vec<PersistedToolArtifact>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedToolArtifact {
    attachment: PersistedAttachment,
    detail: ImageDetail,
}

fn load_tool_artifacts(
    conn: &Connection,
    tool_content_json: Option<String>,
) -> Result<Vec<ToolArtifact>> {
    let Some(json) = tool_content_json else {
        return Ok(Vec::new());
    };
    let persisted: PersistedToolContent =
        serde_json::from_str(&json).context("decoding persisted tool artifacts")?;
    persisted
        .artifacts
        .into_iter()
        .map(|artifact| {
            Ok(ToolArtifact {
                attachment: load_persisted_attachment(conn, artifact.attachment)?,
                detail: artifact.detail,
            })
        })
        .collect()
}

fn persist_tool_artifacts(
    tx: &rusqlite::Transaction<'_>,
    artifacts: &[ToolArtifact],
    now: &str,
) -> Result<Option<String>> {
    if artifacts.is_empty() {
        return Ok(None);
    }
    let artifacts = artifacts
        .iter()
        .map(|artifact| {
            Ok(PersistedToolArtifact {
                attachment: persist_attachment(tx, &artifact.attachment, now)?,
                detail: artifact.detail,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(serde_json::to_string(&PersistedToolContent {
        artifacts,
    })?))
}

fn load_persisted_attachment(
    conn: &Connection,
    attachment: PersistedAttachment,
) -> Result<Attachment> {
    let data = conn
        .query_row(
            "SELECT data FROM attachment_blob WHERE id = ?1",
            params![attachment.blob_id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
        .with_context(|| format!("missing attachment blob {}", attachment.blob_id))?;
    let actual_id = attachment_blob_id(&data);
    if actual_id != attachment.blob_id {
        anyhow::bail!("corrupt attachment blob {}", attachment.blob_id);
    }
    Ok(Attachment {
        filename: attachment.filename,
        media_type: attachment.media_type,
        data,
    })
}

fn persist_attachment(
    tx: &rusqlite::Transaction<'_>,
    attachment: &Attachment,
    now: &str,
) -> Result<PersistedAttachment> {
    let blob_id = attachment_blob_id(&attachment.data);
    tx.execute(
        "INSERT OR IGNORE INTO attachment_blob (id, data, size, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![blob_id, attachment.data, attachment.data.len() as i64, now],
    )?;
    Ok(PersistedAttachment {
        blob_id,
        filename: attachment.filename.clone(),
        media_type: attachment.media_type.clone(),
    })
}

fn load_user_content(
    conn: &Connection,
    content: String,
    user_content_json: Option<String>,
) -> Result<UserContent> {
    let Some(json) = user_content_json else {
        return Ok(UserContent::Text(content));
    };
    let persisted = match serde_json::from_str::<PersistedUserContent>(&json) {
        Ok(persisted) => persisted,
        Err(_) => return Ok(UserContent::Text(content)),
    };
    match persisted {
        PersistedUserContent::Text(text) => Ok(UserContent::Text(text)),
        PersistedUserContent::Parts(parts) => parts
            .into_iter()
            .map(|part| match part {
                PersistedContentPart::Text { text } => Ok(ContentPart::Text { text }),
                PersistedContentPart::Attachment { attachment } => Ok(ContentPart::Attachment {
                    attachment: load_persisted_attachment(conn, attachment)?,
                }),
            })
            .collect::<Result<Vec<_>>>()
            .map(UserContent::Parts),
    }
}

fn persist_user_content(
    tx: &rusqlite::Transaction<'_>,
    content: &UserContent,
    now: &str,
) -> Result<String> {
    let persisted = match content {
        UserContent::Text(text) => PersistedUserContent::Text(text.clone()),
        UserContent::Parts(parts) => PersistedUserContent::Parts(
            parts
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => {
                        Ok(PersistedContentPart::Text { text: text.clone() })
                    }
                    ContentPart::Attachment { attachment } => {
                        Ok(PersistedContentPart::Attachment {
                            attachment: persist_attachment(tx, attachment, now)?,
                        })
                    }
                })
                .collect::<Result<Vec<_>>>()?,
        ),
    };
    Ok(serde_json::to_string(&persisted)?)
}

fn attachment_blob_id(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn u64_to_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn load_context_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContextRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn next_seq_in(tx: &rusqlite::Transaction<'_>, session_id: &str) -> Result<i64> {
    let mut stmt =
        tx.prepare("SELECT COALESCE(MAX(seq), -1) + 1 FROM message WHERE session_id = ?1")?;
    let seq: i64 = stmt.query_row(params![session_id], |row| row.get::<_, i64>(0))?;
    Ok(seq)
}

fn insert_message_in(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    message: &Message,
    now: &str,
) -> Result<i64> {
    let seq = next_seq_in(tx, session_id)?;
    match message {
        Message::User { content } => {
            let user_content_json = persist_user_content(tx, content, now)?;
            tx.execute(
                "INSERT INTO message (session_id, role, content, user_content_json, seq, created_at)
                 VALUES (?1, 'user', ?2, ?3, ?4, ?5)",
                params![session_id, content.text(), user_content_json, seq, now],
            )?;
        }
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            native_replay,
        } => {
            let tc_json = tool_calls.as_ref().map(serde_json::to_string).transpose()?;
            let native_json = native_replay
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            tx.execute(
                "INSERT INTO message (session_id, role, content, reasoning_content, tool_calls_json, native_replay_json, seq, created_at)
                 VALUES (?1, 'assistant', ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    session_id,
                    content.as_deref().unwrap_or(""),
                    reasoning_content,
                    tc_json,
                    native_json,
                    seq,
                    now
                ],
            )?;
        }
        Message::Tool {
            content,
            artifacts,
            tool_call_id,
        } => {
            let tool_content_json = persist_tool_artifacts(tx, artifacts, now)?;
            tx.execute(
                "INSERT INTO message (session_id, role, content, tool_content_json, tool_call_id, seq, created_at)
                 VALUES (?1, 'tool', ?2, ?3, ?4, ?5, ?6)",
                params![session_id, content, tool_content_json, tool_call_id, seq, now],
            )?;
        }
        Message::System { content } => {
            tx.execute(
                "INSERT INTO message (session_id, role, content, seq, created_at)
                 VALUES (?1, 'system', ?2, ?3, ?4)",
                params![session_id, content, seq, now],
            )?;
        }
    }
    Ok(tx.last_insert_rowid())
}

fn load_message_record_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRecord> {
    Ok(MessageRecord {
        id: row.get(0)?,
        role: row.get(1)?,
        content: row.get(2)?,
        tool_call_id: row.get(4)?,
        tool_calls_json: row.get(5)?,
        seq: row.get(6)?,
    })
}

pub(crate) fn parse_tool_calls(json: Option<&str>) -> Option<Vec<ToolCall>> {
    let json = json?;
    let calls: Vec<ToolCall> = serde_json::from_str(json).ok()?;
    (!calls.is_empty()).then_some(calls)
}

fn has_tool_calls(json: Option<&str>) -> bool {
    parse_tool_calls(json).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        Attachment, ContentPart, FunctionCall, NativeReplay, NativeReplayPayload, ToolCall,
    };

    fn temp_store() -> (Store, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!("mu-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        (Store::open(&tmp.join("mu.db")).unwrap(), tmp)
    }

    fn create_session_with_system(store: &Store) -> Session {
        let session = store.create_session("/tmp", "fake-model").unwrap();
        store
            .append_message(
                &session.id,
                &Message::System {
                    content: "system".into(),
                },
            )
            .unwrap();
        session
    }

    #[test]
    #[ignore = "invoked only by the session-lock subprocess test"]
    fn session_lock_holder_for_subprocess_test() {
        let db = std::env::var("MU_STORE_LOCK_TEST_DB").unwrap();
        let session_id = std::env::var("MU_STORE_LOCK_TEST_SESSION").unwrap();
        let ready = std::env::var("MU_STORE_LOCK_TEST_READY").unwrap();
        let store = Store::open(Path::new(&db)).unwrap();
        let _lock = store.acquire_session_lock(&session_id).unwrap();
        std::fs::write(ready, "locked").unwrap();

        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    #[test]
    fn session_lock_blocks_other_process_and_releases_after_holder_exits() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let db = tmp.join("mu.db");
        let ready = tmp.join("lock-ready");
        let mut holder = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::tests::session_lock_holder_for_subprocess_test",
                "--ignored",
                "--nocapture",
            ])
            .env("MU_STORE_LOCK_TEST_DB", &db)
            .env("MU_STORE_LOCK_TEST_SESSION", &session.id)
            .env("MU_STORE_LOCK_TEST_READY", &ready)
            .spawn()
            .unwrap();

        let contention = (|| -> Result<()> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !ready.exists() {
                if let Some(status) = holder.try_wait()? {
                    anyhow::bail!("lock holder exited before becoming ready: {status}");
                }
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for lock holder");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            let contender = Store::open(&db)?;
            assert!(contender.is_session_busy(&session.id)?);
            let error = contender.acquire_session_lock(&session.id).unwrap_err();
            assert!(error.downcast_ref::<SessionBusy>().is_some());
            Ok(())
        })();

        let _ = holder.kill();
        let _ = holder.wait();
        contention.unwrap();

        let contender = Store::open(&db).unwrap();
        assert!(!contender.is_session_busy(&session.id).unwrap());
        let lock = contender.acquire_session_lock(&session.id).unwrap();
        drop(lock);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn new_database_creates_release_schema_version() {
        let tmp = std::env::temp_dir().join(format!("mu-store-schema-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = tmp.join("mu.db");

        let store = Store::open(&db).unwrap();

        let version: i32 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        let tables = store
            .conn
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            tables,
            vec![
                "attachment_blob",
                "message",
                "review",
                "session",
                "tool_call",
                "turn_attempt",
                "turn_usage"
            ]
        );
        let message_columns = store
            .conn
            .prepare("PRAGMA table_info(message)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(message_columns.contains(&"reasoning_content".to_string()));
        assert!(message_columns.contains(&"native_replay_json".to_string()));
        assert!(message_columns.contains(&"tool_content_json".to_string()));
        let ordering_index_is_unique: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_index_list('message')
                 WHERE name = 'idx_message_session_seq' AND \"unique\" = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ordering_index_is_unique, 1);
        let tool_call_columns = store
            .conn
            .prepare("PRAGMA table_info(tool_call)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(tool_call_columns.contains(&"call_id".to_string()));
        assert!(tool_call_columns.contains(&"exit_code".to_string()));
        assert!(tool_call_columns.contains(&"duration_ms".to_string()));
        assert!(!tool_call_columns.contains(&"output".to_string()));
        let session_columns = store
            .conn
            .prepare("PRAGMA table_info(session)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(!session_columns.contains(&"archived".to_string()));
        // v8: AUTOINCREMENT removed from message, turn_attempt, turn_usage, review.
        let autoincrement_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE type = 'table' AND sql LIKE '%AUTOINCREMENT%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(autoincrement_count, 0, "no tables should use AUTOINCREMENT in v8");
        let sqlite_sequence_exists: bool = store
            .conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_schema WHERE name = 'sqlite_sequence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!sqlite_sequence_exists, "sqlite_sequence should not exist without AUTOINCREMENT tables");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn pre_release_database_with_tables_is_rejected() {
        let tmp = std::env::temp_dir().join(format!("mu-store-old-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = tmp.join("mu.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                cwd TEXT NOT NULL,
                model TEXT NOT NULL,
                title TEXT,
                last_total_tokens INTEGER NOT NULL DEFAULT 0
             );",
        )
        .unwrap();
        drop(conn);

        let error = Store::open(&db).err().unwrap().to_string();

        assert!(error.contains("unsupported pre-release session database schema"));
        assert!(error.contains("remove sessions.db"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn schema_v6_is_migrated_without_losing_tool_calls() {
        let tmp = std::env::temp_dir().join(format!("mu-store-v6-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = tmp.join("mu.db");
        let conn = Connection::open(&db).unwrap();
        // Faithful copy of the real v6 baseline schema (commit 170b4f5).
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                cwd TEXT NOT NULL,
                model TEXT NOT NULL,
                title TEXT,
                last_total_tokens INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE message (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                reasoning_content TEXT,
                native_replay_json TEXT,
                user_content_json TEXT,
                tool_content_json TEXT,
                tool_call_id TEXT,
                tool_calls_json TEXT,
                seq INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id)
             );
             CREATE INDEX idx_message_session_seq ON message(session_id, seq);
             CREATE TABLE tool_call (
                id TEXT PRIMARY KEY,
                message_id INTEGER NOT NULL,
                tool TEXT NOT NULL,
                args TEXT NOT NULL,
                risk TEXT,
                output TEXT,
                status TEXT NOT NULL,
                FOREIGN KEY(message_id) REFERENCES message(id)
             );
             CREATE TABLE turn_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
                total_tokens INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id)
             );
             CREATE INDEX idx_turn_usage_session_id ON turn_usage(session_id);
             CREATE TABLE attachment_blob (
                id TEXT PRIMARY KEY,
                data BLOB NOT NULL,
                size INTEGER NOT NULL,
                created_at TEXT NOT NULL
             );
             CREATE TABLE review (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                tool_call_id TEXT,
                action_json TEXT NOT NULL,
                risk_level TEXT NOT NULL,
                user_auth_level TEXT NOT NULL,
                outcome TEXT NOT NULL,
                reason TEXT,
                created_at TEXT NOT NULL
             );
             INSERT INTO session VALUES (
                'session-1', 'now', 'now', '/tmp', 'test/model', NULL, 0
             );
             INSERT INTO message (
                id, session_id, role, content, seq, created_at
             ) VALUES (1, 'session-1', 'assistant', '', 0, 'now');
             INSERT INTO tool_call VALUES (
                'call-1', 1, 'bash', '{}', 'readonly', 'old output', 'ok'
             );
             INSERT INTO review (
                session_id, tool_call_id, action_json, risk_level,
                user_auth_level, outcome, reason, created_at
             ) VALUES (
                'session-1', 'call-1', '{}', 'readonly', 'reversible', 'allow', NULL, 'now'
             );
             PRAGMA user_version = 6;",
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&db).unwrap();

        let version: i32 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        let migrated: (i64, Option<i32>, Option<i64>) = store
            .conn
            .query_row(
                "SELECT message_id, exit_code, duration_ms FROM tool_call WHERE call_id = 'call-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(migrated, (1, None, None));
        let review_outcome: String = store
            .conn
            .query_row(
                "SELECT outcome FROM review WHERE session_id = 'session-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(review_outcome, "allow");
        assert!(
            store
                .conn
                .query_row(
                    "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'turn_attempt'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .is_ok()
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn schema_older_than_compatibility_baseline_is_rejected() {
        let tmp = std::env::temp_dir().join(format!("mu-store-old-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = tmp.join("mu.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA user_version = 5;").unwrap();
        drop(conn);

        let error = Store::open(&db).err().unwrap().to_string();

        assert!(error.contains("schema version 5"));
        assert!(error.contains("compatibility baseline 6"));
        assert!(error.contains("upgrade through a compatible mu release"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn newer_schema_version_requests_a_mu_upgrade() {
        let tmp = std::env::temp_dir().join(format!("mu-store-new-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = tmp.join("mu.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA user_version = 9;").unwrap();
        drop(conn);

        let error = Store::open(&db).err().unwrap().to_string();

        assert!(error.contains("schema version 9 is newer"));
        assert!(error.contains("upgrade mu"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn schema_v7_is_migrated_to_v8_preserving_audit_rows() {
        let tmp = std::env::temp_dir().join(format!("mu-store-v7-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = tmp.join("mu.db");
        let conn = Connection::open(&db).unwrap();
        // v7 = v6 baseline + tool_call execution columns + turn_attempt.
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                cwd TEXT NOT NULL,
                model TEXT NOT NULL,
                title TEXT,
                last_total_tokens INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE message (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                reasoning_content TEXT,
                native_replay_json TEXT,
                user_content_json TEXT,
                tool_content_json TEXT,
                tool_call_id TEXT,
                tool_calls_json TEXT,
                seq INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id)
             );
             CREATE INDEX idx_message_session_seq ON message(session_id, seq);
             CREATE TABLE tool_call (
                id TEXT PRIMARY KEY,
                message_id INTEGER NOT NULL,
                tool TEXT NOT NULL,
                args TEXT NOT NULL,
                risk TEXT,
                output TEXT,
                status TEXT NOT NULL,
                exit_code INTEGER,
                duration_ms INTEGER,
                FOREIGN KEY(message_id) REFERENCES message(id)
             );
             CREATE TABLE turn_attempt (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                model TEXT NOT NULL,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                outcome TEXT NOT NULL,
                error_class TEXT,
                error TEXT,
                partial_output TEXT,
                provider_request_count INTEGER NOT NULL DEFAULT 0,
                iteration_count INTEGER NOT NULL DEFAULT 0,
                retry_count INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER,
                context_tokens INTEGER NOT NULL DEFAULT 0,
                FOREIGN KEY(session_id) REFERENCES session(id)
             );
             CREATE INDEX idx_turn_attempt_session_id ON turn_attempt(session_id);
             CREATE TABLE turn_usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
                total_tokens INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id)
             );
             CREATE INDEX idx_turn_usage_session_id ON turn_usage(session_id);
             CREATE TABLE attachment_blob (
                id TEXT PRIMARY KEY,
                data BLOB NOT NULL,
                size INTEGER NOT NULL,
                created_at TEXT NOT NULL
             );
             CREATE TABLE review (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                tool_call_id TEXT,
                action_json TEXT NOT NULL,
                risk_level TEXT NOT NULL,
                user_auth_level TEXT NOT NULL,
                outcome TEXT NOT NULL,
                reason TEXT,
                created_at TEXT NOT NULL
             );
             INSERT INTO session VALUES (
                'session-1', 'now', 'now', '/tmp', 'test/model', NULL, 0
             );
             INSERT INTO message (
                id, session_id, role, content, seq, created_at
             ) VALUES (1, 'session-1', 'assistant', '', 0, 'now');
             INSERT INTO tool_call VALUES (
                'call-1', 1, 'bash', '{}', 'readonly', 'old output', 'ok', 3, 250
             );
             INSERT INTO review (
                session_id, tool_call_id, action_json, risk_level,
                user_auth_level, outcome, reason, created_at
             ) VALUES (
                'session-1', 'call-1', '{}', 'destructive', 'reversible', 'deny', 'too risky', 'now'
             );
             PRAGMA user_version = 7;",
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&db).unwrap();

        let version: i32 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, CURRENT_SCHEMA_VERSION);
        let migrated: (i64, String, Option<i32>, Option<i64>) = store
            .conn
            .query_row(
                "SELECT message_id, status, exit_code, duration_ms
                 FROM tool_call WHERE call_id = 'call-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(migrated, (1, "ok".into(), Some(3), Some(250)));
        let review: (String, Option<String>) = store
            .conn
            .query_row(
                "SELECT outcome, reason FROM review WHERE tool_call_id = 'call-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(review, ("deny".into(), Some("too risky".into())));
        let ordering_index_is_unique: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_index_list('message')
                 WHERE name = 'idx_message_session_seq' AND \"unique\" = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ordering_index_is_unique, 1);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn reused_provider_call_ids_do_not_clobber_audit_rows() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);

        // Two turns whose provider reuses the same call id ("call_0" style).
        for output in ["first", "second"] {
            let assistant_id = store
                .append_message(
                    &session.id,
                    &Message::Assistant {
                        content: None,
                        reasoning_content: None,
                        native_replay: None,
                        tool_calls: Some(vec![ToolCall {
                            id: "call_0".into(),
                            function: FunctionCall {
                                name: "bash".into(),
                                arguments: "{}".into(),
                            },
                        }]),
                    },
                )
                .unwrap();
            store
                .persist_tool_result(
                    &session.id,
                    ToolCallRecord {
                        message_id: assistant_id,
                        call_id: "call_0",
                        tool: "bash",
                        args: "{}",
                        risk: Some("readonly"),
                        status: "ok",
                        exit_code: Some(0),
                        duration_ms: Some(1),
                    },
                    output,
                    &[],
                )
                .unwrap();
        }

        let audit_rows: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM tool_call WHERE call_id = 'call_0'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(audit_rows, 2);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn insert_summary_before_shifts_multiple_rows_without_collisions() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        for text in ["one", "two", "three", "four"] {
            store
                .append_message(
                    &session.id,
                    &Message::User {
                        content: UserContent::Text(text.into()),
                    },
                )
                .unwrap();
        }

        // Shift the last three rows (seq 2..4) up by one; under the UNIQUE
        // ordering index a naive `seq = seq + 1` would collide row-by-row.
        store
            .insert_summary_before(&session.id, "summary text", 2)
            .unwrap();

        let records = store.message_records_from_seq(&session.id, 0).unwrap();
        let log: Vec<(i64, String, String)> = records
            .into_iter()
            .map(|record| (record.seq, record.role, record.content))
            .collect();
        assert_eq!(
            log,
            vec![
                (0, "system".into(), "system".into()),
                (1, "user".into(), "one".into()),
                (2, "summary".into(), "summary text".into()),
                (3, "user".into(), "two".into()),
                (4, "user".into(), "three".into()),
                (5, "user".into(), "four".into()),
            ]
        );
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let (store, tmp) = temp_store();

        let error = store
            .append_message(
                "no-such-session",
                &Message::User {
                    content: UserContent::Text("orphan".into()),
                },
            )
            .unwrap_err();

        assert!(error.to_string().to_lowercase().contains("foreign key"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn create_session_seeded_writes_system_and_environment_in_one_step() {
        let (store, tmp) = temp_store();

        let session = store
            .create_session_seeded("/tmp", "test/model", "system prompt", "[environment] cwd")
            .unwrap();

        let records = store.message_records_from_seq(&session.id, 0).unwrap();
        let log: Vec<(i64, String, String)> = records
            .into_iter()
            .map(|record| (record.seq, record.role, record.content))
            .collect();
        assert_eq!(
            log,
            vec![
                (0, "system".into(), "system prompt".into()),
                (1, "user".into(), "[environment] cwd".into()),
            ]
        );
        assert_eq!(store.system_prompt(&session.id).unwrap(), "system prompt");
        // The lone environment seed leaves the session clean (no real turn yet).
        assert!(store.is_session_clean(&session.id).unwrap());
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn reloads_full_user_content_with_attachments_and_deduplicates_blobs() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let expected_data = vec![1, 2, 3, 4];

        for filename in ["first.png", "second.png"] {
            store
                .append_message(
                    &session.id,
                    &Message::User {
                        content: UserContent::Parts(vec![
                            ContentPart::Text {
                                text: "describe this".to_string(),
                            },
                            ContentPart::Attachment {
                                attachment: Attachment {
                                    filename: filename.into(),
                                    media_type: "image/png".into(),
                                    data: expected_data.clone(),
                                },
                            },
                        ]),
                    },
                )
                .unwrap();
        }

        let messages = store.load_context_messages(&session.id).unwrap();
        let Message::User {
            content: UserContent::Parts(parts),
        } = &messages[1]
        else {
            panic!("expected user parts");
        };

        assert!(matches!(
            &parts[0],
            ContentPart::Text { text } if text == "describe this"
        ));
        assert!(matches!(
            &parts[1],
            ContentPart::Attachment { attachment }
                if attachment.filename == "first.png"
                    && attachment.media_type == "image/png"
                    && attachment.data == expected_data
        ));
        let blob_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM attachment_blob", [], |row| row.get(0))
            .unwrap();
        assert_eq!(blob_count, 1);

        store
            .conn
            .execute("UPDATE attachment_blob SET data = X'FF'", [])
            .unwrap();
        let error = store.load_context_messages(&session.id).unwrap_err();
        assert!(error.to_string().contains("corrupt attachment blob"));

        store
            .conn
            .execute("DELETE FROM attachment_blob", [])
            .unwrap();
        let error = store.load_context_messages(&session.id).unwrap_err();
        assert!(error.to_string().contains("missing attachment blob"));

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn reloads_tool_image_artifacts_and_reuses_attachment_blobs() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let data = b"\x89PNG\r\n\x1a\nrest".to_vec();
        let artifact = ToolArtifact {
            attachment: Attachment {
                filename: "tool.png".into(),
                media_type: "image/png".into(),
                data: data.clone(),
            },
            detail: ImageDetail::High,
        };
        store
            .append_message(
                &session.id,
                &Message::Tool {
                    content: "Viewed image".into(),
                    artifacts: vec![artifact.clone()],
                    tool_call_id: "call-image".into(),
                },
            )
            .unwrap();
        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Parts(vec![ContentPart::Attachment {
                        attachment: artifact.attachment.clone(),
                    }]),
                },
            )
            .unwrap();

        let messages = store.load_context_messages(&session.id).unwrap();
        assert!(matches!(
            &messages[1],
            Message::Tool { artifacts, .. }
                if artifacts == &vec![artifact]
        ));
        let blob_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM attachment_blob", [], |row| row.get(0))
            .unwrap();
        assert_eq!(blob_count, 1);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn reloads_reasoning_content_without_normalizing_whitespace() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let reasoning = "  first line\n\tsecond line  ".to_string();

        store
            .append_message(
                &session.id,
                &Message::Assistant {
                    content: Some("tool request".into()),
                    reasoning_content: Some(reasoning.clone()),
                    native_replay: None,
                    tool_calls: None,
                },
            )
            .unwrap();

        let messages = store.load_context_messages(&session.id).unwrap();
        assert!(matches!(
            &messages[1],
            Message::Assistant {
                reasoning_content: Some(saved),
                ..
            } if saved == &reasoning
        ));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn reloads_exact_native_responses_items_and_origin() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let native = NativeReplay {
            endpoint: "https://api.test/v1/responses".into(),
            model: "gpt-test".into(),
            payload: NativeReplayPayload::ResponsesOutput(vec![serde_json::json!({
                "type": "reasoning", "id": "rs_1", "encrypted_content": "opaque"
            })]),
        };
        store
            .append_message(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    native_replay: Some(native.clone()),
                },
            )
            .unwrap();

        let messages = store.load_context_messages(&session.id).unwrap();
        assert!(matches!(&messages[1], Message::Assistant {
            native_replay: Some(saved), ..
        } if saved == &native));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn list_sessions_includes_every_session() {
        let (store, tmp) = temp_store();
        let first = store.create_session("/tmp", "first-model").unwrap();
        let second = store.create_session("/tmp", "second-model").unwrap();

        let sessions = store.list_sessions(20).unwrap();

        let ids = sessions
            .iter()
            .map(|(session, _)| session.id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&first.id.as_str()));
        assert!(ids.contains(&second.id.as_str()));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn normalize_interrupted_tail_synthesizes_missing_tool_results() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);

        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Text("run".into()),
                },
            )
            .unwrap();
        let assistant_id = store
            .append_message(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    native_replay: None,
                    tool_calls: Some(vec![
                        ToolCall {
                            id: "call-a".into(),
                            function: FunctionCall {
                                name: "bash".into(),
                                arguments:
                                    "{\"title\":\"a\",\"risk\":\"readonly\",\"command\":\"echo a\"}"
                                        .into(),
                            },
                        },
                        ToolCall {
                            id: "call-b".into(),
                            function: FunctionCall {
                                name: "bash".into(),
                                arguments:
                                    "{\"title\":\"b\",\"risk\":\"readonly\",\"command\":\"echo b\"}"
                                        .into(),
                            },
                        },
                    ]),
                },
            )
            .unwrap();
        // Only the first call finished cleanly.
        store
            .persist_tool_result(
                &session.id,
                ToolCallRecord {
                    message_id: assistant_id,
                    call_id: "call-a",
                    tool: "bash",
                    args: "{\"title\":\"a\",\"risk\":\"readonly\",\"command\":\"echo a\"}",
                    risk: Some("readonly"),
                    status: "ok",
                    exit_code: Some(0),
                    duration_ms: Some(12),
                },
                "a",
                &[],
            )
            .unwrap();
        let execution: (Option<i32>, Option<i64>) = store
            .conn
            .query_row(
                "SELECT exit_code, duration_ms FROM tool_call WHERE call_id = 'call-a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(execution, (Some(0), Some(12)));

        // Session is unclean (last message is a tool result, turn not finished).
        assert!(!store.is_session_clean(&session.id).unwrap());

        let synthesized = store.normalize_interrupted_tail(&session.id).unwrap();
        assert_eq!(synthesized, 1);
        // Idempotent: a second pass synthesizes nothing.
        assert_eq!(store.normalize_interrupted_tail(&session.id).unwrap(), 0);

        let tool_messages = store
            .load_context_messages(&session.id)
            .unwrap()
            .into_iter()
            .filter_map(|message| match message {
                Message::Tool {
                    tool_call_id,
                    content,
                    ..
                } => Some((tool_call_id, content)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_messages.len(), 2);
        // The cleanly finished result is preserved, not clobbered.
        assert_eq!(tool_messages[0], ("call-a".into(), "a".into()));
        assert_eq!(tool_messages[1].0, "call-b".to_string());
        assert!(tool_messages[1].1.contains("interrupted"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn lone_environment_seed_session_is_clean() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Text("[environment]\ncwd: /tmp".into()),
                },
            )
            .unwrap();
        // A session whose only non-system message is the synthetic env seed is clean.
        assert!(store.is_session_clean(&session.id).unwrap());

        // A completed assistant reply is clean.
        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Text("hi".into()),
                },
            )
            .unwrap();
        assert!(!store.is_session_clean(&session.id).unwrap());
        store
            .append_message(
                &session.id,
                &Message::Assistant {
                    content: Some("hello".into()),
                    reasoning_content: None,
                    native_replay: None,
                    tool_calls: None,
                },
            )
            .unwrap();
        assert!(store.is_session_clean(&session.id).unwrap());
        let _ = std::fs::remove_dir_all(tmp);
    }
}
