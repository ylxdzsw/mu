use std::fmt;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::config::{Config, EnvMap, LimitsConfig};
use crate::provider::ToolAttachment;
use crate::redaction::SecretRedactor;
use crate::renderer::Renderer;

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub output: String,
    pub exit_code: i32,
    pub attachments: Vec<ToolAttachment>,
}

#[derive(Debug)]
struct BashExecutionError {
    reason: String,
    partial_output: String,
    redacted: bool,
}

impl BashExecutionError {
    fn new(reason: String, partial_output: String, redacted: bool) -> Self {
        Self {
            reason,
            partial_output,
            redacted,
        }
    }
}

impl fmt::Display for BashExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for BashExecutionError {}

pub struct ToolContext<'a> {
    pub config: &'a Config,
    pub renderer: &'a mut Renderer,
    pub attachment_manifest: Option<&'a Path>,
    pub objects_dir: Option<&'a Path>,
    pub bash_call_id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Sequential,
    Concurrent,
}

// Bump ModelApi request formats when this definition changes.
pub fn tool_definitions() -> Vec<Value> {
    vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "bash",
            "description": description(),
            "parameters": parameters_schema(),
            "strict": false
        }
    })]
}

pub fn resolve_path(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

pub fn apply_truncation(
    output: String,
    limits: &LimitsConfig,
    prefix: &str,
    use_tail: bool,
) -> String {
    truncate_output(&output, limits, prefix, use_tail)
}

pub(crate) fn model_failure_output(error: &anyhow::Error, limits: &LimitsConfig) -> String {
    let Some(error) = error.downcast_ref::<BashExecutionError>() else {
        return format!("error: {error}");
    };
    let mut message = format!("error: {}", error.reason);
    let partial_output = error.partial_output.trim_end_matches('\n');
    if partial_output.is_empty() {
        return message;
    }
    message.push_str("\npartial output:\n");
    message.push_str(&apply_truncation(
        partial_output.to_string(),
        limits,
        "bash",
        true,
    ));
    if error.redacted {
        message.push_str("\n\n");
        message.push_str(REDACTION_REMINDER);
    }
    message
}

fn truncate_output(
    output: &str,
    limits: &LimitsConfig,
    spill_prefix: &str,
    use_tail: bool,
) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total_lines = lines.len();

    if total_lines <= limits.max_lines
        && output.len() <= limits.max_bytes
        && lines.iter().all(|line| line.len() <= limits.max_line_bytes)
    {
        return output.to_string();
    }

    let preview = if use_tail {
        build_tail_preview(
            &lines,
            limits.max_lines,
            limits.max_bytes,
            limits.max_line_bytes,
        )
    } else {
        build_head_preview(
            &lines,
            limits.max_lines,
            limits.max_bytes,
            limits.max_line_bytes,
        )
    };

    // The spill is best-effort: by this point the command has already run, so
    // an unavailable runtime directory or disk-full error must degrade to a
    // preview-only note, never fail the tool result.
    let spill_note = match write_spill(output, spill_prefix) {
        Ok(spill_path) => format!(
            "full output was written to temporary file {}; it may disappear at any time",
            spill_path.display()
        ),
        Err(error) => {
            format!("full output could not be saved ({error}); only this preview is available")
        }
    };

    let elided_lines = total_lines.saturating_sub(preview.lines().count());
    format!("{preview}\n[… {elided_lines} lines elided; {spill_note}]")
}

fn write_spill(output: &str, spill_prefix: &str) -> Result<PathBuf> {
    let directory = crate::paths::runtime_dir()?;
    let (mut file, spill_path) =
        crate::random::create_temp_file(&directory, &format!("spill-{spill_prefix}-"), ".txt")?;
    file.write_all(output.as_bytes())?;
    Ok(spill_path)
}

fn build_head_preview(
    lines: &[&str],
    max_lines: usize,
    max_bytes: usize,
    max_line_bytes: usize,
) -> String {
    let mut out = String::new();
    for (count, line) in lines.iter().enumerate() {
        if count >= max_lines {
            break;
        }
        let truncated_line = truncate_line(line, max_line_bytes);
        if out.len() + truncated_line.len() + 1 > max_bytes {
            break;
        }
        if count > 0 {
            out.push('\n');
        }
        out.push_str(&truncated_line);
    }
    out
}

