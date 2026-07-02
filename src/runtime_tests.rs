use std::collections::HashMap;

use crate::config::{
    CompactionConfig, Config, GuardrailConfig, LimitsConfig, ModelConfig, ProviderConfig,
    RedactionConfig, TerminalBellConfig,
};
use crate::models::EffortLevel;

use super::*;

fn test_config() -> Config {
    Config {
        providers: HashMap::from([(
            "alpha".into(),
            ProviderConfig {
                base_url: "http://localhost".into(),
                api_key_env: "MU_TEST_KEY".into(),
                models: HashMap::from([(
                    "default-model".into(),
                    ModelConfig {
                        context_window: Some(100),
                        price_per_mtok: None,
                        supported_efforts: Some(vec![EffortLevel::Low, EffortLevel::High]),
                    },
                )]),
            },
        )]),
        default_model: "alpha/default-model:low".into(),
        compaction: CompactionConfig::default(),
        limits: LimitsConfig::default(),
        guardrail: GuardrailConfig::default(),
        terminal_bell: TerminalBellConfig::default(),
        redaction: RedactionConfig::default(),
        env: HashMap::new(),
    }
}

#[test]
fn attached_session_reuses_last_model() {
    let store = Store::open_memory().unwrap();
    let session = store
        .create_session_with_origin(
            "/tmp",
            "alpha/default-model:high",
            crate::store::SessionOrigin::Cli,
        )
        .unwrap();
    let resolved = resolve_invocation(
        &store,
        &test_config(),
        &InvocationOverrides {
            session: Some(session.id.clone()),
            continue_latest: false,
            model: None,
        },
    )
    .unwrap();

    assert_eq!(resolved.request.model.canonical, "alpha/default-model:high");
    assert_eq!(
        resolved.session_seed.model.canonical,
        "alpha/default-model:high"
    );
}

#[test]
fn explicit_model_override_seeds_new_session_with_override() {
    let store = Store::open_memory().unwrap();
    store
        .create_session_with_origin(
            "/tmp",
            "alpha/default-model:high",
            crate::store::SessionOrigin::Cli,
        )
        .unwrap();

    let resolved = resolve_invocation(
        &store,
        &test_config(),
        &InvocationOverrides {
            session: None,
            continue_latest: false,
            model: Some("alpha/default-model:low".into()),
        },
    )
    .unwrap();

    assert_eq!(resolved.request.model.canonical, "alpha/default-model:low");
    assert_eq!(
        resolved.session_seed.model.canonical,
        "alpha/default-model:low"
    );
}

#[test]
fn status_report_can_include_available_models() {
    let store = Store::open_memory().unwrap();
    let report = build_status_report(
        &store,
        &test_config(),
        &InvocationOverrides::default(),
        None,
        true,
    )
    .unwrap();

    assert_eq!(report.model_id, "alpha/default-model:low");
    assert!(report.available_models.is_some());
    assert_eq!(
        report.supported_effort_levels,
        vec![EffortLevel::Low, EffortLevel::High]
    );
}

#[test]
fn status_report_surfaces_incomplete_turn() {
    let store = Store::open_memory().unwrap();
    let session = store
        .create_session_with_origin(
            "/tmp",
            "alpha/default-model:high",
            crate::store::SessionOrigin::Cli,
        )
        .unwrap();
    store
        .begin_pending_turn(
            &session.id,
            &crate::provider::UserContent::Text("retry".into()),
        )
        .unwrap();
    store
        .mark_pending_incomplete(&session.id, "previous turn was interrupted")
        .unwrap();

    let report = build_status_report(
        &store,
        &test_config(),
        &InvocationOverrides {
            session: Some(session.id.clone()),
            continue_latest: false,
            model: None,
        },
        None,
        false,
    )
    .unwrap();

    let incomplete = report.incomplete_turn.expect("incomplete turn");
    assert_eq!(incomplete.retry_count, 0);
    assert_eq!(
        incomplete.error_message.as_deref(),
        Some("previous turn was interrupted")
    );
}

#[test]
fn status_report_surfaces_stale_running_turn_without_mutating_history() {
    let store = Store::open_memory().unwrap();
    let session = store
        .create_session_with_origin(
            "/tmp",
            "alpha/default-model:high",
            crate::store::SessionOrigin::Cli,
        )
        .unwrap();
    store
        .begin_pending_turn(
            &session.id,
            &crate::provider::UserContent::Text("retry".into()),
        )
        .unwrap();
    let before = store.session_summary(&session.id).unwrap().unwrap();

    let report = build_status_report(
        &store,
        &test_config(),
        &InvocationOverrides {
            session: Some(session.id.clone()),
            continue_latest: false,
            model: None,
        },
        None,
        false,
    )
    .unwrap();

    assert!(!report.active.busy);
    let incomplete = report.incomplete_turn.expect("incomplete turn");
    assert_eq!(incomplete.retry_count, 0);
    assert_eq!(
        incomplete.error_message.as_deref(),
        Some("previous turn was interrupted")
    );

    let pending = store.pending_turn(&session.id).unwrap().unwrap();
    assert_eq!(pending.state, crate::store::PendingState::Running);

    let after = store.session_summary(&session.id).unwrap().unwrap();
    assert_eq!(after.message_count, before.message_count);
    assert_eq!(after.turn_count, before.turn_count);
}
