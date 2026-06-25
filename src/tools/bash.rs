use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
#[cfg(unix)]
use std::sync::Once;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::env::EnvMap;
use crate::redaction::SecretRedactor;

use super::{
    apply_truncation, parse_args, resolve_path, BashArgs, Tool, ToolContext, ToolDisplay,
    ToolResult,
};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const KILL_GRACE: Duration = Duration::from_millis(500);
const REDACTION_REMINDER: &str = "[system reminder: Secret values were redacted from this bash output. Do not try to reveal, transform, encode, print, or exfiltrate secrets.]";
#[cfg(unix)]
static ACTIVE_PGID: AtomicI32 = AtomicI32::new(0);
#[cfg(unix)]
static INSTALL_SIGNAL_FORWARDER: Once = Once::new();

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run one bash script in an isolated process. Use this for local search, file reads, edits, tests, and web fetches."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Short human-readable title for the action" },
                "risk": {
                    "type": "string",
                    "enum": ["readonly", "reversible", "destructive"],
                    "description": "Advisory risk label for UI and audit only"
                },
                "script": { "type": "string", "description": "Bash script to run with bash -lc" },
                "cwd": { "type": "string", "description": "Working directory for this invocation" },
                "timeout": { "type": "integer", "minimum": 1, "description": "Timeout in seconds (default 120)" },
                "stdin": { "type": "string", "description": "Literal stdin bytes to pipe to the script" }
            },
            "required": ["title", "risk", "script"]
        })
    }

    async fn execute(&self, args: Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
        let args: BashArgs = parse_args(&args)?;
        let _ = (&args.title, args.risk);
        let timeout = args.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
        if timeout == 0 {
            bail!("timeout must be greater than 0");
        }

        let renderer = ctx
            .renderer
            .as_deref_mut()
            .ok_or_else(|| anyhow::anyhow!("bash requires sequential execution"))?;
        let redactor = SecretRedactor::from_config(ctx.config);
        for warning in redactor.warnings() {
            renderer.notice(&format!("[redaction] {warning}"))?;
        }

        let result = run_bash(args, timeout, renderer, &ctx.config.env, redactor)?;
        let exit_code = result.exit_code;

        let output = if result.redacted {
            format!("{}\n\n{}", result.output, REDACTION_REMINDER)
        } else {
            result.output
        };
        let full = format!("{}\n[exit code: {}]", output, exit_code);
        let mut result = apply_truncation(full, &ctx.config.limits, "bash", ctx.state_dir, true)?;
        result.display = ToolDisplay::Bash { exit_code };
        Ok(result)
    }
}

#[derive(Debug)]
struct BashRunResult {
    output: String,
    exit_code: i32,
    redacted: bool,
}