fn build_tail_preview(
    lines: &[&str],
    max_lines: usize,
    max_bytes: usize,
    max_line_bytes: usize,
) -> String {
    let start = lines.len().saturating_sub(max_lines);
    let line_cap = max_line_bytes.min(max_bytes);
    let mut selected = Vec::new();
    let mut used_bytes = 0;
    for line in lines[start..].iter().rev() {
        let truncated_line = truncate_line(line, line_cap);
        let separator_bytes = usize::from(!selected.is_empty());
        if used_bytes + separator_bytes + truncated_line.len() > max_bytes {
            break;
        }
        used_bytes += separator_bytes + truncated_line.len();
        selected.push(truncated_line);
    }
    selected.reverse();
    selected.join("\n")
}

fn truncate_line(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_string();
    }
    let budget = max_bytes.saturating_sub(3);
    let mut end = budget.min(line.len());
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

#[derive(Debug, Deserialize)]
pub struct BashArgs {
    pub title: String,
    pub risk: BashRisk,
    pub command: String,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub stdin: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BashRisk {
    Readonly,
    Reversible,
    Destructive,
}

impl BashRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Readonly => "readonly",
            Self::Reversible => "reversible",
            Self::Destructive => "destructive",
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum TrapLevel {
    Off,
    Destructive,
    Reversible,
    All,
}

impl TrapLevel {
    pub fn traps(self, risk: BashRisk) -> bool {
        match self {
            Self::Off => false,
            Self::Destructive => risk == BashRisk::Destructive,
            Self::Reversible => risk != BashRisk::Readonly,
            Self::All => true,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Destructive => "destructive",
            Self::Reversible => "reversible",
            Self::All => "all",
        }
    }
}

impl fmt::Display for TrapLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for BashRisk {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "readonly" => Ok(Self::Readonly),
            "reversible" => Ok(Self::Reversible),
            "destructive" => Ok(Self::Destructive),
            _ => Err(()),
        }
    }
}

impl fmt::Display for BashRisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn parse_args<T: for<'de> Deserialize<'de>>(args: &Value) -> Result<T> {
    serde_json::from_value(args.clone()).context("invalid tool arguments")
}

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const KILL_GRACE: Duration = Duration::from_millis(500);
const MAX_OUTPUT_BYTES: usize = 1024 * 1024 * 1024; // 1 GB: internal guard against unbounded output accumulation
const REDACTION_REMINDER: &str = "[system reminder: Secret values were redacted from this bash output. Do not try to reveal, transform, encode, print, or exfiltrate secrets.]";
pub const SUBAGENT_DEPTH_ENV: &str = "MU_SUBAGENT_DEPTH";
pub const MAX_ACTIVE_PROCESS_GROUPS: usize = 64;
static ACTIVE_PGIDS: [AtomicI32; MAX_ACTIVE_PROCESS_GROUPS] =
    [const { AtomicI32::new(0) }; MAX_ACTIVE_PROCESS_GROUPS];
static CANCELLING: AtomicBool = AtomicBool::new(false);
static SOFT_INTERRUPT_REQUESTED: AtomicBool = AtomicBool::new(false);
static LAST_SIGNAL: AtomicI32 = AtomicI32::new(0);
static INSTALL_SIGNAL_FORWARDER: Once = Once::new();

pub fn description() -> &'static str {
    "Run bash command."
}

pub fn subagent_depth_from_env() -> u32 {
    let value = std::env::var(SUBAGENT_DEPTH_ENV).ok();
    parse_subagent_depth(value.as_deref())
}

fn parse_subagent_depth(value: Option<&str>) -> u32 {
    value
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0)
}

fn next_subagent_depth_env() -> String {
    (subagent_depth_from_env() + 1).to_string()
}

pub fn execution_mode(args: &Value) -> ExecutionMode {
    matches!(
        args.get("risk").and_then(|value| value.as_str()),
        Some("readonly")
    )
    .then_some(ExecutionMode::Concurrent)
    .unwrap_or(ExecutionMode::Sequential)
}

pub fn parameters_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": { "type": "string", "description": "Short human-readable title for the action" },
            "risk": {
                "type": "string",
                "enum": ["readonly", "reversible", "destructive"],
                "description": "Advisory risk label for UI and auditing. Choose by how reliably and easily the command's effects can be undone, using the highest risk of any part: readonly only when no persistent local or remote state changes; reversible for bounded changes with a known, practical way to restore the prior state; destructive when unique state could be lost, rollback is uncertain, or reversal would be unusually broad or costly. Judge intended material effects; incidental traces of ordinary reads, such as access logs and API usage, do not make them state-changing. Consider the actual target and context rather than the command name, and choose the higher risk when uncertain about recoverability."
            },
            "command": { "type": "string", "description": "Command to run with bash -lc; can be multiline" },
            "cwd": { "type": "string", "description": "Working directory for this invocation; Prefer absolute path; Prefer this argument over `cd`" },
            "timeout": { "type": "integer", "minimum": 1, "description": "Timeout in seconds (default 120)" },
            "stdin": { "type": "string", "description": "Literal stdin bytes to pipe to the command; omit unless the command needs non-empty piped input; prefer this argument over long heredoc" }
        },
        "required": ["title", "risk", "command"],
        "additionalProperties": false
    })
}

