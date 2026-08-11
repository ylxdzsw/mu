use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::bash::BashRisk;
use crate::models::ResolvedModelRef;
use crate::provider::{
    Attachment, ContentPart, ImageDetail, Message, ModelApi, NativeReplay, ReasoningBlock, Request,
    ToolAttachment, ToolCall, Usage, UserContent,
};

pub const BASH_CALL_ID_ENV: &str = "MU_BASH_CALL_ID";
pub const ATTACHMENT_MANIFEST_ENV: &str = "MU_ATTACHMENT_MANIFEST";
pub const OBJECTS_DIR_ENV: &str = "MU_OBJECTS_DIR";
pub const INTERRUPTED_TOOL_RESULT: &str = "error: interrupted — this command may have started and not completed; its effects are unknown. Verify the resulting state before relying on it.";
pub const RESUME_PROMPT: &str = "Continue the current task from where you stopped.";

const FORMAT_VERSION: u32 = 1;
const SESSION_ID_RETRIES: usize = 16;
const EXTERNAL_TEXT_BYTES: usize = 256 * 1024;
const MAX_BASH_ATTACHMENTS: usize = 8;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub last_model: Option<String>,
    pub title: Option<String>,
    pub reported_context_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub cwd: String,
    pub title: Option<String>,
    pub message_count: u64,
    pub turn_count: u64,
}

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

pub struct CompactionCompletion<'a> {
    pub summary: &'a str,
    pub through_seq: i64,
    pub retained_turn_ids: Vec<String>,
    pub native_response: Option<&'a Value>,
    pub usage: Option<&'a Usage>,
}

pub struct GuardrailCompletion<'a> {
    pub call_id: i64,
    pub attempt: u32,
    pub outcome: &'a str,
    pub risk_level: Option<&'a str>,
    pub auth_level: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub native_response: Option<&'a Value>,
    pub usage: Option<&'a Usage>,
}

