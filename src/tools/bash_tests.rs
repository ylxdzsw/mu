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
        providers: HashMap::from([(
            "test".into(),
            ProviderConfig {
                base_url: "https://example.test".into(),
                api_key_env: "OPENAI_API_KEY".into(),
                models: HashMap::new(),
            },
        )]),
        default_model: "test/model".into(),
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