pub async fn execute(args: Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    let args: BashArgs = parse_args(&args)?;
    let _ = (&args.title, args.risk);
    let timeout = args.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
    if timeout == 0 {
        bail!("timeout must be greater than 0");
    }

    let redactor = SecretRedactor::from_config(ctx.config)?;
    for warning in redactor.warnings() {
        ctx.renderer.notice(&format!("[redaction] {warning}"))?;
    }

    let attachment_context =
        ctx.attachment_manifest
            .zip(ctx.objects_dir)
            .map(|(manifest, objects_dir)| AttachmentContext {
                manifest: manifest.to_path_buf(),
                objects_dir: objects_dir.to_path_buf(),
                bash_call_id: ctx.bash_call_id,
            });
    let result = run_bash(
        args,
        timeout,
        ctx.renderer,
        &ctx.config.env,
        redactor,
        attachment_context.as_ref(),
    )?;
    let exit_code = result.exit_code;
    let attachments = result.attachments;

    let output = if result.redacted {
        format!("{}\n\n{}", result.output, REDACTION_REMINDER)
    } else {
        result.output
    };
    let full = format!("{}\n[exit code: {}]", output, exit_code);
    Ok(ToolResult {
        output: apply_truncation(full, &ctx.config.limits, "bash", true),
        exit_code,
        attachments,
    })
}

#[derive(Debug)]
struct BashRunResult {
    output: String,
    exit_code: i32,
    redacted: bool,
    attachments: Vec<ToolAttachment>,
}

#[derive(Default)]
struct SharedBashState {
    output: Mutex<String>,
    finished: AtomicBool,
}

impl SharedBashState {
    fn push_output(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Ok(mut output) = self.output.lock() {
            output.push_str(text);
        }
    }

    fn snapshot_output(&self) -> String {
        self.output
            .lock()
            .map(|output| output.clone())
            .unwrap_or_default()
    }

    fn mark_finished(&self) {
        self.finished.store(true, Ordering::SeqCst);
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
}

pub struct RunningBash {
    warnings: Vec<String>,
    shared: Arc<SharedBashState>,
    task: JoinHandle<(Result<ToolResult>, Duration)>,
}

impl RunningBash {
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn snapshot_output(&self) -> String {
        self.shared.snapshot_output()
    }

    pub fn is_finished(&self) -> bool {
        self.shared.is_finished()
    }

    pub async fn finish(self) -> (Result<ToolResult>, Duration, String) {
        let RunningBash {
            warnings: _,
            shared,
            task,
        } = self;
        let final_output = shared.snapshot_output();
        match task.await {
            Ok((result, elapsed)) => (result, elapsed, shared.snapshot_output()),
            Err(error) => (
                Err(anyhow::anyhow!("bash worker failed: {error}")),
                Duration::ZERO,
                final_output,
            ),
        }
    }
}

trait BashOutputTarget {
    fn push_output(&mut self, text: &str) -> Result<()>;
}

impl BashOutputTarget for Renderer {
    fn push_output(&mut self, text: &str) -> Result<()> {
        self.bash_output(text).map_err(Into::into)
    }
}

struct BufferedBashTarget {
    shared: Arc<SharedBashState>,
}

impl BufferedBashTarget {
    fn new(shared: Arc<SharedBashState>) -> Self {
        Self { shared }
    }
}

impl BashOutputTarget for BufferedBashTarget {
    fn push_output(&mut self, text: &str) -> Result<()> {
        self.shared.push_output(text);
        Ok(())
    }
}

pub fn start_bash_task(
    args: BashArgs,
    config: &Config,
    attachment_manifest: Option<&Path>,
    objects_dir: Option<&Path>,
    bash_call_id: i64,
) -> Result<RunningBash> {
    let redactor = SecretRedactor::from_config(config)?;
    let warnings = redactor.warnings().to_vec();
    let config = config.clone();
    let attachment_context = attachment_manifest
        .zip(objects_dir)
        .map(|(manifest, objects_dir)| AttachmentContext {
            manifest: manifest.to_path_buf(),
            objects_dir: objects_dir.to_path_buf(),
            bash_call_id,
        });
    let shared = Arc::new(SharedBashState::default());
    let shared_for_task = Arc::clone(&shared);
    let task = tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let result = execute_bash_task(
            args,
            &config,
            Arc::clone(&shared_for_task),
            redactor,
            attachment_context.as_ref(),
        );
        shared_for_task.mark_finished();
        (result, started.elapsed())
    });
    Ok(RunningBash {
        warnings,
        shared,
        task,
    })
}

