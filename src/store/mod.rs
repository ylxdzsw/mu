use std::path::Path;

use fs2::FileExt;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use crate::provider::{approx_tokens, Message, ToolCall};

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub title: Option<String>,
    pub last_total_tokens: u64,
    pub cost_total: f64,
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub seq: i64,
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
                cost_total REAL NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS message (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
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
        self.ensure_tool_call_column("risk", "TEXT")?;
        Ok(())
    }

    fn ensure_tool_call_column(&self, name: &str, sql_type: &str) -> Result<()> {
        let mut stmt = self.conn.prepare("PRAGMA table_info(tool_call)")?;
        let columns = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for column in columns {
            if column? == name {
                return Ok(());
            }
        }
        self.conn
            .execute(&format!("ALTER TABLE tool_call ADD COLUMN {name} {sql_type}"), [])?;
        Ok(())
    }

    pub fn create_session(&self, cwd: &str, model: &str) -> Result<Session> {
        let id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO session (id, created_at, updated_at, cwd, model, title, last_total_tokens, cost_total)
             VALUES (?1, ?2, ?2, ?3, ?4, NULL, 0, 0)",
            params![id, now, cwd, model],
        )?;
        Ok(Session {
            id,
            cwd: cwd.into(),
            model: model.into(),
            title: None,
            last_total_tokens: 0,
            cost_total: 0.0,
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_total_tokens, cost_total FROM session WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |row| {
                Ok(Session {
                    id: row.get(0)?,
                    cwd: row.get(1)?,
                    model: row.get(2)?,
                    title: row.get(3)?,
                    last_total_tokens: row.get::<_, i64>(4)? as u64,
                    cost_total: row.get(5)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<(Session, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, cwd, model, title, last_total_tokens, cost_total, updated_at
             FROM session ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok((
                Session {
                    id: row.get(0)?,
                    cwd: row.get(1)?,
                    model: row.get(2)?,
                    title: row.get(3)?,
                    last_total_tokens: row.get::<_, i64>(4)? as u64,
                    cost_total: row.get(5)?,
                },
                row.get::<_, String>(6)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("listing sessions")
    }

    pub fn update_session(
        &self,
        id: &str,
        last_total_tokens: u64,
        cost_delta: f64,
        title: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        if let Some(t) = title {
            self.conn.execute(
                "UPDATE session SET updated_at = ?1, last_total_tokens = ?2,
                 cost_total = cost_total + ?3, title = COALESCE(title, ?4) WHERE id = ?5",
                params![now, last_total_tokens as i64, cost_delta, t, id],
            )?;
        } else {
            self.conn.execute(
                "UPDATE session SET updated_at = ?1, last_total_tokens = ?2,
                 cost_total = cost_total + ?3 WHERE id = ?4",
                params![now, last_total_tokens as i64, cost_delta, id],
            )?;
        }
        Ok(())
    }

    pub fn load_context_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let summary_seq = self.latest_summary_seq(session_id)?;
        let start_seq = summary_seq.unwrap_or(-1);

        let mut stmt = self.conn.prepare(
            "SELECT role, content, tool_call_id, tool_calls_json FROM message
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id, start_seq], |row| {
            let role: String = row.get(0)?;
            let content: String = row.get(1)?;
            let tool_call_id: Option<String> = row.get(2)?;
            let tool_calls_json: Option<String> = row.get(3)?;
            Ok((role, content, tool_call_id, tool_calls_json))
        })?;

        let mut messages = Vec::new();
        if let Some(seq) = summary_seq {
            if let Some(summary) = self.message_at_seq(session_id, seq)? {
                // Framed as a user message (not system) so the assembled
                // context has exactly one leading system message. Servers that
                // reject a non-first system message would otherwise fail.
                messages.push(Message::User {
                    content: format!("[summary of earlier conversation]\n{}", summary.content),
                });
            }
        }

        for row in rows {
            let (role, content, tool_call_id, tool_calls_json) = row?;
            match role.as_str() {
                "user" => messages.push(Message::User { content }),
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
                    content: format!("[{other}] {content}"),
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
            "SELECT id, role, content, tool_call_id, tool_calls_json, seq FROM message
             WHERE session_id = ?1 AND seq = ?2",
        )?;
        let row = stmt
            .query_row(params![session_id, seq], |row| {
                let tool_calls_json: Option<String> = row.get(4)?;
                Ok(StoredMessage {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    tool_call_id: row.get(3)?,
                    tool_calls: tool_calls_json
                        .as_ref()
                        .and_then(|j| serde_json::from_str(j).ok()),
                    seq: row.get(5)?,
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
                self.conn.execute(
                    "INSERT INTO message (session_id, role, content, seq, created_at) VALUES (?1, 'user', ?2, ?3, ?4)",
                    params![session_id, content, seq, now],
                )?;
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let tc_json = tool_calls
                    .as_ref()
                    .map(|t| serde_json::to_string(t))
                    .transpose()?;
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

    pub fn record_tool_call(
        &self,
        message_id: i64,
        id: &str,
        tool: &str,
        args: &str,
        risk: Option<&str>,
        output: &str,
        status: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO tool_call (id, message_id, tool, args, risk, output, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, message_id, tool, args, risk, output, status],
        )?;
        Ok(())
    }

    pub fn record_review(
        &self,
        session_id: &str,
        tool_call_id: Option<&str>,
        action_json: &str,
        risk_level: &str,
        user_auth_level: &str,
        outcome: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO review (session_id, tool_call_id, action_json, risk_level, user_auth_level, outcome, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![session_id, tool_call_id, action_json, risk_level, user_auth_level, outcome, reason, now],
        )?;
        Ok(())
    }

    pub fn all_messages_for_session(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, role, content, tool_call_id, tool_calls_json, seq FROM message
             WHERE session_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            let tool_calls_json: Option<String> = row.get(4)?;
            Ok(StoredMessage {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                tool_call_id: row.get(3)?,
                tool_calls: tool_calls_json
                    .as_ref()
                    .and_then(|j| serde_json::from_str(j).ok()),
                seq: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("loading messages")
    }

    pub fn estimate_context_tokens(&self, session_id: &str) -> u64 {
        self.load_context_messages(session_id)
            .map(|msgs| {
                msgs.iter()
                    .map(|m| match m {
                        Message::User { content } => approx_tokens(content),
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
    let lock_dir = crate::paths::runtime_dir();
    crate::paths::ensure_dir(&lock_dir)?;
    let lock_path = lock_dir.join(format!("{session_id}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .context("opening session lock file")?;
    file.try_lock_exclusive()
        .map_err(|_| anyhow::anyhow!("session busy"))?;
    Ok(SessionLock { _file: file })
}

pub fn write_session_id(path: &Path, id: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, id)?;
    Ok(())
}