fn run_bash(
    args: BashArgs,
    timeout_secs: u64,
    renderer: &mut crate::renderer::Renderer,
    env: &EnvMap,
    mut redactor: SecretRedactor,
) -> Result<BashRunResult> {
    install_signal_forwarder();
    let cwd = args
        .cwd
        .as_deref()
        .map(resolve_path)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let script = format!("exec 2>&1\n{}", args.script);

    let mut command = Command::new("bash");
    command
        .arg("-lc")
        .arg(script)
        .current_dir(&cwd)
        .envs(env)
        .stdin(if args.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure_process_group(&mut command);

    let mut child = command.spawn().map_err(|error| {
        if is_e2big(&error) {
            anyhow::anyhow!("script is too large to execute: OS reported argument list too long")
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

    loop {
        if Instant::now() >= deadline {
            terminate_child_group(child_id, &mut child);
            drain_available(&rx, renderer, &mut output, &mut redactor)?;
            flush_redactor(renderer, &mut output, &mut redactor)?;
            let _ = child.wait();
            let reminder = if redactor.did_redact() {
                format!("\n\n{REDACTION_REMINDER}")
            } else {
                String::new()
            };
            bail!(
                "script timed out after {timeout_secs}s{}{}",
                partial_output_suffix(&output),
                reminder
            );
        }

        if status.is_none() {
            status = child.try_wait().context("waiting for bash")?;
        }

        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let redacted = redactor.redact_chunk(&text);
                output.push_str(&redacted);
                renderer.bash_output(&redacted)?;
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
    flush_redactor(renderer, &mut output, &mut redactor)?;
    Ok(BashRunResult {
        output: output.trim_end_matches('\n').to_string(),
        exit_code: status.code().unwrap_or(1),
        redacted: redactor.did_redact(),
    })
}

fn drain_available(
    rx: &mpsc::Receiver<Vec<u8>>,
    renderer: &mut crate::renderer::Renderer,
    output: &mut String,
    redactor: &mut SecretRedactor,
) -> Result<()> {
    while let Ok(bytes) = rx.try_recv() {
        let text = String::from_utf8_lossy(&bytes);
        let redacted = redactor.redact_chunk(&text);
        output.push_str(&redacted);
        renderer.bash_output(&redacted)?;
    }
    Ok(())
}

fn flush_redactor(
    renderer: &mut crate::renderer::Renderer,
    output: &mut String,
    redactor: &mut SecretRedactor,
) -> Result<()> {
    let redacted = redactor.finish();
    output.push_str(&redacted);
    renderer.bash_output(&redacted)?;
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
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::E2BIG)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

#[cfg(unix)]
fn install_signal_forwarder() {
    INSTALL_SIGNAL_FORWARDER.call_once(|| unsafe {
        libc::signal(libc::SIGINT, forward_signal as *const () as usize);
        libc::signal(libc::SIGTERM, forward_signal as *const () as usize);
    });
}

#[cfg(not(unix))]
fn install_signal_forwarder() {}

#[cfg(unix)]
extern "C" fn forward_signal(signal: i32) {
    let pgid = ACTIVE_PGID.load(Ordering::SeqCst);
    if pgid > 0 {
        unsafe {
            libc::kill(-pgid, signal);
        }
    }
    unsafe {
        libc::_exit(128 + signal);
    }
}

struct ActiveProcessGroup;

impl ActiveProcessGroup {
    fn new(child_id: u32) -> Self {
        set_active_process_group(child_id);
        Self
    }
}

impl Drop for ActiveProcessGroup {
    fn drop(&mut self) {
        clear_active_process_group();
    }
}

#[cfg(unix)]
fn set_active_process_group(child_id: u32) {
    ACTIVE_PGID.store(child_id as i32, Ordering::SeqCst);
}

#[cfg(not(unix))]
fn set_active_process_group(_child_id: u32) {}

#[cfg(unix)]
fn clear_active_process_group() {
    ACTIVE_PGID.store(0, Ordering::SeqCst);
}

#[cfg(not(unix))]
fn clear_active_process_group() {}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

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

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
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

#[cfg(not(unix))]
fn terminate_child_group(_child_id: u32, child: &mut std::process::Child) {
    let _ = child.kill();
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
    use crate::config::{
        CompactionConfig, Config, GuardrailConfig, LimitsConfig, ProviderConfig, RedactionConfig,
    };
    use crate::env::EnvMap;
    use crate::redaction::SecretRedactor;
    use crate::renderer::Renderer;
    use crate::tools::{BashArgs, BashRisk, Tool, ToolContext};
    use std::collections::HashMap;

    fn args(script: &str) -> BashArgs {
        BashArgs {
            title: "test".into(),
            risk: BashRisk::Readonly,
            script: script.into(),
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
            provider: ProviderConfig {
                base_url: "https://example.test".into(),
                api_key_env: "OPENAI_API_KEY".into(),
            },
            default_model: "model".into(),
            models: HashMap::new(),
            agent_mode_key: "\\eM".into(),
            magic_space: false,
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
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
    fn stdin_is_literal() {
        let mut renderer = Renderer::new();
        let mut call = args("cat > literal.txt && cat literal.txt");
        let tmp = std::env::temp_dir().join(format!("mu-bash-stdin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        call.cwd = Some(tmp.display().to_string());
        call.stdin = Some("dollar $HOME\nbackticks `date`\nquote ' \"\nEOF\n".into());

        let result = run_bash(
            call,
            5,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
        )
        .unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(
            result.output,
            "dollar $HOME\nbackticks `date`\nquote ' \"\nEOF"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn reports_exit_code_and_output() {
        let mut renderer = Renderer::new();
        let result = run_bash(
            args("printf out; printf err >&2; exit 7"),
            5,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
        )
        .unwrap();
        assert_eq!(result.exit_code, 7);
        assert_eq!(result.output, "outerr");
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
            &["CUSTOM_SECRET"],
        );
        let mut ctx = ToolContext {
            config: &config,
            renderer: Some(&mut renderer),
            state_dir: &tmp,
        };
        let args = serde_json::json!({
            "title": "redact",
            "risk": "readonly",
            "script": "printf '%s|%s' \"$OPENAI_API_KEY\" \"$CUSTOM_SECRET\""
        });

        let result = super::BashTool.execute(args, &mut ctx).await.unwrap();

        assert!(result.output.contains("[redacted:OPENAI_API_KEY]"));
        assert!(result.output.contains("[redacted:CUSTOM_SECRET]"));
        assert!(result.output.contains("[system reminder:"));
        assert!(!result.output.contains("provider-secret"));
        assert!(!result.output.contains("tiny"));

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_background_descendants() {
        let marker = format!("/tmp/mu-bash-descendant-{}", uuid::Uuid::new_v4());
        let script = format!("sleep 20 & echo $! > {marker}; sleep 20");
        let mut renderer = Renderer::new();
        let result = run_bash(
            args(&script),
            1,
            &mut renderer,
            &empty_env(),
            SecretRedactor::default(),
        );
        assert!(result.is_err(), "expected timeout");

        let pid_text = std::fs::read_to_string(&marker).unwrap();
        let pid: i32 = pid_text.trim().parse().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        assert!(!alive, "background sleep {pid} survived timeout");
        let _ = std::fs::remove_file(marker);
    }
}