fn run_bash(
    args: BashArgs,
    timeout_secs: u64,
    renderer: &mut Renderer,
    env: &EnvMap,
    mut redactor: SecretRedactor,
    attachment_context: Option<&AttachmentContext>,
) -> Result<BashRunResult> {
    run_bash_inner(
        args,
        timeout_secs,
        renderer,
        env,
        &mut redactor,
        attachment_context,
    )
}

#[derive(Debug, Clone)]
struct AttachmentContext {
    manifest: PathBuf,
    objects_dir: PathBuf,
    bash_call_id: i64,
}

fn execute_bash_task(
    args: BashArgs,
    config: &Config,
    shared: Arc<SharedBashState>,
    mut redactor: SecretRedactor,
    attachment_context: Option<&AttachmentContext>,
) -> Result<ToolResult> {
    let timeout = args.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
    if timeout == 0 {
        bail!("timeout must be greater than 0");
    }

    let mut target = BufferedBashTarget::new(shared);
    let result = run_bash_inner(
        args,
        timeout,
        &mut target,
        &config.env,
        &mut redactor,
        attachment_context,
    )?;
    let exit_code = result.exit_code;
    let attachments = result.attachments;
    let output = if result.redacted {
        format!("{}\n\n{}", result.output, REDACTION_REMINDER)
    } else {
        result.output
    };
    let full = format!("{output}\n[exit code: {exit_code}]");
    Ok(ToolResult {
        output: apply_truncation(full, &config.limits, "bash", true),
        exit_code,
        attachments,
    })
}

