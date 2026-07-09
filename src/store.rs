use std::path::{Path, PathBuf};

use fs2::FileExt;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::provider::{Message, ToolCall, Usage, UserContent, approx_tokens};
use crate::tools::BashRisk;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub title: Option<String>,
    pub last_total_tokens: u64,
    pub archived: bool,
}



#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub cwd: String,
    pub title: Option<String>,
    pub last_total_tokens: u64,
    pub archived: bool,
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
    pub id: &'a str,
    pub tool: &'a str,
    pub args: &'a str,
    pub risk: Option<&'a str>,
    pub output: &'a str,
    pub status: &'a str,
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

const CURRENT_SCHEMA_VERSION: i32 = 1;

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        cwd: row.get(1)?,
        model: row.get(2)?,
        title: row.get(3)?,
        last_total_tokens: row.get::<_, i64>(4)? as u64,
        archived: row.get::<_, i64>(5)? != 0,
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let state_dir = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("session database path must have a parent directory"))?;
        std::fs::create_dir_all(state_dir)?;
        let conn = Connection::open(path).context("opening SQLite database")?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;",
        )?;
        let store = Self {
            conn,
            lock_dir: state_dir.join("locks"),
        };
        store.ensure_schema()?;
        Ok(store)
    }

    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory SQLite database")?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;",
        )?;
        let store = Self {
            conn,
            lock_dir: std::env::temp_dir().join(format!("mu-memory-locks-{}", Uuid::new_v4())),
        };
        store.ensure_schema()?;
        Ok(store)
    }

    fn ensure_schema(&self) -> Result<()> {
        let version: i32 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .context("reading SQLite schema version")?;
        match version {
            0 if !self.has_application_tables()? => self.create_schema_v1(),
            0 => anyhow::bail!(
                "unsupported pre-release session database schema; remove sessions.db to create a fresh release database"
            ),
            CURRENT_SCHEMA_VERSION => Ok(()),
            future => anyhow::bail!(
                "unsupported future session database schema version {future}; this mu supports version {CURRENT_SCHEMA_VERSION}"
            ),
        }
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

    fn create_schema_v1(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                cwd TEXT NOT NULL,
                model TEXT NOT NULL,
                title TEXT,
                last_total_tokens INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS message (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                user_content_json TEXT,
                tool_call_id TEXT,
                tool_calls_json TEXT,
                seq INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES session(id)
            );
            CREATE INDEX IF NOT EXISTS idx_message_session_seq ON message(session_id, seq);
            CREATE TABLE IF NOT EXISTS tool_call (
                id TEXT PRIMARY KEY,
                message_id INTEGER NOT NULL,
                tool TEXT NOT NULL,
                args TEXT NOT NULL,
                risk TEXT,
                output TEXT,
                status TEXT NOT NULL,
                FOREIGN KEY(message_id) REFERENCES message(id)
            );
            CREATE TABLE IF NOT EXISTS turn_usage (
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
            CREATE INDEX IF NOT EXISTS idx_turn_usage_session_id ON turn_usage(session_id);
            CREATE TABLE IF NOT EXISTS review (
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
            PRAGMA user_version = 1;",
        )?;
        Ok(())
    }

    pub fn create_session(&self, cwd: &str, model: &str) -> Result<Session> {
        let id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO session (id, created_at, updated_at, cwd, model, title, last_total_tokens, archived)
             VALUES (?1, ?2, ?2, ?3, ?4, NULL, 0, 0)",
            params![id, now, cwd, model],
        )?;
        Ok(Session {
            id,
            cwd: cwd.into(),
            model: model.into(),
            title: None,
            last_total_tokens: 0,
            archived: false,
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_total_tokens, archived
             FROM session WHERE id = ?1",
        )?;
        let row = stmt.query_row(params![id], session_from_row).optional()?;
        Ok(row)
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<(Session, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_total_tokens, archived, updated_at
             FROM session
             WHERE archived = 0
             ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((session_from_row(row)?, row.get::<_, String>(6)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("listing sessions")
    }

    pub fn session_summary(&self, id: &str) -> Result<Option<SessionSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                s.id, s.created_at, s.updated_at, s.cwd, s.title,
                s.last_total_tokens, s.archived,
                COUNT(m.id) AS message_count,
                COALESCE(SUM(CASE WHEN m.role = 'user' THEN 1 ELSE 0 END), 0) AS user_count
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
                    archived: row.get::<_, i64>(6)? != 0,
                    message_count: row.get::<_, i64>(7)? as u64,
                    turn_count: row.get::<_, i64>(8)?.saturating_sub(1) as u64,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn latest_session(&self) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_total_tokens, archived
             FROM session WHERE archived = 0 ORDER BY updated_at DESC LIMIT 1",
        )?;
        let row = stmt.query_row([], session_from_row).optional()?;
        Ok(row)
    }

    pub fn set_session_archived(&self, id: &str, archived: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE session SET archived = ?1 WHERE id = ?2",
            params![if archived { 1 } else { 0 }, id],
        )?;
        Ok(())
    }

    pub fn update_session(
        &self,
        id: &str,
        usage: &Usage,
        title: Option<&str>,
        model: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        if let Some(t) = title {
            tx.execute(
                "UPDATE session SET updated_at = ?1, last_total_tokens = ?2,
                 title = COALESCE(title, ?3), model = ?4 WHERE id = ?5",
                params![now, usage.total_tokens as i64, t, model, id],
            )?;
        } else {
            tx.execute(
                "UPDATE session SET updated_at = ?1, last_total_tokens = ?2,
                 model = ?3 WHERE id = ?4",
                params![now, usage.total_tokens as i64, model, id],
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
                usage.cache_write_input_tokens as i64,
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
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE session SET updated_at = ?1, cwd = ?2 WHERE id = ?3",
            params![now, cwd, id],
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
                    id: &call.id,
                    tool: &call.function.name,
                    args: &call.function.arguments,
                    risk: risk.as_ref().map(|risk| risk.as_str()),
                    output: INTERRUPTED_TOOL_RESULT,
                    status: "interrupted",
                },
                INTERRUPTED_TOOL_RESULT,
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
            "SELECT role, content, user_content_json, tool_call_id, tool_calls_json FROM message
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
            let (role, content, user_content_json, tool_call_id, tool_calls_json) = row?;
            match role.as_str() {
                "user" => messages.push(Message::User {
                    content: load_user_content(content, user_content_json),
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
                        tool_calls,
                    });
                }
                "tool" => messages.push(Message::Tool {
                    content,
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
        let now = chrono::Utc::now().to_rfc3339();
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
        let now = chrono::Utc::now().to_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE message SET seq = seq + 1 WHERE session_id = ?1 AND seq >= ?2",
            params![session_id, before_seq],
        )?;
        tx.execute(
            "INSERT INTO message (session_id, role, content, seq, created_at) VALUES (?1, 'summary', ?2, ?3, ?4)",
            params![session_id, content, before_seq, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn append_summary(&self, session_id: &str, content: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
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
        self.conn.execute(
            "INSERT OR REPLACE INTO tool_call (id, message_id, tool, args, risk, output, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.id,
                record.message_id,
                record.tool,
                record.args,
                record.risk,
                record.output,
                record.status
            ],
        )?;
        Ok(())
    }

    pub fn persist_tool_result(
        &self,
        session_id: &str,
        record: ToolCallRecord<'_>,
        content: &str,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO tool_call (id, message_id, tool, args, risk, output, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.id,
                record.message_id,
                record.tool,
                record.args,
                record.risk,
                record.output,
                record.status
            ],
        )?;
        let message = Message::Tool {
            content: content.to_string(),
            tool_call_id: record.id.to_string(),
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
        let now = chrono::Utc::now().to_rfc3339();
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
                            tool_calls,
                        } => {
                            approx_tokens(content.as_deref().unwrap_or(""))
                                + tool_calls
                                    .as_ref()
                                    .map(|t| {
                                        approx_tokens(&serde_json::to_string(t).unwrap_or_default())
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
        file.try_lock_exclusive()
            .map_err(|_| anyhow::anyhow!("session busy"))?;
        Ok(SessionLock { _file: file })
    }

    pub fn is_session_busy(&self, session_id: &str) -> bool {
        let lock_path = self.session_lock_path(session_id);
        if !lock_path.exists() {
            return false;
        }
        let Ok(file) = std::fs::OpenOptions::new().write(true).open(&lock_path) else {
            return true;
        };
        file.try_lock_exclusive().is_err()
    }

    fn session_lock_path(&self, session_id: &str) -> PathBuf {
        self.lock_dir.join(format!("{session_id}.lock"))
    }
}

#[derive(Debug)]
pub struct SessionLock {
    _file: std::fs::File,
}

type ContextRow = (
    String,
    String,
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

fn load_user_content(content: String, user_content_json: Option<String>) -> UserContent {
    user_content_json
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or(UserContent::Text(content))
}

fn load_context_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ContextRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
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
            let user_content_json = serde_json::to_string(content)?;
            tx.execute(
                "INSERT INTO message (session_id, role, content, user_content_json, seq, created_at)
                 VALUES (?1, 'user', ?2, ?3, ?4, ?5)",
                params![session_id, content.text(), user_content_json, seq, now],
            )?;
        }
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let tc_json = tool_calls.as_ref().map(serde_json::to_string).transpose()?;
            tx.execute(
                "INSERT INTO message (session_id, role, content, tool_calls_json, seq, created_at)
                 VALUES (?1, 'assistant', ?2, ?3, ?4, ?5)",
                params![
                    session_id,
                    content.as_deref().unwrap_or(""),
                    tc_json,
                    seq,
                    now
                ],
            )?;
        }
        Message::Tool {
            content,
            tool_call_id,
        } => {
            tx.execute(
                "INSERT INTO message (session_id, role, content, tool_call_id, seq, created_at)
                 VALUES (?1, 'tool', ?2, ?3, ?4, ?5)",
                params![session_id, content, tool_call_id, seq, now],
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
    use crate::provider::{ContentPart, FunctionCall, ImageUrl, ToolCall};

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
            vec!["message", "review", "session", "tool_call", "turn_usage"]
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
    fn future_schema_version_is_rejected() {
        let tmp = std::env::temp_dir().join(format!("mu-store-future-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let db = tmp.join("mu.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("PRAGMA user_version = 99;").unwrap();
        drop(conn);

        let error = Store::open(&db).err().unwrap().to_string();

        assert!(error.contains("unsupported future session database schema version 99"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn reloads_full_user_content_with_images() {
        let (store, tmp) = temp_store();
        let session = create_session_with_system(&store);
        let expected_image_url = "data:image/png;base64,abcd".to_string();

        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Parts(vec![
                        ContentPart::Text {
                            text: "describe this".to_string(),
                        },
                        ContentPart::ImageUrl {
                            image_url: ImageUrl {
                                url: expected_image_url.clone(),
                            },
                        },
                    ]),
                },
            )
            .unwrap();

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
            ContentPart::ImageUrl { image_url } if image_url.url == expected_image_url
        ));

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn list_sessions_skips_archived() {
        let (store, tmp) = temp_store();
        let cli = store.create_session("/tmp", "cli-model").unwrap();
        let archived = store.create_session("/tmp", "archived-model").unwrap();
        store.set_session_archived(&archived.id, true).unwrap();

        let sessions = store.list_sessions(20).unwrap();

        let ids = sessions
            .iter()
            .map(|(session, _)| session.id.as_str())
            .collect::<Vec<_>>();
        assert!(ids.contains(&cli.id.as_str()));
        assert!(!ids.contains(&archived.id.as_str()));
        assert!(sessions.iter().all(|(session, _)| !session.archived));
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
                    tool_calls: Some(vec![
                        ToolCall {
                            id: "call-a".into(),
                            call_type: "function".into(),
                            function: FunctionCall {
                                name: "bash".into(),
                                arguments:
                                    "{\"title\":\"a\",\"risk\":\"readonly\",\"command\":\"echo a\"}"
                                        .into(),
                            },
                        },
                        ToolCall {
                            id: "call-b".into(),
                            call_type: "function".into(),
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
                    id: "call-a",
                    tool: "bash",
                    args: "{\"title\":\"a\",\"risk\":\"readonly\",\"command\":\"echo a\"}",
                    risk: Some("readonly"),
                    output: "a",
                    status: "ok",
                },
                "a",
            )
            .unwrap();

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
                    tool_calls: None,
                },
            )
            .unwrap();
        assert!(store.is_session_clean(&session.id).unwrap());
        let _ = std::fs::remove_dir_all(tmp);
    }
}
