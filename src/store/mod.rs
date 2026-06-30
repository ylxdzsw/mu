use std::path::Path;
use std::str::FromStr;

use fs2::FileExt;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use uuid::Uuid;

use crate::provider::{Message, UserContent, approx_tokens};

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
    pub created_at: String,
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
    pub cost_total: f64,
    pub origin: SessionOrigin,
    pub archived: bool,
    pub message_count: u64,
    pub turn_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptMessage {
    pub role: String,
    pub content: String,
    pub seq: i64,
    pub created_at: String,
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
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path).context("opening SQLite database")?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;",
        )?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().context("opening in-memory SQLite database")?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;",
        )?;
        let store = Self { conn };
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
                effort TEXT,
                title TEXT,
                last_total_tokens INTEGER NOT NULL DEFAULT 0,
                cost_total REAL NOT NULL DEFAULT 0,
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
        self.ensure_session_column("effort", "TEXT")?;
        self.ensure_session_column("origin", "TEXT NOT NULL DEFAULT 'cli'")?;
        self.ensure_session_column("archived", "INTEGER NOT NULL DEFAULT 0")?;
        self.ensure_message_column("user_content_json", "TEXT")?;
        self.ensure_tool_call_column("risk", "TEXT")?;
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
            "INSERT INTO session (id, created_at, updated_at, cwd, model, effort, title, last_total_tokens, cost_total, origin, archived)
             VALUES (?1, ?2, ?2, ?3, ?4, ?5, NULL, 0, 0, ?6, 0)",
            params![
                id,
                now,
                cwd,
                model,
                Option::<String>::None,
                origin.as_str()
            ],
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
                s.last_total_tokens, s.cost_total, s.origin, s.archived,
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
            let origin: String = row.get(8)?;
            let message_count = row.get::<_, i64>(10)? as u64;
            let user_count = row.get::<_, i64>(11)? as u64;
            summaries.push(SessionSummary {
                id: row.get(0)?,
                created_at: row.get(1)?,
                updated_at: row.get(2)?,
                cwd: row.get(3)?,
                model: row.get(4)?,
                title: row.get(5)?,
                last_total_tokens: row.get::<_, i64>(6)? as u64,
                cost_total: row.get(7)?,
                origin: parse_origin(origin)?,
                archived: row.get::<_, i64>(9)? != 0,
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
                s.last_total_tokens, s.cost_total, s.origin, s.archived,
                COUNT(m.id) AS message_count,
                COALESCE(SUM(CASE WHEN m.role = 'user' THEN 1 ELSE 0 END), 0) AS user_count
             FROM session s
             LEFT JOIN message m ON m.session_id = s.id
             WHERE s.id = ?1
             GROUP BY s.id",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                let origin: String = row.get(8)?;
                let message_count = row.get::<_, i64>(10)? as u64;
                let user_count = row.get::<_, i64>(11)? as u64;
                Ok(SessionSummary {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                    cwd: row.get(3)?,
                    model: row.get(4)?,
                    title: row.get(5)?,
                    last_total_tokens: row.get::<_, i64>(6)? as u64,
                    cost_total: row.get(7)?,
                    origin: parse_origin(origin)?,
                    archived: row.get::<_, i64>(9)? != 0,
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
        last_total_tokens: u64,
        cost_delta: f64,
        title: Option<&str>,
        model: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        if let Some(t) = title {
            self.conn.execute(
                "UPDATE session SET updated_at = ?1, last_total_tokens = ?2,
                 cost_total = cost_total + ?3, title = COALESCE(title, ?4),
                 model = ?5 WHERE id = ?6",
                params![now, last_total_tokens as i64, cost_delta, t, model, id],
            )?;
        } else {
            self.conn.execute(
                "UPDATE session SET updated_at = ?1, last_total_tokens = ?2,
                 cost_total = cost_total + ?3, model = ?4 WHERE id = ?5",
                params![now, last_total_tokens as i64, cost_delta, model, id],
            )?;
        }
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

    pub fn load_context_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let summary_seq = self.latest_summary_seq(session_id)?;
        let start_seq = summary_seq.unwrap_or(-1);

        let mut stmt = self.conn.prepare(
            "SELECT role, content, user_content_json, tool_call_id, tool_calls_json FROM message
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id, start_seq], |row| {
            let role: String = row.get(0)?;
            let content: String = row.get(1)?;
            let user_content_json: Option<String> = row.get(2)?;
            let tool_call_id: Option<String> = row.get(3)?;
            let tool_calls_json: Option<String> = row.get(4)?;
            Ok((
                role,
                content,
                user_content_json,
                tool_call_id,
                tool_calls_json,
            ))
        })?;

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
            "SELECT role, content, seq, created_at FROM message
             WHERE session_id = ?1 AND seq = ?2",
        )?;
        let row = stmt
            .query_row(params![session_id, seq], |row| {
                Ok(StoredMessage {
                    role: row.get(0)?,
                    content: row.get(1)?,
                    seq: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn next_seq(&self, session_id: &str) -> Result<i64> {
        let mut stmt = self
            .conn
            .prepare("SELECT COALESCE(MAX(seq), -1) + 1 FROM message WHERE session_id = ?1")?;
        let seq: i64 = stmt.query_row(params![session_id], |row| row.get(0))?;
        Ok(seq)
    }

    pub fn append_message(&self, session_id: &str, message: &Message) -> Result<i64> {
        let seq = self.next_seq(session_id)?;
        let now = chrono::Utc::now().to_rfc3339();
        match message {
            Message::User { content } => {
                let user_content_json = serde_json::to_string(content)?;
                self.conn.execute(
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
                self.conn.execute(
                    "INSERT INTO message (session_id, role, content, tool_calls_json, seq, created_at)
                     VALUES (?1, 'assistant', ?2, ?3, ?4, ?5)",
                    params![session_id, content.as_deref().unwrap_or(""), tc_json, seq, now],
                )?;
            }
            Message::Tool {
                content,
                tool_call_id,
            } => {
                self.conn.execute(
                    "INSERT INTO message (session_id, role, content, tool_call_id, seq, created_at)
                     VALUES (?1, 'tool', ?2, ?3, ?4, ?5)",
                    params![session_id, content, tool_call_id, seq, now],
                )?;
            }
            Message::System { .. } => {
                bail!("system messages are not persisted directly");
            }
        }
        let id = self.conn.last_insert_rowid();
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
        let seq = self.next_seq(session_id)?;
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO message (session_id, role, content, seq, created_at) VALUES (?1, 'summary', ?2, ?3, ?4)",
            params![session_id, content, seq, now],
        )?;
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
            "SELECT role, content, seq, created_at FROM message
             WHERE session_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(StoredMessage {
                role: row.get(0)?,
                content: row.get(1)?,
                seq: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("loading messages")
    }

    pub fn transcript(&self, session_id: &str) -> Result<Vec<TranscriptMessage>> {
        let messages = self.all_messages_for_session(session_id)?;
        Ok(messages
            .into_iter()
            .map(|message| TranscriptMessage {
                role: message.role,
                content: message.content,
                seq: message.seq,
                created_at: message.created_at,
            })
            .collect())
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

    pub fn get_skill_cache(&self, mtime: i64) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT skills_json FROM skill_cache WHERE dir_mtime = ?1")?;
        let row = stmt
            .query_row(params![mtime], |row| row.get(0))
            .optional()?;
        Ok(row)
    }

    pub fn set_skill_cache(&self, mtime: i64, skills_json: &str) -> Result<()> {
        self.conn.execute("DELETE FROM skill_cache", [])?;
        self.conn.execute(
            "INSERT INTO skill_cache (dir_mtime, skills_json) VALUES (?1, ?2)",
            params![mtime, skills_json],
        )?;
        Ok(())
    }
}

pub struct SessionLock {
    _file: std::fs::File,
}

pub fn acquire_session_lock(session_id: &str) -> Result<SessionLock> {
    let lock_path = session_lock_path(session_id)?;
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

pub fn is_session_busy(session_id: &str) -> bool {
    let lock_path = crate::paths::runtime_dir().join(format!("{session_id}.lock"));
    if !lock_path.exists() {
        return false;
    }
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(&lock_path) else {
        return true;
    };
    file.try_lock_exclusive().is_err()
}

pub fn session_lock_path(session_id: &str) -> Result<std::path::PathBuf> {
    let lock_dir = crate::paths::runtime_dir();
    crate::paths::ensure_dir(&lock_dir)?;
    Ok(lock_dir.join(format!("{session_id}.lock")))
}

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
    use crate::provider::{ContentPart, ImageUrl};

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
    fn reloads_legacy_text_user_content() {
        let (store, tmp) = temp_store();
        let session = store.create_session("/tmp", "fake-model").unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        store
            .conn
            .execute(
                "INSERT INTO message (session_id, role, content, seq, created_at)
                 VALUES (?1, 'user', ?2, 0, ?3)",
                params![session.id, "legacy text", now],
            )
            .unwrap();

        let messages = store.load_context_messages(&session.id).unwrap();
        let Message::User {
            content: UserContent::Text(text),
        } = &messages[0]
        else {
            panic!("expected text user content");
        };

        assert_eq!(text, "legacy text");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn new_sessions_default_to_cli_origin_and_unarchived() {
        let (store, tmp) = temp_store();
        let session = store.create_session("/tmp", "fake-model").unwrap();
        let loaded = store.get_session(&session.id).unwrap().unwrap();

        assert_eq!(loaded.origin, SessionOrigin::Cli);
        assert!(!loaded.archived);
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
    fn all_session_summaries_include_cli_and_web_origins() {
        let (store, tmp) = temp_store();
        let cli = store.create_session("/tmp", "cli-model").unwrap();
        let web = store
            .create_session_with_origin("/tmp", "web-model", SessionOrigin::Web)
            .unwrap();

        let summaries = store.list_all_session_summaries(20).unwrap();
        let ids = summaries
            .iter()
            .map(|summary| (summary.id.as_str(), summary.origin))
            .collect::<Vec<_>>();

        assert!(ids.contains(&(cli.id.as_str(), SessionOrigin::Cli)));
        assert!(ids.contains(&(web.id.as_str(), SessionOrigin::Web)));
        let _ = std::fs::remove_dir_all(tmp);
    }
}