fn run_bash_inner(
    args: BashArgs,
    timeout_secs: u64,
    target: &mut impl BashOutputTarget,
    env: &EnvMap,
    redactor: &mut SecretRedactor,
    attachment_context: Option<&AttachmentContext>,
) -> Result<BashRunResult> {
    let cwd = args
        .cwd
        .as_deref()
        .map(resolve_path)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let cwd_metadata = std::fs::metadata(&cwd).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::anyhow!("working directory does not exist: {}", cwd.display())
        } else {
            anyhow::anyhow!(error).context(format!("accessing working directory {}", cwd.display()))
        }
    })?;
    if !cwd_metadata.is_dir() {
        bail!("working directory is not a directory: {}", cwd.display());
    }
    let applets = crate::paths::applets_dir()?;
    let command_text = format!(
        "export PATH={}:$PATH\nexec 2>&1\n{}",
        shell_quote(&applets.to_string_lossy()),
        args.command
    );

    let mut command = Command::new("bash");
    command
        .arg("-lc")
        .arg(command_text)
        .current_dir(&cwd)
        .envs(env)
        .env(SUBAGENT_DEPTH_ENV, next_subagent_depth_env())
        .stdin(if args.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(attachment_context) = attachment_context {
        command
            .env(
                crate::store::ATTACHMENT_MANIFEST_ENV,
                &attachment_context.manifest,
            )
            .env(
                crate::store::BASH_CALL_ID_ENV,
                attachment_context.bash_call_id.to_string(),
            )
            .env(
                crate::store::OBJECTS_DIR_ENV,
                &attachment_context.objects_dir,
            );
    }
    configure_process_group(&mut command);

    if cancellation_requested() {
        return Err(BashExecutionError::new(
            format!("command interrupted by {}", signal_name(last_signal())),
            String::new(),
            redactor.did_redact(),
        )
        .into());
    }

    let mut child = command.spawn().map_err(|error| {
        if is_e2big(&error) {
            anyhow::anyhow!("command is too large to execute: OS reported argument list too long")
        } else {
            anyhow::anyhow!(error).context("spawning bash")
        }
    })?;
    let child_id = child.id();
    let _active = ActiveProcessGroup::new(child_id);

    if let Some(stdin) = args.stdin {
        let mut child_stdin = child.stdin.take().context("taking bash stdin")?;
        std::thread::spawn(move || {
            let _ = child_stdin.write_all(stdin.as_bytes());
        });
    }

    let stdout = child.stdout.take().context("taking bash stdout")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut stdout = stdout;
        loop {
            let mut buf = [0u8; 4096];
            match std::io::Read::read(&mut stdout, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut output = String::new();
    let mut status: Option<ExitStatus> = None;
    let mut stdout_closed = false;
    let mut interrupted = false;
    let mut terminal_error: Option<BashExecutionError> = None;

    loop {
        if cancellation_requested() {
            interrupted = true;
            terminate_child_group(child_id, &mut child);
            drain_available(&rx, target, &mut output, redactor)?;
            flush_redactor(target, &mut output, redactor)?;
            let _ = child.wait();
            terminal_error = Some(BashExecutionError::new(
                format!("command interrupted by {}", signal_name(last_signal())),
                std::mem::take(&mut output),
                redactor.did_redact(),
            ));
            break;
        }

        if Instant::now() >= deadline {
            terminate_child_group(child_id, &mut child);
            drain_available(&rx, target, &mut output, redactor)?;
            flush_redactor(target, &mut output, redactor)?;
            let _ = child.wait();
            terminal_error = Some(BashExecutionError::new(
                format!("command timed out after {timeout_secs}s"),
                std::mem::take(&mut output),
                redactor.did_redact(),
            ));
            break;
        }

        if status.is_none() {
            status = child.try_wait().context("waiting for bash")?;
        }

        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(bytes) => {
                let redacted = redactor.redact_chunk(&bytes);
                output.push_str(&redacted);
                target.push_output(&redacted)?;
                if output.len() > MAX_OUTPUT_BYTES {
                    terminate_child_group(child_id, &mut child);
                    drain_available(&rx, target, &mut output, redactor)?;
                    flush_redactor(target, &mut output, redactor)?;
                    let _ = child.wait();
                    terminal_error = Some(BashExecutionError::new(
                        format!(
                            "command killed: output exceeded {} MB limit",
                            MAX_OUTPUT_BYTES / (1024 * 1024)
                        ),
                        std::mem::take(&mut output),
                        redactor.did_redact(),
                    ));
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                stdout_closed = true;
            }
        }

        if status.is_some() && stdout_closed {
            break;
        }
    }

    let status = status.unwrap_or_else(|| child.wait().expect("bash status"));
    flush_redactor(target, &mut output, redactor)?;
    if let Some(error) = terminal_error {
        return Err(error.into());
    }
    if interrupted || (cancellation_requested() && status.signal().is_some()) {
        return Err(BashExecutionError::new(
            format!("command interrupted by {}", signal_name(last_signal())),
            output,
            redactor.did_redact(),
        )
        .into());
    }
    Ok(BashRunResult {
        output: output.trim_end_matches('\n').to_string(),
        exit_code: status.code().unwrap_or(1),
        redacted: redactor.did_redact(),
        attachments: attachment_context.map_or_else(
            || Ok(Vec::new()),
            |context| {
                crate::store::read_bash_attachments(
                    &context.manifest,
                    &context.objects_dir,
                    context.bash_call_id,
                )
            },
        )?,
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn drain_available(
    rx: &mpsc::Receiver<Vec<u8>>,
    target: &mut impl BashOutputTarget,
    output: &mut String,
    redactor: &mut SecretRedactor,
) -> Result<()> {
    while let Ok(bytes) = rx.try_recv() {
        let redacted = redactor.redact_chunk(&bytes);
        output.push_str(&redacted);
        target.push_output(&redacted)?;
    }
    Ok(())
}

fn flush_redactor(
    target: &mut impl BashOutputTarget,
    output: &mut String,
    redactor: &mut SecretRedactor,
) -> Result<()> {
    let redacted = redactor.finish();
    output.push_str(&redacted);
    target.push_output(&redacted)?;
    Ok(())
}

fn is_e2big(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::E2BIG)
}

pub fn install_signal_forwarder(soft_interrupt: bool) {
    INSTALL_SIGNAL_FORWARDER.call_once(|| unsafe {
        libc::signal(libc::SIGINT, forward_signal as *const () as usize);
        libc::signal(libc::SIGTERM, forward_signal as *const () as usize);
    });
    if soft_interrupt {
        unsafe {
            libc::signal(libc::SIGQUIT, forward_signal as *const () as usize);
        }
    }
}

extern "C" fn forward_signal(signal: i32) {
    if signal == libc::SIGQUIT {
        SOFT_INTERRUPT_REQUESTED.store(true, Ordering::SeqCst);
        return;
    }
    LAST_SIGNAL.store(signal, Ordering::SeqCst);
    let already_cancelling = CANCELLING.swap(true, Ordering::SeqCst);
    for pgid in &ACTIVE_PGIDS {
        let pgid = pgid.load(Ordering::SeqCst);
        if pgid > 0 {
            unsafe {
                libc::kill(-pgid, signal);
            }
        }
    }
    if already_cancelling || !has_active_process_groups() {
        unsafe {
            libc::_exit(128 + signal);
        }
    }
}

pub fn reset_cancellation_state() {
    CANCELLING.store(false, Ordering::SeqCst);
    SOFT_INTERRUPT_REQUESTED.store(false, Ordering::SeqCst);
    LAST_SIGNAL.store(0, Ordering::SeqCst);
}

pub fn soft_interrupt_requested() -> bool {
    SOFT_INTERRUPT_REQUESTED.load(Ordering::SeqCst)
}

pub fn cancellation_requested() -> bool {
    CANCELLING.load(Ordering::SeqCst)
}

/// If a terminating signal was forwarded during this turn, return its number so
/// the process can exit with the shell-conventional `128 + signal` status
/// (e.g. `130` for SIGINT). Returns `None` when no cancellation occurred.
pub fn cancellation_signal() -> Option<i32> {
    if !cancellation_requested() {
        return None;
    }
    let signal = LAST_SIGNAL.load(Ordering::SeqCst);
    Some(if signal > 0 { signal } else { libc::SIGINT })
}

fn last_signal() -> i32 {
    LAST_SIGNAL.load(Ordering::SeqCst)
}

fn has_active_process_groups() -> bool {
    ACTIVE_PGIDS
        .iter()
        .any(|pgid| pgid.load(Ordering::SeqCst) > 0)
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGINT => "SIGINT",
        libc::SIGTERM => "SIGTERM",
        _ => "signal",
    }
}

struct ActiveProcessGroup {
    slot: Option<usize>,
}

impl ActiveProcessGroup {
    fn new(child_id: u32) -> Self {
        Self {
            slot: set_active_process_group(child_id),
        }
    }
}

impl Drop for ActiveProcessGroup {
    fn drop(&mut self) {
        clear_active_process_group(self.slot);
    }
}

fn set_active_process_group(child_id: u32) -> Option<usize> {
    let pgid = child_id.cast_signed();
    for (idx, slot) in ACTIVE_PGIDS.iter().enumerate() {
        if slot
            .compare_exchange(0, pgid, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Some(idx);
        }
    }
    None
}

fn clear_active_process_group(slot: Option<usize>) {
    if let Some(slot) = slot {
        ACTIVE_PGIDS[slot].store(0, Ordering::SeqCst);
    }
}

fn configure_process_group(command: &mut Command) {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            }
            Ok(())
        });
    }
}

