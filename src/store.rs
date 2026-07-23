use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::provider::{
    Attachment, ContentPart, ImageDetail, Message, ToolAttachment, ToolCall, Usage, UserContent,
    approx_tokens,
};
use crate::tools::BashRisk;

pub const SESSION_DB_ENV: &str = "MU_SESSION_DB";
pub const BASH_CALL_ID_ENV: &str = "MU_BASH_CALL_ID";
pub const SESSION_OWNER_PID_ENV: &str = "MU_SESSION_OWNER_PID";
const MAX_BASH_ATTACHMENTS: i64 = 8;
const SESSION_ID_RETRIES: usize = 16;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub title: Option<String>,
    pub last_context_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub cwd: String,
    pub title: Option<String>,
    pub last_context_tokens: u64,
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
    pub kind: String,
    pub content: String,
    pub bash_calls: Vec<ToolCall>,
    pub seq: i64,
}

pub struct BashResultRecord<'a> {
    pub bash_call_id: i64,
    pub outcome: &'a str,
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
    pub bash_call_id: i64,
    pub risk_level: &'a str,
    pub user_auth_level: &'a str,
    pub outcome: &'a str,
    pub reason: Option<&'a str>,
}

pub struct Store {
    conn: Connection,
    path: Option<PathBuf>,
}

#[derive(Debug)]
pub struct SessionBusy;

impl std::fmt::Display for SessionBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session busy")
    }
}

impl std::error::Error for SessionBusy {}

