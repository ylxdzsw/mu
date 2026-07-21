use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
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
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::artifact::{ARTIFACT_FD, ARTIFACT_FD_ENV, read_artifacts};
use crate::config::Config;
use crate::config::EnvMap;
use crate::provider::ToolArtifact;
use crate::redaction::SecretRedactor;
use crate::renderer::Renderer;

use crate::tools::{
    BashArgs, ExecutionMode, ToolContext, ToolResult, apply_truncation, parse_args, resolve_path,
};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const KILL_GRACE: Duration = Duration::from_millis(500);
const MAX_OUTPUT_BYTES: usize = 1024 * 1024 * 1024; // 1 GB: internal guard against unbounded output accumulation
const REDACTION_REMINDER: &str = "[system reminder: Secret values were redacted from this bash output. Do not try to reveal, transform, encode, print, or exfiltrate secrets.]";
pub const SUBAGENT_DEPTH_ENV: &str = "MU_SUBAGENT_DEPTH";
pub const MAX_ACTIVE_PROCESS_GROUPS: usize = 64;
static ACTIVE_PGIDS: [AtomicI32; MAX_ACTIVE_PROCESS_GROUPS] =
    [const { AtomicI32::new(0) }; MAX_ACTIVE_PROCESS_GROUPS];
static CANCELLING: AtomicBool = AtomicBool::new(false);
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
                "description": "Advisory risk label for UI and auditing"
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

    let result = run_bash(args, timeout, ctx.renderer, &ctx.config.env, redactor)?;
    let exit_code = result.exit_code;
    let artifacts = result.artifacts;

    let output = if result.redacted {
        format!("{}\n\n{}", result.output, REDACTION_REMINDER)
    } else {
        result.output
    };
    let full = format!("{}\n[exit code: {}]", output, exit_code);
    Ok(ToolResult {
        output: apply_truncation(full, &ctx.config.limits, "bash", ctx.state_dir, true)?,
        exit_code,
        artifacts,
    })
}

