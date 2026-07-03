use std::path::{Path, PathBuf};
use std::str::FromStr;

use fs2::FileExt;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use uuid::Uuid;

use crate::provider::{Message, ToolCall, Usage, UserContent, approx_tokens};

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub title: Option<String>,
    pub last_total_tokens: u64,
    pub origin: SessionOrigin,
    pub archived: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionOrigin {
    Cli,
    Web,
}

impl SessionOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionOrigin::Cli => "cli",
            SessionOrigin::Web => "web",
        }
    }
}

impl std::fmt::Display for SessionOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionOrigin {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "cli" => Ok(SessionOrigin::Cli),
            "web" => Ok(SessionOrigin::Web),
            other => bail!("unknown session origin: {other}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
    pub seq: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub cwd: String,
    pub model: String,
    pub title: Option<String>,
    pub last_total_tokens: u64,
    pub origin: SessionOrigin,
    pub archived: bool,
    pub message_count: u64,
    pub turn_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingState {
    Running,
    Incomplete,
}

impl PendingState {
    fn as_str(self) -> &'static str {
        match self {
            PendingState::Running => "running",
            PendingState::Incomplete => "incomplete",
        }
    }
}

impl FromStr for PendingState {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "running" => Ok(PendingState::Running),
            "incomplete" => Ok(PendingState::Incomplete),
            other => bail!("unknown pending state: {other}"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingTurn {
    pub state: PendingState,
    pub prompt_message_id: i64,
    pub checkpoint_message_id: i64,
    pub retry_count: u64,
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptMessage {
    pub role: String,
    pub content: String,
    pub seq: i64,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone)]
pub struct MessageRecord {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub user_content_json: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls_json: Option<String>,
    pub seq: i64,
}

#[derive(Debug, Clone)]
pub struct ToolCallSnapshot {
    pub tool: String,
    pub args: String,
    pub risk: Option<String>,
    pub output: Option<String>,
    pub status: String,
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
        store.migrate()?;
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
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                cwd TEXT NOT NULL,
                model TEXT NOT NULL,
                title TEXT,
                last_total_tokens INTEGER NOT NULL DEFAULT 0,
                origin TEXT NOT NULL DEFAULT 'cli',
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
                output TEXT,
                status TEXT NOT NULL,
                FOREIGN KEY(message_id) REFERENCES message(id)
            );
            CREATE TABLE IF NOT EXISTS skill_cache (
                dir_mtime INTEGER NOT NULL,
                skills_json TEXT NOT NULL
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
            );",
        )?;
        self.ensure_session_column("origin", "TEXT NOT NULL DEFAULT 'cli'")?;
        self.ensure_session_column("archived", "INTEGER NOT NULL DEFAULT 0")?;
        self.ensure_session_column("pending_state", "TEXT")?;
        self.ensure_session_column("pending_prompt_message_id", "INTEGER")?;
        self.ensure_session_column("pending_checkpoint_message_id", "INTEGER")?;
        self.ensure_session_column("pending_retry_count", "INTEGER NOT NULL DEFAULT 0")?;
        self.ensure_session_column("pending_error_message", "TEXT")?;
        self.ensure_message_column("user_content_json", "TEXT")?;
        self.ensure_tool_call_column("risk", "TEXT")?;
        self.rebuild_session_without_stale_columns()?;
        Ok(())
    }

    fn ensure_session_column(&self, name: &str, sql_type: &str) -> Result<()> {
        self.ensure_column("session", name, sql_type)
    }

    fn ensure_message_column(&self, name: &str, sql_type: &str) -> Result<()> {
        self.ensure_column("message", name, sql_type)
    }

    fn ensure_tool_call_column(&self, name: &str, sql_type: &str) -> Result<()> {
        self.ensure_column("tool_call", name, sql_type)
    }