const CURRENT_SCHEMA_VERSION: i32 = 9;
const COMPATIBLE_SCHEMA_BASELINE: i32 = 6;

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        cwd: row.get(1)?,
        model: row.get(2)?,
        title: row.get(3)?,
        last_context_tokens: row.get::<_, i64>(4)? as u64,
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
            path: Some(path.to_path_buf()),
        };
        store.ensure_schema()?;
        store.enable_foreign_keys()?;
        Ok(store)
    }

    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory SQLite database")?;
        configure_connection(&conn)?;
        let store = Self { conn, path: None };
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
                8 => self.migrate_schema_v8_to_v9()?,
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
                last_context_tokens INTEGER NOT NULL DEFAULT 0,
                owner_pid INTEGER CHECK(owner_pid IS NULL OR owner_pid > 0)
            );
            CREATE TABLE IF NOT EXISTS message (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL CHECK(kind IN (
                    'system', 'user', 'assistant', 'bash_result', 'summary'
                )),
                content TEXT NOT NULL,
                reasoning_content TEXT,
                native_replay_json TEXT,
                user_content_json TEXT,
                bash_call_id INTEGER,
                bash_outcome TEXT CHECK(bash_outcome IN (
                    'completed', 'error', 'interrupted'
                )),
                bash_exit_code INTEGER,
                bash_duration_ms INTEGER CHECK(bash_duration_ms IS NULL OR bash_duration_ms >= 0),
                seq INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id),
                FOREIGN KEY(bash_call_id) REFERENCES bash_call(id),
                CHECK(
                    (kind = 'bash_result' AND bash_call_id IS NOT NULL AND bash_outcome IS NOT NULL)
                    OR
                    (kind != 'bash_result' AND bash_call_id IS NULL AND bash_outcome IS NULL
                        AND bash_exit_code IS NULL AND bash_duration_ms IS NULL)
                ),
                CHECK(
                    bash_outcome IS NULL
                    OR (bash_outcome = 'completed') = (bash_exit_code IS NOT NULL)
                )
            );
            CREATE UNIQUE INDEX IF NOT EXISTS idx_message_session_seq ON message(session_id, seq);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_message_bash_result
                ON message(bash_call_id) WHERE bash_call_id IS NOT NULL;
            CREATE TABLE IF NOT EXISTS bash_call (
                id INTEGER PRIMARY KEY,
                assistant_message_id INTEGER NOT NULL,
                position INTEGER NOT NULL CHECK(position >= 0),
                provider_call_id TEXT NOT NULL,
                arguments TEXT NOT NULL,
                declared_risk TEXT CHECK(declared_risk IS NULL OR declared_risk IN (
                    'readonly', 'reversible', 'destructive'
                )),
                UNIQUE(assistant_message_id, position),
                UNIQUE(assistant_message_id, provider_call_id),
                FOREIGN KEY(assistant_message_id) REFERENCES message(id)
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
            CREATE INDEX IF NOT EXISTS idx_session_updated_at ON session(updated_at);
            CREATE TABLE IF NOT EXISTS attachment_blob (
                id TEXT PRIMARY KEY,
                data BLOB NOT NULL,
                size INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                CHECK(size = length(data))
            );
            CREATE TABLE IF NOT EXISTS bash_attachment (
                bash_call_id INTEGER NOT NULL,
                position INTEGER NOT NULL CHECK(position >= 0),
                blob_id TEXT NOT NULL,
                filename TEXT NOT NULL,
                media_type TEXT NOT NULL,
                detail TEXT NOT NULL CHECK(detail IN ('auto', 'low', 'high', 'original')),
                PRIMARY KEY(bash_call_id, position),
                FOREIGN KEY(bash_call_id) REFERENCES bash_call(id),
                FOREIGN KEY(blob_id) REFERENCES attachment_blob(id)
            );
            CREATE TABLE IF NOT EXISTS bash_review (
                bash_call_id INTEGER PRIMARY KEY,
                risk_level TEXT NOT NULL,
                auth_level TEXT NOT NULL,
                outcome TEXT NOT NULL CHECK(outcome IN ('allow', 'deny')),
                reason TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(bash_call_id) REFERENCES bash_call(id)
            );
            PRAGMA user_version = 9;",
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

    /// v9 specializes the durable function-call model for Bash. Assistant
    /// messages and their immutable claims are represented separately, while a
    /// `bash_result` message is itself the unique completion record.
    fn migrate_schema_v8_to_v9(&self) -> Result<()> {
        let non_bash_calls: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tool_call WHERE tool != 'bash'",
            [],
            |row| row.get(0),
        )?;
        if non_bash_calls != 0 {
            bail!("cannot migrate session database containing non-Bash tool calls")
        }
        let has_legacy_context_column: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('session') WHERE name = 'last_total_tokens'",
            [],
            |row| row.get(0),
        )?;
        let tx = self.conn.unchecked_transaction()?;
        let rename = if has_legacy_context_column {
            "ALTER TABLE session RENAME COLUMN last_total_tokens TO last_context_tokens;"
        } else {
            ""
        };
        tx.execute_batch(&format!(
            "{rename}
             ALTER TABLE session ADD COLUMN owner_pid INTEGER CHECK(owner_pid IS NULL OR owner_pid > 0);
             ALTER TABLE message RENAME TO message_v8;
             DROP INDEX idx_message_session_seq;
             CREATE TABLE bash_call (
                id INTEGER PRIMARY KEY,
                assistant_message_id INTEGER NOT NULL,
                position INTEGER NOT NULL CHECK(position >= 0),
                provider_call_id TEXT NOT NULL,
                arguments TEXT NOT NULL,
                declared_risk TEXT CHECK(declared_risk IS NULL OR declared_risk IN (
                    'readonly', 'reversible', 'destructive'
                )),
                UNIQUE(assistant_message_id, position),
                UNIQUE(assistant_message_id, provider_call_id),
                FOREIGN KEY(assistant_message_id) REFERENCES message(id)
             );
             INSERT INTO bash_call (
                id, assistant_message_id, position, provider_call_id, arguments, declared_risk
             ) SELECT
                tc.id,
                tc.message_id,
                ROW_NUMBER() OVER (PARTITION BY tc.message_id ORDER BY tc.id) - 1,
                tc.call_id,
                tc.args,
                tc.risk
             FROM tool_call tc;
             CREATE TEMP TABLE bash_result_v9 AS
             SELECT
                result.id AS message_id,
                tc.id AS bash_call_id,
                CASE tc.status
                    WHEN 'ok' THEN 'completed'
                    WHEN 'interrupted' THEN 'interrupted'
                    ELSE 'error'
                END AS outcome,
                CASE tc.status WHEN 'ok' THEN COALESCE(tc.exit_code, 0) END AS exit_code,
                tc.duration_ms
             FROM message_v8 result
             JOIN tool_call tc ON tc.id = (
                SELECT candidate.id
                FROM tool_call candidate
                JOIN message_v8 assistant ON assistant.id = candidate.message_id
                WHERE result.role = 'tool'
                  AND candidate.call_id = result.tool_call_id
                  AND assistant.session_id = result.session_id
                  AND assistant.seq < result.seq
                ORDER BY assistant.seq DESC
                LIMIT 1
             )
             WHERE result.role = 'tool';
             CREATE TABLE message (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL CHECK(kind IN (
                    'system', 'user', 'assistant', 'bash_result', 'summary'
                )),
                content TEXT NOT NULL,
                reasoning_content TEXT,
                native_replay_json TEXT,
                user_content_json TEXT,
                bash_call_id INTEGER,
                bash_outcome TEXT CHECK(bash_outcome IN (
                    'completed', 'error', 'interrupted'
                )),
                bash_exit_code INTEGER,
                bash_duration_ms INTEGER CHECK(bash_duration_ms IS NULL OR bash_duration_ms >= 0),
                seq INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id),
                FOREIGN KEY(bash_call_id) REFERENCES bash_call(id),
                CHECK(
                    (kind = 'bash_result' AND bash_call_id IS NOT NULL AND bash_outcome IS NOT NULL)
                    OR
                    (kind != 'bash_result' AND bash_call_id IS NULL AND bash_outcome IS NULL
                        AND bash_exit_code IS NULL AND bash_duration_ms IS NULL)
                ),
                CHECK(
                    bash_outcome IS NULL
                    OR (bash_outcome = 'completed') = (bash_exit_code IS NOT NULL)
                )
             );
             INSERT INTO message (
                id, session_id, kind, content, reasoning_content, native_replay_json,
                user_content_json, bash_call_id, bash_outcome, bash_exit_code,
                bash_duration_ms, seq, created_at
             ) SELECT
                old.id,
                old.session_id,
                CASE old.role WHEN 'tool' THEN 'bash_result' ELSE old.role END,
                old.content,
                old.reasoning_content,
                old.native_replay_json,
                old.user_content_json,
                result.bash_call_id,
                result.outcome,
                result.exit_code,
                result.duration_ms,
                old.seq,
                old.created_at
             FROM message_v8 old
             LEFT JOIN bash_result_v9 result ON result.message_id = old.id;
             CREATE UNIQUE INDEX idx_message_session_seq ON message(session_id, seq);
             CREATE UNIQUE INDEX idx_message_bash_result
                ON message(bash_call_id) WHERE bash_call_id IS NOT NULL;
             CREATE TABLE attachment_blob_v9 (
                id TEXT PRIMARY KEY,
                data BLOB NOT NULL,
                size INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                CHECK(size = length(data))
             );
             INSERT INTO attachment_blob_v9 SELECT * FROM attachment_blob;
             DROP TABLE attachment_blob;
             ALTER TABLE attachment_blob_v9 RENAME TO attachment_blob;
             CREATE TABLE bash_attachment (
                bash_call_id INTEGER NOT NULL,
                position INTEGER NOT NULL CHECK(position >= 0),
                blob_id TEXT NOT NULL,
                filename TEXT NOT NULL,
                media_type TEXT NOT NULL,
                detail TEXT NOT NULL CHECK(detail IN ('auto', 'low', 'high', 'original')),
                PRIMARY KEY(bash_call_id, position),
                FOREIGN KEY(bash_call_id) REFERENCES bash_call(id),
                FOREIGN KEY(blob_id) REFERENCES attachment_blob(id)
             );
             CREATE TABLE bash_review (
                bash_call_id INTEGER PRIMARY KEY,
                risk_level TEXT NOT NULL,
                auth_level TEXT NOT NULL,
                outcome TEXT NOT NULL CHECK(outcome IN ('allow', 'deny')),
                reason TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(bash_call_id) REFERENCES bash_call(id)
             );
             INSERT INTO bash_review (
                bash_call_id, risk_level, auth_level, outcome, reason, created_at
             ) SELECT
                bc.id, legacy.risk_level, legacy.user_auth_level, legacy.outcome,
                COALESCE(legacy.reason, ''), legacy.created_at
             FROM review legacy
             JOIN bash_call bc ON bc.id = (
                SELECT candidate.id
                FROM bash_call candidate
                JOIN message assistant ON assistant.id = candidate.assistant_message_id
                WHERE candidate.provider_call_id = legacy.tool_call_id
                  AND assistant.session_id = legacy.session_id
                  AND assistant.created_at <= legacy.created_at
                ORDER BY assistant.seq DESC
                LIMIT 1
             );
             CREATE INDEX idx_session_updated_at ON session(updated_at);
             PRAGMA user_version = 9;"
        ))
        .context("migrating session database to schema v9")?;
        migrate_v8_bash_attachments(&tx)?;
        let legacy_review_count: i64 =
            tx.query_row("SELECT COUNT(*) FROM review", [], |row| row.get(0))?;
        let migrated_review_count: i64 =
            tx.query_row("SELECT COUNT(*) FROM bash_review", [], |row| row.get(0))?;
        if legacy_review_count != migrated_review_count {
            bail!("cannot unambiguously link every legacy guardrail review to a Bash call")
        }
        tx.execute_batch(
            "DROP TABLE review;
             DROP TABLE tool_call;
             DROP TABLE message_v8;
             DROP TABLE bash_result_v9;",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Test-only: production sessions are created via `create_session_seeded`
    /// so the session row never exists without its system prompt.
    #[cfg(test)]
    pub fn create_session(&self, cwd: &str, model: &str) -> Result<Session> {
        for _ in 0..SESSION_ID_RETRIES {
            let id = crate::random::session_id()?;
            let now = now_rfc3339();
            match self.conn.execute(
                "INSERT INTO session (id, created_at, updated_at, cwd, model, title, last_context_tokens)
                 VALUES (?1, ?2, ?2, ?3, ?4, NULL, 0)",
                params![id, now, cwd, model],
            ) {
                Ok(_) => {
                    return Ok(Session {
                        id,
                        cwd: cwd.into(),
                        model: model.into(),
                        title: None,
                        last_context_tokens: 0,
                    });
                }
                Err(error) if is_session_id_conflict(&error) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        bail!("could not allocate a unique session id")
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
        for _ in 0..SESSION_ID_RETRIES {
            let id = crate::random::session_id()?;
            let now = now_rfc3339();
            let tx = self.conn.unchecked_transaction()?;
            match tx.execute(
                "INSERT INTO session (id, created_at, updated_at, cwd, model, title, last_context_tokens)
                 VALUES (?1, ?2, ?2, ?3, ?4, NULL, 0)",
                params![id, now, cwd, model],
            ) {
                Ok(_) => {
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
                    return Ok(Session {
                        id,
                        cwd: cwd.into(),
                        model: model.into(),
                        title: None,
                        last_context_tokens: 0,
                    });
                }
                Err(error) if is_session_id_conflict(&error) => {
                    drop(tx);
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("could not allocate a unique session id")
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_context_tokens
             FROM session WHERE id = ?1",
        )?;
        let row = stmt.query_row(params![id], session_from_row).optional()?;
        Ok(row)
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<(Session, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_context_tokens, updated_at
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
            // turn_count = completed assistant replies with no Bash claims.
            // Counting user rows would be wrong: a session has the environment
            // seed, the actual prompts, AND any cwd-change reminders appended
            // mid-session — all as role='user'. Interrupted turns (assistant
            // row with Bash claims) are intentionally not counted.
            "SELECT
                s.id, s.created_at, s.updated_at, s.cwd, s.title,
                s.last_context_tokens,
                COUNT(m.id) AS message_count,
                COALESCE(SUM(CASE
                    WHEN m.kind = 'assistant'
                     AND NOT EXISTS (
                        SELECT 1 FROM bash_call bc WHERE bc.assistant_message_id = m.id
                     )
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
                    last_context_tokens: row.get::<_, i64>(5)? as u64,
                    message_count: row.get::<_, i64>(6)? as u64,
                    turn_count: row.get::<_, i64>(7)? as u64,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn latest_session(&self) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_context_tokens
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
                "UPDATE session SET updated_at = ?1, last_context_tokens = ?2,
                 title = COALESCE(title, ?3), model = ?4 WHERE id = ?5",
                params![now, context_tokens as i64, t, model, id],
            )?;
        } else {
            tx.execute(
                "UPDATE session SET updated_at = ?1, last_context_tokens = ?2,
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
    /// completed assistant reply with no Bash calls. A trailing user prompt,
    /// Bash result, or assistant message carrying Bash calls means the
    /// turn was interrupted. A session whose only message is the synthetic
    /// environment seed (a lone leading user message) is also clean, since no
    /// real turn has run yet. Derived purely from the log so it can never drift
    /// out of sync with the messages (unlike a stored flag).
    pub fn is_session_clean(&self, session_id: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind FROM message
             WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
        )?;
        let last: Option<(i64, String)> = stmt
            .query_row(params![session_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let Some((message_id, kind)) = last else {
            return Ok(true);
        };
        match kind.as_str() {
            "assistant" => {
                let count: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM bash_call WHERE assistant_message_id = ?1",
                    params![message_id],
                    |row| row.get(0),
                )?;
                Ok(count == 0)
            }
            "summary" => Ok(true),
            "user" => {
                let count: i64 = self.conn.query_row(
                    "SELECT COUNT(*) FROM message WHERE session_id = ?1 AND kind != 'system'",
                    params![session_id],
                    |row| row.get(0),
                )?;
                Ok(count <= 1)
            }
            "system" => Ok(true),
            _ => Ok(false),
        }
    }

    /// Make an interrupted turn's history API-valid: every Bash claim in the
    /// most recent assistant call message must be followed by a Bash result
    /// result. Calls that finished keep their real result; result-less calls get
    /// a synthesized interrupted result (see `INTERRUPTED_TOOL_RESULT`).
    /// Idempotent — a no-op once the latest tool-call message is fully answered,
    /// so it is safe to call before every turn/retry. Returns the number of
    /// results synthesized.
    pub fn normalize_interrupted_tail(&self, session_id: &str) -> Result<usize> {
        let assistant_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT m.id
                 FROM message m
                 WHERE m.session_id = ?1 AND m.kind = 'assistant'
                   AND EXISTS (
                       SELECT 1 FROM bash_call bc WHERE bc.assistant_message_id = m.id
                   )
                 ORDER BY m.seq DESC LIMIT 1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(assistant_id) = assistant_id else {
            return Ok(0);
        };
        let mut stmt = self.conn.prepare(
            "SELECT bc.id
             FROM bash_call bc
             LEFT JOIN message result ON result.bash_call_id = bc.id
             WHERE bc.assistant_message_id = ?1 AND result.id IS NULL
             ORDER BY bc.position",
        )?;
        let unanswered = stmt
            .query_map(params![assistant_id], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut synthesized = 0;
        for bash_call_id in unanswered {
            self.persist_bash_result(
                session_id,
                BashResultRecord {
                    bash_call_id,
                    outcome: "interrupted",
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
            "SELECT id, kind, content, seq
             FROM message
             WHERE session_id = ?1 AND seq >= ?2
             ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id, start_seq], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, kind, content, seq)| {
                Ok(MessageRecord {
                    bash_calls: if kind == "assistant" {
                        load_bash_calls(&self.conn, id)?
                    } else {
                        Vec::new()
                    },
                    kind,
                    content,
                    seq,
                })
            })
            .collect()
    }

    pub fn load_context_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let summary_seq = self.latest_summary_seq(session_id)?;
        let start_seq = summary_seq.unwrap_or(-1);
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.kind, m.content, m.reasoning_content, m.user_content_json,
                    m.native_replay_json, m.bash_call_id, bc.provider_call_id
             FROM message m
             LEFT JOIN bash_call bc ON bc.id = m.bash_call_id
             WHERE m.session_id = ?1 AND m.seq > ?2 AND m.kind NOT IN ('system', 'summary')
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
                message_id,
                kind,
                content,
                reasoning_content,
                user_content_json,
                native_replay_json,
                bash_call_id,
                provider_call_id,
            ) = row?;
            match kind.as_str() {
                "user" => messages.push(Message::User {
                    content: load_user_content(&self.conn, content, user_content_json)?,
                }),
                "assistant" => {
                    let tool_calls = load_bash_calls(&self.conn, message_id)?;
                    messages.push(Message::Assistant {
                        content: if content.is_empty() {
                            None
                        } else {
                            Some(content)
                        },
                        reasoning_content,
                        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
                        native_replay: native_replay_json
                            .as_deref()
                            .and_then(|json| serde_json::from_str(json).ok()),
                    });
                }
                "bash_result" => messages.push(Message::Tool {
                    content,
                    attachments: load_bash_attachments(
                        &self.conn,
                        bash_call_id.context("Bash result is missing its call identity")?,
                    )?,
                    tool_call_id: provider_call_id
                        .context("Bash result is missing its provider call identity")?,
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
             WHERE session_id = ?1 AND kind = 'system'
             ORDER BY seq ASC LIMIT 1",
        )?;
        stmt.query_row(params![session_id], |row| row.get(0))
            .optional()?
            .ok_or_else(|| anyhow::anyhow!("session is missing persisted system prompt"))
    }

    fn latest_summary_seq(&self, session_id: &str) -> Result<Option<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT seq FROM message WHERE session_id = ?1 AND kind = 'summary' ORDER BY seq DESC LIMIT 1",
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
        self.append_message_with_bash_calls(session_id, message)
            .map(|(message_id, _)| message_id)
    }

    /// Persist a provider response and all of its Bash execution claims as one
    /// atomic log mutation. Returned ids correspond positionally to the
    /// assistant message's calls and are the only identities execution uses.
    pub fn append_message_with_bash_calls(
        &self,
        session_id: &str,
        message: &Message,
    ) -> Result<(i64, Vec<i64>)> {
        let now = now_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        let (message_id, bash_call_ids) = insert_message_in(&tx, session_id, message, &now)?;
        tx.commit()?;
        Ok((message_id, bash_call_ids))
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
            "INSERT INTO message (session_id, kind, content, seq, created_at) VALUES (?1, 'summary', ?2, ?3, ?4)",
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
            "INSERT INTO message (session_id, kind, content, seq, created_at) VALUES (?1, 'summary', ?2, ?3, ?4)",
            params![session_id, content, seq, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn append_bash_attachment(
        &self,
        bash_call_id: i64,
        owner_pid: i64,
        attachment: &Attachment,
        detail: ImageDetail,
    ) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let state: Option<(Option<i64>, bool)> = tx
            .query_row(
                "SELECT session.owner_pid,
                        EXISTS(
                            SELECT 1 FROM message result
                            WHERE result.bash_call_id = bash_call.id
                        )
                 FROM bash_call
                 JOIN message assistant ON assistant.id = bash_call.assistant_message_id
                 JOIN session ON session.id = assistant.session_id
                 WHERE bash_call.id = ?1",
                params![bash_call_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((actual_owner, has_result)) = state else {
            bail!("Bash attachment sink does not exist")
        };
        if actual_owner != Some(owner_pid) {
            bail!("Bash attachment sink is not owned by this Mu process")
        }
        if has_result {
            bail!("Bash attachment sink is already closed")
        }
        persist_bash_attachment_in(
            &tx,
            bash_call_id,
            &ToolAttachment {
                attachment: attachment.clone(),
                detail,
            },
            &now_rfc3339(),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn persist_bash_result(
        &self,
        session_id: &str,
        record: BashResultRecord<'_>,
        content: &str,
        attachments: &[ToolAttachment],
    ) -> Result<(i64, Vec<ToolAttachment>)> {
        let now = now_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        let call_session_id: String = tx
            .query_row(
                "SELECT assistant.session_id
                 FROM bash_call bc
                 JOIN message assistant ON assistant.id = bc.assistant_message_id
                 WHERE bc.id = ?1",
                params![record.bash_call_id],
                |row| row.get(0),
            )
            .context("locating Bash claim for result persistence")?;
        if call_session_id != session_id {
            bail!("Bash claim belongs to a different session")
        }
        for attachment in attachments {
            persist_bash_attachment_in(&tx, record.bash_call_id, attachment, &now)?;
        }
        let attachments = load_bash_attachments(&tx, record.bash_call_id)?;
        let seq = next_seq_in(&tx, session_id)?;
        tx.execute(
            "INSERT INTO message (
                session_id, kind, content, bash_call_id, bash_outcome,
                bash_exit_code, bash_duration_ms, seq, created_at
             ) VALUES (?1, 'bash_result', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id,
                content,
                record.bash_call_id,
                record.outcome,
                record.exit_code,
                record.duration_ms.map(u64_to_i64),
                seq,
                now,
            ],
        )?;
        let message_id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE session SET updated_at = ?1 WHERE id = ?2",
            params![now, session_id],
        )?;
        tx.commit()?;
        Ok((message_id, attachments))
    }

    pub fn record_review(&self, record: ReviewRecord<'_>) -> Result<()> {
        let now = now_rfc3339();
        self.conn.execute(
            "INSERT INTO bash_review (
                bash_call_id, risk_level, auth_level, outcome, reason, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                record.bash_call_id,
                record.risk_level,
                record.user_auth_level,
                record.outcome,
                record.reason.unwrap_or(""),
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

    pub fn database_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn acquire_session_lock(&self, session_id: &str) -> Result<SessionLock<'_>> {
        let pid = i64::from(std::process::id());
        let changed = self.conn.execute(
            "UPDATE session SET owner_pid = ?1 WHERE id = ?2 AND owner_pid IS NULL",
            params![pid, session_id],
        )?;
        if changed == 1 {
            return Ok(SessionLock {
                store: self,
                session_id: session_id.to_string(),
                pid,
            });
        }

        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let owner: Option<Option<i64>> = tx
            .query_row(
                "SELECT owner_pid FROM session WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(owner) = owner else {
            bail!("session not found: {session_id}");
        };
        if let Some(owner) = owner
            && process_is_alive(owner)?
        {
            return Err(anyhow::Error::new(SessionBusy));
        }
        let changed = tx.execute(
            "UPDATE session SET owner_pid = ?1 WHERE id = ?2",
            params![pid, session_id],
        )?;
        if changed != 1 {
            bail!("session disappeared while acquiring ownership: {session_id}");
        }
        tx.commit()?;
        Ok(SessionLock {
            store: self,
            session_id: session_id.to_string(),
            pid,
        })
    }

    pub fn is_session_busy(&self, session_id: &str) -> Result<bool> {
        let owner: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT owner_pid FROM session WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        match owner.flatten() {
            Some(pid) => process_is_alive(pid),
            None => Ok(false),
        }
    }

    fn release_session_lock(&self, session_id: &str, pid: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE session SET owner_pid = NULL WHERE id = ?1 AND owner_pid = ?2",
            params![session_id, pid],
        )?;
        Ok(())
    }
}

pub struct SessionLock<'a> {
    store: &'a Store,
    session_id: String,
    pid: i64,
}

impl std::fmt::Debug for SessionLock<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLock")
            .field("session_id", &self.session_id)
            .field("pid", &self.pid)
            .finish()
    }
}

impl Drop for SessionLock<'_> {
    fn drop(&mut self) {
        let _ = self.store.release_session_lock(&self.session_id, self.pid);
    }
}

fn process_is_alive(pid: i64) -> Result<bool> {
    let pid = i32::try_from(pid).context("invalid session owner PID")?;
    if pid <= 0 {
        return Ok(false);
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::EPERM) => Ok(true),
        Some(libc::ESRCH) => Ok(false),
        _ => Err(std::io::Error::last_os_error()).context("checking session owner process"),
    }
}

/// Connection-level tuning, applied before the schema check.
///
/// - WAL + `synchronous=NORMAL`: in WAL mode NORMAL fsyncs only at
///   checkpoints, so the per-message commits inside a turn stop paying an
///   fsync each. An application crash loses nothing; an OS crash or power
///   loss may lose the most recent commits but cannot corrupt the database —
///   the right trade for a conversation log.
/// - `busy_timeout`: rides out the rare cross-session write overlap under WAL
///   (same-session turns are serialized by `session.owner_pid`).
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
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

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
    #[serde(alias = "artifacts")]
    attachments: Vec<PersistedToolAttachment>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedToolAttachment {
    attachment: PersistedAttachment,
    detail: ImageDetail,
}

fn migrate_v8_bash_attachments(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT result.bash_call_id, legacy.tool_content_json
         FROM bash_result_v9 result
         JOIN message_v8 legacy ON legacy.id = result.message_id
         WHERE legacy.tool_content_json IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (bash_call_id, json) in rows {
        let persisted: PersistedToolContent = serde_json::from_str(&json)
            .context("decoding legacy Bash attachments during schema v9 migration")?;
        for (position, attachment) in persisted.attachments.into_iter().enumerate() {
            tx.execute(
                "INSERT INTO bash_attachment (
                    bash_call_id, position, blob_id, filename, media_type, detail
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    bash_call_id,
                    position as i64,
                    attachment.attachment.blob_id,
                    attachment.attachment.filename,
                    attachment.attachment.media_type,
                    attachment.detail.to_string(),
                ],
            )?;
        }
    }
    Ok(())
}

fn persist_bash_attachment_in(
    tx: &rusqlite::Transaction<'_>,
    bash_call_id: i64,
    attachment: &ToolAttachment,
    now: &str,
) -> Result<()> {
    let next: i64 = tx.query_row(
        "SELECT COALESCE(MAX(position), -1) + 1
         FROM bash_attachment WHERE bash_call_id = ?1",
        params![bash_call_id],
        |row| row.get(0),
    )?;
    if next >= MAX_BASH_ATTACHMENTS {
        bail!("Bash emitted more than {MAX_BASH_ATTACHMENTS} attachments")
    }
    let persisted = persist_attachment(tx, &attachment.attachment, now)?;
    tx.execute(
        "INSERT INTO bash_attachment (
            bash_call_id, position, blob_id, filename, media_type, detail
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            bash_call_id,
            next,
            persisted.blob_id,
            persisted.filename,
            persisted.media_type,
            attachment.detail.to_string(),
        ],
    )?;
    Ok(())
}

fn load_bash_attachments(conn: &Connection, bash_call_id: i64) -> Result<Vec<ToolAttachment>> {
    let mut stmt = conn.prepare(
        "SELECT ta.blob_id, ta.filename, ta.media_type, ta.detail, ab.data
         FROM bash_attachment ta
         JOIN attachment_blob ab ON ab.id = ta.blob_id
         WHERE ta.bash_call_id = ?1
         ORDER BY ta.position",
    )?;
    let rows = stmt.query_map(params![bash_call_id], |row| {
        let blob_id: String = row.get(0)?;
        let filename: String = row.get(1)?;
        let media_type: String = row.get(2)?;
        let detail: String = row.get(3)?;
        let data: Vec<u8> = row.get(4)?;
        Ok((blob_id, filename, media_type, detail, data))
    })?;
    let mut attachments = Vec::new();
    for row in rows {
        let (blob_id, filename, media_type, detail, data) = row?;
        if attachment_blob_id(&data) != blob_id {
            bail!("corrupt attachment blob {blob_id}")
        }
        let detail = match detail.as_str() {
            "auto" => ImageDetail::Auto,
            "low" => ImageDetail::Low,
            "high" => ImageDetail::High,
            "original" => ImageDetail::Original,
            other => bail!("invalid Bash attachment detail {other}"),
        };
        attachments.push(ToolAttachment {
            attachment: Attachment {
                filename,
                media_type,
                data,
            },
            detail,
        });
    }
    Ok(attachments)
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

fn is_session_id_conflict(error: &rusqlite::Error) -> bool {
    error
        .to_string()
        .contains("UNIQUE constraint failed: session.id")
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
) -> Result<(i64, Vec<i64>)> {
    let seq = next_seq_in(tx, session_id)?;
    match message {
        Message::User { content } => {
            let user_content_json = persist_user_content(tx, content, now)?;
            tx.execute(
                "INSERT INTO message (session_id, kind, content, user_content_json, seq, created_at)
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
            let calls = tool_calls.as_deref().unwrap_or_default();
            for call in calls {
                if call.function.name != "bash" {
                    bail!(
                        "provider requested unsupported function: {}",
                        call.function.name
                    )
                }
            }
            let native_json = native_replay
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            tx.execute(
                "INSERT INTO message (session_id, kind, content, reasoning_content, native_replay_json, seq, created_at)
                 VALUES (?1, 'assistant', ?2, ?3, ?4, ?5, ?6)",
                params![
                    session_id,
                    content.as_deref().unwrap_or(""),
                    reasoning_content,
                    native_json,
                    seq,
                    now
                ],
            )?;
            let message_id = tx.last_insert_rowid();
            let mut bash_call_ids = Vec::with_capacity(calls.len());
            for (position, call) in calls.iter().enumerate() {
                let risk = BashRisk::from_args_json(&call.function.arguments);
                tx.execute(
                    "INSERT INTO bash_call (
                        assistant_message_id, position, provider_call_id, arguments, declared_risk
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        message_id,
                        position as i64,
                        call.id,
                        call.function.arguments,
                        risk.map(BashRisk::as_str),
                    ],
                )?;
                bash_call_ids.push(tx.last_insert_rowid());
            }
            return Ok((message_id, bash_call_ids));
        }
        Message::Tool { .. } => bail!("Bash results require an internal Bash call identity"),
        Message::System { content } => {
            tx.execute(
                "INSERT INTO message (session_id, kind, content, seq, created_at)
                 VALUES (?1, 'system', ?2, ?3, ?4)",
                params![session_id, content, seq, now],
            )?;
        }
    }
    Ok((tx.last_insert_rowid(), Vec::new()))
}

fn load_bash_calls(conn: &Connection, assistant_message_id: i64) -> Result<Vec<ToolCall>> {
    let mut stmt = conn.prepare(
        "SELECT provider_call_id, arguments
         FROM bash_call
         WHERE assistant_message_id = ?1
         ORDER BY position",
    )?;
    let rows = stmt.query_map(params![assistant_message_id], |row| {
        Ok(ToolCall {
            id: row.get(0)?,
            function: crate::provider::FunctionCall {
                name: "bash".to_string(),
                arguments: row.get(1)?,
            },
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("loading Bash calls")
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

    fn bash_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            function: FunctionCall {
                name: "bash".into(),
                arguments: r#"{"title":"inspect","risk":"readonly","command":"true"}"#.into(),
            },
        }
    }

    #[test]
    fn assistant_and_bash_claims_commit_atomically() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let before: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM message WHERE session_id = ?1",
                params![session.id],
                |row| row.get(0),
            )
            .unwrap();
        let duplicate = bash_call("duplicate");

        let error = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    native_replay: None,
                    tool_calls: Some(vec![duplicate.clone(), duplicate]),
                },
            )
            .unwrap_err();

        assert!(error.to_string().contains("UNIQUE constraint failed"));
        let after: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM message WHERE session_id = ?1",
                params![session.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after, before);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn rejects_non_bash_calls_but_persists_malformed_bash_arguments() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let before: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM message", [], |row| row.get(0))
            .unwrap();
        let mut unsupported = bash_call("unsupported");
        unsupported.function.name = "python".into();
        let error = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    native_replay: None,
                    tool_calls: Some(vec![unsupported]),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("unsupported function: python"));
        let after: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM message", [], |row| row.get(0))
            .unwrap();
        assert_eq!(after, before);

        let mut malformed = bash_call("malformed");
        malformed.function.arguments = "{not json".into();
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    native_replay: None,
                    tool_calls: Some(vec![malformed]),
                },
            )
            .unwrap();
        let stored: (String, Option<String>) = store
            .conn
            .query_row(
                "SELECT arguments, declared_risk FROM bash_call WHERE id = ?1",
                params![call_ids[0]],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stored, ("{not json".into(), None));
        store
            .persist_bash_result(
                &session.id,
                BashResultRecord {
                    bash_call_id: call_ids[0],
                    outcome: "error",
                    exit_code: None,
                    duration_ms: Some(0),
                },
                "error: malformed Bash arguments",
                &[],
            )
            .unwrap();
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn bash_result_requires_a_persisted_execution_claim() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let call = bash_call("call-unclaimed");
        let (_assistant_id, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    native_replay: None,
                    tool_calls: Some(vec![call.clone()]),
                },
            )
            .unwrap();

        let error = store
            .persist_bash_result(
                &session.id,
                BashResultRecord {
                    bash_call_id: i64::MAX,
                    outcome: "completed",
                    exit_code: Some(0),
                    duration_ms: Some(1),
                },
                "unexpected",
                &[],
            )
            .unwrap_err();

        assert!(error.to_string().contains("locating Bash claim"));
        assert_eq!(call_ids.len(), 1);
        assert_eq!(store.normalize_interrupted_tail(&session.id).unwrap(), 1);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn review_records_internal_bash_call_reference() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let call = bash_call("call-reviewed");
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    native_replay: None,
                    tool_calls: Some(vec![call.clone()]),
                },
            )
            .unwrap();
        let claim_id = call_ids[0];

        store
            .record_review(ReviewRecord {
                bash_call_id: claim_id,
                risk_level: "destructive",
                user_auth_level: "reversible",
                outcome: "deny",
                reason: Some("too risky"),
            })
            .unwrap();

        let stored: (i64, String) = store
            .conn
            .query_row("SELECT bash_call_id, outcome FROM bash_review", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(stored, (claim_id, "deny".into()));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn bash_attachment_sink_is_owned_until_result_and_hydrates() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let lock = store.acquire_session_lock(&session.id).unwrap();
        let call = bash_call("call-image");
        let (assistant_id, ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    native_replay: None,
                    tool_calls: Some(vec![call.clone()]),
                },
            )
            .unwrap();
        assert_eq!(ids.len(), 1);
        let attachment = Attachment {
            filename: "tool.png".into(),
            media_type: "image/png".into(),
            data: b"png bytes".to_vec(),
        };

        let wrong_owner = store
            .append_bash_attachment(ids[0], lock.pid + 1, &attachment, ImageDetail::High)
            .unwrap_err();
        assert!(wrong_owner.to_string().contains("not owned"));
        store
            .append_bash_attachment(ids[0], lock.pid, &attachment, ImageDetail::High)
            .unwrap();

        let (message_id, hydrated) = store
            .persist_bash_result(
                &session.id,
                BashResultRecord {
                    bash_call_id: ids[0],
                    outcome: "completed",
                    exit_code: Some(0),
                    duration_ms: Some(4),
                },
                "Viewed image",
                &[],
            )
            .unwrap();
        assert!(message_id > assistant_id);
        assert_eq!(hydrated.len(), 1);
        assert_eq!(hydrated[0].attachment, attachment);
        assert_eq!(hydrated[0].detail, ImageDetail::High);

        let closed = store
            .append_bash_attachment(ids[0], lock.pid, &attachment, ImageDetail::Low)
            .unwrap_err();
        assert!(closed.to_string().contains("already closed"));
        let result: (String, i64) = store
            .conn
            .query_row(
                "SELECT bash_outcome, bash_call_id FROM message
                 WHERE bash_call_id = ?1",
                params![ids[0]],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(result, ("completed".into(), ids[0]));
        let messages = store.load_context_messages(&session.id).unwrap();
        assert!(
            matches!(messages.last(), Some(Message::Tool { attachments, .. }) if attachments == &hydrated)
        );

        drop(lock);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn interrupted_bash_result_keeps_committed_attachments_and_enforces_cap() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let lock = store.acquire_session_lock(&session.id).unwrap();
        let call = bash_call("call-many");
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    native_replay: None,
                    tool_calls: Some(vec![call.clone()]),
                },
            )
            .unwrap();
        let bash_call_id = call_ids[0];
        for index in 0..8u8 {
            store
                .append_bash_attachment(
                    bash_call_id,
                    lock.pid,
                    &Attachment {
                        filename: format!("{index}.png"),
                        media_type: "image/png".into(),
                        data: vec![index],
                    },
                    ImageDetail::Auto,
                )
                .unwrap();
        }
        let cap_error = store
            .append_bash_attachment(
                bash_call_id,
                lock.pid,
                &Attachment {
                    filename: "too-many.png".into(),
                    media_type: "image/png".into(),
                    data: vec![9],
                },
                ImageDetail::Auto,
            )
            .unwrap_err();
        assert!(cap_error.to_string().contains("more than 8 attachments"));

        assert_eq!(store.normalize_interrupted_tail(&session.id).unwrap(), 1);
        assert_eq!(store.normalize_interrupted_tail(&session.id).unwrap(), 0);
        let tool = store
            .load_context_messages(&session.id)
            .unwrap()
            .into_iter()
            .find_map(|message| match message {
                Message::Tool {
                    attachments,
                    content,
                    ..
                } => Some((attachments, content)),
                _ => None,
            })
            .unwrap();
        assert_eq!(tool.0.len(), 8);
        assert!(tool.1.contains("interrupted"));
        assert_eq!(tool.0[7].attachment.data, vec![7]);

        drop(lock);
        let _ = std::fs::remove_dir_all(tmp);
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
                "bash_attachment",
                "bash_call",
                "bash_review",
                "message",
                "session",
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
        assert!(message_columns.contains(&"bash_call_id".to_string()));
        assert!(message_columns.contains(&"bash_outcome".to_string()));
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
        let bash_call_columns = store
            .conn
            .prepare("PRAGMA table_info(bash_call)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(bash_call_columns.contains(&"provider_call_id".to_string()));
        assert!(bash_call_columns.contains(&"arguments".to_string()));
        assert!(!bash_call_columns.contains(&"status".to_string()));
        let session_columns = store
            .conn
            .prepare("PRAGMA table_info(session)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(!session_columns.contains(&"archived".to_string()));
        // v9 uses native rowids without AUTOINCREMENT.
        let autoincrement_count: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE type = 'table' AND sql LIKE '%AUTOINCREMENT%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            autoincrement_count, 0,
            "no tables should use AUTOINCREMENT in v9"
        );
        let sqlite_sequence_exists: bool = store
            .conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_schema WHERE name = 'sqlite_sequence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !sqlite_sequence_exists,
            "sqlite_sequence should not exist without AUTOINCREMENT tables"
        );
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
                last_context_tokens INTEGER NOT NULL DEFAULT 0
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
                last_context_tokens INTEGER NOT NULL DEFAULT 0
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
        let migrated: (i64, String, String, Option<String>) = store
            .conn
            .query_row(
                "SELECT assistant_message_id, provider_call_id, arguments, declared_risk
                 FROM bash_call WHERE provider_call_id = 'call-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            migrated,
            (1, "call-1".into(), "{}".into(), Some("readonly".into()))
        );
        let review_outcome: String = store
            .conn
            .query_row("SELECT outcome FROM bash_review", [], |row| row.get(0))
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
        conn.execute_batch("PRAGMA user_version = 10;").unwrap();
        drop(conn);

        let error = Store::open(&db).err().unwrap().to_string();

        assert!(error.contains("schema version 10 is newer"));
        assert!(error.contains("upgrade mu"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn schema_v7_is_migrated_to_v9_preserving_bash_audit_rows() {
        let tmp = std::env::temp_dir().join(format!("mu-store-v7-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = tmp.join("mu.db");
        let conn = Connection::open(&db).unwrap();
        // v7 = v6 baseline + tool_call execution columns + turn_attempt.
        conn.execute_batch(
            r#"CREATE TABLE session (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                cwd TEXT NOT NULL,
                model TEXT NOT NULL,
                title TEXT,
                last_context_tokens INTEGER NOT NULL DEFAULT 0
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
             INSERT INTO attachment_blob VALUES (
                '2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881',
                X'78', 1, 'now'
             );
             INSERT INTO message (
                id, session_id, role, content, seq, created_at
             ) VALUES (1, 'session-1', 'assistant', '', 0, 'now');
             INSERT INTO message (
                id, session_id, role, content, tool_content_json,
                tool_call_id, seq, created_at
             ) VALUES (
                2, 'session-1', 'tool', 'old output',
                '{"attachments":[{"attachment":{"blob_id":"2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881","filename":"old.png","media_type":"image/png"},"detail":"high"}]}',
                'call-1', 1, 'now'
             );
             INSERT INTO tool_call VALUES (
                'call-1', 1, 'bash', '{}', 'readonly', 'old output', 'ok', 3, 250
             );
             INSERT INTO review (
                session_id, tool_call_id, action_json, risk_level,
                user_auth_level, outcome, reason, created_at
             ) VALUES (
                'session-1', 'call-1', '{}', 'destructive', 'reversible', 'deny', 'too risky', 'now'
             );
             PRAGMA user_version = 7;"#,
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
                "SELECT bc.assistant_message_id, m.bash_outcome,
                        m.bash_exit_code, m.bash_duration_ms
                 FROM bash_call bc JOIN message m ON m.bash_call_id = bc.id
                 WHERE bc.provider_call_id = 'call-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(migrated, (1, "completed".into(), Some(3), Some(250)));
        let review: (String, String) = store
            .conn
            .query_row("SELECT outcome, reason FROM bash_review", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(review, ("deny".into(), "too risky".into()));
        let migrated_attachment: (String, String, String, String, Vec<u8>) = store
            .conn
            .query_row(
                "SELECT ba.filename, ba.media_type, ba.detail, ba.blob_id, ab.data
                 FROM bash_attachment ba
                 JOIN attachment_blob ab ON ab.id = ba.blob_id",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            migrated_attachment,
            (
                "old.png".into(),
                "image/png".into(),
                "high".into(),
                "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881".into(),
                b"x".to_vec(),
            )
        );
        let foreign_key_errors: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(foreign_key_errors, 0);
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
            let (_, call_ids) = store
                .append_message_with_bash_calls(
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
                .persist_bash_result(
                    &session.id,
                    BashResultRecord {
                        bash_call_id: call_ids[0],
                        outcome: "completed",
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
                "SELECT COUNT(*) FROM bash_call WHERE provider_call_id = 'call_0'",
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
            .map(|record| (record.seq, record.kind, record.content))
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
            .map(|record| (record.seq, record.kind, record.content))
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
            .execute("UPDATE attachment_blob SET data = X'FF', size = 1", [])
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
    fn reloads_tool_image_attachments_and_reuses_attachment_blobs() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let data = b"\x89PNG\r\n\x1a\nrest".to_vec();
        let attachment = ToolAttachment {
            attachment: Attachment {
                filename: "tool.png".into(),
                media_type: "image/png".into(),
                data: data.clone(),
            },
            detail: ImageDetail::High,
        };
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![bash_call("call-image")]),
                    native_replay: None,
                },
            )
            .unwrap();
        store
            .persist_bash_result(
                &session.id,
                BashResultRecord {
                    bash_call_id: call_ids[0],
                    outcome: "completed",
                    exit_code: Some(0),
                    duration_ms: Some(1),
                },
                "Viewed image",
                std::slice::from_ref(&attachment),
            )
            .unwrap();
        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Parts(vec![ContentPart::Attachment {
                        attachment: attachment.attachment.clone(),
                    }]),
                },
            )
            .unwrap();

        let messages = store.load_context_messages(&session.id).unwrap();
        assert!(messages.iter().any(|message| matches!(
            message,
            Message::Tool { attachments, .. } if attachments == &vec![attachment.clone()]
        )));
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
        let (_, call_ids) = store
            .append_message_with_bash_calls(
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
            .persist_bash_result(
                &session.id,
                BashResultRecord {
                    bash_call_id: call_ids[0],
                    outcome: "completed",
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
                "SELECT bash_exit_code, bash_duration_ms
                 FROM message WHERE bash_call_id = ?1",
                params![call_ids[0]],
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
