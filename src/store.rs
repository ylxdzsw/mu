use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::models::ResolvedModelRef;
use crate::provider::{
    AssistantItem, Attachment, ContentPart, ImageDetail, Message, ModelApi, NativeReplay,
    NativeReplayPayload, ReplayOrigin, Request, ToolAttachment, ToolCall, Usage, UserContent,
    estimate_messages_tokens, filter_native_replay_for_config, native_replay_origins,
};

pub const BASH_CALL_ID_ENV: &str = "MU_BASH_CALL_ID";
pub const ATTACHMENT_MANIFEST_ENV: &str = "MU_ATTACHMENT_MANIFEST";
pub const OBJECTS_DIR_ENV: &str = "MU_OBJECTS_DIR";
pub const INTERRUPTED_TOOL_RESULT: &str = "error: interrupted — this command may have started and not completed; its effects are unknown. Verify the resulting state before relying on it.";
pub const RESUME_PROMPT: &str = "Continue the current task from where you stopped.";

const FORMAT_VERSION: u32 = 3;
const SESSION_ID_RETRIES: usize = 16;
const EXTERNAL_TEXT_BYTES: usize = 256 * 1024;
const MAX_BASH_ATTACHMENTS: usize = 8;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub cwd: String,
    pub last_model: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextTokenEstimate {
    pub tokens: u64,
    pub reported: bool,
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

pub struct SessionListing {
    pub sessions: Vec<(Session, String)>,
    pub skipped: Vec<UnsupportedSessionVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptEvent {
    User {
        text: String,
        cwd: String,
        model: Option<String>,
        context: Option<TranscriptContext>,
        internal: bool,
    },
    Assistant {
        turn_state: String,
        items: Vec<TranscriptAssistantItem>,
        internal: bool,
    },
    CompactionTriggered {
        trigger: CompactionTrigger,
        context_tokens: u64,
        context_window: Option<u64>,
        reason: Option<String>,
    },
    CompactionApplied {
        from_epoch: u64,
        to_epoch: u64,
        before_context_tokens: u64,
        before_context_window: Option<u64>,
        after_context_tokens_estimate: u64,
        after_context_window: Option<u64>,
        elapsed_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptContext {
    pub tokens: u64,
    pub estimated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptAssistantItem {
    Reasoning(Option<String>),
    Text(String),
    BashCall {
        arguments: String,
        result: Option<TranscriptBashResult>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptBashResult {
    pub outcome: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
}

pub struct BashResultRecord<'a> {
    pub bash_call_id: i64,
    pub outcome: &'a str,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Soft,
    Hard,
    Emergency,
    Manual,
}

impl CompactionTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Hard => "hard",
            Self::Emergency => "emergency",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionMode {
    AwaitUser,
    ContinueTurn,
}

impl CompactionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwaitUser => "await_user",
            Self::ContinueTurn => "continue_turn",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingCompaction {
    pub turn_id: String,
    pub trigger: CompactionTrigger,
    pub mode: CompactionMode,
    pub from_epoch: u64,
    pub started_at: String,
    /// Estimated input size of the provider request whose threshold or
    /// context-length failure triggered this compaction. For soft compaction,
    /// this includes the queued prompt.
    pub before_context_tokens: u64,
    pub before_context_window: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct QueuedPrompt {
    pub prompt_id: String,
    pub epoch: u64,
}

pub(crate) struct AssistantCompletion<'a> {
    pub message: &'a Message,
    pub native_response: Option<&'a Value>,
    pub usage: Option<&'a Usage>,
    pub resumable: bool,
    pub response_complete: bool,
    pub context_output_complete: bool,
}

pub struct CompactionApplication {
    /// Estimated immediate next provider input after applying the checkpoint.
    /// Automatic await-user compaction includes its queued prompt here.
    pub after_context_tokens_estimate: u64,
    pub after_context_window: Option<u64>,
}

pub struct CompactionStart<'a> {
    pub cwd: &'a str,
    pub prompt: &'a UserContent,
    pub trigger: CompactionTrigger,
    pub mode: CompactionMode,
    pub before_context_tokens: u64,
    pub before_context_window: Option<u64>,
}

pub struct GuardrailCompletion<'a> {
    pub outcome: &'a str,
    pub risk_level: Option<&'a str>,
    pub auth_level: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub native_response: Option<&'a Value>,
    pub usage: Option<&'a Usage>,
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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RequestSubject {
    Agent,
    Guardrail { call_id: i64, attempt: u32 },
}

impl RequestSubject {
    fn is_agent(&self) -> bool {
        matches!(self, Self::Agent)
    }

    fn guardrail(&self) -> Option<(i64, u32)> {
        match self {
            Self::Guardrail { call_id, attempt } => Some((*call_id, *attempt)),
            Self::Agent => None,
        }
    }
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
}

#[derive(Deserialize)]
struct MetaVersion {
    #[serde(rename = "type")]
    kind: String,
    format: String,
    version: u32,
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
    PromptMaterialized {
        prompt_id: String,
        turn_id: String,
    },
    CompactionStarted {
        turn_id: String,
        cwd: String,
        prompt: PersistedUserContent,
        trigger: CompactionTrigger,
        mode: CompactionMode,
        before_context_tokens: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        before_context_window: Option<u64>,
    },
    PromptQueued {
        prompt_id: String,
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        git_worktree_root: Option<String>,
        prompt: PersistedUserContent,
    },
    ProviderRequested {
        turn_id: String,
        exchange_id: String,
        subject: RequestSubject,
        origin: ProviderOrigin,
        request_recipe: RequestRecipe,
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
        call_id: i64,
        outcome: String,
        output: PersistedText,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<PersistedToolAttachment>,
    },
    CompactionApplied {
        after_context_tokens_estimate: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_context_window: Option<u64>,
    },
}

fn agent_compaction_attempt(event: &Event) -> Option<(&str, &str, Option<&str>)> {
    match event {
        Event::ProviderRequested {
            turn_id,
            exchange_id,
            request_recipe,
            subject,
            ..
        } if subject.is_agent() => Some((
            turn_id,
            exchange_id,
            request_recipe.input["compaction_attempt"].as_str(),
        )),
        _ => None,
    }
}

fn true_value() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Projection {
    Assistant {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        resumable: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        incomplete: bool,
        #[serde(default = "true_value", skip_serializing_if = "is_true")]
        context_output_complete: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        native_replay: Option<NativeReplayPayload>,
        items: Vec<PersistedAssistantItem>,
    },
    Guardrail {
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
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum PersistedAssistantItem {
    Reasoning {
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Text {
        text: String,
    },
    BashCall {
        call_id: i64,
        provider_call_id: String,
        arguments: String,
    },
}

impl PersistedAssistantItem {
    fn bash_call(&self) -> Option<(i64, &str, &str)> {
        match self {
            Self::BashCall {
                call_id,
                provider_call_id,
                arguments,
                ..
            } => Some((*call_id, provider_call_id, arguments)),
            _ => None,
        }
    }

    fn assistant_item(&self) -> AssistantItem {
        match self {
            Self::Reasoning { text } => AssistantItem::Reasoning { text: text.clone() },
            Self::Text { text } => AssistantItem::Text { text: text.clone() },
            Self::BashCall {
                provider_call_id,
                arguments,
                ..
            } => AssistantItem::BashCall(ToolCall {
                id: provider_call_id.clone(),
                arguments: arguments.clone(),
            }),
        }
    }

    fn from_assistant(item: &AssistantItem, next_call_id: &mut i64) -> Self {
        match item {
            AssistantItem::Reasoning { text } => Self::Reasoning { text: text.clone() },
            AssistantItem::Text { text } => Self::Text { text: text.clone() },
            AssistantItem::BashCall(call) => {
                let call_id = *next_call_id;
                *next_call_id += 1;
                Self::BashCall {
                    call_id,
                    provider_call_id: call.id.clone(),
                    arguments: call.arguments.clone(),
                }
            }
        }
    }
}

fn assistant_turn_state(resumable: bool, items: &[PersistedAssistantItem]) -> &'static str {
    if items
        .iter()
        .any(|item| matches!(item, PersistedAssistantItem::BashCall { .. }))
    {
        "continue"
    } else if resumable {
        "resume"
    } else {
        "complete"
    }
}

fn assistant_summary(
    resumable: bool,
    incomplete: bool,
    items: &[PersistedAssistantItem],
) -> Option<String> {
    if incomplete || assistant_turn_state(resumable, items) != "complete" {
        return None;
    }
    let summary = items
        .iter()
        .filter_map(|item| match item {
            PersistedAssistantItem::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    (!summary.trim().is_empty()).then_some(summary)
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

impl Journal {
    fn next_seq(&self) -> i64 {
        self.events.last().map_or(1, |line| line.seq + 1)
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedSessionVersion {
    pub session_id: Option<String>,
    pub found: u32,
    pub supported: u32,
}

impl std::fmt::Display for UnsupportedSessionVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(session_id) = &self.session_id {
            write!(
                f,
                "session {session_id} uses journal version {}; this Mu supports version {}",
                self.found, self.supported
            )
        } else {
            write!(
                f,
                "session uses journal version {}; this Mu supports version {}",
                self.found, self.supported
            )
        }
    }
}

impl std::error::Error for UnsupportedSessionVersion {}

impl Store {
    // Store setup and session discovery.
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

    #[cfg(test)]
    pub fn set_session_version_for_test(&self, session_id: &str, version: u32) {
        let path = self.session_path(session_id);
        let text = std::fs::read_to_string(&path).unwrap();
        let (meta, events) = text.split_once('\n').unwrap();
        let mut meta: Value = serde_json::from_str(meta).unwrap();
        meta["version"] = version.into();
        std::fs::write(
            path,
            format!("{}\n{events}", serde_json::to_string(&meta).unwrap()),
        )
        .unwrap();
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

    pub fn list_sessions(&self, limit: usize) -> Result<SessionListing> {
        let mut sessions = Vec::new();
        let mut skipped = Vec::new();
        for entry in std::fs::read_dir(self.root.join("sessions"))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let journal = match self.load_path(&path) {
                Ok(journal) => journal,
                Err(_) if incomplete_session_initialization(&path)? => continue,
                Err(error) => match error.downcast_ref::<UnsupportedSessionVersion>() {
                    Some(unsupported) => {
                        skipped.push(unsupported.clone());
                        continue;
                    }
                    None => return Err(error),
                },
            };
            let session = self.project_session(&journal)?;
            let updated = activity_at(&journal).to_string();
            sessions.push((session, updated));
        }
        sessions.sort_by(|left, right| right.1.cmp(&left.1));
        sessions.truncate(limit);
        skipped.sort_by(|left, right| left.session_id.cmp(&right.session_id));
        Ok(SessionListing { sessions, skipped })
    }

    pub fn session_summary(&self, id: &str) -> Result<Option<SessionSummary>> {
        let Some(journal) = self.load_optional(id)? else {
            return Ok(None);
        };
        let session = self.project_session(&journal)?;
        Ok(Some(SessionSummary {
            id: id.to_string(),
            created_at: journal
                .events
                .first()
                .expect("validated journal has a system prompt")
                .at
                .clone(),
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
                .filter(|line| {
                    matches!(
                        line.event,
                        Event::PromptMaterialized { .. } | Event::CompactionStarted { .. }
                    )
                })
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
                        subject, origin, ..
                    } if subject.is_agent() => Some(origin.canonical_model_ref.clone()),
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
        let request = Request {
            model: ResolvedModelRef {
                canonical: model.to_string(),
                provider_id: provider_id.to_string(),
                model_id: wire_model.to_string(),
                effort: effort.map(str::to_string),
            },
            cache_key: None,
            messages: self.load_context_messages(session_id)?,
            bash: true,
            max_output_tokens: None,
        };
        let native = request.json(ModelApi::ChatCompletions)?;
        let exchange_id = self.start_provider_request(
            session_id,
            &turn_id,
            ProviderOrigin {
                canonical_model_ref: model.to_string(),
                provider_id: provider_id.to_string(),
                api: ModelApi::ChatCompletions.name().into(),
                endpoint: "http://localhost/chat/completions".into(),
                wire_model: wire_model.to_string(),
                effort: effort.map(str::to_string),
            },
            self.request_recipe(
                ModelApi::ChatCompletions.request_format(),
                &native,
                serde_json::json!({
                    "native_replay_origins": native_replay_origins(&request.messages),
                }),
            )?,
            RequestSubject::Agent,
        )?;
        if outcome == "completed" {
            self.complete_assistant_exchange(
                session_id,
                &exchange_id,
                &Message::Assistant {
                    items: Vec::new(),
                    native_replay: None,
                },
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

    // Recovery and read-only projections.
    pub fn is_session_clean(&self, session_id: &str) -> Result<bool> {
        let journal = self.load(session_id)?;
        if self.pending_compaction(session_id)?.is_some() {
            return Ok(false);
        }
        let Some((turn_seq, initially_complete)) =
            journal
                .events
                .iter()
                .rev()
                .find_map(|line| match &line.event {
                    Event::PromptMaterialized { .. } | Event::CompactionStarted { .. } => {
                        Some((line.seq, false))
                    }
                    Event::CompactionApplied { .. } => Some((
                        line.seq,
                        compaction_start_before(&journal, line.seq.saturating_sub(1)).is_some_and(
                            |(_, _, compaction)| compaction.mode == CompactionMode::AwaitUser,
                        ),
                    )),
                    _ => None,
                })
        else {
            return Ok(true);
        };
        let mut calls = HashSet::new();
        let mut results = HashSet::new();
        let mut requested = HashSet::new();
        let mut terminal = HashSet::new();
        let mut complete = initially_complete;
        for line in journal.events.iter().filter(|line| line.seq > turn_seq) {
            match &line.event {
                Event::ProviderRequested { exchange_id, .. } => {
                    requested.insert(exchange_id.as_str());
                }
                Event::ProviderCompleted {
                    exchange_id,
                    projection:
                        Projection::Assistant {
                            resumable, items, ..
                        },
                    ..
                } => {
                    terminal.insert(exchange_id.as_str());
                    calls.extend(
                        items
                            .iter()
                            .filter_map(|item| item.bash_call().map(|call| call.0)),
                    );
                    complete = assistant_turn_state(*resumable, items) == "complete";
                }
                Event::BashCompleted { call_id, .. } => {
                    results.insert(*call_id);
                }
                Event::ProviderCompleted { exchange_id, .. }
                | Event::ProviderFailed { exchange_id, .. }
                | Event::ProviderInterrupted { exchange_id } => {
                    terminal.insert(exchange_id.as_str());
                }
                _ => {}
            }
        }
        Ok(complete && calls.is_subset(&results) && requested.is_subset(&terminal))
    }

    pub fn resume_reminder_needed(&self, session_id: &str) -> Result<bool> {
        let journal = self.load(session_id)?;
        let Some(turn_id) = latest_active_turn_id(&journal) else {
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
                    projection: Projection::Assistant { resumable, .. },
                    ..
                } if exchange_turns.get(exchange_id.as_str()) == Some(&turn_id) => {
                    Some((line.seq, *resumable))
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
        let mut guardrail_calls = HashMap::new();
        let mut calls = HashSet::new();
        let mut results = HashSet::new();
        let mut denied = HashMap::new();
        for line in journal.events.iter() {
            match &line.event {
                Event::ProviderRequested {
                    exchange_id,
                    subject,
                    ..
                } => {
                    requested.push(exchange_id.clone());
                    if let Some((call_id, _)) = subject.guardrail() {
                        guardrail_calls.insert(exchange_id.clone(), call_id);
                    }
                }
                Event::ProviderCompleted {
                    exchange_id,
                    projection,
                    ..
                } => {
                    terminal.insert(exchange_id.clone());
                    match projection {
                        Projection::Assistant { items, .. } => {
                            for call_id in items
                                .iter()
                                .filter_map(|item| item.bash_call().map(|call| call.0))
                            {
                                calls.insert(call_id);
                            }
                        }
                        Projection::Guardrail {
                            outcome, reason, ..
                        } if outcome == "deny" => {
                            let call_id = guardrail_calls
                                .get(exchange_id)
                                .context("guardrail completion has no request subject")?;
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
            .filter(|call_id| !results.contains(call_id))
            .copied()
            .collect::<Vec<_>>();
        unresolved.sort_unstable();
        let mut normalized = 0;
        for call_id in unresolved {
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

    #[cfg(test)]
    pub fn transcript_events(&self, session_id: &str) -> Result<Vec<TranscriptEvent>> {
        self.transcript_events_for_epoch(session_id, None)
    }

    pub fn transcript_events_for_epoch(
        &self,
        session_id: &str,
        selected_epoch: Option<u64>,
    ) -> Result<Vec<TranscriptEvent>> {
        let journal = self.load(session_id)?;
        let prompts = queued_prompt_records(&journal);
        let mut results = HashMap::new();
        let mut turn_models = HashMap::new();
        let turn_epochs = journal
            .events
            .iter()
            .filter_map(|line| match &line.event {
                Event::CompactionApplied { .. } => continuation_turn_id(&journal, line.seq)
                    .map(|turn_id| (turn_id, context_epoch(&journal, line.seq))),
                event => project_turn(event, &prompts).map(|turn| {
                    (
                        turn.turn_id.to_string(),
                        context_epoch(&journal, line.seq.saturating_sub(1)),
                    )
                }),
            })
            .collect::<HashMap<_, _>>();
        let compaction_metadata = journal
            .events
            .iter()
            .filter_map(|line| {
                let turn = project_turn(&line.event, &prompts)?;
                turn.compaction.map(|compaction| (turn.turn_id, compaction))
            })
            .collect::<HashMap<_, _>>();
        let exchange_turns = provider_exchange_turns(&journal);
        let exchange_attempts = journal
            .events
            .iter()
            .filter_map(|line| {
                agent_compaction_attempt(&line.event)
                    .map(|(_, exchange_id, attempt)| (exchange_id, attempt))
            })
            .collect::<HashMap<_, _>>();
        for line in journal.events.iter() {
            match &line.event {
                Event::BashCompleted {
                    call_id,
                    outcome,
                    output,
                    exit_code,
                    duration_ms,
                    ..
                } => {
                    results.insert(
                        *call_id,
                        TranscriptBashResult {
                            outcome: outcome.clone(),
                            output: self.hydrate_text(output)?,
                            exit_code: *exit_code,
                            duration_ms: *duration_ms,
                        },
                    );
                }
                Event::ProviderRequested {
                    turn_id,
                    subject,
                    origin,
                    ..
                } if subject.is_agent() => {
                    turn_models
                        .entry(turn_id.as_str())
                        .or_insert_with(|| origin.canonical_model_ref.clone());
                }
                _ => {}
            }
        }

        let mut events = Vec::new();
        let mut remembered_model = None;
        let mut seen_turn = false;
        for line in journal.events.iter() {
            match &line.event {
                event @ (Event::PromptMaterialized { .. } | Event::CompactionStarted { .. }) => {
                    let turn = project_turn(event, &prompts)
                        .context("turn references an unknown queued prompt")?;
                    let epoch = context_epoch(&journal, line.seq.saturating_sub(1));
                    if selected_epoch.is_some_and(|selected| selected != epoch) {
                        continue;
                    }
                    if let Some(compaction) = turn.compaction {
                        events.push(TranscriptEvent::CompactionTriggered {
                            trigger: compaction.trigger,
                            context_tokens: compaction.before_context_tokens,
                            context_window: compaction.before_context_window,
                            reason: None,
                        });
                    }
                    let context = if seen_turn {
                        match reported_context_tokens_before(&journal, line.seq - 1) {
                            Some(tokens) => Some(TranscriptContext {
                                tokens,
                                estimated: false,
                            }),
                            None => Some(TranscriptContext {
                                tokens: self
                                    .context_until(&journal, line.seq - 1)?
                                    .iter()
                                    .map(Message::approx_tokens)
                                    .sum(),
                                estimated: true,
                            }),
                        }
                    } else {
                        None
                    };
                    events.push(TranscriptEvent::User {
                        text: user_text(turn.prompt),
                        cwd: turn.cwd.to_string(),
                        model: turn_models
                            .get(turn.turn_id)
                            .cloned()
                            .or_else(|| remembered_model.clone()),
                        context,
                        internal: turn.compaction.is_some(),
                    });
                    seen_turn = true;
                }
                Event::ProviderRequested {
                    subject, origin, ..
                } if subject.is_agent() => {
                    remembered_model = Some(origin.canonical_model_ref.clone());
                }
                Event::ProviderCompleted {
                    exchange_id,
                    projection:
                        Projection::Assistant {
                            resumable, items, ..
                        },
                    ..
                } => {
                    let turn_id = exchange_turns.get(exchange_id.as_str()).map(String::as_str);
                    let epoch = turn_id.and_then(|turn_id| turn_epochs.get(turn_id).copied());
                    if selected_epoch.is_some_and(|selected| epoch != Some(selected)) {
                        continue;
                    }
                    events.push(TranscriptEvent::Assistant {
                        turn_state: assistant_turn_state(*resumable, items).into(),
                        items: items
                            .iter()
                            .map(|item| match item {
                                PersistedAssistantItem::Reasoning { text } => {
                                    TranscriptAssistantItem::Reasoning(text.clone())
                                }
                                PersistedAssistantItem::Text { text } => {
                                    TranscriptAssistantItem::Text(text.clone())
                                }
                                PersistedAssistantItem::BashCall {
                                    call_id, arguments, ..
                                } => TranscriptAssistantItem::BashCall {
                                    arguments: arguments.clone(),
                                    result: results.get(call_id).cloned(),
                                },
                            })
                            .collect(),
                        internal: turn_id
                            .is_some_and(|turn_id| compaction_metadata.contains_key(turn_id)),
                    });
                }
                Event::ProviderFailed {
                    exchange_id,
                    error_class,
                    ..
                } if error_class == "context_length"
                    && exchange_attempts.get(exchange_id.as_str()).copied()
                        != Some(Some("emergency")) =>
                {
                    let turn_id = exchange_turns.get(exchange_id.as_str()).map(String::as_str);
                    let epoch = turn_id.and_then(|turn_id| turn_epochs.get(turn_id).copied());
                    if selected_epoch.is_some_and(|selected| epoch != Some(selected)) {
                        continue;
                    }
                    if let Some(compaction) =
                        turn_id.and_then(|turn_id| compaction_metadata.get(turn_id).copied())
                    {
                        events.push(TranscriptEvent::CompactionTriggered {
                            trigger: CompactionTrigger::Emergency,
                            context_tokens: compaction.before_context_tokens,
                            context_window: compaction.before_context_window,
                            reason: Some("compaction request exceeded provider context".into()),
                        });
                    }
                }
                Event::CompactionApplied {
                    after_context_tokens_estimate,
                    after_context_window,
                    ..
                } => {
                    let (start, _, compaction) =
                        compaction_start_before(&journal, line.seq.saturating_sub(1))
                            .context("compaction application has no start")?;
                    let from_epoch = context_epoch(&journal, line.seq.saturating_sub(1));
                    if selected_epoch.is_some_and(|selected| selected != from_epoch) {
                        continue;
                    }
                    events.push(TranscriptEvent::CompactionApplied {
                        from_epoch,
                        to_epoch: from_epoch.saturating_add(1),
                        before_context_tokens: compaction.before_context_tokens,
                        before_context_window: compaction.before_context_window,
                        after_context_tokens_estimate: *after_context_tokens_estimate,
                        after_context_window: *after_context_window,
                        elapsed_ms: elapsed_ms_between(&journal, start.seq, line.seq),
                    });
                }
                _ => {}
            }
        }
        Ok(events)
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

    // Append-only turn, tool, and provider events.
    #[cfg(test)]
    pub fn start_turn(
        &self,
        session_id: &str,
        cwd: &str,
        git_worktree_root: Option<&str>,
        prompt: &UserContent,
    ) -> Result<String> {
        self.queue_prompt(session_id, cwd, git_worktree_root, prompt)?;
        self.materialize_queued_prompt(session_id)?
            .context("queued prompt was not materialized")
    }

    pub fn start_compaction_turn(
        &self,
        session_id: &str,
        start: CompactionStart<'_>,
    ) -> Result<String> {
        let turn_id = format!("t{}", self.next_seq(session_id)?);
        let prompt = self.persist_user_content(start.prompt)?;
        self.append(
            session_id,
            Event::CompactionStarted {
                turn_id: turn_id.clone(),
                cwd: start.cwd.to_string(),
                prompt,
                trigger: start.trigger,
                mode: start.mode,
                before_context_tokens: start.before_context_tokens,
                before_context_window: start.before_context_window,
            },
        )?;
        Ok(turn_id)
    }

    pub fn queue_prompt(
        &self,
        session_id: &str,
        cwd: &str,
        git_worktree_root: Option<&str>,
        prompt: &UserContent,
    ) -> Result<String> {
        if self.pending_compaction(session_id)?.is_some() {
            bail!("session compaction is incomplete; run `mu retry -s {session_id}`")
        }
        if self.queued_prompt(session_id)?.is_some() {
            bail!("session already has a queued prompt; run `mu retry -s {session_id}`")
        }
        let prompt_id = format!("q{}", self.next_seq(session_id)?);
        self.append(
            session_id,
            Event::PromptQueued {
                prompt_id: prompt_id.clone(),
                cwd: cwd.to_string(),
                git_worktree_root: git_worktree_root.map(str::to_string),
                prompt: self.persist_user_content(prompt)?,
            },
        )?;
        Ok(prompt_id)
    }

    pub fn queued_prompt(&self, session_id: &str) -> Result<Option<QueuedPrompt>> {
        let journal = self.load(session_id)?;
        let consumed = journal
            .events
            .iter()
            .filter_map(|line| match &line.event {
                Event::PromptMaterialized { prompt_id, .. } => Some(prompt_id.as_str()),
                _ => None,
            })
            .collect::<HashSet<_>>();
        journal
            .events
            .iter()
            .rev()
            .find_map(|line| match &line.event {
                Event::PromptQueued { prompt_id, .. } if !consumed.contains(prompt_id.as_str()) => {
                    Some(QueuedPrompt {
                        prompt_id: prompt_id.clone(),
                        epoch: context_epoch(&journal, line.seq.saturating_sub(1)),
                    })
                }
                _ => None,
            })
            .map(Ok)
            .transpose()
    }

    pub fn materialize_queued_prompt(&self, session_id: &str) -> Result<Option<String>> {
        let Some(queued) = self.queued_prompt(session_id)? else {
            return Ok(None);
        };
        let turn_id = format!("t{}", self.next_seq(session_id)?);
        self.append(
            session_id,
            Event::PromptMaterialized {
                prompt_id: queued.prompt_id,
                turn_id: turn_id.clone(),
            },
        )?;
        Ok(Some(turn_id))
    }

    pub fn context_epoch(&self, session_id: &str) -> Result<u64> {
        self.with_journal(session_id, |journal| Ok(context_epoch(journal, i64::MAX)))
    }

    pub fn has_user_turn(&self, session_id: &str) -> Result<bool> {
        self.with_journal(session_id, |journal| {
            Ok(journal
                .events
                .iter()
                .any(|line| matches!(&line.event, Event::PromptMaterialized { .. })))
        })
    }

    pub fn pending_compaction(&self, session_id: &str) -> Result<Option<PendingCompaction>> {
        self.with_journal(session_id, |journal| {
            Ok(
                compaction_start_before(journal, i64::MAX).map(|(line, turn_id, compaction)| {
                    PendingCompaction {
                        turn_id: turn_id.to_string(),
                        trigger: compaction.trigger,
                        mode: compaction.mode,
                        from_epoch: context_epoch(journal, line.seq.saturating_sub(1)),
                        started_at: line.at.clone(),
                        before_context_tokens: compaction.before_context_tokens,
                        before_context_window: compaction.before_context_window,
                    }
                }),
            )
        })
    }

    pub fn pending_compaction_is_emergency(&self, session_id: &str, turn_id: &str) -> Result<bool> {
        self.with_journal(session_id, |journal| {
            let exchanges = journal
                .events
                .iter()
                .filter_map(|line| {
                    agent_compaction_attempt(&line.event).and_then(
                        |(request_turn, exchange_id, attempt)| {
                            (request_turn == turn_id).then_some((exchange_id, attempt))
                        },
                    )
                })
                .collect::<HashMap<_, _>>();
            Ok(journal
                .events
                .iter()
                .rev()
                .find_map(|line| match &line.event {
                    Event::ProviderFailed {
                        exchange_id,
                        error_class,
                        ..
                    } if exchanges.contains_key(exchange_id.as_str()) => Some(
                        error_class == "context_length"
                            || exchanges[exchange_id.as_str()] == Some("emergency"),
                    ),
                    Event::ProviderCompleted { exchange_id, .. }
                        if exchanges.contains_key(exchange_id.as_str()) =>
                    {
                        Some(exchanges[exchange_id.as_str()] == Some("emergency"))
                    }
                    _ => None,
                })
                .unwrap_or(false))
        })
    }

    pub fn pending_compaction_summary(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Result<Option<String>> {
        self.with_journal(session_id, |journal| {
            Ok(compaction_summary_before(journal, turn_id, i64::MAX))
        })
    }

    pub fn call_ids_for_provider_call_ids(
        &self,
        session_id: &str,
        provider_call_ids: &[String],
    ) -> Result<Vec<i64>> {
        self.with_journal(session_id, |journal| {
            let start_seq = journal
                .events
                .iter()
                .rev()
                .find_map(|line| match &line.event {
                    Event::CompactionApplied { .. } => Some(line.seq),
                    _ => None,
                })
                .unwrap_or(0);
            let completed = journal
                .events
                .iter()
                .filter_map(|line| match &line.event {
                    Event::BashCompleted { call_id, .. } => Some(*call_id),
                    _ => None,
                })
                .collect::<HashSet<_>>();
            let visible_calls = journal
                .events
                .iter()
                .filter(|line| line.seq > start_seq)
                .flat_map(|line| match &line.event {
                    Event::ProviderCompleted {
                        projection: Projection::Assistant { items, .. },
                        ..
                    } => items.as_slice(),
                    _ => &[],
                })
                .filter_map(|item| match item {
                    PersistedAssistantItem::BashCall {
                        call_id,
                        provider_call_id,
                        ..
                    } if completed.contains(call_id) => Some((provider_call_id.as_str(), *call_id)),
                    _ => None,
                });
            let mut visible_calls = visible_calls;
            let mut call_ids = Vec::with_capacity(provider_call_ids.len());
            for provider_call_id in provider_call_ids {
                let Some((_, call_id)) = visible_calls
                    .by_ref()
                    .find(|(visible_provider_id, _)| *visible_provider_id == provider_call_id)
                else {
                    bail!("emergency Bash elision references no visible tool result")
                };
                call_ids.push(call_id);
            }
            Ok(call_ids)
        })
    }

    pub fn apply_compaction(
        &self,
        session_id: &str,
        application: CompactionApplication,
    ) -> Result<u64> {
        let to_epoch = self.with_journal(session_id, |journal| {
            let (_, turn_id, _) = compaction_start_before(journal, i64::MAX)
                .context("session has no pending compaction")?;
            compaction_summary_before(journal, turn_id, i64::MAX)
                .context("pending compaction has no accepted summary")?;
            Ok(context_epoch(journal, i64::MAX).saturating_add(1))
        })?;
        self.append(
            session_id,
            Event::CompactionApplied {
                after_context_tokens_estimate: application.after_context_tokens_estimate,
                after_context_window: application.after_context_window,
            },
        )?;
        Ok(to_epoch)
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
                            Event::PromptMaterialized { turn_id: id, .. } if id == &turn_id => {
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
                let exchange_id = self.start_test_provider_request(session_id, &turn_id)?;
                self.complete_assistant_exchange(session_id, &exchange_id, message, None, None)
            }
            Message::System { .. } => Ok((1, Vec::new())),
            Message::Tool { .. } => bail!("Bash results require an internal Bash call identity"),
        }
    }

    #[cfg(test)]
    pub fn apply_test_compaction(&self, session_id: &str, summary: &str) -> Result<()> {
        let source_turn_id = self.start_compaction_turn(
            session_id,
            CompactionStart {
                cwd: "/tmp",
                prompt: &"compact".into(),
                trigger: CompactionTrigger::Manual,
                mode: CompactionMode::AwaitUser,
                before_context_tokens: 0,
                before_context_window: None,
            },
        )?;
        let exchange_id = self.start_test_provider_request(session_id, &source_turn_id)?;
        self.complete_assistant_exchange(
            session_id,
            &exchange_id,
            &Message::assistant(Some(summary.to_string()), None, None, None),
            None,
            None,
        )?;
        self.apply_compaction(
            session_id,
            CompactionApplication {
                after_context_tokens_estimate: 0,
                after_context_window: None,
            },
        )
        .map(|_| ())
    }

    #[cfg(test)]
    fn start_test_provider_request(&self, session_id: &str, turn_id: &str) -> Result<String> {
        let native_request = serde_json::json!({"model":"test"});
        let recipe = self.request_recipe("test.v1", &native_request, serde_json::json!({}))?;
        self.start_provider_request(
            session_id,
            turn_id,
            ProviderOrigin {
                canonical_model_ref: "test/model".into(),
                provider_id: "test".into(),
                api: "test".into(),
                endpoint: String::new(),
                wire_model: "model".into(),
                effort: None,
            },
            recipe,
            RequestSubject::Agent,
        )
    }

    pub fn persist_bash_result(
        &self,
        session_id: &str,
        record: BashResultRecord<'_>,
        content: &str,
        attachments: &[ToolAttachment],
    ) -> Result<(i64, Vec<ToolAttachment>)> {
        let already_completed = self.with_journal(session_id, |journal| {
            find_call(journal, record.bash_call_id)
                .context("locating Bash claim for result persistence")?;
            Ok(journal.events.iter().any(|line| {
                matches!(
                    line.event,
                    Event::BashCompleted { call_id, .. } if call_id == record.bash_call_id
                )
            }))
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
                call_id: record.bash_call_id,
                outcome: record.outcome.to_string(),
                output,
                exit_code: record.exit_code,
                duration_ms: record.duration_ms,
                attachments: attachments.clone(),
            },
        )?;
        let hydrated = attachments
            .iter()
            .map(|attachment| self.hydrate_tool_attachment(attachment))
            .collect::<Result<_>>()?;
        if let Ok((manifest, _)) = self.attachment_paths(session_id) {
            let _ = cleanup_bash_attachments(&manifest, record.bash_call_id);
        }
        Ok((seq, hydrated))
    }

    pub fn latest_summary_sequence(&self, session_id: &str) -> Result<Option<i64>> {
        let journal = self.load(session_id)?;
        Ok(journal.events.iter().rev().find_map(|line| {
            matches!(line.event, Event::CompactionApplied { .. }).then_some(line.seq)
        }))
    }

    pub fn context_tokens(
        &self,
        session_id: &str,
        config: &Config,
        target: &ResolvedModelRef,
        api: ModelApi,
    ) -> Result<ContextTokenEstimate> {
        let journal = self.load(session_id)?;
        self.context_tokens_for_journal(&journal, config, target, api)
    }

    pub fn queued_context_tokens(
        &self,
        session_id: &str,
        config: &Config,
        target: &ResolvedModelRef,
        api: ModelApi,
    ) -> Result<ContextTokenEstimate> {
        let mut journal = self.load(session_id)?;
        append_queued_turn_projection(&mut journal)?;
        self.context_tokens_for_journal(&journal, config, target, api)
    }

    pub fn projected_compaction_context_tokens(
        &self,
        session_id: &str,
        config: &Config,
        target: &ResolvedModelRef,
        api: ModelApi,
    ) -> Result<u64> {
        let mut journal = self.load(session_id)?;
        let mode = compaction_start_before(&journal, i64::MAX)
            .context("session has no pending compaction")?
            .2
            .mode;
        let seq = journal.next_seq();
        Arc::make_mut(&mut journal.events).push(EventLine {
            seq,
            at: now(),
            event: Event::CompactionApplied {
                after_context_tokens_estimate: 0,
                after_context_window: None,
            },
        });
        if mode == CompactionMode::AwaitUser {
            append_queued_turn_projection(&mut journal)?;
        }
        Ok(estimate_messages_tokens(
            &self.context(&journal)?,
            config,
            target,
            api,
        ))
    }

    fn context_tokens_for_journal(
        &self,
        journal: &Journal,
        config: &Config,
        target: &ResolvedModelRef,
        api: ModelApi,
    ) -> Result<ContextTokenEstimate> {
        let context = self.context(journal)?;
        if let Some((message_count, reported)) =
            self.latest_compatible_context_anchor(journal, config, target, api)?
            && context.len() >= message_count
        {
            let suffix = &context[message_count..];
            return Ok(ContextTokenEstimate {
                tokens: reported
                    .saturating_add(estimate_messages_tokens(suffix, config, target, api)),
                reported: suffix.is_empty(),
            });
        }
        Ok(ContextTokenEstimate {
            tokens: estimate_messages_tokens(&context, config, target, api),
            reported: false,
        })
    }

    fn latest_compatible_context_anchor(
        &self,
        journal: &Journal,
        config: &Config,
        target: &ResolvedModelRef,
        api: ModelApi,
    ) -> Result<Option<(usize, u64)>> {
        let replay_target = ReplayTarget {
            config,
            model: target,
            api,
        };
        let mut requests = HashMap::new();
        let mut anchor = None;
        for line in journal.events.iter() {
            match &line.event {
                Event::ProviderRequested {
                    exchange_id,
                    subject,
                    origin,
                    request_recipe,
                    ..
                } if subject.is_agent() => {
                    requests.insert(exchange_id.as_str(), (line.seq, origin, request_recipe));
                }
                Event::ProviderCompleted {
                    exchange_id,
                    usage: Some(usage),
                    projection:
                        Projection::Assistant {
                            context_output_complete,
                            native_replay,
                            items,
                            ..
                        },
                    ..
                } => {
                    let Some((request_seq, origin, recipe)) = requests.get(exchange_id.as_str())
                    else {
                        continue;
                    };
                    if !self.request_context_compatible(
                        journal,
                        *request_seq,
                        origin,
                        recipe,
                        replay_target,
                    )? {
                        continue;
                    }
                    let through_seq = request_seq.saturating_sub(1);
                    let input_message_count = self.context_until(journal, through_seq)?.len();
                    let output_complete = *context_output_complete
                        && (native_replay.is_some()
                            || !items.iter().any(|item| {
                                matches!(item, PersistedAssistantItem::Reasoning { .. })
                            }))
                        && native_replay.as_ref().is_none_or(|native| {
                            let message = Message::Assistant {
                                items: items
                                    .iter()
                                    .map(PersistedAssistantItem::assistant_item)
                                    .collect(),
                                native_replay: Some(hydrate_native_replay(native, origin)),
                            };
                            matches!(
                                filter_native_replay_for_config(&[message], config, target, api)
                                    .as_slice(),
                                [Message::Assistant {
                                    native_replay: Some(_),
                                    ..
                                }]
                            )
                        });
                    if output_complete && let Some(tokens) = usage.context_total() {
                        anchor = Some((input_message_count.saturating_add(1), tokens));
                    } else if usage.input_tokens > 0 {
                        anchor = Some((input_message_count, usage.input_tokens));
                    }
                }
                Event::CompactionApplied { .. } => anchor = None,
                _ => {}
            }
        }
        Ok(anchor)
    }

    fn request_context_compatible(
        &self,
        journal: &Journal,
        request_seq: i64,
        origin: &ProviderOrigin,
        recipe: &RequestRecipe,
        target: ReplayTarget<'_>,
    ) -> Result<bool> {
        if !request_format_is_current(&origin.api, &recipe.format)
            || !context_origin_compatible(target.config, origin, target.model, target.api)
            || recipe.input.get("native_fields").is_some()
        {
            return Ok(false);
        }
        let through_seq = request_seq.saturating_sub(1);
        if let Some(elided) = recipe.input.get("emergency_elided_call_ids") {
            let elided = serde_json::from_value::<Vec<i64>>(elided.clone())
                .context("invalid emergency Bash elision ids in request recipe")?;
            if !elided.is_empty() {
                return Ok(false);
            }
        }
        let Some(origins) = recipe.input.get("native_replay_origins") else {
            return Ok(false);
        };
        let recorded = serde_json::from_value::<Vec<ReplayOrigin>>(origins.clone())
            .context("invalid native replay origins in request recipe")?;
        let historical_context = self.context_until(journal, through_seq)?;
        let current_projection = filter_native_replay_for_config(
            &historical_context,
            target.config,
            target.model,
            target.api,
        );
        Ok(native_replay_origins(&current_projection) == recorded)
    }

    pub fn acquire_session_lock(&self, session_id: &str) -> Result<SessionLock<'_>> {
        if !valid_session_id(session_id) {
            bail!("session not found: {session_id}")
        }
        let mut locks = self.locks.lock().expect("session lock map poisoned");
        if locks.contains_key(session_id) {
            return Err(anyhow::Error::new(SessionBusy));
        }
        let locked = self.open_locked_session(session_id)?;
        locks.insert(session_id.to_string(), locked);
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
        match open_current_locked(&self.session_path(session_id)) {
            Ok(file) => {
                flock(&file, libc::LOCK_UN)?;
                Ok(false)
            }
            Err(error) if error.downcast_ref::<SessionBusy>().is_some() => Ok(true),
            Err(error) => Err(error),
        }
    }

    pub fn start_provider_request(
        &self,
        session_id: &str,
        turn_id: &str,
        origin: ProviderOrigin,
        recipe: RequestRecipe,
        subject: RequestSubject,
    ) -> Result<String> {
        self.with_journal(session_id, |journal| {
            if latest_active_turn_id(journal).as_deref() != Some(turn_id) {
                bail!("provider request does not reference the active turn")
            }
            if let RequestSubject::Guardrail { call_id, .. } = &subject
                && call_turn_id(journal, *call_id).as_deref() != Some(turn_id)
            {
                bail!("guardrail request does not match its Bash call turn")
            }
            Ok(())
        })?;
        let exchange_id = format!("e{}", self.next_seq(session_id)?);
        self.append(
            session_id,
            Event::ProviderRequested {
                turn_id: turn_id.to_string(),
                exchange_id: exchange_id.clone(),
                subject,
                origin,
                request_recipe: recipe,
            },
        )?;
        self.reconstruct_provider_request(session_id, &exchange_id)
            .with_context(|| {
                format!("verifying persisted provider request recipe {exchange_id}")
            })?;
        Ok(exchange_id)
    }

    pub fn current_turn_id(&self, session_id: &str) -> Result<String> {
        self.with_journal(session_id, |journal| {
            latest_active_turn_id(journal).context("session has no submitted turn")
        })
    }

    fn complete_provider_exchange(
        &self,
        session_id: &str,
        exchange_id: &str,
        native_response: Option<&Value>,
        usage: Option<&Usage>,
        projection: Projection,
    ) -> Result<i64> {
        self.append(
            session_id,
            Event::ProviderCompleted {
                exchange_id: exchange_id.to_string(),
                response_json: self.persist_json(native_response)?,
                usage: usage.cloned(),
                projection,
            },
        )
    }

    #[cfg(test)]
    pub fn complete_assistant_exchange(
        &self,
        session_id: &str,
        exchange_id: &str,
        message: &Message,
        native_response: Option<&Value>,
        usage: Option<&Usage>,
    ) -> Result<(i64, Vec<i64>)> {
        self.complete_assistant_exchange_inner(
            session_id,
            exchange_id,
            AssistantCompletion {
                message,
                native_response,
                usage,
                resumable: false,
                response_complete: true,
                context_output_complete: true,
            },
        )
    }

    #[cfg(test)]
    pub fn complete_resumable_assistant_exchange(
        &self,
        session_id: &str,
        exchange_id: &str,
        message: &Message,
        native_response: Option<&Value>,
        usage: Option<&Usage>,
    ) -> Result<(i64, Vec<i64>)> {
        self.complete_assistant_exchange_inner(
            session_id,
            exchange_id,
            AssistantCompletion {
                message,
                native_response,
                usage,
                resumable: true,
                response_complete: false,
                context_output_complete: true,
            },
        )
    }

    pub(crate) fn complete_assistant_exchange_record(
        &self,
        session_id: &str,
        exchange_id: &str,
        completion: AssistantCompletion<'_>,
    ) -> Result<(i64, Vec<i64>)> {
        self.complete_assistant_exchange_inner(session_id, exchange_id, completion)
    }

    fn complete_assistant_exchange_inner(
        &self,
        session_id: &str,
        exchange_id: &str,
        completion: AssistantCompletion<'_>,
    ) -> Result<(i64, Vec<i64>)> {
        let AssistantCompletion {
            message,
            native_response,
            usage,
            resumable,
            response_complete,
            context_output_complete,
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
            items,
            native_replay,
        } = message
        else {
            unreachable!()
        };
        let request_origin = self.with_journal(session_id, |journal| {
            journal
                .events
                .iter()
                .find_map(|line| match &line.event {
                    Event::ProviderRequested {
                        exchange_id: requested_exchange,
                        subject,
                        origin,
                        ..
                    } if requested_exchange == exchange_id && subject.is_agent() => {
                        Some(origin.clone())
                    }
                    _ => None,
                })
                .context("assistant completion has no agent request")
        })?;
        if let Some(native) = native_replay
            && (native.provider_id != request_origin.provider_id
                || native.model != request_origin.wire_model
                || request_origin.api != "test"
                    && (native.endpoint != request_origin.endpoint
                        || native.api().name() != request_origin.api))
        {
            self.fail_provider_exchange(
                session_id,
                exchange_id,
                "invalid_response",
                serde_json::json!({"message":"native replay origin does not match provider request"}),
                native_response,
                usage,
            )?;
            bail!("native replay origin does not match provider request")
        }
        let mut provider_call_ids = HashSet::new();
        if let Some(call) = items
            .iter()
            .filter_map(|item| match item {
                AssistantItem::BashCall(call) => Some(call),
                _ => None,
            })
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
        let items = items
            .iter()
            .map(|item| PersistedAssistantItem::from_assistant(item, &mut next_call))
            .collect::<Vec<_>>();
        let ids: Vec<i64> = items
            .iter()
            .filter_map(|item| item.bash_call().map(|call| call.0))
            .collect();
        let seq = self.complete_provider_exchange(
            session_id,
            exchange_id,
            native_response,
            usage,
            Projection::Assistant {
                resumable: ids.is_empty() && resumable,
                incomplete: !response_complete,
                context_output_complete,
                native_replay: native_replay.as_ref().map(|native| native.payload.clone()),
                items,
            },
        )?;
        Ok((seq, ids))
    }

    pub fn complete_guardrail_exchange(
        &self,
        session_id: &str,
        exchange_id: &str,
        completion: GuardrailCompletion<'_>,
    ) -> Result<()> {
        self.complete_provider_exchange(
            session_id,
            exchange_id,
            completion.native_response,
            completion.usage,
            Projection::Guardrail {
                outcome: completion.outcome.to_string(),
                risk_level: completion.risk_level.map(str::to_string),
                auth_level: completion.auth_level.map(str::to_string),
                reason: completion.reason.map(str::to_string),
            },
        )
        .map(|_| ())
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
        let partial_response_json = self.persist_json(partial_response)?;
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
        if (input.get("native_replay_origins").is_none() || format.starts_with("test."))
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
        self.reconstruct_provider_request_from(&journal, exchange_id)
    }

    fn reconstruct_provider_request_from(
        &self,
        journal: &Journal,
        exchange_id: &str,
    ) -> Result<Value> {
        let (request_seq, origin, recipe) = journal
            .events
            .iter()
            .find_map(|line| match &line.event {
                Event::ProviderRequested {
                    exchange_id: id,
                    origin,
                    request_recipe,
                    ..
                } if id == exchange_id => Some((line.seq, origin, request_recipe)),
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
            let through_seq = request_seq.saturating_sub(1);
            let elided_call_ids = recipe
                .input
                .get("emergency_elided_call_ids")
                .map(|call_ids| {
                    serde_json::from_value::<HashSet<i64>>(call_ids.clone())
                        .context("invalid emergency Bash elision ids in request recipe")
                })
                .transpose()?
                .unwrap_or_default();
            let messages =
                self.context_until_with_elisions(journal, through_seq, &elided_call_ids)?;
            let origins: Vec<crate::provider::ReplayOrigin> =
                serde_json::from_value(recipe.input["native_replay_origins"].clone())
                    .context("agent request recipe has no valid native replay origins")?;
            let messages =
                crate::provider::filter_native_replay_for_origins(&messages, api, &origins);
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
                max_output_tokens: recipe
                    .envelope
                    .get("max_output_tokens")
                    .or_else(|| recipe.envelope.get("max_completion_tokens"))
                    .or_else(|| recipe.envelope.get("max_tokens"))
                    .and_then(Value::as_u64),
            };
            request.json(api)?
        };
        if hex(Sha256::digest(canonical_json(&request))) != recipe.canonical_sha256 {
            bail!("reconstructed provider request checksum mismatch")
        }
        Ok(request)
    }

    // Semantic projections over the durable event stream.
    fn project_session(&self, journal: &Journal) -> Result<Session> {
        let prompts = queued_prompt_records(journal);
        let cwd = journal
            .events
            .iter()
            .rev()
            .find_map(|line| {
                project_turn(&line.event, &prompts)
                    .filter(|turn| turn.compaction.is_none())
                    .map(|turn| turn.cwd.to_string())
            })
            .unwrap_or_default();
        let title = journal.events.iter().find_map(|line| {
            project_turn(&line.event, &prompts)
                .filter(|turn| turn.compaction.is_none())
                .map(|turn| user_text(turn.prompt).chars().take(60).collect::<String>())
        });
        let last_model = journal
            .events
            .iter()
            .rev()
            .find_map(|line| match &line.event {
                Event::ProviderRequested {
                    subject, origin, ..
                } if subject.is_agent() => Some(origin.canonical_model_ref.clone()),
                _ => None,
            });
        Ok(Session {
            id: journal.meta.session_id.clone(),
            cwd,
            last_model,
            title,
        })
    }

    fn context(&self, journal: &Journal) -> Result<Vec<Message>> {
        self.context_until(journal, i64::MAX)
    }

    fn context_until(&self, journal: &Journal, max_seq: i64) -> Result<Vec<Message>> {
        self.context_until_with_elisions(journal, max_seq, &HashSet::new())
    }

    fn context_until_with_elisions(
        &self,
        journal: &Journal,
        max_seq: i64,
        elided_call_ids: &HashSet<i64>,
    ) -> Result<Vec<Message>> {
        let system = journal
            .events
            .iter()
            .find_map(|line| match &line.event {
                Event::SystemPrompt { content } => Some(content.clone()),
                _ => None,
            })
            .context("missing persisted system prompt")?;
        let applied = journal.events.iter().rev().find(|line| {
            line.seq <= max_seq && matches!(line.event, Event::CompactionApplied { .. })
        });
        let through_seq = applied.map(|line| line.seq);
        let mut messages = vec![Message::System { content: system }];
        let prompts = queued_prompt_records(journal);
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
        let mut found_elisions = HashSet::new();
        if let Some(line) = applied {
            messages.push(Message::User {
                content: compaction_checkpoint_for_application(journal, line.seq)?.into(),
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
                event @ (Event::PromptMaterialized { .. } | Event::CompactionStarted { .. }) => {
                    let turn = project_turn(event, &prompts)
                        .context("turn references an unknown queued prompt")?;
                    if let Some(location) = location_context(
                        previous_location.as_ref(),
                        turn.cwd,
                        turn.git_worktree_root,
                    ) {
                        messages.push(Message::User {
                            content: location.into(),
                        });
                    }
                    previous_location = Some((
                        turn.cwd.to_string(),
                        turn.git_worktree_root.map(str::to_string),
                    ));
                    messages.push(Message::User {
                        content: self.hydrate_user_content(turn.prompt)?,
                    });
                }
                Event::ProviderCompleted {
                    exchange_id,
                    projection:
                        Projection::Assistant {
                            resumable,
                            native_replay,
                            items,
                            ..
                        },
                    ..
                } => {
                    let native_replay = native_replay
                        .as_ref()
                        .map(|payload| {
                            exchange_origins
                                .get(exchange_id.as_str())
                                .map(|origin| hydrate_native_replay(payload, origin))
                                .context("assistant completion has no request origin")
                        })
                        .transpose()?;
                    messages.push(Message::Assistant {
                        items: items
                            .iter()
                            .map(PersistedAssistantItem::assistant_item)
                            .collect(),
                        native_replay,
                    });
                    if *resumable
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
                    if elided_call_ids.contains(call_id) {
                        found_elisions.insert(*call_id);
                        messages.push(Message::Tool {
                            content: crate::compaction::EMERGENCY_OUTPUT_UNAVAILABLE.into(),
                            attachments: Vec::new(),
                            tool_call_id: call.0.to_string(),
                        });
                        continue;
                    }
                    messages.push(Message::Tool {
                        content: self.hydrate_text(output)?,
                        attachments: attachments
                            .iter()
                            .map(|attachment| self.hydrate_tool_attachment(attachment))
                            .collect::<Result<_>>()?,
                        tool_call_id: call.0.to_string(),
                    });
                }
                _ => {}
            }
        }
        if &found_elisions != elided_call_ids {
            bail!("emergency Bash elision references no visible tool result")
        }
        Ok(messages)
    }

    // Object-backed content persistence.
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

    fn persist_json(&self, value: Option<&Value>) -> Result<Option<ObjectRef>> {
        value
            .map(serde_json::to_vec)
            .transpose()?
            .map(|bytes| self.write_object(&bytes))
            .transpose()
    }

    fn read_object(&self, object: &ObjectRef) -> Result<Vec<u8>> {
        read_object_from(&self.objects_dir(), &object.sha256)
    }

    // Journal I/O and locking.
    fn append(&self, session_id: &str, event: Event) -> Result<i64> {
        self.with_writer(session_id, |locked| {
            let seq = locked.journal.next_seq();
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
        self.with_journal(session_id, |journal| Ok(journal.next_seq()))
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
        let mut locked = self.open_locked_session(session_id)?;
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
        match journal_version(&mut file)? {
            FORMAT_VERSION => read_journal(&mut file, expected.as_deref(), false),
            version => Err(anyhow::Error::new(UnsupportedSessionVersion {
                session_id: expected,
                found: version,
                supported: FORMAT_VERSION,
            })),
        }
    }

    fn open_locked_session(&self, session_id: &str) -> Result<LockedSession> {
        if !valid_session_id(session_id) {
            bail!("session not found: {session_id}")
        }
        let mut file = open_current_locked(&self.session_path(session_id))
            .with_context(|| format!("opening session journal: {session_id}"))?;
        match journal_version(&mut file)? {
            FORMAT_VERSION => {
                let journal = read_journal(&mut file, Some(session_id), true)?;
                Ok(LockedSession { file, journal })
            }
            version => Err(anyhow::Error::new(UnsupportedSessionVersion {
                session_id: Some(session_id.to_string()),
                found: version,
                supported: FORMAT_VERSION,
            })),
        }
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
    let prefix = complete_prefix(&mut file, true)?;
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
    let entries = parse_manifest(&complete_prefix(&mut file, true)?)?
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
    let entries = parse_manifest(&complete_prefix(&mut file, true)?)?;
    file.seek(SeekFrom::Start(0))?;
    file.set_len(0)?;
    for entry in entries.into_iter().filter(|entry| entry.call_id != call_id) {
        write_json_line(&mut file, &entry)?;
    }
    file.sync_data()?;
    flock(&file, libc::LOCK_UN)?;
    Ok(())
}

fn complete_prefix(file: &mut File, truncate: bool) -> Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position + 1);
    if truncate && complete < bytes.len() {
        file.set_len(complete as u64)?;
    }
    bytes.truncate(complete);
    Ok(bytes)
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

fn journal_version(file: &mut File) -> Result<u32> {
    file.seek(SeekFrom::Start(0))?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line)?;
    if !line.ends_with('\n') {
        bail!("session journal has no complete meta line")
    }
    let meta: MetaVersion = serde_json::from_str(&line).context("decoding session meta")?;
    if meta.kind != "meta" || meta.format != "mu-session" {
        bail!("unsupported session journal format")
    }
    Ok(meta.version)
}

fn open_current_locked(path: &Path) -> Result<File> {
    loop {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        match flock_nonblocking(&file) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::EWOULDBLOCK) => {
                return Err(anyhow::Error::new(SessionBusy));
            }
            Err(error) => return Err(error.into()),
        }
        let opened = file.metadata()?;
        let current = std::fs::metadata(path)?;
        if opened.dev() == current.dev() && opened.ino() == current.ino() {
            return Ok(file);
        }
        flock(&file, libc::LOCK_UN)?;
    }
}

fn read_journal(
    file: &mut File,
    expected_id: Option<&str>,
    truncate_incomplete_tail: bool,
) -> Result<Journal> {
    let bytes = complete_prefix(file, truncate_incomplete_tail)?;
    if bytes.is_empty() {
        bail!("session journal has no complete meta line")
    }
    let text = std::str::from_utf8(&bytes).context("session journal is not UTF-8")?;
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
    let mut turns = HashSet::new();
    let mut exchanges = HashMap::new();
    let mut terminal = HashSet::new();
    let mut calls = HashMap::new();
    let mut results = HashSet::new();
    let mut guardrail_attempts = HashSet::new();
    let mut queued_prompts = HashSet::new();
    let mut unresolved_prompt = None;
    let mut pending_compaction: Option<(String, CompactionMode, bool)> = None;
    let mut active_turn = None;
    for line in events {
        match &line.event {
            Event::SystemPrompt { .. } if line.seq != 1 => {
                bail!("system prompt must be the first event")
            }
            Event::PromptMaterialized { prompt_id, turn_id } => {
                if calls.keys().any(|call_id| !results.contains(call_id)) {
                    bail!("new turn starts before prior Bash claims are resolved")
                }
                if pending_compaction.is_some() {
                    bail!("normal turn starts while compaction is incomplete")
                }
                if unresolved_prompt.as_ref() != Some(prompt_id) {
                    bail!("turn references an unknown or consumed queued prompt")
                }
                unresolved_prompt = None;
                if !turns.insert(turn_id.clone()) {
                    bail!("duplicate turn id: {turn_id}")
                }
                active_turn = Some(turn_id.clone());
            }
            Event::CompactionStarted { turn_id, mode, .. } => {
                if calls.keys().any(|call_id| !results.contains(call_id)) {
                    bail!("new turn starts before prior Bash claims are resolved")
                }
                if pending_compaction.is_some() {
                    bail!("compaction starts before the prior compaction is applied")
                }
                if !turns.insert(turn_id.clone()) {
                    bail!("duplicate turn id: {turn_id}")
                }
                pending_compaction = Some((turn_id.clone(), *mode, false));
                active_turn = Some(turn_id.clone());
            }
            Event::PromptQueued { prompt_id, .. } => {
                if pending_compaction.is_some() {
                    bail!("prompt is queued while compaction is incomplete")
                }
                if unresolved_prompt.is_some() {
                    bail!("multiple unresolved queued prompts")
                }
                if !queued_prompts.insert(prompt_id.clone()) {
                    bail!("duplicate queued prompt id: {prompt_id}")
                }
                unresolved_prompt = Some(prompt_id.clone());
            }
            Event::ProviderRequested {
                turn_id,
                exchange_id,
                request_recipe,
                subject,
                origin,
            } => {
                if active_turn.as_ref() != Some(turn_id) {
                    bail!("provider request does not reference the active turn")
                }
                if subject.is_agent() && request_recipe.input.get("native_fields").is_none() {
                    serde_json::from_value::<Vec<ReplayOrigin>>(
                        request_recipe.input["native_replay_origins"].clone(),
                    )
                    .context("agent request recipe has no valid native replay origins")?;
                }
                if subject.is_agent()
                    && let Some((pending_turn, _, accepted)) = &mut pending_compaction
                    && pending_turn == turn_id
                {
                    *accepted = false;
                }
                if let Some((call_id, attempt)) = subject.guardrail() {
                    if !calls.contains_key(&call_id) {
                        bail!(
                            "guardrail request references unknown Bash call: {}",
                            call_id
                        )
                    }
                    if !guardrail_attempts.insert((call_id, attempt)) {
                        bail!(
                            "duplicate guardrail attempt {} for Bash call {}",
                            attempt,
                            call_id
                        )
                    }
                    if calls.get(&call_id) != Some(turn_id) {
                        bail!("guardrail request does not match its Bash call turn")
                    }
                }
                if exchanges
                    .insert(
                        exchange_id.clone(),
                        (subject.clone(), turn_id.clone(), origin.api.clone()),
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
                let Some(exchange) = exchanges.get(exchange_id) else {
                    bail!("provider completion references unknown exchange: {exchange_id}")
                };
                if !terminal.insert(exchange_id) {
                    bail!("duplicate terminal provider event: {exchange_id}")
                }
                match projection {
                    Projection::Assistant {
                        resumable,
                        incomplete,
                        native_replay,
                        items,
                        ..
                    } => {
                        if !exchange.0.is_agent() {
                            bail!("assistant projection does not complete an agent request")
                        }
                        if exchange.2 != "test"
                            && native_replay
                                .as_ref()
                                .is_some_and(|native| native.api().name() != exchange.2)
                        {
                            bail!("native replay protocol does not match provider request")
                        }
                        let has_calls = items
                            .iter()
                            .any(|item| matches!(item, PersistedAssistantItem::BashCall { .. }));
                        if *resumable && has_calls {
                            bail!("resumable assistant projection has Bash claims")
                        }
                        let mut provider_ids = HashSet::new();
                        for item in items {
                            let PersistedAssistantItem::BashCall {
                                call_id,
                                provider_call_id,
                                arguments,
                            } = item
                            else {
                                continue;
                            };
                            if !provider_ids.insert(provider_call_id) {
                                bail!("duplicate provider call id")
                            }
                            if !serde_json::from_str::<Value>(arguments)
                                .is_ok_and(|arguments| arguments.is_object())
                            {
                                bail!("Bash claim arguments are not a JSON object")
                            }
                            let turn_id = exchange.1.clone();
                            if calls.insert(*call_id, turn_id).is_some() {
                                bail!("duplicate Bash call id: {call_id}")
                            }
                        }
                        if let Some((turn_id, _, accepted)) = &mut pending_compaction
                            && turn_id == &exchange.1
                            && assistant_summary(*resumable, *incomplete, items).is_some()
                        {
                            *accepted = true;
                        }
                    }
                    Projection::Guardrail { outcome, .. } => {
                        if exchange.0.guardrail().is_none() {
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
                let Some(exchange) = exchanges.get(exchange_id) else {
                    bail!("provider terminal event references unknown exchange: {exchange_id}")
                };
                if !terminal.insert(exchange_id) {
                    bail!("duplicate terminal provider event: {exchange_id}")
                }
                if exchange.0.is_agent()
                    && let Some((turn_id, _, accepted)) = &mut pending_compaction
                    && turn_id == &exchange.1
                {
                    *accepted = false;
                }
            }
            Event::BashCompleted {
                call_id,
                outcome,
                exit_code,
                ..
            } => {
                if !calls.contains_key(call_id) {
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
            Event::CompactionApplied { .. } => {
                let Some((_, mode, accepted)) = pending_compaction.take() else {
                    bail!("compaction application references no pending compaction")
                };
                if !accepted {
                    bail!("compaction application has no accepted summary")
                }
                if mode == CompactionMode::ContinueTurn {
                    let turn_id = format!("t{}", line.seq);
                    if !turns.insert(turn_id.clone()) {
                        bail!("duplicate continuation turn id: {turn_id}")
                    }
                    active_turn = Some(turn_id);
                } else {
                    active_turn = None;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn latest_active_turn_id(journal: &Journal) -> Option<String> {
    journal
        .events
        .iter()
        .rev()
        .find_map(|line| match &line.event {
            Event::PromptMaterialized { turn_id, .. }
            | Event::CompactionStarted { turn_id, .. } => Some(turn_id.clone()),
            Event::CompactionApplied { .. } => continuation_turn_id(journal, line.seq),
            _ => None,
        })
}

fn context_epoch(journal: &Journal, max_seq: i64) -> u64 {
    journal
        .events
        .iter()
        .filter(|line| line.seq <= max_seq)
        .filter(|line| matches!(line.event, Event::CompactionApplied { .. }))
        .count() as u64
}

#[derive(Clone, Copy)]
struct CompactionProjection {
    trigger: CompactionTrigger,
    mode: CompactionMode,
    before_context_tokens: u64,
    before_context_window: Option<u64>,
}

struct TurnProjection<'a> {
    turn_id: &'a str,
    cwd: &'a str,
    git_worktree_root: Option<&'a str>,
    prompt: &'a PersistedUserContent,
    compaction: Option<CompactionProjection>,
}

fn queued_prompt_records(
    journal: &Journal,
) -> HashMap<&str, (&str, Option<&str>, &PersistedUserContent)> {
    journal
        .events
        .iter()
        .filter_map(|line| match &line.event {
            Event::PromptQueued {
                prompt_id,
                cwd,
                git_worktree_root,
                prompt,
            } => Some((
                prompt_id.as_str(),
                (cwd.as_str(), git_worktree_root.as_deref(), prompt),
            )),
            _ => None,
        })
        .collect()
}

fn project_turn<'a>(
    event: &'a Event,
    prompts: &HashMap<&str, (&'a str, Option<&'a str>, &'a PersistedUserContent)>,
) -> Option<TurnProjection<'a>> {
    match event {
        Event::PromptMaterialized { prompt_id, turn_id } => {
            let (cwd, git_worktree_root, prompt) = prompts.get(prompt_id.as_str())?;
            Some(TurnProjection {
                turn_id,
                cwd,
                git_worktree_root: *git_worktree_root,
                prompt,
                compaction: None,
            })
        }
        Event::CompactionStarted {
            turn_id,
            cwd,
            prompt,
            trigger,
            mode,
            before_context_tokens,
            before_context_window,
        } => Some(TurnProjection {
            turn_id,
            cwd,
            git_worktree_root: None,
            prompt,
            compaction: Some(CompactionProjection {
                trigger: *trigger,
                mode: *mode,
                before_context_tokens: *before_context_tokens,
                before_context_window: *before_context_window,
            }),
        }),
        _ => None,
    }
}

fn compaction_start_before(
    journal: &Journal,
    max_seq: i64,
) -> Option<(&EventLine, &str, CompactionProjection)> {
    let mut pending = None;
    for line in journal.events.iter().filter(|line| line.seq <= max_seq) {
        match &line.event {
            Event::CompactionStarted {
                turn_id,
                trigger,
                mode,
                before_context_tokens,
                before_context_window,
                ..
            } => {
                pending = Some((
                    line,
                    turn_id.as_str(),
                    CompactionProjection {
                        trigger: *trigger,
                        mode: *mode,
                        before_context_tokens: *before_context_tokens,
                        before_context_window: *before_context_window,
                    },
                ));
            }
            Event::CompactionApplied { .. } => pending = None,
            _ => {}
        }
    }
    pending
}

fn elapsed_ms_between(journal: &Journal, start_seq: i64, end_seq: i64) -> u64 {
    let timestamp = |seq| {
        journal
            .events
            .iter()
            .find(|line| line.seq == seq)
            .and_then(|line| chrono::DateTime::parse_from_rfc3339(&line.at).ok())
    };
    timestamp(start_seq)
        .zip(timestamp(end_seq))
        .and_then(|(start, end)| (end - start).to_std().ok())
        .map_or(0, |elapsed| {
            elapsed.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

fn compaction_summary_before(journal: &Journal, turn_id: &str, max_seq: i64) -> Option<String> {
    let exchange_id = journal
        .events
        .iter()
        .rev()
        .filter(|line| line.seq <= max_seq)
        .find_map(|line| match &line.event {
            Event::ProviderRequested {
                exchange_id,
                turn_id: request_turn,
                subject,
                ..
            } if request_turn == turn_id && subject.is_agent() => Some(exchange_id.as_str()),
            _ => None,
        })?;
    journal
        .events
        .iter()
        .rev()
        .filter(|line| line.seq <= max_seq)
        .find_map(|line| match &line.event {
            Event::ProviderCompleted {
                exchange_id: completed_exchange,
                projection:
                    Projection::Assistant {
                        resumable,
                        incomplete,
                        items,
                        ..
                    },
                ..
            } if completed_exchange == exchange_id => {
                assistant_summary(*resumable, *incomplete, items)
            }
            Event::ProviderCompleted {
                exchange_id: completed_exchange,
                ..
            }
            | Event::ProviderFailed {
                exchange_id: completed_exchange,
                ..
            }
            | Event::ProviderInterrupted {
                exchange_id: completed_exchange,
            } if completed_exchange == exchange_id => None,
            _ => None,
        })
}

fn compaction_checkpoint_for_application(journal: &Journal, seq: i64) -> Result<String> {
    let (_, turn_id, compaction) = compaction_start_before(journal, seq.saturating_sub(1))
        .context("compaction application has no pending compaction")?;
    let summary = compaction_summary_before(journal, turn_id, seq.saturating_sub(1))
        .context("compaction application has no accepted summary")?;
    Ok(crate::compaction::checkpoint(
        &summary,
        compaction.mode,
        context_epoch(journal, seq),
    ))
}

fn continuation_turn_id(journal: &Journal, seq: i64) -> Option<String> {
    compaction_start_before(journal, seq.saturating_sub(1))
        .filter(|(_, _, compaction)| compaction.mode == CompactionMode::ContinueTurn)
        .map(|_| format!("t{seq}"))
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
            Event::PromptMaterialized { .. } | Event::CompactionStarted { .. } => return false,
            Event::ProviderRequested {
                turn_id: request_turn,
                subject,
                ..
            } if request_turn == turn_id && subject.is_agent() => return true,
            _ => {}
        }
    }
    false
}

fn append_queued_turn_projection(journal: &mut Journal) -> Result<()> {
    let consumed = journal
        .events
        .iter()
        .filter_map(|line| match &line.event {
            Event::PromptMaterialized { prompt_id, .. } => Some(prompt_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let queued = journal
        .events
        .iter()
        .rev()
        .find_map(|line| match &line.event {
            Event::PromptQueued { prompt_id, .. } if !consumed.contains(prompt_id.as_str()) => {
                Some(prompt_id.clone())
            }
            _ => None,
        })
        .context("session has no queued prompt")?;
    let seq = journal.next_seq();
    Arc::make_mut(&mut journal.events).push(EventLine {
        seq,
        at: now(),
        event: Event::PromptMaterialized {
            prompt_id: queued,
            turn_id: format!("projected-t{seq}"),
        },
    });
    Ok(())
}

fn next_call_id(journal: &Journal) -> i64 {
    journal
        .events
        .iter()
        .filter_map(|line| match &line.event {
            Event::ProviderCompleted {
                projection: Projection::Assistant { items, .. },
                ..
            } => items
                .iter()
                .filter_map(|item| item.bash_call().map(|call| call.0))
                .max(),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        + 1
}

fn find_call(journal: &Journal, call_id: i64) -> Option<(&str, &str)> {
    journal.events.iter().find_map(|line| match &line.event {
        Event::ProviderCompleted {
            projection: Projection::Assistant { items, .. },
            ..
        } => items.iter().find_map(|item| match item {
            PersistedAssistantItem::BashCall {
                call_id: candidate,
                provider_call_id,
                arguments,
                ..
            } if *candidate == call_id => Some((provider_call_id.as_str(), arguments.as_str())),
            _ => None,
        }),
        _ => None,
    })
}

fn call_turn_id(journal: &Journal, call_id: i64) -> Option<String> {
    let exchange_id = journal.events.iter().find_map(|line| match &line.event {
        Event::ProviderCompleted {
            exchange_id,
            projection: Projection::Assistant { items, .. },
            ..
        } if items
            .iter()
            .any(|item| item.bash_call().is_some_and(|call| call.0 == call_id)) =>
        {
            Some(exchange_id.as_str())
        }
        _ => None,
    })?;
    journal.events.iter().find_map(|line| match &line.event {
        Event::ProviderRequested {
            exchange_id: candidate,
            turn_id,
            ..
        } if candidate == exchange_id => Some(turn_id.clone()),
        _ => None,
    })
}

fn is_semantic(event: &Event) -> bool {
    matches!(
        event,
        Event::SystemPrompt { .. }
            | Event::PromptMaterialized { .. }
            | Event::CompactionStarted { .. }
            | Event::CompactionApplied { .. }
            | Event::BashCompleted { .. }
            | Event::ProviderCompleted {
                projection: Projection::Assistant { .. },
                ..
            }
    )
}

fn reported_context_tokens_before(journal: &Journal, max_seq: i64) -> Option<u64> {
    let mut request_formats = HashMap::new();
    let mut latest = None;
    let mut semantic_after = false;
    for line in journal.events.iter().filter(|line| line.seq <= max_seq) {
        match &line.event {
            Event::ProviderRequested {
                exchange_id,
                subject,
                origin,
                request_recipe,
                ..
            } if subject.is_agent() => {
                let current = request_format_is_current(&origin.api, &request_recipe.format);
                request_formats.insert(exchange_id.as_str(), current);
            }
            Event::ProviderCompleted {
                exchange_id,
                usage: Some(usage),
                projection:
                    Projection::Assistant {
                        context_output_complete,
                        native_replay,
                        items,
                        ..
                    },
                ..
            } => {
                let output_complete = *context_output_complete
                    && (native_replay.is_some()
                        || !items
                            .iter()
                            .any(|item| matches!(item, PersistedAssistantItem::Reasoning { .. })));
                latest = output_complete
                    .then(|| {
                        request_formats
                            .get(exchange_id.as_str())
                            .and_then(|current| {
                                usage.context_total().map(|tokens| (tokens, *current))
                            })
                    })
                    .flatten();
                semantic_after = !output_complete;
            }
            Event::ProviderCompleted {
                projection: Projection::Assistant { .. },
                ..
            } => semantic_after = true,
            Event::PromptMaterialized { .. }
            | Event::CompactionStarted { .. }
            | Event::CompactionApplied { .. }
            | Event::BashCompleted { .. } => semantic_after = true,
            _ => {}
        }
    }
    latest
        .filter(|(_, current)| !semantic_after && *current)
        .map(|(tokens, _)| tokens)
}

fn hydrate_native_replay(payload: &NativeReplayPayload, origin: &ProviderOrigin) -> NativeReplay {
    NativeReplay {
        provider_id: origin.provider_id.clone(),
        endpoint: origin.endpoint.clone(),
        model: origin.wire_model.clone(),
        payload: payload.clone(),
    }
}

#[derive(Clone, Copy)]
struct ReplayTarget<'a> {
    config: &'a Config,
    model: &'a ResolvedModelRef,
    api: ModelApi,
}

fn context_origin_compatible(
    config: &Config,
    origin: &ProviderOrigin,
    target: &ResolvedModelRef,
    api: ModelApi,
) -> bool {
    #[cfg(test)]
    let same_api = if origin.api == "test" {
        api == ModelApi::ChatCompletions
    } else {
        origin.api == api.name()
    };
    #[cfg(not(test))]
    let same_api = origin.api == api.name();
    if !same_api || origin.wire_model != target.model_id {
        return false;
    }
    if config
        .model_config(&origin.provider_id, &origin.wire_model)
        .is_some()
    {
        config.replay_key(&origin.provider_id, &origin.wire_model)
            == config.replay_key(&target.provider_id, &target.model_id)
    } else {
        origin.provider_id == target.provider_id
    }
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
        .expect("validated journal has a system prompt")
        .at
        .as_str()
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
    use crate::config::{
        CompactionConfig, GuardrailConfig, LimitsConfig, ModelConfig, OrderedMap, ProviderConfig,
        RedactionConfig, TerminalBellConfig,
    };
    use crate::provider::{Message, NativeReplayPayload};

    fn test_session() -> (Store, Session) {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system").unwrap();
        (store, session)
    }

    fn context_test_config(source_key: Option<&str>, target_key: Option<&str>) -> Config {
        let provider = |replay_key: Option<&str>| ProviderConfig {
            endpoint: "http://localhost/chat/completions".into(),
            api_key_env: String::new(),
            models: OrderedMap::from_iter([(
                "model".into(),
                ModelConfig {
                    context_window: Some(200_000),
                    supported_efforts: None,
                    replay_key: replay_key.map(str::to_string),
                },
            )]),
        };
        Config {
            providers: OrderedMap::from_iter([
                ("source".into(), provider(source_key)),
                ("target".into(), provider(target_key)),
            ]),
            output: Default::default(),
            auto_resume: false,
            soft_interrupt: crate::config::bundled_test_default("/soft_interrupt"),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            terminal_bell: TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: Default::default(),
        }
    }

    #[test]
    fn context_tokens_prefer_compatible_provider_anchor_and_estimate_only_the_suffix() {
        let (store, session) = test_session();
        store
            .append_test_agent_exchange(&session.id, "source/model", "completed", 100)
            .unwrap();
        let config = context_test_config(Some("shared"), Some("shared"));
        let target = crate::models::resolve_model_ref(&config, "target/model").unwrap();

        assert_eq!(
            store
                .context_tokens(&session.id, &config, &target, ModelApi::ChatCompletions)
                .unwrap(),
            ContextTokenEstimate {
                tokens: 100,
                reported: true
            }
        );
        assert!(
            !store
                .context_tokens(&session.id, &config, &target, ModelApi::Responses)
                .unwrap()
                .reported
        );

        let incompatible = context_test_config(Some("source"), Some("target"));
        let incompatible_target =
            crate::models::resolve_model_ref(&incompatible, "target/model").unwrap();
        assert!(
            !store
                .context_tokens(
                    &session.id,
                    &incompatible,
                    &incompatible_target,
                    ModelApi::ChatCompletions,
                )
                .unwrap()
                .reported
        );

        store
            .start_turn(&session.id, "/tmp", None, &"12345678".into())
            .unwrap();
        assert_eq!(
            store
                .context_tokens(&session.id, &config, &target, ModelApi::ChatCompletions)
                .unwrap(),
            ContextTokenEstimate {
                tokens: 102,
                reported: false
            }
        );
    }

    #[test]
    fn invalid_provider_context_total_falls_back_to_projection() {
        let (store, session) = test_session();
        store
            .append_test_agent_exchange(&session.id, "source/model", "completed", 0)
            .unwrap();
        let config = context_test_config(None, None);
        let target = crate::models::resolve_model_ref(&config, "source/model").unwrap();
        let estimate = store
            .context_tokens(&session.id, &config, &target, ModelApi::ChatCompletions)
            .unwrap();
        assert!(!estimate.reported);
        assert!(estimate.tokens > 0);
    }

    #[test]
    fn queued_context_projection_matches_materialized_turn() {
        let (store, session) = test_session();
        store
            .append_test_agent_exchange(&session.id, "source/model", "completed", 100)
            .unwrap();
        store
            .queue_prompt(&session.id, "/other", Some("/repo"), &"12345678".into())
            .unwrap();
        let config = context_test_config(Some("shared"), Some("shared"));
        let target = crate::models::resolve_model_ref(&config, "target/model").unwrap();

        let projected = store
            .queued_context_tokens(&session.id, &config, &target, ModelApi::ChatCompletions)
            .unwrap();
        assert!(!projected.reported);
        assert!(projected.tokens > 102, "{projected:?}");

        store.materialize_queued_prompt(&session.id).unwrap();
        let materialized = store
            .audit_events(&session.id)
            .unwrap()
            .into_iter()
            .find(|event| event["type"] == "prompt_materialized")
            .unwrap();
        assert!(materialized["prompt_id"].is_string());
        for field in ["cwd", "git_worktree_root", "prompt"] {
            assert!(materialized.get(field).is_none(), "unexpected {field}");
        }
        assert_eq!(
            projected,
            store
                .context_tokens(&session.id, &config, &target, ModelApi::ChatCompletions,)
                .unwrap()
        );
    }

    #[test]
    fn incomplete_retained_output_anchors_at_reported_input() {
        let (store, session) = test_session();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"test".into())
            .unwrap();
        let request = Request {
            model: ResolvedModelRef {
                canonical: "source/model".into(),
                provider_id: "source".into(),
                model_id: "model".into(),
                effort: None,
            },
            cache_key: None,
            messages: store.load_context_messages(&session.id).unwrap(),
            bash: true,
            max_output_tokens: None,
        };
        let native = request.json(ModelApi::ChatCompletions).unwrap();
        let exchange = store
            .start_provider_request(
                &session.id,
                &turn,
                ProviderOrigin {
                    canonical_model_ref: "source/model".into(),
                    provider_id: "source".into(),
                    api: "chat_completions".into(),
                    endpoint: "http://localhost/chat/completions".into(),
                    wire_model: "model".into(),
                    effort: None,
                },
                store
                    .request_recipe(
                        "openai.chat_completions.v1",
                        &native,
                        serde_json::json!({
                            "native_replay_origins": [],
                        }),
                    )
                    .unwrap(),
                RequestSubject::Agent,
            )
            .unwrap();
        store
            .complete_assistant_exchange_record(
                &session.id,
                &exchange,
                AssistantCompletion {
                    message: &Message::assistant(Some("12345678".into()), None, None, None),
                    native_response: None,
                    usage: Some(&Usage {
                        input_tokens: 80,
                        output_tokens: 20,
                        total_tokens: 100,
                        ..Usage::default()
                    }),
                    resumable: false,
                    response_complete: true,
                    context_output_complete: false,
                },
            )
            .unwrap();
        let config = context_test_config(None, None);
        let target = crate::models::resolve_model_ref(&config, "source/model").unwrap();

        assert_eq!(
            store
                .context_tokens(&session.id, &config, &target, ModelApi::ChatCompletions,)
                .unwrap(),
            ContextTokenEstimate {
                tokens: 82,
                reported: false,
            }
        );
    }

    #[test]
    fn context_anchor_uses_recorded_replay_selection_after_config_drift() {
        let (store, session) = test_session();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"test".into())
            .unwrap();
        let replay_exchange = store
            .start_provider_request(
                &session.id,
                &turn,
                ProviderOrigin {
                    canonical_model_ref: "legacy/model".into(),
                    provider_id: "legacy".into(),
                    api: ModelApi::Responses.name().into(),
                    endpoint: "http://localhost/responses".into(),
                    wire_model: "model".into(),
                    effort: None,
                },
                store
                    .request_recipe(
                        "test.v1",
                        &serde_json::json!({"model":"model"}),
                        serde_json::json!({}),
                    )
                    .unwrap(),
                RequestSubject::Agent,
            )
            .unwrap();
        store
            .complete_assistant_exchange(
                &session.id,
                &replay_exchange,
                &Message::assistant(
                    Some("answer".into()),
                    None,
                    None,
                    Some(NativeReplay {
                        provider_id: "legacy".into(),
                        endpoint: "http://localhost/responses".into(),
                        model: "model".into(),
                        payload: NativeReplayPayload::ResponsesOutput(vec![
                            serde_json::json!({
                                "type": "reasoning",
                                "encrypted_content": "x".repeat(1_000_000),
                            }),
                            serde_json::json!({
                                "type": "message",
                                "content": [{"type": "output_text", "text": "answer"}],
                            }),
                        ]),
                    }),
                ),
                None,
                None,
            )
            .unwrap();
        let mut config = context_test_config(Some("shared"), Some("shared"));
        config.providers = OrderedMap::from_iter([
            (
                "source".into(),
                ProviderConfig {
                    endpoint: "http://localhost/responses".into(),
                    api_key_env: String::new(),
                    models: OrderedMap::from_iter([(
                        "model".into(),
                        ModelConfig {
                            context_window: Some(200_000),
                            supported_efforts: None,
                            replay_key: Some("shared".into()),
                        },
                    )]),
                },
            ),
            (
                "legacy".into(),
                ProviderConfig {
                    endpoint: "http://localhost/responses".into(),
                    api_key_env: String::new(),
                    models: OrderedMap::from_iter([(
                        "model".into(),
                        ModelConfig {
                            context_window: Some(200_000),
                            supported_efforts: None,
                            replay_key: Some("shared".into()),
                        },
                    )]),
                },
            ),
        ]);
        let target = crate::models::resolve_model_ref(&config, "source/model").unwrap();
        let messages = filter_native_replay_for_config(
            &store.load_context_messages(&session.id).unwrap(),
            &config,
            &target,
            ModelApi::Responses,
        );
        let request = Request {
            model: target.clone(),
            cache_key: None,
            messages: messages.clone(),
            bash: true,
            max_output_tokens: None,
        };
        let native = request.json(ModelApi::Responses).unwrap();
        let exchange = store
            .start_provider_request(
                &session.id,
                &store.current_turn_id(&session.id).unwrap(),
                ProviderOrigin {
                    canonical_model_ref: target.canonical.clone(),
                    provider_id: target.provider_id.clone(),
                    api: ModelApi::Responses.name().into(),
                    endpoint: "http://localhost/responses".into(),
                    wire_model: target.model_id.clone(),
                    effort: None,
                },
                store
                    .request_recipe(
                        ModelApi::Responses.request_format(),
                        &native,
                        serde_json::json!({
                            "native_replay_origins": native_replay_origins(&messages),
                        }),
                    )
                    .unwrap(),
                RequestSubject::Agent,
            )
            .unwrap();
        store
            .complete_assistant_exchange(
                &session.id,
                &exchange,
                &Message::assistant(Some("done".into()), None, None, None),
                None,
                Some(&Usage {
                    input_tokens: 180,
                    output_tokens: 20,
                    total_tokens: 200,
                    ..Usage::default()
                }),
            )
            .unwrap();
        assert_eq!(
            store
                .context_tokens(&session.id, &config, &target, ModelApi::Responses)
                .unwrap(),
            ContextTokenEstimate {
                tokens: 200,
                reported: true,
            }
        );

        config
            .providers
            .iter_mut()
            .find(|(provider, _)| provider.as_str() == "source")
            .unwrap()
            .1
            .models
            .iter_mut()
            .find(|(model, _)| model.as_str() == "model")
            .unwrap()
            .1
            .replay_key = Some("changed".into());
        let target = crate::models::resolve_model_ref(&config, "source/model").unwrap();
        assert!(
            !store
                .context_tokens(&session.id, &config, &target, ModelApi::Responses)
                .unwrap()
                .reported
        );
    }

    #[test]
    fn projected_compaction_context_matches_applied_journal() {
        let (store, session) = test_session();
        store
            .start_turn(&session.id, "/tmp", None, &"test".into())
            .unwrap();
        store
            .queue_prompt(&session.id, "/other", Some("/repo"), &"queued".into())
            .unwrap();
        let source_turn_id = store
            .start_compaction_turn(
                &session.id,
                CompactionStart {
                    cwd: "/tmp",
                    prompt: &"compact".into(),
                    trigger: CompactionTrigger::Manual,
                    mode: CompactionMode::AwaitUser,
                    before_context_tokens: 100,
                    before_context_window: Some(200_000),
                },
            )
            .unwrap();
        let exchange = store
            .start_test_provider_request(&session.id, &source_turn_id)
            .unwrap();
        store
            .complete_assistant_exchange(
                &session.id,
                &exchange,
                &Message::assistant(Some("summary".into()), None, None, None),
                None,
                None,
            )
            .unwrap();
        let config = context_test_config(None, None);
        let target = crate::models::resolve_model_ref(&config, "source/model").unwrap();
        let projected = store
            .projected_compaction_context_tokens(
                &session.id,
                &config,
                &target,
                ModelApi::ChatCompletions,
            )
            .unwrap();

        store
            .apply_compaction(
                &session.id,
                CompactionApplication {
                    after_context_tokens_estimate: projected,
                    after_context_window: Some(200_000),
                },
            )
            .unwrap();
        store.materialize_queued_prompt(&session.id).unwrap();

        assert_eq!(
            projected,
            store
                .context_tokens(&session.id, &config, &target, ModelApi::ChatCompletions,)
                .unwrap()
                .tokens
        );
    }

    #[test]
    fn context_preserves_user_and_tool_attachments() {
        let (store, session) = test_session();
        store
            .append_message(
                &session.id,
                &Message::User {
                    content: UserContent::Parts(vec![
                        ContentPart::Text {
                            text: "before".into(),
                        },
                        ContentPart::Attachment {
                            attachment: Attachment {
                                filename: "image.png".into(),
                                media_type: "image/png".into(),
                                data: vec![1, 2, 3],
                            },
                        },
                        ContentPart::Text {
                            text: "after".into(),
                        },
                    ]),
                },
            )
            .unwrap();

        let context = store.load_context_messages(&session.id).unwrap();
        assert!(matches!(
            context.last(),
            Some(Message::User {
                content: UserContent::Parts(parts),
            }) if matches!(
                parts.as_slice(),
                [
                    ContentPart::Text { text: before },
                    ContentPart::Attachment {
                        attachment: Attachment { filename, data, .. },
                    },
                    ContentPart::Text { text: after },
                ] if before == "before"
                    && filename == "image.png"
                    && data == &[1, 2, 3]
                    && after == "after"
            )
        ));

        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::assistant(
                    None,
                    None,
                    Some(vec![ToolCall {
                        id: "call".into(),
                        arguments: r#"{"title":"view","risk":"readonly","command":"true"}"#.into(),
                    }]),
                    None,
                ),
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
                "result",
                &[ToolAttachment {
                    attachment: Attachment {
                        filename: "result.png".into(),
                        media_type: "image/png".into(),
                        data: vec![4, 5, 6],
                    },
                    detail: ImageDetail::High,
                    object_sha256: None,
                }],
            )
            .unwrap();
        let context = store.load_context_messages(&session.id).unwrap();
        let result = context
            .iter()
            .find(|message| matches!(message, Message::Tool { .. }))
            .unwrap();
        assert!(matches!(
            result,
            Message::Tool {
                content,
                attachments,
                ..
            } if content == "result"
                && matches!(
                    attachments.as_slice(),
                    [ToolAttachment {
                        attachment: Attachment { filename, data, .. },
                        ..
                    }] if filename == "result.png" && data == &[4, 5, 6]
                )
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
    fn unsupported_session_versions_are_reported_without_rewriting() {
        let store = Store::open_memory().unwrap();
        let supported = store.create_session_seeded("system").unwrap();
        let unsupported = store.create_session_seeded("system").unwrap();
        store.select_session(&unsupported.id).unwrap();
        store.set_session_version_for_test(&unsupported.id, 1);
        let path = store.session_path(&unsupported.id);
        let before = std::fs::read(&path).unwrap();

        let error = store.get_session(&unsupported.id).unwrap_err();
        assert_eq!(
            error.downcast_ref::<UnsupportedSessionVersion>(),
            Some(&UnsupportedSessionVersion {
                session_id: Some(unsupported.id.clone()),
                found: 1,
                supported: FORMAT_VERSION,
            })
        );
        assert!(store.current_session().is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);

        let listing = store.list_sessions(20).unwrap();
        assert_eq!(listing.sessions.len(), 1);
        assert_eq!(listing.sessions[0].0.id, supported.id);
        assert_eq!(
            listing.skipped,
            vec![UnsupportedSessionVersion {
                session_id: Some(unsupported.id),
                found: 1,
                supported: FORMAT_VERSION,
            }]
        );
    }

    #[test]
    fn transcript_projection_pairs_bash_results_without_native_state() {
        let (store, session) = test_session();
        store
            .start_turn(&session.id, "/tmp", None, &"Run it".into())
            .unwrap();
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::Assistant {
                    items: vec![
                        AssistantItem::Reasoning {
                            text: Some("Inspect first.".into()),
                        },
                        AssistantItem::Text {
                            text: "I will run it.".into(),
                        },
                        AssistantItem::BashCall(ToolCall {
                            id: "call-1".into(),
                            arguments: r#"{"title":"Run","command":"printf ok","risk":"readonly"}"#
                                .into(),
                        }),
                        AssistantItem::Text {
                            text: " Finished.".into(),
                        },
                    ],
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
                    duration_ms: Some(7),
                },
                "ok",
                &[],
            )
            .unwrap();

        assert_eq!(
            store.transcript_events(&session.id).unwrap(),
            vec![
                TranscriptEvent::User {
                    text: "Run it".into(),
                    cwd: "/tmp".into(),
                    model: Some("test/model".into()),
                    context: None,
                    internal: false,
                },
                TranscriptEvent::Assistant {
                    turn_state: "continue".into(),
                    items: vec![
                        TranscriptAssistantItem::Reasoning(Some("Inspect first.".into())),
                        TranscriptAssistantItem::Text("I will run it.".into()),
                        TranscriptAssistantItem::BashCall {
                            arguments: r#"{"title":"Run","command":"printf ok","risk":"readonly"}"#
                                .into(),
                            result: Some(TranscriptBashResult {
                                outcome: "completed".into(),
                                output: "ok".into(),
                                exit_code: Some(0),
                                duration_ms: Some(7),
                            }),
                        },
                        TranscriptAssistantItem::Text(" Finished.".into()),
                    ],
                    internal: false,
                },
            ]
        );
    }

    #[test]
    fn transcript_projection_reconstructs_prompt_model_and_context_state() {
        let (store, session) = test_session();
        store
            .start_turn(&session.id, "/first", None, &"First".into())
            .unwrap();
        store
            .append_test_agent_exchange(&session.id, "test/model:high", "completed", 25)
            .unwrap();
        store
            .start_turn(&session.id, "/second", None, &"Second".into())
            .unwrap();
        store
            .start_turn(&session.id, "/third", None, &"Third".into())
            .unwrap();

        let prompts = store
            .transcript_events(&session.id)
            .unwrap()
            .into_iter()
            .filter_map(|event| match event {
                TranscriptEvent::User {
                    cwd,
                    model,
                    context,
                    ..
                } => Some((cwd, model, context)),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            prompts[0],
            ("/first".into(), Some("test/model:high".into()), None)
        );
        assert_eq!(
            prompts[1],
            (
                "/second".into(),
                Some("test/model:high".into()),
                Some(TranscriptContext {
                    tokens: 25,
                    estimated: false,
                }),
            )
        );
        assert_eq!(prompts[2].0, "/third");
        assert_eq!(prompts[2].1.as_deref(), Some("test/model:high"));
        assert!(
            matches!(
                prompts[2].2,
                Some(TranscriptContext {
                    tokens,
                    estimated: true
                }) if tokens > 0
            ),
            "{:?}",
            prompts[2].2
        );
    }

    #[test]
    fn interrupted_claim_gets_one_synthetic_result() {
        let (store, session) = test_session();
        store
            .start_turn(&session.id, "/tmp", None, &"run".into())
            .unwrap();
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::assistant(
                    None,
                    None,
                    Some(
                        ["first", "second"]
                            .into_iter()
                            .map(|id| ToolCall {
                                id: id.into(),
                                arguments: r#"{"risk":"readonly","command":"true"}"#.into(),
                            })
                            .collect(),
                    ),
                    None,
                ),
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
        let (store, session) = test_session();
        store
            .start_turn(&session.id, "/tmp", None, &"run".into())
            .unwrap();
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::assistant(
                    None,
                    None,
                    Some(vec![ToolCall {
                        id: "provider-call".into(),
                        arguments: r#"{"risk":"readonly","command":"true"}"#.into(),
                    }]),
                    None,
                ),
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
            parse_manifest(&complete_prefix(&mut file, true).unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn object_checksum_mismatch_is_rejected() {
        let store = Store::open_memory().unwrap();
        let object = store.write_object(b"content").unwrap();
        std::fs::write(store.objects_dir().join(&object.sha256), b"corrupt").unwrap();
        assert!(
            store
                .read_object(&object)
                .unwrap_err()
                .to_string()
                .contains("object checksum mismatch")
        );
    }

    #[test]
    fn agent_request_reconstructs_from_semantic_history() {
        let (store, session) = test_session();
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
            max_output_tokens: None,
            messages,
            bash: true,
        };
        let native = request.json(ModelApi::ChatCompletions).unwrap();
        let recipe = store
            .request_recipe(
                "openai.chat_completions.v1",
                &native,
                serde_json::json!({
                    "native_replay_origins": [],
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
                ProviderOrigin {
                    canonical_model_ref: request.model.canonical.clone(),
                    provider_id: request.model.provider_id.clone(),
                    api: "chat_completions".into(),
                    endpoint: endpoint.into(),
                    wire_model: request.model.model_id.clone(),
                    effort: request.model.effort.clone(),
                },
                recipe,
                RequestSubject::Agent,
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
        let (store, session) = test_session();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"work".into())
            .unwrap();
        let first = store
            .start_test_provider_request(&session.id, &turn)
            .unwrap();
        let endpoint = "https://example.test/v1/messages";
        store
            .complete_resumable_assistant_exchange(
                &session.id,
                &first,
                &Message::assistant(
                    None,
                    None,
                    None,
                    Some(NativeReplay {
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
                ),
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
            max_output_tokens: None,
            messages: messages.clone(),
            bash: true,
        };
        let native = request.json(ModelApi::AnthropicMessages).unwrap();
        let recipe = store
            .request_recipe(
                "anthropic.messages.v1",
                &native,
                serde_json::json!({
                    "native_replay_origins":
                        crate::provider::native_replay_origins(&messages),
                }),
            )
            .unwrap();
        let resumed = store
            .start_provider_request(
                &session.id,
                &turn,
                ProviderOrigin {
                    canonical_model_ref: request.model.canonical.clone(),
                    provider_id: request.model.provider_id.clone(),
                    api: "anthropic_messages".into(),
                    endpoint: endpoint.into(),
                    wire_model: request.model.model_id.clone(),
                    effort: request.model.effort.clone(),
                },
                recipe,
                RequestSubject::Agent,
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
    fn recorded_replay_selection_reconstructs_exactly_after_model_change() {
        let (store, session) = test_session();
        store
            .start_turn(&session.id, "/tmp", None, &"run".into())
            .unwrap();
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::assistant(
                    None,
                    Some("trace".into()),
                    Some(vec![ToolCall {
                        id: "provider-call".into(),
                        arguments: r#"{"risk":"readonly","command":"pwd"}"#.into(),
                    }]),
                    Some(NativeReplay {
                        provider_id: "test".into(),
                        endpoint: String::new(),
                        model: "model".into(),
                        payload: crate::provider::NativeReplayPayload::ChatReasoning(
                            "trace".into(),
                        ),
                    }),
                ),
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
            max_output_tokens: None,
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
                    "native_replay_origins": replay_origins,
                }),
            )
            .unwrap();
        let exchange = store
            .start_provider_request(
                &session.id,
                &store.current_turn_id(&session.id).unwrap(),
                ProviderOrigin {
                    canonical_model_ref: request.model.canonical.clone(),
                    provider_id: request.model.provider_id.clone(),
                    api: "chat_completions".into(),
                    endpoint: endpoint.into(),
                    wire_model: request.model.model_id.clone(),
                    effort: None,
                },
                recipe,
                RequestSubject::Agent,
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
        let (store, session) = test_session();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"hello".into())
            .unwrap();
        let native = serde_json::json!({"model":"model"});
        let exchange = store
            .start_provider_request(
                &session.id,
                &turn,
                ProviderOrigin {
                    canonical_model_ref: "test/model".into(),
                    provider_id: "test".into(),
                    api: "test".into(),
                    endpoint: String::new(),
                    wire_model: "model".into(),
                    effort: None,
                },
                store
                    .request_recipe("test.v0", &native, serde_json::json!({}))
                    .unwrap(),
                RequestSubject::Agent,
            )
            .unwrap();
        store
            .complete_assistant_exchange(
                &session.id,
                &exchange,
                &Message::assistant(Some("done".into()), None, None, None),
                None,
                Some(&Usage {
                    total_tokens: 10,
                    ..Usage::default()
                }),
            )
            .unwrap();

        assert_eq!(
            reported_context_tokens_before(&store.load(&session.id).unwrap(), i64::MAX),
            None
        );
    }

    #[test]
    fn duplicate_provider_tool_call_ids_are_rejected_before_persistence() {
        let (store, session) = test_session();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"hello".into())
            .unwrap();
        let native = serde_json::json!({"model":"model"});
        let exchange = store
            .start_provider_request(
                &session.id,
                &turn,
                ProviderOrigin {
                    canonical_model_ref: "test/model".into(),
                    provider_id: "test".into(),
                    api: "test".into(),
                    endpoint: String::new(),
                    wire_model: "model".into(),
                    effort: None,
                },
                store
                    .request_recipe("test.v1", &native, serde_json::json!({}))
                    .unwrap(),
                RequestSubject::Agent,
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
                &Message::assistant(None, None, Some(vec![call.clone(), call]), None),
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
        let (store, session) = test_session();
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
        let (store, session) = test_session();
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
        let (store, session) = test_session();
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
        let (store, session) = test_session();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"remove it".into())
            .unwrap();
        let (_, calls) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::assistant(
                    None,
                    None,
                    Some(vec![ToolCall {
                        id: "provider-call".into(),
                        arguments: r#"{"risk":"destructive","command":"rm x"}"#.into(),
                    }]),
                    None,
                ),
            )
            .unwrap();
        let native = serde_json::json!({"model":"reviewer","messages":[]});
        let recipe = store
            .request_recipe("test.v1", &native, serde_json::json!({}))
            .unwrap();
        let exchange = store
            .start_provider_request(
                &session.id,
                &turn,
                ProviderOrigin {
                    canonical_model_ref: "test/reviewer".into(),
                    provider_id: "test".into(),
                    api: "test".into(),
                    endpoint: String::new(),
                    wire_model: "reviewer".into(),
                    effort: None,
                },
                recipe,
                RequestSubject::Guardrail {
                    call_id: calls[0],
                    attempt: 1,
                },
            )
            .unwrap();
        store
            .complete_guardrail_exchange(
                &session.id,
                &exchange,
                GuardrailCompletion {
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
                        serde_json::json!({}),
                    )
                    .unwrap(),
                RequestSubject::Agent,
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
        let (store, session) = test_session();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"work".into())
            .unwrap();
        let exchange = store
            .start_test_provider_request(&session.id, &turn)
            .unwrap();
        store
            .complete_resumable_assistant_exchange(
                &session.id,
                &exchange,
                &Message::assistant(None, None, None, None),
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
            .start_test_provider_request(&session.id, &turn)
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
    fn compaction_requires_the_latest_response_to_be_final() {
        let (store, session) = test_session();
        store
            .start_turn(&session.id, "/tmp", None, &"work".into())
            .unwrap();
        let compaction_turn = store
            .start_compaction_turn(
                &session.id,
                CompactionStart {
                    cwd: "/tmp",
                    prompt: &"checkpoint".into(),
                    trigger: CompactionTrigger::Manual,
                    mode: CompactionMode::AwaitUser,
                    before_context_tokens: 100,
                    before_context_window: Some(1_000),
                },
            )
            .unwrap();
        let partial = store
            .start_test_provider_request(&session.id, &compaction_turn)
            .unwrap();
        store
            .complete_assistant_exchange_record(
                &session.id,
                &partial,
                AssistantCompletion {
                    message: &Message::assistant(Some("partial".into()), None, None, None),
                    native_response: None,
                    usage: None,
                    resumable: false,
                    response_complete: false,
                    context_output_complete: true,
                },
            )
            .unwrap();
        assert!(
            store
                .pending_compaction_summary(&session.id, &compaction_turn)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .apply_compaction(
                    &session.id,
                    CompactionApplication {
                        after_context_tokens_estimate: 10,
                        after_context_window: Some(1_000),
                    },
                )
                .is_err()
        );

        let final_exchange = store
            .start_test_provider_request(&session.id, &compaction_turn)
            .unwrap();
        store
            .complete_assistant_exchange(
                &session.id,
                &final_exchange,
                &Message::assistant(Some("final".into()), None, None, None),
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .pending_compaction_summary(&session.id, &compaction_turn)
                .unwrap()
                .as_deref(),
            Some("final")
        );
    }

    #[test]
    fn mismatched_native_replay_origin_is_audited_as_invalid() {
        let (store, session) = test_session();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"work".into())
            .unwrap();
        let exchange = store
            .start_test_provider_request(&session.id, &turn)
            .unwrap();
        let error = store
            .complete_assistant_exchange(
                &session.id,
                &exchange,
                &Message::assistant(
                    Some("answer".into()),
                    None,
                    None,
                    Some(NativeReplay {
                        provider_id: "other".into(),
                        endpoint: String::new(),
                        model: "model".into(),
                        payload: NativeReplayPayload::ChatReasoning("trace".into()),
                    }),
                ),
                None,
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("native replay origin"));
        assert!(
            store
                .audit_events(&session.id)
                .unwrap()
                .iter()
                .any(|event| {
                    event["type"] == "provider_failed" && event["error_class"] == "invalid_response"
                })
        );
    }

    #[test]
    fn new_prompt_supersedes_unused_resume_without_synthetic_message() {
        let (store, session) = test_session();
        let turn = store
            .start_turn(&session.id, "/tmp", None, &"work".into())
            .unwrap();
        let exchange = store
            .start_test_provider_request(&session.id, &turn)
            .unwrap();
        store
            .complete_resumable_assistant_exchange(
                &session.id,
                &exchange,
                &Message::assistant(None, None, None, None),
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

    #[test]
    fn applied_compaction_advances_epoch_and_projects_only_the_checkpoint() {
        let (store, session) = test_session();
        let turn = store
            .start_turn(&session.id, "/work", None, &"original task".into())
            .unwrap();
        let exchange = store
            .start_test_provider_request(&session.id, &turn)
            .unwrap();
        store
            .complete_assistant_exchange(
                &session.id,
                &exchange,
                &Message::assistant(Some("work so far".into()), None, None, None),
                None,
                None,
            )
            .unwrap();

        let compaction_turn = store
            .start_compaction_turn(
                &session.id,
                CompactionStart {
                    cwd: "/work",
                    prompt: &"checkpoint prompt".into(),
                    trigger: CompactionTrigger::Manual,
                    mode: CompactionMode::AwaitUser,
                    before_context_tokens: 1_700,
                    before_context_window: Some(2_000),
                },
            )
            .unwrap();
        assert!(
            store
                .queue_prompt(&session.id, "/work", None, &"too early".into())
                .unwrap_err()
                .to_string()
                .contains("compaction is incomplete")
        );
        let summary_exchange = store
            .start_test_provider_request(&session.id, &compaction_turn)
            .unwrap();
        store
            .complete_assistant_exchange(
                &session.id,
                &summary_exchange,
                &Message::assistant(Some("## Progress\nReady.".into()), None, None, None),
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            store
                .pending_compaction_summary(&session.id, &compaction_turn)
                .unwrap()
                .as_deref(),
            Some("## Progress\nReady.")
        );

        let checkpoint = "<session_checkpoint mode=\"await_user\" epoch=\"1\">\n## Progress\nReady.\n</session_checkpoint>";
        assert_eq!(
            store
                .apply_compaction(
                    &session.id,
                    CompactionApplication {
                        after_context_tokens_estimate: 30,
                        after_context_window: Some(2_000),
                    },
                )
                .unwrap(),
            1
        );
        assert_eq!(store.context_epoch(&session.id).unwrap(), 1);
        assert!(store.pending_compaction(&session.id).unwrap().is_none());
        assert!(store.is_session_clean(&session.id).unwrap());
        let audit = store.audit_events(&session.id).unwrap();
        let applied = audit
            .iter()
            .find(|event| event["type"] == "compaction_applied")
            .unwrap();
        for field in [
            "source_turn_id",
            "trigger",
            "mode",
            "summary",
            "checkpoint",
            "continuation_turn_id",
            "before_context_tokens",
            "before_context_window",
            "elapsed_ms",
            "emergency_elided_call_ids",
        ] {
            assert!(applied.get(field).is_none(), "unexpected {field}");
        }
        let context = store.load_context_messages(&session.id).unwrap();
        assert_eq!(context.len(), 2);
        assert!(matches!(
            &context[0],
            Message::System { content } if content == "system"
        ));
        assert!(matches!(
            &context[1],
            Message::User { content } if content.text() == checkpoint
        ));

        let transcript = store.transcript_events(&session.id).unwrap();
        assert!(matches!(
            transcript[2],
            TranscriptEvent::CompactionTriggered {
                trigger: CompactionTrigger::Manual,
                ..
            }
        ));
        assert!(matches!(
            transcript.last(),
            Some(TranscriptEvent::CompactionApplied {
                from_epoch: 0,
                to_epoch: 1,
                ..
            })
        ));
    }

    #[test]
    fn emergency_elision_maps_reused_provider_ids_in_context_order() {
        let (store, session) = test_session();
        let mut durable_ids = Vec::new();
        for turn in 0..2 {
            store
                .start_turn(&session.id, "/work", None, &format!("turn {turn}").into())
                .unwrap();
            let (_, call_ids) = store
                .append_message_with_bash_calls(
                    &session.id,
                    &Message::assistant(
                        None,
                        None,
                        Some(vec![ToolCall {
                            id: "reused-provider-id".into(),
                            arguments: r#"{"title":"inspect","risk":"readonly","command":"true"}"#
                                .into(),
                        }]),
                        None,
                    ),
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
                    "output",
                    &[],
                )
                .unwrap();
            store
                .append_message(
                    &session.id,
                    &Message::assistant(Some("done".into()), None, None, None),
                )
                .unwrap();
            durable_ids.push(call_ids[0]);
        }

        assert_eq!(
            store
                .call_ids_for_provider_call_ids(
                    &session.id,
                    &["reused-provider-id".into(), "reused-provider-id".into()],
                )
                .unwrap(),
            durable_ids
        );
        let context = store
            .context_until_with_elisions(
                &store.load(&session.id).unwrap(),
                i64::MAX,
                &HashSet::from([durable_ids[1]]),
            )
            .unwrap();
        let outputs = context
            .iter()
            .filter_map(|message| match message {
                Message::Tool { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            outputs,
            ["output", crate::compaction::EMERGENCY_OUTPUT_UNAVAILABLE]
        );
    }

    #[test]
    fn continued_epoch_owns_its_tool_calls_and_resume_state() {
        let (store, session) = test_session();
        let turn = store
            .start_turn(&session.id, "/work", None, &"task".into())
            .unwrap();
        let exchange = store
            .start_test_provider_request(&session.id, &turn)
            .unwrap();
        store
            .complete_assistant_exchange(
                &session.id,
                &exchange,
                &Message::assistant(Some("partial".into()), None, None, None),
                None,
                None,
            )
            .unwrap();
        let compaction_turn = store
            .start_compaction_turn(
                &session.id,
                CompactionStart {
                    cwd: "/work",
                    prompt: &"checkpoint prompt".into(),
                    trigger: CompactionTrigger::Hard,
                    mode: CompactionMode::ContinueTurn,
                    before_context_tokens: 1_700,
                    before_context_window: Some(2_000),
                },
            )
            .unwrap();
        let summary_exchange = store
            .start_test_provider_request(&session.id, &compaction_turn)
            .unwrap();
        store
            .complete_assistant_exchange(
                &session.id,
                &summary_exchange,
                &Message::assistant(Some("summary".into()), None, None, None),
                None,
                None,
            )
            .unwrap();
        store
            .apply_compaction(
                &session.id,
                CompactionApplication {
                    after_context_tokens_estimate: 20,
                    after_context_window: Some(2_000),
                },
            )
            .unwrap();
        let continuation_turn = store.current_turn_id(&session.id).unwrap();

        let (_, call_ids) = store
            .append_message_with_bash_calls(
                &session.id,
                &Message::assistant(
                    None,
                    None,
                    Some(vec![ToolCall {
                        id: "continued-call".into(),
                        arguments: r#"{"title":"inspect","risk":"readonly","command":"true"}"#
                            .into(),
                    }]),
                    None,
                ),
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
                "ok",
                &[],
            )
            .unwrap();
        let bash_turn = store
            .audit_events(&session.id)
            .unwrap()
            .into_iter()
            .find(|event| event["type"] == "bash_completed" && event["call_id"] == call_ids[0])
            .unwrap();
        assert!(bash_turn.get("turn_id").is_none());
        assert_eq!(
            call_turn_id(&store.load(&session.id).unwrap(), call_ids[0]).as_deref(),
            Some(continuation_turn.as_str())
        );

        let resume_exchange = store
            .start_test_provider_request(&session.id, &continuation_turn)
            .unwrap();
        store
            .complete_resumable_assistant_exchange(
                &session.id,
                &resume_exchange,
                &Message::assistant(None, None, None, None),
                None,
                None,
            )
            .unwrap();
        assert!(store.resume_reminder_needed(&session.id).unwrap());
    }
}