    fn ensure_column(&self, table: &str, name: &str, sql_type: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for column in columns {
            if column? == name {
                return Ok(());
            }
        }
        self.conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {name} {sql_type}"),
            [],
        )?;
        Ok(())
    }

    fn session_has_column(&self, name: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(session)")?;
        let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for column in columns {
            if column? == name {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn rebuild_session_without_stale_columns(&self) -> Result<()> {
        if !self.session_has_column("effort")? && !self.session_has_column("cost_total")? {
            return Ok(());
        }

        self.conn.execute_batch(
            "PRAGMA foreign_keys=off;
             BEGIN;
             CREATE TABLE session_new (
                id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                cwd TEXT NOT NULL,
                model TEXT NOT NULL,
                title TEXT,
                last_total_tokens INTEGER NOT NULL DEFAULT 0,
                origin TEXT NOT NULL DEFAULT 'cli',
                archived INTEGER NOT NULL DEFAULT 0,
                pending_state TEXT,
                pending_prompt_message_id INTEGER,
                pending_checkpoint_message_id INTEGER,
                pending_retry_count INTEGER NOT NULL DEFAULT 0,
                pending_error_message TEXT
             );
             INSERT INTO session_new (
                id, created_at, updated_at, cwd, model, title, last_total_tokens,
                origin, archived, pending_state, pending_prompt_message_id,
                pending_checkpoint_message_id, pending_retry_count, pending_error_message
             )
             SELECT
                id, created_at, updated_at, cwd, model, title, last_total_tokens,
                origin, archived, pending_state, pending_prompt_message_id,
                pending_checkpoint_message_id, pending_retry_count, pending_error_message
             FROM session;
             DROP TABLE session;
             ALTER TABLE session_new RENAME TO session;
             COMMIT;
             PRAGMA foreign_keys=on;",
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn create_session(&self, cwd: &str, model: &str) -> Result<Session> {
        self.create_session_with_origin(cwd, model, SessionOrigin::Cli)
    }

    pub fn create_session_with_origin(
        &self,
        cwd: &str,
        model: &str,
        origin: SessionOrigin,
    ) -> Result<Session> {
        let id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO session (id, created_at, updated_at, cwd, model, title, last_total_tokens, origin, archived)
             VALUES (?1, ?2, ?2, ?3, ?4, NULL, 0, ?5, 0)",
            params![id, now, cwd, model, origin.as_str()],
        )?;
        Ok(Session {
            id,
            cwd: cwd.into(),
            model: model.into(),
            title: None,
            last_total_tokens: 0,
            origin,
            archived: false,
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_total_tokens, origin, archived
             FROM session WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                let origin: String = row.get(5)?;
                Ok(Session {
                    id: row.get(0)?,
                    cwd: row.get(1)?,
                    model: row.get(2)?,
                    title: row.get(3)?,
                    last_total_tokens: row.get::<_, i64>(4)? as u64,
                    origin: parse_origin(origin)?,
                    archived: row.get::<_, i64>(6)? != 0,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<(Session, String)>> {
        self.list_sessions_by_origin(SessionOrigin::Cli, limit)
    }

    pub fn list_sessions_by_origin(
        &self,
        origin: SessionOrigin,
        limit: usize,
    ) -> Result<Vec<(Session, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_total_tokens, origin, archived, updated_at
             FROM session
             WHERE origin = ?1 AND archived = 0
             ORDER BY updated_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![origin.as_str(), limit as i64], |row| {
            let origin: String = row.get(5)?;
            Ok((
                Session {
                    id: row.get(0)?,
                    cwd: row.get(1)?,
                    model: row.get(2)?,
                    title: row.get(3)?,
                    last_total_tokens: row.get::<_, i64>(4)? as u64,
                    origin: parse_origin(origin)?,
                    archived: row.get::<_, i64>(6)? != 0,
                },
                row.get::<_, String>(7)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("listing sessions")
    }

    pub fn list_session_summaries_by_origin(
        &self,
        origin: SessionOrigin,
        limit: usize,
    ) -> Result<Vec<SessionSummary>> {
        self.list_session_summaries(Some(origin), limit)
    }

    pub fn list_all_session_summaries(&self, limit: usize) -> Result<Vec<SessionSummary>> {
        self.list_session_summaries(None, limit)
    }

    fn list_session_summaries(
        &self,
        origin: Option<SessionOrigin>,
        limit: usize,
    ) -> Result<Vec<SessionSummary>> {
        let (where_origin, limit_param) = if origin.is_some() {
            ("s.origin = ?1 AND s.archived = 0", "?2")
        } else {
            ("s.archived = 0", "?1")
        };
        let sql = format!(
            "SELECT
                s.id, s.created_at, s.updated_at, s.cwd, s.model, s.title,
                s.last_total_tokens, s.origin, s.archived,
                COUNT(m.id) AS message_count,
                COALESCE(SUM(CASE WHEN m.role = 'user' THEN 1 ELSE 0 END), 0) AS user_count
             FROM session s
             LEFT JOIN message m ON m.session_id = s.id
             WHERE {where_origin}
             GROUP BY s.id
             ORDER BY s.updated_at DESC LIMIT {limit_param}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = match origin {
            Some(origin) => stmt.query(params![origin.as_str(), limit as i64])?,
            None => stmt.query(params![limit as i64])?,
        };
        let mut summaries = Vec::new();
        while let Some(row) = rows.next()? {
            let origin: String = row.get(7)?;
            let message_count = row.get::<_, i64>(9)? as u64;
            let user_count = row.get::<_, i64>(10)? as u64;
            summaries.push(SessionSummary {
                id: row.get(0)?,
                created_at: row.get(1)?,
                updated_at: row.get(2)?,
                cwd: row.get(3)?,
                model: row.get(4)?,
                title: row.get(5)?,
                last_total_tokens: row.get::<_, i64>(6)? as u64,
                origin: parse_origin(origin)?,
                archived: row.get::<_, i64>(8)? != 0,
                message_count,
                turn_count: user_count.saturating_sub(1),
            });
        }
        Ok(summaries)
    }

    pub fn session_summary(&self, id: &str) -> Result<Option<SessionSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                s.id, s.created_at, s.updated_at, s.cwd, s.model, s.title,
                s.last_total_tokens, s.origin, s.archived,
                COUNT(m.id) AS message_count,
                COALESCE(SUM(CASE WHEN m.role = 'user' THEN 1 ELSE 0 END), 0) AS user_count
             FROM session s
             LEFT JOIN message m ON m.session_id = s.id
             WHERE s.id = ?1
             GROUP BY s.id",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                let origin: String = row.get(7)?;
                let message_count = row.get::<_, i64>(9)? as u64;
                let user_count = row.get::<_, i64>(10)? as u64;
                Ok(SessionSummary {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    cwd: row.get(3)?,
                    model: row.get(4)?,
                    title: row.get(5)?,
                    last_total_tokens: row.get::<_, i64>(6)? as u64,
                    origin: parse_origin(origin)?,
                    archived: row.get::<_, i64>(8)? != 0,
                    message_count,
                    turn_count: user_count.saturating_sub(1),
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn latest_session(&self) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_total_tokens, origin, archived
             FROM session ORDER BY updated_at DESC LIMIT 1",
        )?;
        let row = stmt
            .query_row([], |row| {
                let origin: String = row.get(5)?;
                Ok(Session {
                    id: row.get(0)?,
                    cwd: row.get(1)?,
                    model: row.get(2)?,
                    title: row.get(3)?,
                    last_total_tokens: row.get::<_, i64>(4)? as u64,
                    origin: parse_origin(origin)?,
                    archived: row.get::<_, i64>(6)? != 0,
                })
            })
            .optional()?;
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

    pub fn pending_turn(&self, session_id: &str) -> Result<Option<PendingTurn>> {
        let mut stmt = self.conn.prepare(
            "SELECT pending_state, pending_prompt_message_id, pending_checkpoint_message_id,
                    pending_retry_count, pending_error_message
             FROM session
             WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![session_id], |row| {
                let state: Option<String> = row.get(0)?;
                let Some(state) = state else {
                    return Ok(None);
                };
                Ok(Some(PendingTurn {
                    state: parse_pending_state(state)?,
                    prompt_message_id: row.get(1)?,
                    checkpoint_message_id: row.get(2)?,
                    retry_count: row.get::<_, i64>(3)? as u64,
                    error_message: row.get(4)?,
                }))
            })
            .optional()?;
        Ok(row.flatten())
    }

    pub fn reconcile_pending_turn_locked(
        &self,
        _lock: &SessionLock,
        session_id: &str,
    ) -> Result<Option<PendingTurn>> {
        let Some(pending) = self.pending_turn(session_id)? else {
            return Ok(None);
        };
        if pending.state != PendingState::Running {
            return Ok(Some(pending));
        }

        let prompt_record = self
            .message_record(session_id, pending.prompt_message_id)?
            .ok_or_else(|| anyhow::anyhow!("pending prompt message missing from session"))?;
        let records = self.message_records_from_seq(session_id, prompt_record.seq)?;

        let mut open_tool_message: Option<(i64, Vec<ToolCall>, usize)> = None;
        let mut last_message_role = "user".to_string();
        let mut last_message_had_tool_calls = false;

        for record in records.iter().skip(1) {
            last_message_role = record.role.clone();
            last_message_had_tool_calls = false;
            match record.role.as_str() {
                "assistant" => {
                    let tool_calls = record
                        .tool_calls_json
                        .as_ref()
                        .and_then(|json| serde_json::from_str::<Vec<ToolCall>>(json).ok());
                    if let Some(tool_calls) = tool_calls {
                        last_message_had_tool_calls = true;
                        open_tool_message = Some((record.id, tool_calls, 0));
                    } else {
                        open_tool_message = None;
                    }
                }
                "tool" => {
                    if let Some((_, calls, completed)) = open_tool_message.as_mut()
                        && let Some(call_id) = record.tool_call_id.as_deref()
                        && let Some(relative_idx) = calls[*completed..]
                            .iter()
                            .position(|call| call.id == call_id)
                    {
                        *completed += relative_idx + 1;
                        if *completed >= calls.len() {
                            open_tool_message = None;
                        }
                    }
                }
                _ => {}
            }
        }

        if let Some((assistant_message_id, calls, completed)) = open_tool_message {
            for (index, call) in calls.iter().enumerate().skip(completed) {
                let snapshot = self.tool_call_snapshot(&call.id)?;
                let content = snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.output.clone())
                    .unwrap_or_else(|| {
                        if index == completed {
                            "error: interrupted before tool result was completed".to_string()
                        } else {
                            "error: not started because the turn was interrupted".to_string()
                        }
                    });
                let tool_name = snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.tool.as_str())
                    .unwrap_or(call.function.name.as_str());
                let args = snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.args.as_str())
                    .unwrap_or(call.function.arguments.as_str());
                let risk = snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.risk.as_deref());
                let status = snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.status.as_str())
                    .unwrap_or("error");
                self.persist_tool_result(
                    session_id,
                    ToolCallRecord {
                        message_id: assistant_message_id,
                        id: &call.id,
                        tool: tool_name,
                        args,
                        risk,
                        output: &content,
                        status,
                    },
                    &content,
                )?;
            }
        }

        if last_message_role == "assistant" && !last_message_had_tool_calls {
            self.clear_pending_turn(session_id)?;
            return Ok(None);
        }

        self.mark_pending_incomplete(session_id, "previous turn was interrupted")?;
        self.pending_turn(session_id)
    }

    pub fn begin_pending_turn(&self, session_id: &str, content: &UserContent) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        let message = Message::User {
            content: content.clone(),
        };
        let id = insert_message_in(&tx, session_id, &message, &now)?;
        tx.execute(
            "UPDATE session
             SET updated_at = ?1,
                 pending_state = ?2,
                 pending_prompt_message_id = ?3,
                 pending_checkpoint_message_id = ?3,
                 pending_retry_count = 0,
                 pending_error_message = NULL
             WHERE id = ?4",
            params![now, PendingState::Running.as_str(), id, session_id],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn resume_pending_turn(&self, session_id: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE session
             SET updated_at = ?1,
                 pending_state = ?2,
                 pending_error_message = NULL
             WHERE id = ?3",
            params![now, PendingState::Running.as_str(), session_id],
        )?;
        Ok(())
    }

    pub fn advance_pending_checkpoint_with_message(
        &self,
        session_id: &str,
        message: &Message,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        let id = insert_message_in(&tx, session_id, message, &now)?;
        tx.execute(
            "UPDATE session
             SET updated_at = ?1,
                 pending_checkpoint_message_id = ?2
             WHERE id = ?3",
            params![now, id, session_id],
        )?;
        tx.commit()?;
        Ok(id)
    }

    pub fn increment_pending_retry_count(&self, session_id: &str) -> Result<u64> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE session
             SET updated_at = ?1,
                 pending_retry_count = pending_retry_count + 1
             WHERE id = ?2",
            params![now, session_id],
        )?;
        let retry_count = self
            .pending_turn(session_id)?
            .ok_or_else(|| anyhow::anyhow!("session missing pending turn after retry increment"))?
            .retry_count;
        Ok(retry_count)
    }

    pub fn mark_pending_incomplete(&self, session_id: &str, error_message: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE session
             SET updated_at = ?1,
                 pending_state = ?2,
                 pending_error_message = ?3
             WHERE id = ?4",
            params![
                now,
                PendingState::Incomplete.as_str(),
                error_message,
                session_id
            ],
        )?;
        Ok(())
    }

    pub fn clear_pending_turn(&self, session_id: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "UPDATE session
             SET updated_at = ?1,
                 pending_state = NULL,
                 pending_prompt_message_id = NULL,
                 pending_checkpoint_message_id = NULL,
                 pending_retry_count = 0,
                 pending_error_message = NULL
             WHERE id = ?2",
            params![now, session_id],
        )?;
        Ok(())
    }

    pub fn prompt_user_content(
        &self,
        session_id: &str,
        message_id: i64,
    ) -> Result<Option<UserContent>> {
        let record = self.message_record(session_id, message_id)?;
        Ok(record.and_then(|record| {
            (record.role == "user")
                .then(|| load_user_content(record.content, record.user_content_json))
        }))
    }

    pub fn message_record(
        &self,
        session_id: &str,
        message_id: i64,
    ) -> Result<Option<MessageRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, role, content, user_content_json, tool_call_id, tool_calls_json, seq, created_at
             FROM message
             WHERE session_id = ?1 AND id = ?2",
        )?;
        let row = stmt
            .query_row(params![session_id, message_id], |row| {
                load_message_record_row(row)
            })
            .optional()?;
        Ok(row)
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

    pub fn tool_call_snapshot(&self, id: &str) -> Result<Option<ToolCallSnapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, tool, args, risk, output, status
             FROM tool_call
             WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                Ok(ToolCallSnapshot {
                    tool: row.get(1)?,
                    args: row.get(2)?,
                    risk: row.get(3)?,
                    output: row.get(4)?,
                    status: row.get(5)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn message_seq(&self, session_id: &str, message_id: i64) -> Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq FROM message WHERE session_id = ?1 AND id = ?2")?;
        let seq = stmt
            .query_row(params![session_id, message_id], |row| row.get::<_, i64>(0))
            .optional()?;
        Ok(seq)
    }

    pub fn load_context_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        self.load_context_messages_until(session_id, None)
    }

    pub fn load_context_messages_until(
        &self,
        session_id: &str,
        max_message_id: Option<i64>,
    ) -> Result<Vec<Message>> {
        let summary_seq = self.latest_summary_seq(session_id)?;
        let start_seq = summary_seq.unwrap_or(-1);
        let end_seq = if let Some(message_id) = max_message_id {
            Some(
                self.message_seq(session_id, message_id)?
                    .ok_or_else(|| anyhow::anyhow!("message not found in session: {message_id}"))?,
            )
        } else {
            None
        };

        let sql = if end_seq.is_some() {
            "SELECT role, content, user_content_json, tool_call_id, tool_calls_json FROM message
             WHERE session_id = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq ASC"
        } else {
            "SELECT role, content, user_content_json, tool_call_id, tool_calls_json FROM message
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = if let Some(end_seq) = end_seq {
            stmt.query_map(params![session_id, start_seq, end_seq], load_context_row)?
        } else {
            stmt.query_map(params![session_id, start_seq], load_context_row)?
        };

        let mut messages = Vec::new();
        if let Some(seq) = summary_seq
            && let Some(summary) = self.message_at_seq(session_id, seq)?
        {
            // Framed as a user message (not system) so the assembled
            // context has exactly one leading system message. Servers that
            // reject a non-first system message would otherwise fail.
            messages.push(Message::User {
                content: UserContent::Text(format!(
                    "[summary of earlier conversation]\n{}",
                    summary.content
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
                "summary" => {}
                other => messages.push(Message::User {
                    content: UserContent::Text(format!("[{other}] {content}")),
                }),
            }
        }
        Ok(messages)
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

    fn message_at_seq(&self, session_id: &str, seq: i64) -> Result<Option<StoredMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT role, content, seq FROM message
             WHERE session_id = ?1 AND seq = ?2",
        )?;
        let row = stmt
            .query_row(params![session_id, seq], |row| {
                Ok(StoredMessage {
                    role: row.get(0)?,
                    content: row.get(1)?,
                    seq: row.get(2)?,
                })
            })
            .optional()?;
        Ok(row)
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
            "UPDATE session
             SET updated_at = ?1,
                 pending_checkpoint_message_id = ?2
             WHERE id = ?3",
            params![now, message_id, session_id],
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

    pub fn all_messages_for_session(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT role, content, seq FROM message
             WHERE session_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(StoredMessage {
                role: row.get(0)?,
                content: row.get(1)?,
                seq: row.get(2)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("loading messages")
    }

    pub fn transcript(&self, session_id: &str) -> Result<Vec<TranscriptMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT role, content, tool_call_id, tool_calls_json, seq, created_at FROM message
             WHERE session_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let tool_calls_json: Option<String> = row.get(3)?;
            let tool_calls = tool_calls_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
            Ok(TranscriptMessage {
                role: row.get(0)?,
                content: row.get(1)?,
                tool_call_id: row.get(2)?,
                tool_calls,
                seq: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("loading transcript messages")
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
        Message::System { .. } => {
            bail!("system messages are not persisted directly");
        }
    }
    Ok(tx.last_insert_rowid())
}

fn load_message_record_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRecord> {
    Ok(MessageRecord {
        id: row.get(0)?,
        role: row.get(1)?,
        content: row.get(2)?,
        user_content_json: row.get(3)?,
        tool_call_id: row.get(4)?,
        tool_calls_json: row.get(5)?,
        seq: row.get(6)?,
    })
}

fn parse_pending_state(value: String) -> rusqlite::Result<PendingState> {
    PendingState::from_str(&value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            )),
        )
    })
}

fn parse_origin(value: String) -> rusqlite::Result<SessionOrigin> {
    SessionOrigin::from_str(&value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            )),
        )
    })
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

    #[test]
    fn reloads_full_user_content_with_images() {
        let (store, tmp) = temp_store();
        let session = store.create_session("/tmp", "fake-model").unwrap();
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
        } = &messages[0]
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
    fn list_sessions_defaults_to_cli_origin_and_skips_archived() {
        let (store, tmp) = temp_store();
        let cli = store.create_session("/tmp", "cli-model").unwrap();
        let web = store
            .create_session_with_origin("/tmp", "web-model", SessionOrigin::Web)
            .unwrap();
        let archived = store.create_session("/tmp", "archived-model").unwrap();
        store.set_session_archived(&archived.id, true).unwrap();

        let sessions = store.list_sessions(20).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0.id, cli.id);
        assert_eq!(sessions[0].0.origin, SessionOrigin::Cli);
        assert!(!sessions[0].0.archived);

        let web_sessions = store
            .list_sessions_by_origin(SessionOrigin::Web, 20)
            .unwrap();
        assert_eq!(web_sessions.len(), 1);
        assert_eq!(web_sessions[0].0.id, web.id);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn reconcile_pending_turn_synthesizes_missing_tool_results() {
        let (store, tmp) = temp_store();
        let session = store.create_session("/tmp", "fake-model").unwrap();

        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Text("seed".into()),
                },
            )
            .unwrap();
        let prompt_id = store
            .begin_pending_turn(&session.id, &UserContent::Text("run".into()))
            .unwrap();
        let assistant_id = store
            .advance_pending_checkpoint_with_message(
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
                                    "{\"title\":\"a\",\"risk\":\"readonly\",\"script\":\"echo a\"}"
                                        .into(),
                            },
                        },
                        ToolCall {
                            id: "call-b".into(),
                            call_type: "function".into(),
                            function: FunctionCall {
                                name: "bash".into(),
                                arguments:
                                    "{\"title\":\"b\",\"risk\":\"readonly\",\"script\":\"echo b\"}"
                                        .into(),
                            },
                        },
                    ]),
                },
            )
            .unwrap();
        store
            .persist_tool_result(
                &session.id,
                ToolCallRecord {
                    message_id: assistant_id,
                    id: "call-a",
                    tool: "bash",
                    args: "{\"title\":\"a\",\"risk\":\"readonly\",\"script\":\"echo a\"}",
                    risk: Some("readonly"),
                    output: "a",
                    status: "ok",
                },
                "a",
            )
            .unwrap();

        let lock = store.acquire_session_lock(&session.id).unwrap();
        let pending = store
            .reconcile_pending_turn_locked(&lock, &session.id)
            .unwrap()
            .unwrap();
        assert_eq!(pending.state, PendingState::Incomplete);
        assert_eq!(pending.prompt_message_id, prompt_id);
        assert_eq!(
            pending.error_message.as_deref(),
            Some("previous turn was interrupted")
        );

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
        assert_eq!(tool_messages[0], ("call-a".into(), "a".into()));
        assert_eq!(
            tool_messages[1],
            (
                "call-b".into(),
                "error: interrupted before tool result was completed".into(),
            )
        );
        let _ = std::fs::remove_dir_all(tmp);
    }
}