#[derive(Debug)]
struct BashRunResult {
    output: String,
    exit_code: i32,
    redacted: bool,
    artifacts: Vec<ToolArtifact>,
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
        self.bash_output(None, "bash", text).map_err(Into::into)
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

pub fn start_bash_task(args: BashArgs, config: &Config, state_dir: &Path) -> Result<RunningBash> {
    let redactor = SecretRedactor::from_config(config)?;
    let warnings = redactor.warnings().to_vec();
    let config = config.clone();
    let state_dir = state_dir.to_path_buf();
    let shared = Arc::new(SharedBashState::default());
    let shared_for_task = Arc::clone(&shared);
    let task = tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let result = execute_bash_task(
            args,
            &config,
            &state_dir,
            Arc::clone(&shared_for_task),
            redactor,
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
) -> Result<BashRunResult> {
    run_bash_inner(args, timeout_secs, renderer, env, &mut redactor)
}

fn execute_bash_task(
    args: BashArgs,
    config: &Config,
    state_dir: &Path,
    shared: Arc<SharedBashState>,
    mut redactor: SecretRedactor,
) -> Result<ToolResult> {
    let timeout = args.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
    if timeout == 0 {
        bail!("timeout must be greater than 0");
    }

    let mut target = BufferedBashTarget::new(shared);
    let result = run_bash_inner(args, timeout, &mut target, &config.env, &mut redactor)?;
    let exit_code = result.exit_code;
    let artifacts = result.artifacts;
    let output = if result.redacted {
        format!("{}\n\n{}", result.output, REDACTION_REMINDER)
    } else {
        result.output
    };
    let full = format!("{output}\n[exit code: {exit_code}]");
    Ok(ToolResult {
        output: apply_truncation(full, &config.limits, "bash", state_dir, true)?,
        exit_code,
        artifacts,
    })
}

fn run_bash_inner(
    args: BashArgs,
    timeout_secs: u64,
    target: &mut impl BashOutputTarget,
    env: &EnvMap,
    redactor: &mut SecretRedactor,
) -> Result<BashRunResult> {
    install_signal_forwarder();
    let cwd = args
        .cwd
        .as_deref()
        .map(resolve_path)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let command_text = format!(
        "export PATH=/usr/libexec/mu:$PATH\nexec 2>&1\n{}",
        args.command
    );

    let (artifact_reader, artifact_writer) =
        UnixStream::pair().context("creating Mu artifact channel")?;
    let artifact_writer_fd = artifact_writer.as_raw_fd();

    let mut command = Command::new("bash");
    command
        .arg("-lc")
        .arg(command_text)
        .current_dir(&cwd)
        .envs(env)
        .env(ARTIFACT_FD_ENV, ARTIFACT_FD.to_string())
        .env(SUBAGENT_DEPTH_ENV, next_subagent_depth_env())
        .stdin(if args.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure_process_group(&mut command);
    configure_artifact_fd(&mut command, artifact_writer_fd);

    let mut child = command.spawn().map_err(|error| {
        if is_e2big(&error) {
            anyhow::anyhow!("command is too large to execute: OS reported argument list too long")
        } else {
            anyhow::anyhow!(error).context("spawning bash")
        }
    })?;
    drop(artifact_writer);
    let artifact_task = std::thread::spawn(move || read_artifacts(artifact_reader));
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
    let mut terminal_error: Option<anyhow::Error> = None;

    loop {
        if cancellation_requested() {
            interrupted = true;
            terminate_child_group(child_id, &mut child);
            drain_available(&rx, target, &mut output, redactor)?;
            flush_redactor(target, &mut output, redactor)?;
            let _ = child.wait();
            let reminder = if redactor.did_redact() {
                format!("\n\n{REDACTION_REMINDER}")
            } else {
                String::new()
            };
            terminal_error = Some(anyhow::anyhow!(
                "command interrupted by {}{}{}",
                signal_name(last_signal()),
                partial_output_suffix(&output),
                reminder
            ));
            break;
        }

        if Instant::now() >= deadline {
            terminate_child_group(child_id, &mut child);
            drain_available(&rx, target, &mut output, redactor)?;
            flush_redactor(target, &mut output, redactor)?;
            let _ = child.wait();
            let reminder = if redactor.did_redact() {
                format!("\n\n{REDACTION_REMINDER}")
            } else {
                String::new()
            };
            terminal_error = Some(anyhow::anyhow!(
                "command timed out after {timeout_secs}s{}{}",
                partial_output_suffix(&output),
                reminder
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
                    let reminder = if redactor.did_redact() {
                        format!("\n\n{REDACTION_REMINDER}")
                    } else {
                        String::new()
                    };
                    terminal_error = Some(anyhow::anyhow!(
                        "command killed: output exceeded {} MB limit{}{}",
                        MAX_OUTPUT_BYTES / (1024 * 1024),
                        partial_output_suffix(&output),
                        reminder
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
    let artifacts = artifact_task
        .join()
        .map_err(|_| anyhow::anyhow!("Mu artifact reader panicked"))??;
    if let Some(error) = terminal_error {
        return Err(error);
    }
    if interrupted || (cancellation_requested() && status.signal().is_some()) {
        let reminder = if redactor.did_redact() {
            format!("\n\n{REDACTION_REMINDER}")
        } else {
            String::new()
        };
        bail!(
            "command interrupted by {}{}{}",
            signal_name(last_signal()),
            partial_output_suffix(&output),
            reminder
        );
    }
    Ok(BashRunResult {
        output: output.trim_end_matches('\n').to_string(),
        exit_code: status.code().unwrap_or(1),
        redacted: redactor.did_redact(),
        artifacts,
    })
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

fn partial_output_suffix(output: &str) -> String {
    let output = output.trim_end_matches('\n');
    if output.is_empty() {
        String::new()
    } else {
        format!("; partial output:\n{output}")
    }
}

fn is_e2big(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(libc::E2BIG)
}

pub fn install_signal_forwarder() {
    INSTALL_SIGNAL_FORWARDER.call_once(|| unsafe {
        libc::signal(libc::SIGINT, forward_signal as *const () as usize);
        libc::signal(libc::SIGTERM, forward_signal as *const () as usize);
    });
}

extern "C" fn forward_signal(signal: i32) {
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
    LAST_SIGNAL.store(0, Ordering::SeqCst);
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
    let pgid = child_id as i32;
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

fn configure_artifact_fd(command: &mut Command, source_fd: i32) {
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(source_fd, ARTIFACT_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(ARTIFACT_FD, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

fn terminate_child_group(child_id: u32, child: &mut std::process::Child) {
    let pgid = -(child_id as i32);
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
    use super::run_bash;
    use crate::config::EnvMap;
    use crate::config::{
        CompactionConfig, Config, GuardrailConfig, LimitsConfig, ProviderConfig, RedactionConfig,
    };
    use crate::redaction::SecretRedactor;
    use crate::renderer::Renderer;
    use crate::tools::{BashArgs, BashRisk, ToolContext};

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
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
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
        let tmp = std::env::temp_dir().join(format!("mu-bash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let mut renderer = Renderer::new();

        let mut first = args("cd / && export MU_TEST=works && pwd");
        first.cwd = Some(tmp.display().to_string());
        let first_result = run_bash(
            first,
            5,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
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
        )
        .unwrap();
        assert_eq!(second_result.exit_code, 0);
        assert_eq!(second_result.output, format!("{}|unset", tmp.display()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn subagent_depth_parsing_defaults_invalid_values_to_zero() {
        assert_eq!(super::parse_subagent_depth(None), 0);
        assert_eq!(super::parse_subagent_depth(Some("")), 0);
        assert_eq!(super::parse_subagent_depth(Some("nope")), 0);
        assert_eq!(super::parse_subagent_depth(Some("2")), 2);
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
        )
        .unwrap();
        assert!(
            result.output.starts_with("/usr/libexec/mu:"),
            "{}",
            result.output
        );
    }

    #[test]
    fn bash_collects_framed_image_artifacts_separately_from_stdout() {
        let mut renderer = Renderer::new();
        let command = r#"python - <<'PY'
import json, os, struct
data = b'\x89PNG\r\n\x1a\nrest'
header = json.dumps({
    'version': 1,
    'kind': 'image',
    'filename': 'tool.png',
    'media_type': 'image/png',
    'detail': 'high',
    'byte_length': len(data),
}, separators=(',', ':')).encode()
fd = int(os.environ['MU_ARTIFACT_FD'])
os.write(fd, struct.pack('>I', len(header)) + header + struct.pack('>Q', len(data)) + data)
print('visible')
PY"#;
        let result = run_bash(
            args(command),
            5,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
        )
        .unwrap();
        assert_eq!(result.output, "visible");
        assert_eq!(result.artifacts.len(), 1);
        assert_eq!(result.artifacts[0].attachment.filename, "tool.png");
        assert_eq!(
            result.artifacts[0].attachment.data,
            b"\x89PNG\r\n\x1a\nrest"
        );
        assert_eq!(
            result.artifacts[0].detail,
            crate::provider::ImageDetail::High
        );
    }

    #[tokio::test]
    async fn bash_receives_env_and_redacts_configured_values() {
        let mut renderer = Renderer::new();
        let tmp = std::env::temp_dir().join(format!("mu-bash-redact-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
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
            state_dir: &tmp,
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

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn timeout_kills_background_descendants() {
        let marker = format!("/tmp/mu-bash-descendant-{}", uuid::Uuid::new_v4());
        let script = format!("sleep 20 & echo $! > {marker}; sleep 20");
        let mut renderer = Renderer::new();
        let result = run_bash(
            args(&script),
            3,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
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
        std::thread::sleep(std::time::Duration::from_millis(200));
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        assert!(!alive, "background sleep {pid} survived timeout");
        let _ = std::fs::remove_file(marker);
    }
}