struct AssistantCompletion<'a> {
    message: &'a Message,
    reasoning_blocks: &'a [ReasoningBlock],
    resumable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderOrigin {
    pub canonical_model_ref: String,
    pub provider_id: String,
    pub api: String,
    pub endpoint: String,
    pub wire_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecipe {
    pub format: String,
    pub input: Value,
    pub envelope: Value,
    pub canonical_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestSubject {
    pub call_id: i64,
    pub attempt: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectRef {
    #[serde(rename = "object")]
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Meta {
    #[serde(rename = "type")]
    kind: String,
    format: String,
    version: u32,
    session_id: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EventLine {
    seq: i64,
    at: String,
    #[serde(flatten)]
    event: Event,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Event {
    SystemPrompt {
        content: String,
    },
    TurnStarted {
        turn_id: String,
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        git_worktree_root: Option<String>,
        prompt: PersistedUserContent,
    },
    ProviderRequested {
        turn_id: String,
        exchange_id: String,
        purpose: String,
        origin: ProviderOrigin,
        request_recipe: RequestRecipe,
        #[serde(skip_serializing_if = "Option::is_none")]
        subject: Option<RequestSubject>,
    },
    ProviderCompleted {
        exchange_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        response_json: Option<ObjectRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        projection: Projection,
    },
    ProviderFailed {
        exchange_id: String,
        error_class: String,
        error: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        partial_response_json: Option<ObjectRef>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    ProviderInterrupted {
        exchange_id: String,
    },
    BashCompleted {
        turn_id: String,
        call_id: i64,
        outcome: String,
        output: PersistedText,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        attachments: Vec<PersistedToolAttachment>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Projection {
    Assistant {
        turn_state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasoning_blocks: Vec<ReasoningBlock>,
        #[serde(skip_serializing_if = "Option::is_none")]
        native_replay: Option<NativeReplay>,
        bash_calls: Vec<PersistedBashCall>,
    },
    Compaction {
        summary: String,
        through_seq: i64,
        retained_turn_ids: Vec<String>,
    },
    Guardrail {
        call_id: i64,
        attempt: u32,
        outcome: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        risk_level: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        auth_level: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedBashCall {
    call_id: i64,
    provider_call_id: String,
    position: usize,
    arguments: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    declared_risk: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersistedUserContent {
    Text { text: String },
    Parts { parts: Vec<PersistedContentPart> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersistedContentPart {
    Text {
        text: String,
    },
    Attachment {
        object: ObjectRef,
        filename: String,
        media_type: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedToolAttachment {
    object: ObjectRef,
    filename: String,
    media_type: String,
    detail: ImageDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestEntry {
    call_id: i64,
    sha256: String,
    filename: String,
    media_type: String,
    detail: ImageDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersistedText {
    Inline { text: String },
    Object { object: ObjectRef },
}

#[derive(Clone)]
struct Journal {
    meta: Meta,
    events: Arc<Vec<EventLine>>,
}

pub struct Store {
    root: PathBuf,
    attachment_scope: PathBuf,
    locks: Mutex<HashMap<String, LockedSession>>,
    ephemeral: bool,
}

struct LockedSession {
    file: File,
    journal: Journal,
}

#[derive(Debug)]
pub struct SessionBusy;

impl std::fmt::Display for SessionBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session busy")
    }
}

impl std::error::Error for SessionBusy {}

impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        ensure_private_dir(root)?;
        ensure_private_dir(&root.join("sessions"))?;
        ensure_private_dir(&root.join("objects"))?;
        let canonical = root.canonicalize()?;
        let scope_key = hex(Sha256::digest(canonical.as_os_str().as_bytes()));
        let attachment_scope = crate::paths::runtime_dir()?.join(scope_key);
        ensure_private_dir(&attachment_scope)?;
        Ok(Self {
            root: root.to_path_buf(),
            attachment_scope,
            locks: Mutex::new(HashMap::new()),
            ephemeral: false,
        })
    }

    pub fn open_memory() -> Result<Self> {
        let base = std::env::temp_dir();
        for _ in 0..16 {
            let suffix = hex(crate::random::random_bytes::<12>()?);
            let root = base.join(format!("mu-store-{suffix}"));
            match std::fs::create_dir(&root) {
                Ok(()) => {
                    let mut store = Self::open(&root)?;
                    store.ephemeral = true;
                    return Ok(store);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        bail!("could not create temporary session store")
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }

    pub fn attachment_paths(&self, session_id: &str) -> Result<(PathBuf, PathBuf)> {
        let directory = self.attachment_scope.join(session_id);
        ensure_private_dir(&directory)?;
        Ok((directory.join("attachments.jsonl"), self.objects_dir()))
    }

    fn session_path(&self, session_id: &str) -> PathBuf {
        self.root
            .join("sessions")
            .join(format!("{session_id}.jsonl"))
    }

    #[cfg(test)]
    pub fn create_session(&self, _cwd: &str) -> Result<Session> {
        self.create_session_seeded("system prompt")
    }

    pub fn create_session_seeded(&self, system_prompt: &str) -> Result<Session> {
        self.create_session_with(system_prompt, crate::random::session_id)
    }

    fn create_session_with(
        &self,
        system_prompt: &str,
        mut next_id: impl FnMut() -> Result<String>,
    ) -> Result<Session> {
        for _ in 0..SESSION_ID_RETRIES {
            let id = next_id()?;
            let path = self.session_path(&id);
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true).mode(0o600);
            let mut file = match options.open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            flock(&file, libc::LOCK_EX)?;
            let created_at = now();
            let meta = Meta {
                kind: "meta".into(),
                format: "mu-session".into(),
                version: FORMAT_VERSION,
                session_id: id.clone(),
                created_at: created_at.clone(),
            };
            write_json_line(&mut file, &meta)?;
            write_json_line(
                &mut file,
                &EventLine {
                    seq: 1,
                    at: created_at.clone(),
                    event: Event::SystemPrompt {
                        content: system_prompt.to_string(),
                    },
                },
            )?;
            file.sync_all()?;
            sync_dir(&self.root.join("sessions"))?;
            flock(&file, libc::LOCK_UN)?;
            return Ok(Session {
                id,
                cwd: String::new(),
                last_model: None,
                title: None,
                reported_context_tokens: None,
            });
        }
        bail!("could not allocate a unique session id")
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let Some(journal) = self.load_optional(id)? else {
            return Ok(None);
        };
        Ok(Some(self.project_session(&journal)?))
    }

    pub fn list_sessions(&self, limit: usize) -> Result<Vec<(Session, String)>> {
        let mut sessions = Vec::new();
        for entry in std::fs::read_dir(self.root.join("sessions"))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let journal = match self.load_path(&path) {
                Ok(journal) => journal,
                Err(_) if incomplete_session_initialization(&path)? => continue,
                Err(error) => return Err(error),
            };
            if !journal
                .events
                .iter()
                .any(|line| matches!(line.event, Event::SystemPrompt { .. }))
            {
                continue;
            }
            let session = self.project_session(&journal)?;
            let updated = activity_at(&journal).to_string();
            sessions.push((session, updated));
        }
        sessions.sort_by(|left, right| right.1.cmp(&left.1));
        sessions.truncate(limit);
        Ok(sessions)
    }

    pub fn session_summary(&self, id: &str) -> Result<Option<SessionSummary>> {
        let Some(journal) = self.load_optional(id)? else {
            return Ok(None);
        };
        let session = self.project_session(&journal)?;
        Ok(Some(SessionSummary {
            id: id.to_string(),
            created_at: journal.meta.created_at.clone(),
            updated_at: activity_at(&journal).to_string(),
            cwd: session.cwd,
            title: session.title,
            message_count: journal
                .events
                .iter()
                .filter(|line| is_semantic(&line.event))
                .count() as u64,
            turn_count: journal
                .events
                .iter()
                .filter(|line| matches!(line.event, Event::TurnStarted { .. }))
                .count() as u64,
        }))
    }

    pub fn current_session(&self) -> Result<Option<Session>> {
        let target = match std::fs::read_link(self.root.join("current-session")) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let id = target
            .file_stem()
            .and_then(|value| value.to_str())
            .context("invalid current-session target")?;
        self.get_session(id)
    }

    pub fn select_session(&self, session_id: &str) -> Result<()> {
        if !valid_session_id(session_id) {
            bail!("session not found: {session_id}")
        }
        if !self.session_path(session_id).is_file() {
            bail!("session not found: {session_id}")
        }
        for _ in 0..SESSION_ID_RETRIES {
            let suffix = hex(crate::random::random_bytes::<8>()?);
            let temporary = self.root.join(format!(".current-session.{suffix}"));
            match std::os::unix::fs::symlink(
                Path::new("sessions").join(format!("{session_id}.jsonl")),
                &temporary,
            ) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
            if let Err(error) = std::fs::rename(&temporary, self.root.join("current-session")) {
                let _ = std::fs::remove_file(&temporary);
                return Err(error.into());
            }
            sync_dir(&self.root)?;
            return Ok(());
        }
        bail!("could not allocate a temporary current-session link")
    }

    pub fn latest_attempt_model(&self, session_id: &str) -> Result<Option<String>> {
        self.with_journal(session_id, |journal| {
            Ok(journal
                .events
                .iter()
                .rev()
                .find_map(|line| match &line.event {
                    Event::ProviderRequested {
                        purpose, origin, ..
                    } if purpose == "agent" || purpose == "compaction" => {
                        Some(origin.canonical_model_ref.clone())
                    }
                    _ => None,
                }))
        })
    }

    pub fn latest_floating_provider(
        &self,
        session_id: &str,
        model_id: &str,
        mut provider_has_model: impl FnMut(&str) -> bool,
    ) -> Result<Option<String>> {
        self.with_journal(session_id, |journal| {
            Ok(journal.events.iter().rev().find_map(|line| {
                let Event::ProviderRequested { origin, .. } = &line.event else {
                    return None;
                };
                let floating_provider = origin
                    .canonical_model_ref
                    .strip_prefix('(')
                    .and_then(|reference| reference.split_once(")/"))
                    .map(|(provider_id, _)| provider_id);
                (floating_provider == Some(origin.provider_id.as_str())
                    && origin.wire_model == model_id
                    && provider_has_model(&origin.provider_id))
                .then(|| origin.provider_id.clone())
            }))
        })
    }

    #[cfg(test)]
    pub fn append_test_agent_exchange(
        &self,
        session_id: &str,
        model: &str,
        outcome: &str,
        context_tokens: u64,
    ) -> Result<()> {
        let turn_id = match self.current_turn_id(session_id) {
            Ok(turn_id) => turn_id,
            Err(_) => self.start_turn(session_id, "/tmp", None, &"test".into())?,
        };
        let (provider_id, wire_model) = model.split_once('/').unwrap_or(("test", model));
        let provider_id = provider_id
            .strip_prefix('(')
            .and_then(|provider_id| provider_id.strip_suffix(')'))
            .unwrap_or(provider_id);
        let (wire_model, effort) = wire_model
            .split_once(':')
            .map_or((wire_model, None), |(model, effort)| (model, Some(effort)));
        let native = serde_json::json!({"model":wire_model});
        let exchange_id = self.start_provider_request(
            session_id,
            &turn_id,
            "agent",
            ProviderOrigin {
                canonical_model_ref: model.to_string(),
                provider_id: provider_id.to_string(),
                api: "test".into(),
                endpoint: String::new(),
                wire_model: wire_model.to_string(),
                effort: effort.map(str::to_string),
            },
            self.request_recipe("test.v1", &native, serde_json::json!({"kind":"agent"}))?,
            None,
        )?;
        if outcome == "completed" {
            self.complete_assistant_exchange(
                session_id,
                &exchange_id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    native_replay: None,
                },
                &[],
                None,
                Some(&Usage {
                    total_tokens: context_tokens,
                    ..Usage::default()
                }),
            )?;
        } else {
            self.fail_provider_exchange(
                session_id,
                &exchange_id,
                "test",
                serde_json::json!({"message":"test failure"}),
                None,
                None,
            )?;
        }
        Ok(())
    }

    pub fn is_session_clean(&self, session_id: &str) -> Result<bool> {
        let journal = self.load(session_id)?;
        let Some(turn_seq) =
            journal.events.iter().rev().find_map(|line| {
                matches!(line.event, Event::TurnStarted { .. }).then_some(line.seq)
            })
        else {
            return Ok(true);
        };
        let mut calls = HashSet::new();
        let mut results = HashSet::new();
        let mut complete = false;
        for line in journal.events.iter().filter(|line| line.seq > turn_seq) {
            match &line.event {
                Event::ProviderCompleted {
                    projection:
                        Projection::Assistant {
                            turn_state,
                            bash_calls,
                            ..
                        },
                    ..
                } => {
                    calls.extend(bash_calls.iter().map(|call| call.call_id));
                    complete = turn_state == "complete";
                }
                Event::BashCompleted { call_id, .. } => {
                    results.insert(*call_id);
                }
                _ => {}
            }
        }
        Ok(complete && calls.is_subset(&results))
    }

    pub fn resume_reminder_needed(&self, session_id: &str) -> Result<bool> {
        let journal = self.load(session_id)?;
        let Some(turn_id) = latest_turn_id(&journal) else {
            return Ok(false);
        };
        let exchange_turns = provider_exchange_turns(&journal);
        let resumable = journal
            .events
            .iter()
            .rev()
            .find_map(|line| match &line.event {
                Event::ProviderCompleted {
                    exchange_id,
                    projection: Projection::Assistant { turn_state, .. },
                    ..
                } if exchange_turns.get(exchange_id.as_str()) == Some(&turn_id) => {
                    Some((line.seq, turn_state == "resume"))
                }
                _ => None,
            });
        Ok(resumable
            .is_some_and(|(seq, resume)| resume && !resume_was_requested(&journal, &turn_id, seq)))
    }

    pub fn normalize_interrupted_tail(&self, session_id: &str) -> Result<usize> {
        let journal = self.load(session_id)?;
        let mut terminal = HashSet::new();
        let mut requested = Vec::new();
        let mut exchange_turns = HashMap::new();
        let mut calls = HashMap::new();
        let mut results = HashSet::new();
        let mut denied = HashMap::new();
        for line in journal.events.iter() {
            match &line.event {
                Event::ProviderRequested {
                    exchange_id,
                    turn_id,
                    ..
                } => {
                    requested.push(exchange_id.clone());
                    exchange_turns.insert(exchange_id.clone(), turn_id.clone());
                }
                Event::ProviderCompleted {
                    exchange_id,
                    projection,
                    ..
                } => {
                    terminal.insert(exchange_id.clone());
                    match projection {
                        Projection::Assistant { bash_calls, .. } => {
                            let turn_id = exchange_turns
                                .get(exchange_id)
                                .cloned()
                                .or_else(|| latest_turn_before(&journal.events, line.seq))
                                .context("assistant completion has no request turn")?;
                            for call in bash_calls {
                                calls.insert(call.call_id, turn_id.clone());
                            }
                        }
                        Projection::Guardrail {
                            call_id,
                            outcome,
                            reason,
                            ..
                        } if outcome == "deny" => {
                            denied.insert(*call_id, reason.clone().unwrap_or_default());
                        }
                        _ => {}
                    }
                }
                Event::ProviderFailed { exchange_id, .. }
                | Event::ProviderInterrupted { exchange_id } => {
                    terminal.insert(exchange_id.clone());
                }
                Event::BashCompleted { call_id, .. } => {
                    results.insert(*call_id);
                }
                _ => {}
            }
        }
        for exchange_id in requested
            .into_iter()
            .filter(|exchange_id| !terminal.contains(exchange_id))
        {
            self.append(session_id, Event::ProviderInterrupted { exchange_id })?;
        }
        let mut unresolved = calls
            .iter()
            .filter(|(call_id, _)| !results.contains(call_id))
            .map(|(call_id, turn_id)| (*call_id, turn_id.clone()))
            .collect::<Vec<_>>();
        unresolved.sort_unstable_by_key(|(call_id, _)| *call_id);
        let mut normalized = 0;
        for (call_id, turn_id) in unresolved {
            let (outcome, output) = if let Some(reason) = denied.get(&call_id) {
                (
                    "error",
                    format!("error: guardrail denied this command: {reason}"),
                )
            } else {
                ("interrupted", INTERRUPTED_TOOL_RESULT.to_string())
            };
            self.append(
                session_id,
                Event::BashCompleted {
                    turn_id,
                    call_id,
                    outcome: outcome.into(),
                    output: PersistedText::Inline { text: output },
                    exit_code: None,
                    duration_ms: None,
                    attachments: Vec::new(),
                },
            )?;
            normalized += 1;
        }
        Ok(normalized)
    }

    pub fn message_records_from_seq(
        &self,
        session_id: &str,
        start_seq: i64,
    ) -> Result<Vec<MessageRecord>> {
        Ok(self
            .records(&self.load(session_id)?)?
            .into_iter()
            .filter(|record| record.seq >= start_seq)
            .collect())
    }

    #[cfg(test)]
    pub fn audit_events(&self, session_id: &str) -> Result<Vec<Value>> {
        self.load(session_id)?
            .events
            .iter()
            .map(|event| serde_json::to_value(event).map_err(Into::into))
            .collect()
    }

    pub fn load_context_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let journal = self.load(session_id)?;
        self.context(&journal)
    }

    pub fn start_turn(
        &self,
        session_id: &str,
        cwd: &str,
        git_worktree_root: Option<&str>,
        prompt: &UserContent,
    ) -> Result<String> {
        let turn_id = format!("t{}", self.next_seq(session_id)?);
        let prompt = self.persist_user_content(prompt)?;
        self.append(
            session_id,
            Event::TurnStarted {
                turn_id: turn_id.clone(),
                cwd: cwd.to_string(),
                git_worktree_root: git_worktree_root.map(str::to_string),
                prompt,
            },
        )?;
        Ok(turn_id)
    }

    #[cfg(test)]
    pub fn append_message(&self, session_id: &str, message: &Message) -> Result<i64> {
        self.append_message_with_bash_calls(session_id, message)
            .map(|item| item.0)
    }

    #[cfg(test)]
    pub fn append_message_with_bash_calls(
        &self,
        session_id: &str,
        message: &Message,
    ) -> Result<(i64, Vec<i64>)> {
        match message {
            Message::User { content } => {
                let session = self.get_session(session_id)?.context("session not found")?;
                let cwd = if session.cwd.is_empty() {
                    "/tmp"
                } else {
                    &session.cwd
                };
                let turn_id = self.start_turn(session_id, cwd, None, content)?;
                let seq = self.with_journal(session_id, |journal| {
                    journal
                        .events
                        .iter()
                        .find_map(|line| match &line.event {
                            Event::TurnStarted { turn_id: id, .. } if id == &turn_id => {
                                Some(line.seq)
                            }
                            _ => None,
                        })
                        .context("test turn event missing after append")
                })?;
                Ok((seq, Vec::new()))
            }
            Message::Assistant { .. } => {
                let turn_id = self.current_turn_id(session_id)?;
                let exchange_id =
                    self.start_test_provider_request(session_id, &turn_id, "agent")?;
                self.complete_assistant_exchange(session_id, &exchange_id, message, &[], None, None)
            }
            Message::System { .. } => Ok((1, Vec::new())),
            Message::Tool { .. } => bail!("Bash results require an internal Bash call identity"),
        }
    }

    #[cfg(test)]
    pub fn append_summary(&self, session_id: &str, content: &str) -> Result<()> {
        let through_seq = self
            .records(&self.load(session_id)?)?
            .last()
            .map_or(0, |record| record.seq);
        self.append_compaction(session_id, content, through_seq)
    }

    #[cfg(test)]
    fn append_compaction(&self, session_id: &str, content: &str, through_seq: i64) -> Result<()> {
        let turn_id = self.current_turn_id(session_id)?;
        let retained_turn_ids = self.turn_ids_after(session_id, through_seq)?;
        let exchange_id = self.start_test_provider_request(session_id, &turn_id, "compaction")?;
        self.complete_compaction_exchange(
            session_id,
            &exchange_id,
            CompactionCompletion {
                summary: content,
                through_seq,
                retained_turn_ids,
                native_response: None,
                usage: None,
            },
        )
    }

    #[cfg(test)]
    fn start_test_provider_request(
        &self,
        session_id: &str,
        turn_id: &str,
        purpose: &str,
    ) -> Result<String> {
        let canonical_model_ref = if purpose == "compaction" {
            self.latest_attempt_model(session_id)?
                .unwrap_or_else(|| "test/model".into())
        } else {
            "test/model".into()
        };
        let native_request = serde_json::json!({"model":"test"});
        let recipe = self.request_recipe(
            "test.v1",
            &native_request,
            serde_json::json!({"kind":purpose}),
        )?;
        self.start_provider_request(
            session_id,
            turn_id,
            purpose,
            ProviderOrigin {
                canonical_model_ref,
                provider_id: "test".into(),
                api: "test".into(),
                endpoint: String::new(),
                wire_model: "model".into(),
                effort: None,
            },
            recipe,
            None,
        )
    }

    pub fn persist_bash_result(
        &self,
        session_id: &str,
        record: BashResultRecord<'_>,
        content: &str,
        attachments: &[ToolAttachment],
    ) -> Result<(i64, Vec<ToolAttachment>)> {
        let (turn_id, already_completed) = self.with_journal(session_id, |journal| {
            Ok((
                call_turn_id(journal, record.bash_call_id)
                    .context("locating Bash claim for result persistence")?,
                journal.events.iter().any(|line| {
                    matches!(
                        line.event,
                        Event::BashCompleted { call_id, .. } if call_id == record.bash_call_id
                    )
                }),
            ))
        })?;
        if already_completed {
            bail!("Bash result already exists")
        }
        let attachments = attachments
            .iter()
            .map(|attachment| self.persist_tool_attachment(attachment))
            .collect::<Result<Vec<_>>>()?;
        let output = self.persist_text(content)?;
        let seq = self.append(
            session_id,
            Event::BashCompleted {
                turn_id,
                call_id: record.bash_call_id,
                outcome: record.outcome.to_string(),
                output,
                exit_code: record.exit_code,
                duration_ms: record.duration_ms,
                attachments,
            },
        )?;
        let hydrated = self.load_tool_attachments(session_id, record.bash_call_id)?;
        if let Ok((manifest, _)) = self.attachment_paths(session_id) {
            let _ = cleanup_bash_attachments(&manifest, record.bash_call_id);
        }
        Ok((seq, hydrated))
    }

    pub fn latest_summary_sequence(&self, session_id: &str) -> Result<Option<i64>> {
        let journal = self.load(session_id)?;
        Ok(latest_compaction(&journal).map(|line| line.seq))
    }

    pub fn latest_compaction_through_seq(&self, session_id: &str) -> Result<Option<i64>> {
        let journal = self.load(session_id)?;
        Ok(
            latest_compaction(&journal).and_then(|line| match &line.event {
                Event::ProviderCompleted {
                    projection: Projection::Compaction { through_seq, .. },
                    ..
                } => Some(*through_seq),
                _ => None,
            }),
        )
    }

    pub fn estimate_context_tokens(&self, session_id: &str) -> Result<u64> {
        Ok(self
            .load_context_messages(session_id)?
            .iter()
            .map(Message::approx_tokens)
            .sum())
    }

    pub fn acquire_session_lock(&self, session_id: &str) -> Result<SessionLock<'_>> {
        if !valid_session_id(session_id) {
            bail!("session not found: {session_id}")
        }
        let mut locks = self.locks.lock().expect("session lock map poisoned");
        if locks.contains_key(session_id) {
            return Err(anyhow::Error::new(SessionBusy));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.session_path(session_id))
            .with_context(|| format!("opening session journal: {session_id}"))?;
        match flock_nonblocking(&file) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::EWOULDBLOCK) => {
                return Err(anyhow::Error::new(SessionBusy));
            }
            Err(error) => return Err(error.into()),
        }
        let mut file = file;
        let journal = read_journal(&mut file, Some(session_id), true)?;
        locks.insert(session_id.to_string(), LockedSession { file, journal });
        Ok(SessionLock {
            store: self,
            session_id: session_id.to_string(),
        })
    }

    pub fn is_session_busy(&self, session_id: &str) -> Result<bool> {
        if !valid_session_id(session_id) {
            bail!("session not found: {session_id}")
        }
        if self
            .locks
            .lock()
            .expect("session lock map poisoned")
            .contains_key(session_id)
        {
            return Ok(true);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.session_path(session_id))?;
        match flock_nonblocking(&file) {
            Ok(()) => {
                flock(&file, libc::LOCK_UN)?;
                Ok(false)
            }
            Err(error) if error.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(true),
            Err(error) => Err(error.into()),
        }
    }

    pub fn start_provider_request(
        &self,
        session_id: &str,
        turn_id: &str,
        purpose: &str,
        origin: ProviderOrigin,
        recipe: RequestRecipe,
        subject: Option<RequestSubject>,
    ) -> Result<String> {
        let exchange_id = format!("e{}", self.next_seq(session_id)?);
        self.append(
            session_id,
            Event::ProviderRequested {
                turn_id: turn_id.to_string(),
                exchange_id: exchange_id.clone(),
                purpose: purpose.to_string(),
                origin,
                request_recipe: recipe,
                subject,
            },
        )?;
        self.reconstruct_provider_request(session_id, &exchange_id)
            .context("verifying persisted provider request recipe")?;
        Ok(exchange_id)
    }

    pub fn current_turn_id(&self, session_id: &str) -> Result<String> {
        self.with_journal(session_id, |journal| {
            latest_turn_id(journal).context("session has no submitted turn")
        })
    }

    pub fn current_context_seq(&self, session_id: &str) -> Result<i64> {
        self.with_journal(session_id, |journal| {
            Ok(journal
                .events
                .iter()
                .rev()
                .find(|line| is_semantic(&line.event))
                .map_or(1, |line| line.seq))
        })
    }

    pub fn complete_assistant_exchange(
        &self,
        session_id: &str,
        exchange_id: &str,
        message: &Message,
        reasoning_blocks: &[ReasoningBlock],
        native_response: Option<&Value>,
        usage: Option<&Usage>,
    ) -> Result<(i64, Vec<i64>)> {
        self.complete_assistant_exchange_inner(
            session_id,
            exchange_id,
            AssistantCompletion {
                message,
                reasoning_blocks,
                resumable: false,
            },
            native_response,
            usage,
        )
    }

    pub fn complete_resumable_assistant_exchange(
        &self,
        session_id: &str,
        exchange_id: &str,
        message: &Message,
        reasoning_blocks: &[ReasoningBlock],
        native_response: Option<&Value>,
        usage: Option<&Usage>,
    ) -> Result<(i64, Vec<i64>)> {
        self.complete_assistant_exchange_inner(
            session_id,
            exchange_id,
            AssistantCompletion {
                message,
                reasoning_blocks,
                resumable: true,
            },
            native_response,
            usage,
        )
    }

    fn complete_assistant_exchange_inner(
        &self,
        session_id: &str,
        exchange_id: &str,
        completion: AssistantCompletion<'_>,
        native_response: Option<&Value>,
        usage: Option<&Usage>,
    ) -> Result<(i64, Vec<i64>)> {
        let AssistantCompletion {
            message,
            reasoning_blocks,
            resumable,
        } = completion;
        if !matches!(message, Message::Assistant { .. }) {
            self.fail_provider_exchange(
                session_id,
                exchange_id,
                "invalid_response",
                serde_json::json!({"message":"provider completion is not an assistant message"}),
                native_response,
                usage,
            )?;
            bail!("provider completion is not an assistant message")
        }
        let Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            native_replay,
        } = message
        else {
            unreachable!()
        };
        let mut provider_call_ids = HashSet::new();
        if let Some(call) = tool_calls
            .as_deref()
            .unwrap_or_default()
            .iter()
            .find(|call| !provider_call_ids.insert(call.id.as_str()))
        {
            self.fail_provider_exchange(
                session_id,
                exchange_id,
                "invalid_response",
                serde_json::json!({
                    "message": format!("duplicate provider tool call id: {}", call.id)
                }),
                native_response,
                usage,
            )?;
            bail!("duplicate provider tool call id: {}", call.id)
        }
        let mut next_call = self.with_journal(session_id, |journal| Ok(next_call_id(journal)))?;
        let bash_calls = tool_calls
            .as_deref()
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(position, call)| {
                let persisted = PersistedBashCall {
                    call_id: next_call,
                    provider_call_id: call.id.clone(),
                    position,
                    arguments: call.arguments.clone(),
                    declared_risk: BashRisk::from_args_json(&call.arguments)
                        .map(|risk| risk.as_str().to_string()),
                };
                next_call += 1;
                persisted
            })
            .collect::<Vec<_>>();
        let ids = bash_calls.iter().map(|call| call.call_id).collect();
        let response_json = native_response
            .map(serde_json::to_vec)
            .transpose()?
            .map(|bytes| self.write_object(&bytes))
            .transpose()?;
        let seq = self.append(
            session_id,
            Event::ProviderCompleted {
                exchange_id: exchange_id.to_string(),
                response_json,
                usage: usage.cloned(),
                projection: Projection::Assistant {
                    turn_state: if bash_calls.is_empty() {
                        if resumable { "resume" } else { "complete" }.into()
                    } else {
                        "continue".into()
                    },
                    text: content.clone(),
                    reasoning_content: reasoning_content.clone(),
                    reasoning_blocks: reasoning_blocks.to_vec(),
                    native_replay: native_replay.clone(),
                    bash_calls,
                },
            },
        )?;
        Ok((seq, ids))
    }

    pub fn complete_compaction_exchange(
        &self,
        session_id: &str,
        exchange_id: &str,
        completion: CompactionCompletion<'_>,
    ) -> Result<()> {
        let response_json = completion
            .native_response
            .map(serde_json::to_vec)
            .transpose()?
            .map(|bytes| self.write_object(&bytes))
            .transpose()?;
        self.append(
            session_id,
            Event::ProviderCompleted {
                exchange_id: exchange_id.to_string(),
                response_json,
                usage: completion.usage.cloned(),
                projection: Projection::Compaction {
                    summary: completion.summary.to_string(),
                    through_seq: completion.through_seq,
                    retained_turn_ids: completion.retained_turn_ids,
                },
            },
        )?;
        Ok(())
    }

    pub fn complete_guardrail_exchange(
        &self,
        session_id: &str,
        exchange_id: &str,
        completion: GuardrailCompletion<'_>,
    ) -> Result<()> {
        let response_json = completion
            .native_response
            .map(serde_json::to_vec)
            .transpose()?
            .map(|bytes| self.write_object(&bytes))
            .transpose()?;
        self.append(
            session_id,
            Event::ProviderCompleted {
                exchange_id: exchange_id.to_string(),
                response_json,
                usage: completion.usage.cloned(),
                projection: Projection::Guardrail {
                    call_id: completion.call_id,
                    attempt: completion.attempt,
                    outcome: completion.outcome.to_string(),
                    risk_level: completion.risk_level.map(str::to_string),
                    auth_level: completion.auth_level.map(str::to_string),
                    reason: completion.reason.map(str::to_string),
                },
            },
        )?;
        Ok(())
    }

    pub fn turn_ids_after(&self, session_id: &str, through_seq: i64) -> Result<Vec<String>> {
        Ok(self
            .load(session_id)?
            .events
            .iter()
            .filter_map(|line| match &line.event {
                Event::TurnStarted { turn_id, .. } if line.seq > through_seq => {
                    Some(turn_id.clone())
                }
                _ => None,
            })
            .collect())
    }

    pub fn fail_provider_exchange(
        &self,
        session_id: &str,
        exchange_id: &str,
        error_class: &str,
        error: Value,
        partial_response: Option<&Value>,
        usage: Option<&Usage>,
    ) -> Result<()> {
        let partial_response_json = partial_response
            .map(serde_json::to_vec)
            .transpose()?
            .map(|bytes| self.write_object(&bytes))
            .transpose()?;
        self.append(
            session_id,
            Event::ProviderFailed {
                exchange_id: exchange_id.to_string(),
                error_class: error_class.to_string(),
                error,
                partial_response_json,
                usage: usage.cloned(),
            },
        )?;
        Ok(())
    }

    pub fn interrupt_provider_exchange(&self, session_id: &str, exchange_id: &str) -> Result<()> {
        self.append(
            session_id,
            Event::ProviderInterrupted {
                exchange_id: exchange_id.to_string(),
            },
        )?;
        Ok(())
    }

    pub fn request_recipe(
        &self,
        format: &str,
        native_request: &Value,
        mut input: Value,
    ) -> Result<RequestRecipe> {
        let mut envelope = native_request.clone();
        let mut native_fields = serde_json::Map::new();
        if let Some(object) = envelope.as_object_mut() {
            for key in ["input", "messages", "system", "tools"] {
                if let Some(value) = object.remove(key) {
                    native_fields.insert(key.to_string(), value);
                }
            }
        }
        if (input["kind"] != "agent" || format.starts_with("test."))
            && let Some(object) = input.as_object_mut()
        {
            object.insert("native_fields".into(), Value::Object(native_fields));
        }
        Ok(RequestRecipe {
            format: format.to_string(),
            input,
            envelope,
            canonical_sha256: hex(Sha256::digest(canonical_json(native_request))),
        })
    }

    pub fn reconstruct_provider_request(
        &self,
        session_id: &str,
        exchange_id: &str,
    ) -> Result<Value> {
        let journal = self.load(session_id)?;
        let (origin, recipe) = journal
            .events
            .iter()
            .find_map(|line| match &line.event {
                Event::ProviderRequested {
                    exchange_id: id,
                    origin,
                    request_recipe,
                    ..
                } if id == exchange_id => Some((origin, request_recipe)),
                _ => None,
            })
            .with_context(|| format!("provider request not found: {exchange_id}"))?;
        let request = if let Some(fields) = recipe.input.get("native_fields") {
            let mut request = recipe.envelope.clone();
            let object = request
                .as_object_mut()
                .context("provider request envelope is not an object")?;
            for (key, value) in fields
                .as_object()
                .context("provider request native_fields is not an object")?
            {
                object.insert(key.clone(), value.clone());
            }
            request
        } else {
            let api = match recipe.format.as_str() {
                "openai.chat_completions.v1" => ModelApi::ChatCompletions,
                "openai.responses.v1" => ModelApi::Responses,
                "anthropic.messages.v1" => ModelApi::AnthropicMessages,
                format => bail!("unsupported provider request format: {format}"),
            };
            let through_seq = recipe.input["context_through_seq"]
                .as_i64()
                .context("agent request recipe has no context boundary")?;
            let messages = self.context_until(&journal, through_seq)?;
            let messages = if let Some(origins) = recipe.input.get("native_replay_origins") {
                let origins: Vec<crate::provider::ReplayOrigin> =
                    serde_json::from_value(origins.clone())
                        .context("invalid native replay origins in request recipe")?;
                crate::provider::filter_native_replay_for_origins(&messages, api, &origins)
            } else {
                crate::provider::filter_native_replay_for_legacy_origin(
                    &messages,
                    api,
                    &origin.provider_id,
                    &origin.endpoint,
                    &origin.wire_model,
                )
            };
            let cache_key = recipe
                .envelope
                .get("prompt_cache_key")
                .map(|value| {
                    value
                        .as_str()
                        .context("provider request prompt_cache_key is not a string")
                })
                .transpose()?
                .map(str::to_owned);
            let request = Request {
                model: ResolvedModelRef {
                    canonical: origin.canonical_model_ref.clone(),
                    provider_id: origin.provider_id.clone(),
                    model_id: origin.wire_model.clone(),
                    effort: origin.effort.clone(),
                },
                cache_key,
                messages,
                bash: true,
            };
            request.json(api)?
        };
        if hex(Sha256::digest(canonical_json(&request))) != recipe.canonical_sha256 {
            bail!("reconstructed provider request checksum mismatch")
        }
        Ok(request)
    }

    fn project_session(&self, journal: &Journal) -> Result<Session> {
        let cwd = journal
            .events
            .iter()
            .rev()
            .find_map(|line| match &line.event {
                Event::TurnStarted { cwd, .. } => Some(cwd.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let title = journal.events.iter().find_map(|line| match &line.event {
            Event::TurnStarted { prompt, .. } => {
                Some(user_text(prompt).chars().take(60).collect::<String>())
            }
            _ => None,
        });
        let last_model = journal
            .events
            .iter()
            .rev()
            .find_map(|line| match &line.event {
                Event::ProviderRequested {
                    purpose, origin, ..
                } if purpose == "agent" || purpose == "compaction" => {
                    Some(origin.canonical_model_ref.clone())
                }
                _ => None,
            });
        Ok(Session {
            id: journal.meta.session_id.clone(),
            cwd,
            last_model,
            title,
            reported_context_tokens: reported_context_tokens(journal),
        })
    }

    fn context(&self, journal: &Journal) -> Result<Vec<Message>> {
        self.context_until(journal, i64::MAX)
    }

    fn context_until(&self, journal: &Journal, max_seq: i64) -> Result<Vec<Message>> {
        let system = journal
            .events
            .iter()
            .find_map(|line| match &line.event {
                Event::SystemPrompt { content } => Some(content.clone()),
                _ => None,
            })
            .context("missing persisted system prompt")?;
        let compaction = latest_compaction_before(journal, max_seq);
        let through_seq = compaction.and_then(|line| match &line.event {
            Event::ProviderCompleted {
                projection: Projection::Compaction { through_seq, .. },
                ..
            } => Some(*through_seq),
            _ => None,
        });
        let mut messages = vec![Message::System { content: system }];
        let exchange_origins = journal
            .events
            .iter()
            .filter_map(|line| match &line.event {
                Event::ProviderRequested {
                    exchange_id,
                    origin,
                    ..
                } => Some((exchange_id.as_str(), origin)),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        let exchange_turns = provider_exchange_turns(journal);
        if let Some(line) = compaction
            && let Event::ProviderCompleted {
                projection: Projection::Compaction { summary, .. },
                ..
            } = &line.event
        {
            messages.push(Message::User {
                content: format!("[summary of earlier conversation]\n{summary}").into(),
            });
        }
        let mut previous_location: Option<(String, Option<String>)> = None;
        for line in journal.events.iter() {
            if line.seq > max_seq {
                break;
            }
            if through_seq.is_some_and(|through| line.seq <= through) {
                continue;
            }
            match &line.event {
                Event::TurnStarted {
                    cwd,
                    git_worktree_root,
                    prompt,
                    ..
                } => {
                    if let Some(location) = location_context(
                        previous_location.as_ref(),
                        cwd,
                        git_worktree_root.as_deref(),
                    ) {
                        messages.push(Message::User {
                            content: location.into(),
                        });
                    }
                    previous_location = Some((cwd.clone(), git_worktree_root.clone()));
                    messages.push(Message::User {
                        content: self.hydrate_user_content(prompt)?,
                    });
                }
                Event::ProviderCompleted {
                    exchange_id,
                    projection:
                        Projection::Assistant {
                            turn_state,
                            text,
                            reasoning_content,
                            native_replay,
                            bash_calls,
                            ..
                        },
                    ..
                } => {
                    let mut native_replay = native_replay.clone();
                    if let Some(native) = &mut native_replay
                        && native.provider_id.is_empty()
                        && let Some(origin) = exchange_origins.get(exchange_id.as_str())
                    {
                        native.provider_id = origin.provider_id.clone();
                    }
                    messages.push(Message::Assistant {
                        content: text.clone(),
                        reasoning_content: reasoning_content.clone(),
                        tool_calls: (!bash_calls.is_empty()).then(|| {
                            bash_calls
                                .iter()
                                .map(|call| ToolCall {
                                    id: call.provider_call_id.clone(),
                                    arguments: call.arguments.clone(),
                                })
                                .collect()
                        }),
                        native_replay,
                    });
                    if turn_state == "resume"
                        && let Some(turn_id) = exchange_turns.get(exchange_id.as_str())
                        && resume_was_requested(journal, turn_id, line.seq)
                    {
                        messages.push(Message::User {
                            content: RESUME_PROMPT.into(),
                        });
                    }
                }
                Event::BashCompleted {
                    call_id,
                    output,
                    attachments,
                    ..
                } => {
                    let call = find_call(journal, *call_id).context("Bash result claim missing")?;
                    messages.push(Message::Tool {
                        content: self.hydrate_text(output)?,
                        attachments: attachments
                            .iter()
                            .map(|attachment| self.hydrate_tool_attachment(attachment))
                            .collect::<Result<_>>()?,
                        tool_call_id: call.provider_call_id.clone(),
                    });
                }
                _ => {}
            }
        }
        Ok(messages)
    }

    fn records(&self, journal: &Journal) -> Result<Vec<MessageRecord>> {
        let mut records = Vec::new();
        for line in journal.events.iter() {
            match &line.event {
                Event::SystemPrompt { content } => records.push(MessageRecord {
                    kind: "system".into(),
                    content: content.clone(),
                    bash_calls: Vec::new(),
                    seq: line.seq,
                }),
                Event::TurnStarted { prompt, .. } => records.push(MessageRecord {
                    kind: "user".into(),
                    content: user_text(prompt),
                    bash_calls: Vec::new(),
                    seq: line.seq,
                }),
                Event::ProviderCompleted {
                    projection:
                        Projection::Assistant {
                            text, bash_calls, ..
                        },
                    ..
                } => records.push(MessageRecord {
                    kind: "assistant".into(),
                    content: text.clone().unwrap_or_default(),
                    bash_calls: bash_calls
                        .iter()
                        .map(|call| ToolCall {
                            id: call.provider_call_id.clone(),
                            arguments: call.arguments.clone(),
                        })
                        .collect(),
                    seq: line.seq,
                }),
                Event::ProviderCompleted {
                    projection: Projection::Compaction { summary, .. },
                    ..
                } => records.push(MessageRecord {
                    kind: "summary".into(),
                    content: summary.clone(),
                    bash_calls: Vec::new(),
                    seq: line.seq,
                }),
                Event::BashCompleted { output, .. } => records.push(MessageRecord {
                    kind: "bash_result".into(),
                    content: self.hydrate_text(output)?,
                    bash_calls: Vec::new(),
                    seq: line.seq,
                }),
                _ => {}
            }
        }
        Ok(records)
    }

    fn persist_user_content(&self, content: &UserContent) -> Result<PersistedUserContent> {
        Ok(match content {
            UserContent::Text(text) => PersistedUserContent::Text { text: text.clone() },
            UserContent::Parts(parts) => PersistedUserContent::Parts {
                parts: parts
                    .iter()
                    .map(|part| match part {
                        ContentPart::Text { text } => {
                            Ok(PersistedContentPart::Text { text: text.clone() })
                        }
                        ContentPart::Attachment { attachment } => {
                            Ok(PersistedContentPart::Attachment {
                                object: self.write_object(&attachment.data)?,
                                filename: attachment.filename.clone(),
                                media_type: attachment.media_type.clone(),
                            })
                        }
                    })
                    .collect::<Result<_>>()?,
            },
        })
    }

    fn hydrate_user_content(&self, content: &PersistedUserContent) -> Result<UserContent> {
        Ok(match content {
            PersistedUserContent::Text { text } => text.clone().into(),
            PersistedUserContent::Parts { parts } => UserContent::Parts(
                parts
                    .iter()
                    .map(|part| match part {
                        PersistedContentPart::Text { text } => {
                            Ok(ContentPart::Text { text: text.clone() })
                        }
                        PersistedContentPart::Attachment {
                            object,
                            filename,
                            media_type,
                        } => Ok(ContentPart::Attachment {
                            attachment: Attachment {
                                filename: filename.clone(),
                                media_type: media_type.clone(),
                                data: self.read_object(object)?,
                            },
                        }),
                    })
                    .collect::<Result<_>>()?,
            ),
        })
    }

    fn persist_tool_attachment(
        &self,
        attachment: &ToolAttachment,
    ) -> Result<PersistedToolAttachment> {
        Ok(PersistedToolAttachment {
            object: match &attachment.object_sha256 {
                Some(sha256) => ObjectRef {
                    sha256: sha256.clone(),
                },
                None => self.write_object(&attachment.attachment.data)?,
            },
            filename: attachment.attachment.filename.clone(),
            media_type: attachment.attachment.media_type.clone(),
            detail: attachment.detail,
        })
    }

    fn hydrate_tool_attachment(
        &self,
        attachment: &PersistedToolAttachment,
    ) -> Result<ToolAttachment> {
        Ok(ToolAttachment {
            attachment: Attachment {
                filename: attachment.filename.clone(),
                media_type: attachment.media_type.clone(),
                data: self.read_object(&attachment.object)?,
            },
            detail: attachment.detail,
            object_sha256: Some(attachment.object.sha256.clone()),
        })
    }

    fn load_tool_attachments(&self, session_id: &str, call_id: i64) -> Result<Vec<ToolAttachment>> {
        let journal = self.load(session_id)?;
        let attachments = journal
            .events
            .iter()
            .find_map(|line| match &line.event {
                Event::BashCompleted {
                    call_id: candidate,
                    attachments,
                    ..
                } if *candidate == call_id => Some(attachments),
                _ => None,
            })
            .context("Bash result not found")?;
        attachments
            .iter()
            .map(|attachment| self.hydrate_tool_attachment(attachment))
            .collect()
    }

    fn persist_text(&self, text: &str) -> Result<PersistedText> {
        if text.len() > EXTERNAL_TEXT_BYTES {
            Ok(PersistedText::Object {
                object: self.write_object(text.as_bytes())?,
            })
        } else {
            Ok(PersistedText::Inline {
                text: text.to_string(),
            })
        }
    }

    fn hydrate_text(&self, text: &PersistedText) -> Result<String> {
        match text {
            PersistedText::Inline { text } => Ok(text.clone()),
            PersistedText::Object { object } => {
                String::from_utf8(self.read_object(object)?).context("tool output is not UTF-8")
            }
        }
    }

    fn write_object(&self, bytes: &[u8]) -> Result<ObjectRef> {
        write_object_to(&self.objects_dir(), bytes)
    }

    fn read_object(&self, object: &ObjectRef) -> Result<Vec<u8>> {
        read_object_from(&self.objects_dir(), &object.sha256)
    }

    fn append(&self, session_id: &str, event: Event) -> Result<i64> {
        self.with_writer(session_id, |locked| {
            let seq = locked.journal.events.last().map_or(1, |line| line.seq + 1);
            let line = EventLine {
                seq,
                at: now(),
                event,
            };
            Arc::make_mut(&mut locked.journal.events).push(line.clone());
            let offset = locked.file.seek(SeekFrom::End(0))?;
            if let Err(error) = write_json_line(&mut locked.file, &line)
                .and_then(|()| locked.file.sync_data().map_err(Into::into))
            {
                let _ = locked.file.set_len(offset);
                Arc::make_mut(&mut locked.journal.events).pop();
                return Err(error);
            }
            Ok(seq)
        })
    }

    fn next_seq(&self, session_id: &str) -> Result<i64> {
        self.with_journal(session_id, |journal| {
            Ok(journal.events.last().map_or(1, |line| line.seq + 1))
        })
    }

    fn with_writer<T>(
        &self,
        session_id: &str,
        operation: impl FnOnce(&mut LockedSession) -> Result<T>,
    ) -> Result<T> {
        let mut locks = self.locks.lock().expect("session lock map poisoned");
        if let Some(locked) = locks.get_mut(session_id) {
            return operation(locked);
        }
        drop(locks);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.session_path(session_id))?;
        match flock_nonblocking(&file) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::EWOULDBLOCK) => {
                return Err(anyhow::Error::new(SessionBusy));
            }
            Err(error) => return Err(error.into()),
        }
        let journal = read_journal(&mut file, Some(session_id), true)?;
        let mut locked = LockedSession { file, journal };
        let result = operation(&mut locked);
        flock(&locked.file, libc::LOCK_UN)?;
        result
    }

    fn load_optional(&self, session_id: &str) -> Result<Option<Journal>> {
        if !valid_session_id(session_id) {
            return Ok(None);
        }
        if let Some(locked) = self
            .locks
            .lock()
            .expect("session lock map poisoned")
            .get(session_id)
        {
            return Ok(Some(locked.journal.clone()));
        }
        let path = self.session_path(session_id);
        if !path.exists() {
            return Ok(None);
        }
        self.load_path(&path).map(Some)
    }

    fn with_journal<T>(
        &self,
        session_id: &str,
        operation: impl FnOnce(&Journal) -> Result<T>,
    ) -> Result<T> {
        if !valid_session_id(session_id) {
            bail!("session not found: {session_id}")
        }
        let locks = self.locks.lock().expect("session lock map poisoned");
        if let Some(locked) = locks.get(session_id) {
            return operation(&locked.journal);
        }
        drop(locks);
        let journal = self
            .load_optional(session_id)?
            .with_context(|| format!("session not found: {session_id}"))?;
        operation(&journal)
    }

    fn load(&self, session_id: &str) -> Result<Journal> {
        self.load_optional(session_id)?
            .with_context(|| format!("session not found: {session_id}"))
    }

    fn load_path(&self, path: &Path) -> Result<Journal> {
        let mut file = File::open(path)?;
        let expected = path
            .file_stem()
            .and_then(|value| value.to_str())
            .map(str::to_string);
        read_journal(&mut file, expected.as_deref(), false)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if self.ephemeral {
            let _ = std::fs::remove_dir_all(&self.root);
            let _ = std::fs::remove_dir_all(&self.attachment_scope);
        }
    }
}

pub struct SessionLock<'a> {
    store: &'a Store,
    session_id: String,
}

impl std::fmt::Debug for SessionLock<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionLock")
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl Drop for SessionLock<'_> {
    fn drop(&mut self) {
        if let Some(locked) = self
            .store
            .locks
            .lock()
            .expect("session lock map poisoned")
            .remove(&self.session_id)
        {
            let _ = flock(&locked.file, libc::LOCK_UN);
        }
    }
}

pub fn stage_bash_attachment(
    manifest: &Path,
    objects_dir: &Path,
    call_id: i64,
    attachment: &Attachment,
    detail: ImageDetail,
) -> Result<()> {
    let parent = manifest
        .parent()
        .context("attachment manifest has no parent")?;
    ensure_private_dir(parent)?;
    ensure_private_dir(objects_dir)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).mode(0o600);
    let mut file = options.open(manifest)?;
    flock(&file, libc::LOCK_EX)?;
    let prefix = complete_prefix(&mut file)?;
    let entries = parse_manifest(&prefix)?;
    if entries
        .iter()
        .filter(|entry| entry.call_id == call_id)
        .count()
        >= MAX_BASH_ATTACHMENTS
    {
        flock(&file, libc::LOCK_UN)?;
        bail!("Bash emitted more than {MAX_BASH_ATTACHMENTS} attachments")
    }
    let object = write_object_to(objects_dir, &attachment.data)?;
    file.seek(SeekFrom::End(0))?;
    write_json_line(
        &mut file,
        &ManifestEntry {
            call_id,
            sha256: object.sha256,
            filename: attachment.filename.clone(),
            media_type: attachment.media_type.clone(),
            detail,
        },
    )?;
    file.sync_data()?;
    flock(&file, libc::LOCK_UN)?;
    Ok(())
}

pub fn read_bash_attachments(
    manifest: &Path,
    objects_dir: &Path,
    call_id: i64,
) -> Result<Vec<ToolAttachment>> {
    let mut file = match OpenOptions::new().read(true).write(true).open(manifest) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    flock(&file, libc::LOCK_EX)?;
    let entries = parse_manifest(&complete_prefix(&mut file)?)?
        .into_iter()
        .filter(|entry| entry.call_id == call_id)
        .collect::<Vec<_>>();
    if entries.len() > MAX_BASH_ATTACHMENTS {
        flock(&file, libc::LOCK_UN)?;
        bail!("Bash emitted more than {MAX_BASH_ATTACHMENTS} attachments")
    }
    flock(&file, libc::LOCK_UN)?;
    let mut cache: HashMap<String, Vec<u8>> = HashMap::new();
    entries
        .into_iter()
        .map(|entry| {
            let data = match cache.get(&entry.sha256) {
                Some(data) => data.clone(),
                None => {
                    let data = read_object_from(objects_dir, &entry.sha256)?;
                    cache.insert(entry.sha256.clone(), data.clone());
                    data
                }
            };
            Ok(ToolAttachment {
                attachment: Attachment {
                    filename: entry.filename,
                    media_type: entry.media_type,
                    data,
                },
                detail: entry.detail,
                object_sha256: Some(entry.sha256),
            })
        })
        .collect()
}

fn cleanup_bash_attachments(manifest: &Path, call_id: i64) -> Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(manifest) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    flock(&file, libc::LOCK_EX)?;
    let entries = parse_manifest(&complete_prefix(&mut file)?)?;
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    for entry in entries.into_iter().filter(|entry| entry.call_id != call_id) {
        write_json_line(&mut file, &entry)?;
    }
    file.sync_data()?;
    flock(&file, libc::LOCK_UN)?;
    Ok(())
}

fn complete_prefix(file: &mut File) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    if complete < bytes.len() {
        file.set_len(complete as u64)?;
    }
    Ok(bytes[..complete].to_vec())
}

fn parse_manifest(bytes: &[u8]) -> Result<Vec<ManifestEntry>> {
    std::str::from_utf8(bytes)?
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("decoding attachment manifest line {}", index + 1))
        })
        .collect()
}

fn write_object_to(objects_dir: &Path, bytes: &[u8]) -> Result<ObjectRef> {
    let sha256 = hex(Sha256::digest(bytes));
    let path = objects_dir.join(&sha256);
    let mut created = false;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true).mode(0o600);
    let mut file = match options.open(&path) {
        Ok(file) => {
            created = true;
            file
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            OpenOptions::new().read(true).write(true).open(&path)?
        }
        Err(error) => return Err(error.into()),
    };
    flock(&file, libc::LOCK_EX)?;
    let mut existing = Vec::new();
    file.read_to_end(&mut existing)?;
    if existing != bytes {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(bytes)?;
    }
    file.sync_all()?;
    if created {
        sync_dir(objects_dir)?;
    }
    flock(&file, libc::LOCK_UN)?;
    Ok(ObjectRef { sha256 })
}

fn read_object_from(objects_dir: &Path, sha256: &str) -> Result<Vec<u8>> {
    let path = objects_dir.join(sha256);
    let mut file = File::open(&path).with_context(|| format!("opening object {sha256}"))?;
    flock(&file, libc::LOCK_SH)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    flock(&file, libc::LOCK_UN)?;
    if hex(Sha256::digest(&bytes)) != sha256 {
        bail!("object checksum mismatch: {sha256}")
    }
    Ok(bytes)
}

fn read_journal(
    file: &mut File,
    expected_id: Option<&str>,
    truncate_incomplete_tail: bool,
) -> Result<Journal> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    if complete == 0 {
        bail!("session journal has no complete meta line")
    }
    if truncate_incomplete_tail && complete < bytes.len() {
        file.set_len(complete as u64)?;
    }
    let text = std::str::from_utf8(&bytes[..complete]).context("session journal is not UTF-8")?;
    let mut lines = text.lines();
    let meta: Meta = serde_json::from_str(lines.next().context("missing session meta")?)
        .context("decoding session meta")?;
    if meta.kind != "meta" || meta.format != "mu-session" || meta.version != FORMAT_VERSION {
        bail!("unsupported session journal format")
    }
    if !valid_session_id(&meta.session_id) {
        bail!("invalid session id in journal meta")
    }
    if expected_id.is_some_and(|expected| expected != meta.session_id) {
        bail!("session filename does not match meta id")
    }
    let mut events = Vec::new();
    for (index, line) in lines.enumerate() {
        let event: EventLine = serde_json::from_str(line)
            .with_context(|| format!("decoding session event at line {}", index + 2))?;
        let expected = events
            .last()
            .map_or(1, |previous: &EventLine| previous.seq + 1);
        if event.seq != expected {
            bail!(
                "noncontiguous session sequence at line {}: expected {}, found {}",
                index + 2,
                expected,
                event.seq
            )
        }
        events.push(event);
    }
    validate_events(&events)?;
    Ok(Journal {
        meta,
        events: events.into(),
    })
}