fn terminate_child_group(child_id: u32, child: &mut std::process::Child) {
    let pgid = -child_id.cast_signed();
    unsafe {
        if libc::kill(pgid, libc::SIGTERM) != 0 {
            let _ = child.kill();
        }
    }
    let _ = wait_for_exit(child, KILL_GRACE);
    unsafe {
        if libc::kill(pgid, libc::SIGKILL) != 0 {
            let _ = child.kill();
        }
    }
}

fn wait_for_exit(child: &mut std::process::Child, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use super::{
        AttachmentContext, BashArgs, BashExecutionError, BashRisk, REDACTION_REMINDER, ToolContext,
        apply_truncation, model_failure_output, run_bash,
    };
    use crate::config::EnvMap;
    use crate::config::{CompactionConfig, Config, LimitsConfig, ProviderConfig, RedactionConfig};
    use crate::redaction::SecretRedactor;
    use crate::renderer::Renderer;

    #[test]
    fn trap_levels_match_declared_risk_thresholds() {
        assert!(!crate::bash::TrapLevel::Off.traps(BashRisk::Destructive));
        assert!(crate::bash::TrapLevel::Destructive.traps(BashRisk::Destructive));
        assert!(!crate::bash::TrapLevel::Destructive.traps(BashRisk::Reversible));
        assert!(crate::bash::TrapLevel::Reversible.traps(BashRisk::Reversible));
        assert!(crate::bash::TrapLevel::Reversible.traps(BashRisk::Destructive));
        assert!(!crate::bash::TrapLevel::Reversible.traps(BashRisk::Readonly));
        assert!(crate::bash::TrapLevel::All.traps(BashRisk::Readonly));
    }

    fn args(command: &str) -> BashArgs {
        BashArgs {
            title: "test".into(),
            risk: BashRisk::Readonly,
            command: command.into(),
            timeout: None,
            cwd: None,
            stdin: None,
        }
    }

    fn empty_env() -> EnvMap {
        EnvMap::new()
    }

    fn process_is_running(pid: i32) -> bool {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }

        #[cfg(target_os = "linux")]
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && stat
                .rsplit_once(") ")
                .is_some_and(|(_, status)| status.starts_with('Z'))
        {
            return false;
        }

        true
    }

    fn test_config(env: &[(&str, &str)], redaction_env: &[&str]) -> Config {
        Config {
            providers: crate::config::OrderedMap::from_iter([(
                "test".into(),
                ProviderConfig {
                    endpoint: "https://example.test/chat/completions".into(),
                    api_key_env: "OPENAI_API_KEY".into(),
                    models: crate::config::OrderedMap::default(),
                },
            )]),
            output: Default::default(),
            trap: crate::bash::TrapLevel::Off,
            auto_resume: false,
            soft_interrupt: crate::config::bundled_test_default("/soft_interrupt"),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            terminal_bell: crate::config::TerminalBellConfig::default(),
            redaction: RedactionConfig {
                env: redaction_env.iter().map(|name| name.to_string()).collect(),
            },
            env: env
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
        }
    }

    #[test]
    fn cwd_and_environment_do_not_persist_between_calls() {
        let tmp = crate::random::create_temp_dir(&std::env::temp_dir(), "mu-bash-").unwrap();
        let mut renderer = Renderer::new();

        let mut first = args("cd / && export MU_TEST=works && pwd");
        first.cwd = Some(tmp.display().to_string());
        let first_result = run_bash(
            first,
            5,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
            None,
        )
        .unwrap();
        assert_eq!(first_result.exit_code, 0);
        assert_eq!(first_result.output, "/");

        let mut second = args("printf '%s|%s' \"$PWD\" \"${MU_TEST-unset}\"");
        second.cwd = Some(tmp.display().to_string());
        let second_result = run_bash(
            second,
            5,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
            None,
        )
        .unwrap();
        assert_eq!(second_result.exit_code, 0);
        assert_eq!(second_result.output, format!("{}|unset", tmp.display()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn bash_overrides_configured_subagent_depth_for_child_process() {
        let mut renderer = Renderer::new();
        let mut env = EnvMap::new();
        env.insert(super::SUBAGENT_DEPTH_ENV.into(), "99".into());
        let expected = (super::subagent_depth_from_env() + 1).to_string();

        let result = run_bash(
            args("printf '%s' \"$MU_SUBAGENT_DEPTH\""),
            5,
            &mut renderer,
            &env,
            SecretRedactor::default(),
            None,
        )
        .unwrap();

        assert_eq!(result.output, expected);
    }

    #[test]
    fn bash_prepends_mu_libexec_after_login_initialization() {
        let mut renderer = Renderer::new();
        let result = run_bash(
            args("printf '%s' \"$PATH\""),
            5,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
            None,
        )
        .unwrap();
        let applets = crate::paths::applets_dir().unwrap();
        assert!(
            result
                .output
                .starts_with(&format!("{}:", applets.display()))
        );
    }

    #[test]
    fn bash_attachment_context_exports_manifest_call_and_object_paths() {
        let mut renderer = Renderer::new();
        let context = AttachmentContext {
            manifest: PathBuf::from("/tmp/attachments.jsonl"),
            objects_dir: PathBuf::from("/tmp/objects"),
            bash_call_id: 42,
        };
        let result = run_bash(
            args("printf '%s|%s|%s' \"$MU_ATTACHMENT_MANIFEST\" \"$MU_BASH_CALL_ID\" \"$MU_OBJECTS_DIR\""),
            5,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
            Some(&context),
        )
        .unwrap();
        assert_eq!(result.output, "/tmp/attachments.jsonl|42|/tmp/objects");
    }

    #[tokio::test]
    async fn bash_receives_env_and_redacts_configured_values() {
        let mut renderer = Renderer::new();
        let config = test_config(
            &[
                ("OPENAI_API_KEY", "provider-secret"),
                ("CUSTOM_SECRET", "tiny"),
            ],
            &["*SECRET"],
        );
        let mut ctx = ToolContext {
            config: &config,
            renderer: &mut renderer,
            attachment_manifest: None,
            objects_dir: None,
            bash_call_id: 0,
        };
        let args = serde_json::json!({
            "title": "redact",
            "risk": "readonly",
            "command": "printf '%s|%s' \"$OPENAI_API_KEY\" \"$CUSTOM_SECRET\""
        });

        let result = super::execute(args, &mut ctx).await.unwrap();

        assert!(result.output.contains("[redacted:OPENAI_API_KEY]"));
        assert!(result.output.contains("[redacted:CUSTOM_SECRET]"));
        assert!(!result.output.contains("provider-secret"));
        assert!(!result.output.contains("tiny"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn redirected_setsid_command_detaches_with_pid_as_sid() {
        let tmp = crate::random::create_temp_dir(&std::env::temp_dir(), "mu-bg-test-").unwrap();
        let log = tmp.join("output");
        let command = format!(
            "setsid sleep 10 </dev/null >{} 2>&1 & pid=$!; sleep 0.05; sid=$(ps -o sid= -p \"$pid\"); printf '%s %s' \"$pid\" \"$sid\"",
            log.display()
        );
        let mut renderer = Renderer::new();
        let started = std::time::Instant::now();
        let result = run_bash(
            args(&command),
            5,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
            None,
        )
        .unwrap();
        let ids = result
            .output
            .split_whitespace()
            .map(|value| value.parse::<i32>().unwrap())
            .collect::<Vec<_>>();
        if let Some(pid) = ids.first() {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_dir_all(tmp);

        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ids[1]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn redirected_setsid_command_can_read_tool_stdin() {
        let tmp = crate::random::create_temp_dir(&std::env::temp_dir(), "mu-bg-stdin-").unwrap();
        let output = tmp.join("output");
        let command = format!(
            "setsid sh -c 'cat >\"$1\"' sh {} <&0 >/dev/null 2>&1 &",
            output.display()
        );
        let mut input = args(&command);
        input.stdin = Some("delegated prompt\n".into());
        let mut renderer = Renderer::new();
        run_bash(
            input,
            5,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
            None,
        )
        .unwrap();

        let expected = "delegated prompt\n";
        let contents = (0..40).find_map(|_| {
            let contents = std::fs::read_to_string(&output).ok();
            if contents.as_deref() == Some(expected) {
                return contents;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
            None
        });
        let _ = std::fs::remove_dir_all(tmp);
        assert_eq!(contents.as_deref(), Some(expected));
    }

    #[test]
    fn timeout_kills_background_descendants() {
        let tmp =
            crate::random::create_temp_dir(&std::env::temp_dir(), "mu-bash-descendant-").unwrap();
        let marker = tmp.join("marker");
        let script = format!("sleep 20 & echo $! > {}; sleep 20", marker.display());
        let mut renderer = Renderer::new();
        let result = run_bash(
            args(&script),
            3,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
            None,
        );
        assert!(result.is_err(), "expected timeout");

        let pid_text = (0..20)
            .find_map(|_| {
                std::fs::read_to_string(&marker).ok().or_else(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    None
                })
            })
            .expect("background process marker should be written before timeout");
        let pid: i32 = pid_text.trim().parse().unwrap();
        let stopped = (0..40).any(|_| {
            if !process_is_running(pid) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            false
        });
        assert!(stopped, "background sleep {pid} survived timeout");
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn bash_schema_requires_title_risk_and_command() {
        let schema = super::parameters_schema();
        assert_eq!(schema["required"], json!(["title", "risk", "command"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["risk"]["enum"],
            json!(["readonly", "reversible", "destructive"])
        );
        assert!(schema["properties"].get("command").is_some());
        assert!(schema["properties"].get("script").is_none());
        assert!(schema["properties"].get("workdir").is_none());
        assert!(schema["properties"].get("cwd").is_some());
    }

    fn tight_limits() -> LimitsConfig {
        LimitsConfig {
            max_lines: 2,
            max_bytes: 10_000,
            max_line_bytes: 10_000,
        }
    }

    fn spill_path(output: &str) -> PathBuf {
        let runtime = crate::paths::runtime_dir().unwrap();
        output
            .split_whitespace()
            .map(|word| PathBuf::from(word.trim_end_matches(';')))
            .find(|path| path.starts_with(&runtime))
            .expect("spill path")
    }

    #[test]
    fn truncation_spills_full_output_to_the_runtime_directory() {
        let clamped = apply_truncation(
            "one\ntwo\nthree\nfour".into(),
            &tight_limits(),
            "bash",
            true,
        );

        assert_ne!(clamped, "one\ntwo\nthree\nfour");
        let spill = spill_path(&clamped);
        assert_eq!(
            spill.parent().unwrap(),
            crate::paths::runtime_dir().unwrap()
        );
        let _ = std::fs::remove_file(spill);
    }

    #[test]
    fn model_failure_keeps_reason_and_bounds_partial_output() {
        let error = anyhow::Error::new(BashExecutionError::new(
            "command timed out after 120s".into(),
            "one\ntwo\nthree\nfour".into(),
            true,
        ));
        let output = model_failure_output(&error, &tight_limits());

        assert!(output.contains("command timed out after 120s"));
        assert!(!output.contains("\none\n"));
        assert!(output.contains("three\nfour"));
        assert!(output.ends_with(REDACTION_REMINDER));

        let spill = spill_path(&output);
        assert_eq!(
            std::fs::read_to_string(&spill).unwrap(),
            "one\ntwo\nthree\nfour"
        );
        let _ = std::fs::remove_file(spill);
    }
}