fn validate_events(events: &[EventLine]) -> Result<()> {
    if !matches!(
        events.first().map(|line| &line.event),
        Some(Event::SystemPrompt { .. })
    ) {
        bail!("session journal is missing its initial system prompt")
    }
    let mut turns = HashMap::new();
    let mut exchanges = HashMap::new();
    let mut terminal = HashSet::new();
    let mut calls = HashMap::new();
    let mut results = HashSet::new();
    let mut guardrail_attempts = HashSet::new();
    let mut latest_compaction_through = None;
    for line in events {
        match &line.event {
            Event::SystemPrompt { .. } if line.seq != 1 => {
                bail!("system prompt must be the first event")
            }
            Event::TurnStarted { turn_id, .. } => {
                if calls.keys().any(|call_id| !results.contains(call_id)) {
                    bail!("new turn starts before prior Bash claims are resolved")
                }
                if turns.insert(turn_id.clone(), line.seq).is_some() {
                    bail!("duplicate turn id: {turn_id}")
                }
            }
            Event::ProviderRequested {
                turn_id,
                exchange_id,
                purpose,
                subject,
                ..
            } => {
                if !turns.contains_key(turn_id) {
                    bail!("provider request references unknown turn: {turn_id}")
                }
                if !matches!(purpose.as_str(), "agent" | "compaction" | "guardrail") {
                    bail!("unknown provider request purpose: {purpose}")
                }
                if purpose == "guardrail" && subject.is_none()
                    || purpose != "guardrail" && subject.is_some()
                {
                    bail!("provider request subject does not match purpose: {purpose}")
                }
                if let Some(subject) = subject {
                    if !calls.contains_key(&subject.call_id) {
                        bail!(
                            "guardrail request references unknown Bash call: {}",
                            subject.call_id
                        )
                    }
                    if !guardrail_attempts.insert((subject.call_id, subject.attempt)) {
                        bail!(
                            "duplicate guardrail attempt {} for Bash call {}",
                            subject.attempt,
                            subject.call_id
                        )
                    }
                }
                if exchanges
                    .insert(
                        exchange_id.clone(),
                        (purpose.clone(), turn_id.clone(), subject.clone()),
                    )
                    .is_some()
                {
                    bail!("duplicate exchange id: {exchange_id}")
                }
            }
            Event::ProviderCompleted {
                exchange_id,
                projection,
                ..
            } => {
                let exchange = exchanges.get(exchange_id);
                if exchange.is_none() {
                    bail!("provider completion references unknown exchange: {exchange_id}")
                }
                if !terminal.insert(exchange_id) {
                    bail!("duplicate terminal provider event: {exchange_id}")
                }
                match projection {
                    Projection::Assistant {
                        turn_state,
                        bash_calls,
                        ..
                    } => {
                        if exchange.is_some_and(|(purpose, _, _)| purpose != "agent") {
                            bail!("assistant projection does not complete an agent request")
                        }
                        if !matches!(turn_state.as_str(), "continue" | "resume" | "complete")
                            || (turn_state == "continue") != !bash_calls.is_empty()
                        {
                            bail!("assistant turn state does not match its Bash claims")
                        }
                        let mut positions = HashSet::new();
                        let mut provider_ids = HashSet::new();
                        for call in bash_calls {
                            if !positions.insert(call.position)
                                || !provider_ids.insert(&call.provider_call_id)
                            {
                                bail!("duplicate Bash claim position or provider call id")
                            }
                            if call.position >= bash_calls.len() {
                                bail!("noncontiguous Bash claim position: {}", call.position)
                            }
                            if let Some(risk) = &call.declared_risk
                                && risk.parse::<BashRisk>().is_err()
                            {
                                bail!("invalid declared Bash risk: {risk}")
                            }
                            let turn_id = exchange
                                .map(|(_, turn_id, _)| turn_id.clone())
                                .or_else(|| latest_turn_before(events, line.seq))
                                .context("assistant projection has no turn")?;
                            if calls.insert(call.call_id, turn_id).is_some() {
                                bail!("duplicate Bash call id: {}", call.call_id)
                            }
                        }
                    }
                    Projection::Compaction {
                        through_seq,
                        retained_turn_ids,
                        ..
                    } => {
                        if exchange.is_some_and(|(purpose, _, _)| purpose != "compaction") {
                            bail!("compaction projection does not complete a compaction request")
                        }
                        if *through_seq < 1 || *through_seq >= line.seq {
                            bail!("compaction boundary is not before its completion")
                        }
                        if latest_compaction_through
                            .is_some_and(|previous| *through_seq <= previous)
                        {
                            bail!("compaction boundary did not advance")
                        }
                        latest_compaction_through = Some(*through_seq);
                        let retained = turns
                            .iter()
                            .filter(|(_, seq)| **seq > *through_seq)
                            .map(|(id, seq)| (*seq, id.clone()))
                            .collect::<Vec<_>>();
                        let mut retained = retained;
                        retained.sort_by_key(|(seq, _)| *seq);
                        let expected = retained.into_iter().map(|(_, id)| id).collect::<Vec<_>>();
                        if &expected != retained_turn_ids {
                            bail!("compaction retained turns do not match its boundary")
                        }
                        if let Some(first) = retained_turn_ids.first()
                            && turns[first] != through_seq.saturating_add(1)
                        {
                            bail!("compaction boundary is not immediately before a turn")
                        }
                    }
                    Projection::Guardrail {
                        call_id,
                        attempt,
                        outcome,
                        ..
                    } => {
                        let Some((purpose, _, subject)) = exchange else {
                            bail!("guardrail projection has no request")
                        };
                        if purpose != "guardrail"
                            || subject.as_ref().is_none_or(|subject| {
                                subject.call_id != *call_id || subject.attempt != *attempt
                            })
                            || !calls.contains_key(call_id)
                        {
                            bail!("guardrail projection does not match its Bash claim")
                        }
                        if !matches!(outcome.as_str(), "allow" | "deny") {
                            bail!("invalid guardrail outcome: {outcome}")
                        }
                    }
                }
            }
            Event::ProviderFailed { exchange_id, .. }
            | Event::ProviderInterrupted { exchange_id } => {
                if !exchanges.contains_key(exchange_id) {
                    bail!("provider terminal event references unknown exchange: {exchange_id}")
                }
                if !terminal.insert(exchange_id) {
                    bail!("duplicate terminal provider event: {exchange_id}")
                }
            }
            Event::BashCompleted {
                turn_id,
                call_id,
                outcome,
                exit_code,
                ..
            } => {
                if calls.get(call_id) != Some(turn_id) {
                    bail!("Bash result references unknown call: {call_id}")
                }
                if !results.insert(call_id) {
                    bail!("duplicate Bash result: {call_id}")
                }
                if !matches!(outcome.as_str(), "completed" | "error" | "interrupted")
                    || (outcome == "completed") != exit_code.is_some()
                {
                    bail!("Bash result outcome does not match its exit code")
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn latest_compaction(journal: &Journal) -> Option<&EventLine> {
    latest_compaction_before(journal, i64::MAX)
}

fn latest_compaction_before(journal: &Journal, max_seq: i64) -> Option<&EventLine> {
    journal.events.iter().rev().find(|line| {
        line.seq <= max_seq
            && matches!(
                line.event,
                Event::ProviderCompleted {
                    projection: Projection::Compaction { .. },
                    ..
                }
            )
    })
}

fn latest_turn_before(events: &[EventLine], seq: i64) -> Option<String> {
    events.iter().rev().find_map(|line| match &line.event {
        Event::TurnStarted { turn_id, .. } if line.seq < seq => Some(turn_id.clone()),
        _ => None,
    })
}

fn latest_turn_id(journal: &Journal) -> Option<String> {
    journal
        .events
        .iter()
        .rev()
        .find_map(|line| match &line.event {
            Event::TurnStarted { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        })
}

fn provider_exchange_turns(journal: &Journal) -> HashMap<&str, String> {
    journal
        .events
        .iter()
        .filter_map(|line| match &line.event {
            Event::ProviderRequested {
                exchange_id,
                turn_id,
                ..
            } => Some((exchange_id.as_str(), turn_id.clone())),
            _ => None,
        })
        .collect()
}

fn resume_was_requested(journal: &Journal, turn_id: &str, after_seq: i64) -> bool {
    for line in journal.events.iter().filter(|line| line.seq > after_seq) {
        match &line.event {
            Event::TurnStarted { .. } => return false,
            Event::ProviderRequested {
                turn_id: request_turn,
                purpose,
                ..
            } if request_turn == turn_id && purpose == "agent" => return true,
            _ => {}
        }
    }
    false
}

fn next_call_id(journal: &Journal) -> i64 {
    journal
        .events
        .iter()
        .filter_map(|line| match &line.event {
            Event::ProviderCompleted {
                projection: Projection::Assistant { bash_calls, .. },
                ..
            } => bash_calls.iter().map(|call| call.call_id).max(),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        + 1
}

fn find_call(journal: &Journal, call_id: i64) -> Option<&PersistedBashCall> {
    journal.events.iter().find_map(|line| match &line.event {
        Event::ProviderCompleted {
            projection: Projection::Assistant { bash_calls, .. },
            ..
        } => bash_calls.iter().find(|call| call.call_id == call_id),
        _ => None,
    })
}

fn call_turn_id(journal: &Journal, call_id: i64) -> Option<String> {
    let assistant_seq = journal.events.iter().find_map(|line| match &line.event {
        Event::ProviderCompleted {
            projection: Projection::Assistant { bash_calls, .. },
            ..
        } if bash_calls.iter().any(|call| call.call_id == call_id) => Some(line.seq),
        _ => None,
    })?;
    journal
        .events
        .iter()
        .rev()
        .find(|line| line.seq < assistant_seq && matches!(line.event, Event::TurnStarted { .. }))
        .and_then(|line| match &line.event {
            Event::TurnStarted { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        })
}

fn is_semantic(event: &Event) -> bool {
    matches!(
        event,
        Event::SystemPrompt { .. }
            | Event::TurnStarted { .. }
            | Event::BashCompleted { .. }
            | Event::ProviderCompleted {
                projection: Projection::Assistant { .. } | Projection::Compaction { .. },
                ..
            }
    )
}

fn reported_context_tokens(journal: &Journal) -> Option<u64> {
    let mut request_formats = HashMap::new();
    let mut latest = None;
    let mut semantic_after = false;
    for line in journal.events.iter() {
        match &line.event {
            Event::ProviderRequested {
                exchange_id,
                purpose,
                origin,
                request_recipe,
                ..
            } if purpose == "agent" => {
                let current = request_format_is_current(&origin.api, &request_recipe.format);
                request_formats.insert(exchange_id.as_str(), current);
            }
            Event::ProviderCompleted {
                exchange_id,
                usage: Some(usage),
                projection: Projection::Assistant { .. },
                ..
            } => {
                latest = request_formats
                    .get(exchange_id.as_str())
                    .map(|current| (usage.total_tokens, *current));
                semantic_after = false;
            }
            Event::ProviderCompleted {
                projection: Projection::Assistant { .. },
                ..
            } => semantic_after = true,
            Event::TurnStarted { .. }
            | Event::BashCompleted { .. }
            | Event::ProviderCompleted {
                projection: Projection::Compaction { .. },
                ..
            } => semantic_after = true,
            _ => {}
        }
    }
    latest
        .filter(|(_, current)| !semantic_after && *current)
        .map(|(tokens, _)| tokens)
}

fn request_format_is_current(api: &str, format: &str) -> bool {
    #[cfg(test)]
    if api == "test" {
        return format == "test.v1";
    }
    ModelApi::from_name(api).is_some_and(|api| format == api.request_format())
}

fn user_text(content: &PersistedUserContent) -> String {
    match content {
        PersistedUserContent::Text { text } => text.clone(),
        PersistedUserContent::Parts { parts } => parts
            .iter()
            .filter_map(|part| match part {
                PersistedContentPart::Text { text } => Some(text.as_str()),
                PersistedContentPart::Attachment { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn location_context(
    previous: Option<&(String, Option<String>)>,
    cwd: &str,
    git_worktree_root: Option<&str>,
) -> Option<String> {
    if previous
        .is_some_and(|(old_cwd, old_git)| old_cwd == cwd && old_git.as_deref() == git_worktree_root)
    {
        return None;
    }
    let mut lines = if previous.is_none() {
        vec!["[environment]".to_string()]
    } else {
        vec!["<system-reminder>".to_string()]
    };
    if let Some(root) = git_worktree_root {
        lines.push(format!("git worktree root: {root}"));
    } else if previous.is_some_and(|(_, old_git)| old_git.is_some()) {
        lines.push("git worktree root: (none)".to_string());
    }
    lines.push(format!("current working directory: {cwd}"));
    if previous.is_some() {
        lines.push("</system-reminder>".to_string());
    }
    Some(lines.join("\n"))
}

fn activity_at(journal: &Journal) -> &str {
    journal
        .events
        .last()
        .map_or(&journal.meta.created_at, |line| &line.at)
}

fn valid_session_id(id: &str) -> bool {
    id.len() == 12
        && id.starts_with("ses_")
        && id[4..]
            .bytes()
            .all(|byte| b"0123456789abcdefghjkmnpqrstvwxyz".contains(&byte))
}

fn incomplete_session_initialization(path: &Path) -> Result<bool> {
    // A fresh journal becomes listable only after both meta and system-prompt
    // lines are durable. Listing also hides a genuinely truncated one-line
    // journal; opening that session directly still reports the corruption.
    let bytes = std::fs::read(path)?;
    Ok(bytes.iter().filter(|byte| **byte == b'\n').count() < 2)
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn write_json_line(file: &mut File, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *file, value)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn flock(file: &File, operation: libc::c_int) -> std::io::Result<()> {
    let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn flock_nonblocking(file: &File) -> std::io::Result<()> {
    flock(file, libc::LOCK_EX | libc::LOCK_NB)
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn canonical_json(value: &Value) -> Vec<u8> {
    fn write(value: &Value, output: &mut Vec<u8>) {
        match value {
            Value::Null => output.extend_from_slice(b"null"),
            Value::Bool(value) => output.extend_from_slice(value.to_string().as_bytes()),
            Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
            Value::String(value) => output.extend_from_slice(
                serde_json::to_string(value)
                    .expect("serializing a JSON string cannot fail")
                    .as_bytes(),
            ),
            Value::Array(values) => {
                output.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    write(value, output);
                }
                output.push(b']');
            }
            Value::Object(values) => {
                output.push(b'{');
                let mut entries = values.iter().collect::<Vec<_>>();
                entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
                for (index, (key, value)) in entries.into_iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    write(&Value::String(key.clone()), output);
                    output.push(b':');
                    write(value, output);
                }
                output.push(b'}');
            }
        }
    }

    let mut output = Vec::new();
    write(value, &mut output);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;

    #[test]
    fn assistant_projection_accepts_absent_reasoning_blocks() {
        let projection: Projection = serde_json::from_value(serde_json::json!({
            "kind": "assistant",
            "turn_state": "complete",
            "text": "older entry",
            "bash_calls": [],
        }))
        .unwrap();

        assert!(matches!(
            projection,
            Projection::Assistant {
                reasoning_blocks,
                ..
            } if reasoning_blocks.is_empty()
        ));
    }

    #[test]
    fn short_id_collision_retries_and_journal_replays() {
        let store = Store::open_memory().unwrap();
        let collision = "ses_00000000";
        File::create(store.session_path(collision)).unwrap();
        let mut ids = [collision, "ses_00000001"].into_iter();
        let session = store
            .create_session_with("system", || Ok(ids.next().unwrap().to_string()))
            .unwrap();
        assert_eq!(session.id, "ses_00000001");
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"hello".into())
            .unwrap();
        assert!(turn.starts_with('t'));
        assert_eq!(store.load_context_messages(&session.id).unwrap().len(), 3);
    }

    #[test]
    fn interrupted_claim_gets_one_synthetic_result() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        store
            .start_turn(&session.id, "/tmp", None, &"run".into())
            .unwrap();
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(
                        ["first", "second"]
                            .into_iter()
                            .map(|id| ToolCall {
                                id: id.into(),
                                arguments: r#"{"risk":"readonly","command":"true"}"#.into(),
                            })
                            .collect(),
                    ),
                    native_replay: None,
                },
            )
            .unwrap();
        let (manifest, objects) = store.attachment_paths(&session.id).unwrap();
        stage_bash_attachment(
            &manifest,
            &objects,
            call_ids[0],
            &Attachment {
                filename: "uncommitted.png".into(),
                media_type: "image/png".into(),
                data: b"uncommitted".to_vec(),
            },
            ImageDetail::Auto,
        )
        .unwrap();

        assert_eq!(store.normalize_interrupted_tail(&session.id).unwrap(), 2);
        assert_eq!(store.normalize_interrupted_tail(&session.id).unwrap(), 0);
        let context = store.load_context_messages(&session.id).unwrap();
        let recovered = context
            .iter()
            .filter_map(|message| match message {
                Message::Tool {
                    attachments,
                    tool_call_id,
                    ..
                } => Some((tool_call_id.as_str(), attachments)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            recovered
                .iter()
                .map(|(tool_call_id, _)| *tool_call_id)
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert!(
            recovered
                .iter()
                .all(|(_, attachments)| attachments.is_empty())
        );
    }

    #[test]
    fn committed_bash_result_cleans_its_manifest_entries() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        store
            .start_turn(&session.id, "/tmp", None, &"run".into())
            .unwrap();
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "provider-call".into(),
                        arguments: r#"{"risk":"readonly","command":"true"}"#.into(),
                    }]),
                    native_replay: None,
                },
            )
            .unwrap();
        let (manifest, objects) = store.attachment_paths(&session.id).unwrap();
        stage_bash_attachment(
            &manifest,
            &objects,
            call_ids[0],
            &Attachment {
                filename: "image.png".into(),
                media_type: "image/png".into(),
                data: b"image".to_vec(),
            },
            ImageDetail::Auto,
        )
        .unwrap();
        let attachments = read_bash_attachments(&manifest, &objects, call_ids[0]).unwrap();
        store
            .persist_bash_result(
                &session.id,
                BashResultRecord {
                    bash_call_id: call_ids[0],
                    outcome: "completed",
                    exit_code: Some(0),
                    duration_ms: Some(1),
                },
                "done",
                &attachments,
            )
            .unwrap();

        let mut file = File::open(manifest).unwrap();
        assert!(
            parse_manifest(&complete_prefix(&mut file).unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn object_file_is_verified() {
        let store = Store::open_memory().unwrap();
        let object = store.write_object(b"content").unwrap();
        assert_eq!(store.read_object(&object).unwrap(), b"content");
    }

    #[test]
    fn agent_request_reconstructs_from_semantic_history() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        let turn = store
            .start_turn(&session.id, "/tmp", Some("/repo"), &"hello".into())
            .unwrap();
        let messages = store.load_context_messages(&session.id).unwrap();
        let endpoint = "https://example.test/v1/chat/completions";
        let request = Request {
            model: ResolvedModelRef {
                canonical: "test/model:high".into(),
                provider_id: "test".into(),
                model_id: "model".into(),
                effort: Some("high".into()),
            },
            cache_key: Some(format!("mu:{}:agent", session.id)),
            messages,
            bash: true,
        };
        let native = request.json(ModelApi::ChatCompletions).unwrap();
        let recipe = store
            .request_recipe(
                "openai.chat_completions.v1",
                &native,
                serde_json::json!({
                    "kind": "agent",
                    "context_through_seq": store.current_context_seq(&session.id).unwrap()
                }),
            )
            .unwrap();
        assert_eq!(
            recipe.envelope["prompt_cache_key"],
            format!("mu:{}:agent", session.id)
        );
        let exchange = store
            .start_provider_request(
                &session.id,
                &turn,
                "agent",
                ProviderOrigin {
                    canonical_model_ref: request.model.canonical.clone(),
                    provider_id: request.model.provider_id.clone(),
                    api: "chat_completions".into(),
                    endpoint: endpoint.into(),
                    wire_model: request.model.model_id.clone(),
                    effort: request.model.effort.clone(),
                },
                recipe,
                None,
            )
            .unwrap();

        assert_eq!(
            store
                .reconstruct_provider_request(&session.id, &exchange)
                .unwrap(),
            native
        );
    }

    #[test]
    fn resumed_anthropic_request_reconstructs_derived_continuation() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"work".into())
            .unwrap();
        let first = store
            .start_test_provider_request(&session.id, &turn, "agent")
            .unwrap();
        let endpoint = "https://example.test/v1/messages";
        store
            .complete_resumable_assistant_exchange(
                &session.id,
                &first,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    native_replay: Some(NativeReplay {
                        provider_id: "test".into(),
                        endpoint: endpoint.into(),
                        model: "model".into(),
                        payload: crate::provider::NativeReplayPayload::AnthropicContent(vec![
                            serde_json::json!({
                                "type": "thinking",
                                "thinking": "working",
                                "signature": "sig",
                            }),
                        ]),
                    }),
                },
                &[],
                None,
                None,
            )
            .unwrap();

        let mut messages = store.load_context_messages(&session.id).unwrap();
        messages.push(Message::User {
            content: RESUME_PROMPT.into(),
        });
        let request = Request {
            model: ResolvedModelRef {
                canonical: "test/model:high".into(),
                provider_id: "test".into(),
                model_id: "model".into(),
                effort: Some("high".into()),
            },
            cache_key: None,
            messages: messages.clone(),
            bash: true,
        };
        let native = request.json(ModelApi::AnthropicMessages).unwrap();
        let recipe = store
            .request_recipe(
                "anthropic.messages.v1",
                &native,
                serde_json::json!({
                    "kind": "agent",
                    "context_through_seq": store.current_context_seq(&session.id).unwrap(),
                    "native_replay_origins":
                        crate::provider::native_replay_origins(&messages),
                }),
            )
            .unwrap();
        let resumed = store
            .start_provider_request(
                &session.id,
                &turn,
                "agent",
                ProviderOrigin {
                    canonical_model_ref: request.model.canonical.clone(),
                    provider_id: request.model.provider_id.clone(),
                    api: "anthropic_messages".into(),
                    endpoint: endpoint.into(),
                    wire_model: request.model.model_id.clone(),
                    effort: request.model.effort.clone(),
                },
                recipe,
                None,
            )
            .unwrap();

        assert_eq!(
            store
                .reconstruct_provider_request(&session.id, &resumed)
                .unwrap(),
            native
        );
    }

    #[test]
    fn legacy_replay_origin_and_recorded_selection_reconstruct_exactly() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        store
            .start_turn(&session.id, "/tmp", None, &"run".into())
            .unwrap();
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: Some("trace".into()),
                    tool_calls: Some(vec![ToolCall {
                        id: "provider-call".into(),
                        arguments: r#"{"risk":"readonly","command":"pwd"}"#.into(),
                    }]),
                    native_replay: Some(NativeReplay {
                        provider_id: String::new(),
                        endpoint: String::new(),
                        model: "model".into(),
                        payload: crate::provider::NativeReplayPayload::ChatReasoning(
                            "trace".into(),
                        ),
                    }),
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
                "/tmp",
                &[],
            )
            .unwrap();

        let messages = store.load_context_messages(&session.id).unwrap();
        let replay = messages.iter().find_map(|message| match message {
            Message::Assistant {
                native_replay: Some(native),
                ..
            } => Some(native),
            _ => None,
        });
        assert_eq!(replay.unwrap().provider_id, "test");

        let replay_origins = crate::provider::native_replay_origins(&messages);
        let request_messages = crate::provider::filter_native_replay_for_origins(
            &messages,
            ModelApi::ChatCompletions,
            &replay_origins,
        );
        let request = Request {
            model: ResolvedModelRef {
                canonical: "target/other-model".into(),
                provider_id: "target".into(),
                model_id: "other-model".into(),
                effort: None,
            },
            cache_key: None,
            messages: request_messages,
            bash: true,
        };
        let endpoint = "https://target.test/v1/chat/completions";
        let native = request.json(ModelApi::ChatCompletions).unwrap();
        let recipe = store
            .request_recipe(
                "openai.chat_completions.v1",
                &native,
                serde_json::json!({
                    "kind": "agent",
                    "context_through_seq": store.current_context_seq(&session.id).unwrap(),
                    "native_replay_origins": replay_origins,
                }),
            )
            .unwrap();
        let exchange = store
            .start_provider_request(
                &session.id,
                &store.current_turn_id(&session.id).unwrap(),
                "agent",
                ProviderOrigin {
                    canonical_model_ref: request.model.canonical.clone(),
                    provider_id: request.model.provider_id.clone(),
                    api: "chat_completions".into(),
                    endpoint: endpoint.into(),
                    wire_model: request.model.model_id.clone(),
                    effort: None,
                },
                recipe,
                None,
            )
            .unwrap();

        assert_eq!(
            store
                .reconstruct_provider_request(&session.id, &exchange)
                .unwrap(),
            native
        );
    }

    #[test]
    fn reported_context_is_invalidated_when_the_request_format_is_not_current() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"hello".into())
            .unwrap();
        let native = serde_json::json!({"model":"model"});
        let exchange = store
            .start_provider_request(
                &session.id,
                &turn,
                "agent",
                ProviderOrigin {
                    canonical_model_ref: "test/model".into(),
                    provider_id: "test".into(),
                    api: "test".into(),
                    endpoint: String::new(),
                    wire_model: "model".into(),
                    effort: None,
                },
                store
                    .request_recipe("test.v0", &native, serde_json::json!({"kind":"agent"}))
                    .unwrap(),
                None,
            )
            .unwrap();
        store
            .complete_assistant_exchange(
                &session.id,
                &exchange,
                &Message::Assistant {
                    content: Some("done".into()),
                    reasoning_content: None,
                    tool_calls: None,
                    native_replay: None,
                },
                &[],
                None,
                Some(&Usage {
                    total_tokens: 10,
                    ..Usage::default()
                }),
            )
            .unwrap();

        assert_eq!(
            store
                .get_session(&session.id)
                .unwrap()
                .unwrap()
                .reported_context_tokens,
            None
        );
    }

    #[test]
    fn duplicate_provider_tool_call_ids_are_rejected_before_persistence() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"hello".into())
            .unwrap();
        let native = serde_json::json!({"model":"model"});
        let exchange = store
            .start_provider_request(
                &session.id,
                &turn,
                "agent",
                ProviderOrigin {
                    canonical_model_ref: "test/model".into(),
                    provider_id: "test".into(),
                    api: "test".into(),
                    endpoint: String::new(),
                    wire_model: "model".into(),
                    effort: None,
                },
                store
                    .request_recipe("test.v1", &native, serde_json::json!({"kind":"agent"}))
                    .unwrap(),
                None,
            )
            .unwrap();
        let call = ToolCall {
            id: "call_1".into(),
            arguments: r#"{"risk":"readonly","command":"pwd"}"#.into(),
        };

        let error = store
            .complete_assistant_exchange(
                &session.id,
                &exchange,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![call.clone(), call]),
                    native_replay: None,
                },
                &[],
                None,
                None,
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("duplicate provider tool call id")
        );
        assert!(store.load_context_messages(&session.id).is_ok());
        assert!(!store.is_session_clean(&session.id).unwrap());
        let events = store.audit_events(&session.id).unwrap();
        assert!(events.iter().any(|event| {
            event["type"] == "provider_failed" && event["error_class"] == "invalid_response"
        }));
        assert!(!events.iter().any(|event| {
            event["type"] == "provider_completed" && event["exchange_id"] == exchange
        }));
    }

    #[test]
    fn incomplete_tail_is_ignored_then_truncated_by_the_next_writer() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        let path = store.session_path(&session.id);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(br#"{"seq":2,"#)
            .unwrap();

        assert_eq!(store.load_context_messages(&session.id).unwrap().len(), 1);
        store
            .start_turn(&session.id, "/tmp", None, &"hello".into())
            .unwrap();

        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert_eq!(
            String::from_utf8(bytes)
                .unwrap()
                .matches("\"seq\":2")
                .count(),
            1
        );
    }

    #[test]
    fn reopening_validates_events_written_by_the_typed_append_path() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        store
            .append(
                &session.id,
                Event::ProviderInterrupted {
                    exchange_id: "unknown".into(),
                },
            )
            .unwrap();

        let reopened = Store::open(&store.root).unwrap();
        let error = reopened.get_session(&session.id).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("provider terminal event references unknown exchange")
        );
    }

    #[test]
    fn session_lock_fails_fast_and_current_pointer_is_last_selected() {
        let store = Store::open_memory().unwrap();
        let first = store.create_session_seeded("system").unwrap();
        let second = store.create_session_seeded("system").unwrap();
        assert!(store.current_session().unwrap().is_none());
        store.select_session(&first.id).unwrap();
        assert_eq!(store.current_session().unwrap().unwrap().id, first.id);
        store.select_session(&second.id).unwrap();
        assert_eq!(store.current_session().unwrap().unwrap().id, second.id);

        let other = Store::open(store.root.as_path()).unwrap();
        let _lock = store.acquire_session_lock(&second.id).unwrap();
        assert!(other.acquire_session_lock(&second.id).is_err());
    }

    #[test]
    fn invalid_session_ids_never_become_paths() {
        let store = Store::open_memory().unwrap();
        assert!(store.get_session("../../outside").unwrap().is_none());
        assert!(store.select_session("../../outside").is_err());
        assert!(store.acquire_session_lock("../../outside").is_err());
    }

    #[test]
    fn location_reminder_reports_leaving_a_worktree() {
        assert_eq!(
            location_context(Some(&("/repo".into(), Some("/repo".into()))), "/tmp", None).unwrap(),
            "<system-reminder>\ngit worktree root: (none)\ncurrent working directory: /tmp\n</system-reminder>"
        );
    }

    #[test]
    fn manifest_preserves_duplicates_and_enforces_the_limit() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        let (manifest, objects) = store.attachment_paths(&session.id).unwrap();
        let attachment = Attachment {
            filename: "same.png".into(),
            media_type: "image/png".into(),
            data: b"image".to_vec(),
        };
        for _ in 0..MAX_BASH_ATTACHMENTS {
            stage_bash_attachment(&manifest, &objects, 7, &attachment, ImageDetail::Original)
                .unwrap();
        }
        assert!(
            stage_bash_attachment(&manifest, &objects, 7, &attachment, ImageDetail::Original)
                .is_err()
        );
        assert_eq!(
            read_bash_attachments(&manifest, &objects, 7).unwrap().len(),
            MAX_BASH_ATTACHMENTS
        );
    }

    #[test]
    fn recovery_closes_unmatched_requests_and_durable_denials() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"remove it".into())
            .unwrap();
        let (_, calls) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![ToolCall {
                        id: "provider-call".into(),
                        arguments: r#"{"risk":"destructive","command":"rm x"}"#.into(),
                    }]),
                    native_replay: None,
                },
            )
            .unwrap();
        let native = serde_json::json!({"model":"reviewer","messages":[]});
        let recipe = store
            .request_recipe("test.v1", &native, serde_json::json!({"kind":"guardrail"}))
            .unwrap();
        let exchange = store
            .start_provider_request(
                &session.id,
                &turn,
                "guardrail",
                ProviderOrigin {
                    canonical_model_ref: "test/reviewer".into(),
                    provider_id: "test".into(),
                    api: "test".into(),
                    endpoint: String::new(),
                    wire_model: "reviewer".into(),
                    effort: None,
                },
                recipe,
                Some(RequestSubject {
                    call_id: calls[0],
                    attempt: 1,
                }),
            )
            .unwrap();
        store
            .complete_guardrail_exchange(
                &session.id,
                &exchange,
                GuardrailCompletion {
                    call_id: calls[0],
                    attempt: 1,
                    outcome: "deny",
                    risk_level: Some("critical"),
                    auth_level: Some("none"),
                    reason: Some("not authorized"),
                    native_response: None,
                    usage: None,
                },
            )
            .unwrap();
        let unmatched = store
            .start_provider_request(
                &session.id,
                &turn,
                "agent",
                ProviderOrigin {
                    canonical_model_ref: "test/model".into(),
                    provider_id: "test".into(),
                    api: "test".into(),
                    endpoint: String::new(),
                    wire_model: "model".into(),
                    effort: None,
                },
                store
                    .request_recipe(
                        "test.v1",
                        &serde_json::json!({"model":"model"}),
                        serde_json::json!({"kind":"agent"}),
                    )
                    .unwrap(),
                None,
            )
            .unwrap();

        assert_eq!(store.normalize_interrupted_tail(&session.id).unwrap(), 1);
        let audit = store.audit_events(&session.id).unwrap();
        assert!(audit.iter().any(|event| {
            event["type"] == "provider_interrupted" && event["exchange_id"] == unmatched
        }));
        let result = audit
            .iter()
            .find(|event| event["type"] == "bash_completed")
            .unwrap();
        assert_eq!(result["outcome"], "error");
        assert!(
            result["output"]["text"]
                .as_str()
                .unwrap()
                .contains("not authorized")
        );
    }

    #[test]
    fn resumable_completion_is_dirty_and_projects_a_used_resume_prompt() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"work".into())
            .unwrap();
        let exchange = store
            .start_test_provider_request(&session.id, &turn, "agent")
            .unwrap();
        store
            .complete_resumable_assistant_exchange(
                &session.id,
                &exchange,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    native_replay: None,
                },
                &[],
                None,
                None,
            )
            .unwrap();

        assert!(!store.is_session_clean(&session.id).unwrap());
        assert!(store.resume_reminder_needed(&session.id).unwrap());
        assert!(!store.load_context_messages(&session.id).unwrap().iter().any(
            |message| matches!(message, Message::User { content } if content.text() == RESUME_PROMPT)
        ));

        let resumed = store
            .start_test_provider_request(&session.id, &turn, "agent")
            .unwrap();
        store
            .interrupt_provider_exchange(&session.id, &resumed)
            .unwrap();

        assert!(!store.resume_reminder_needed(&session.id).unwrap());
        assert!(store.load_context_messages(&session.id).unwrap().iter().any(
            |message| matches!(message, Message::User { content } if content.text() == RESUME_PROMPT)
        ));
    }

    #[test]
    fn new_prompt_supersedes_unused_resume_without_synthetic_message() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"work".into())
            .unwrap();
        let exchange = store
            .start_test_provider_request(&session.id, &turn, "agent")
            .unwrap();
        store
            .complete_resumable_assistant_exchange(
                &session.id,
                &exchange,
                &Message::Assistant {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    native_replay: None,
                },
                &[],
                None,
                None,
            )
            .unwrap();
        store
            .start_turn(&session.id, "/tmp", None, &"new direction".into())
            .unwrap();

        let context = store.load_context_messages(&session.id).unwrap();
        assert!(!store.resume_reminder_needed(&session.id).unwrap());
        assert!(!context.iter().any(
            |message| matches!(message, Message::User { content } if content.text() == RESUME_PROMPT)
        ));
        assert!(matches!(
            context.last(),
            Some(Message::User { content }) if content.text() == "new direction"
        ));
    }
}
