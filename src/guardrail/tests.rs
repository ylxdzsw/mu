use super::*;
use crate::config::CircuitBreakerConfig;
use serde_json::json;

#[test]
fn ordinal_scale_low_allows_unknown() {
    let a = Assessment {
        risk_level: RiskLevel::Low,
        user_auth_level: UserAuthLevel::Unknown,
        reason: "".into(),
    };
    assert!(a.is_allowed());
}

#[test]
fn ordinal_scale_medium_denied_by_unknown() {
    let a = Assessment {
        risk_level: RiskLevel::Medium,
        user_auth_level: UserAuthLevel::Unknown,
        reason: "".into(),
    };
    assert!(!a.is_allowed());
}

#[test]
fn ordinal_scale_high_allowed_by_medium() {
    let a = Assessment {
        risk_level: RiskLevel::High,
        user_auth_level: UserAuthLevel::Medium,
        reason: "".into(),
    };
    assert!(a.is_allowed());
}

#[test]
fn ordinal_scale_critical_allowed_by_explicit() {
    let a = Assessment {
        risk_level: RiskLevel::Critical,
        user_auth_level: UserAuthLevel::Explicit,
        reason: "".into(),
    };
    assert!(a.is_allowed());
}

fn test_guardrail(consecutive: u32, window_denials: u32) -> Guardrail {
    Guardrail {
        config: GuardrailConfig {
            enabled: true,
            review_model: None,
            timeout_ms: 1000,
            circuit_breaker: CircuitBreakerConfig {
                consecutive,
                window: 50,
                window_denials,
            },
        },
        runtime: Config {
            providers: Default::default(),
            default_model: String::new(),
            compaction: crate::config::CompactionConfig::default(),
            limits: crate::config::LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            terminal_bell: crate::config::TerminalBellConfig::default(),
            redaction: crate::config::RedactionConfig::default(),
            env: Default::default(),
        },
        consecutive_denials: 0,
        recent_denials: VecDeque::new(),
        interrupt_triggered: false,
    }
}

#[test]
fn circuit_breaker_trips_on_consecutive_denials() {
    let mut g = test_guardrail(3, 10);
    assert!(g.circuit_breaker_tripped().is_none());
    g.record_denial();
    assert!(g.circuit_breaker_tripped().is_none());
    g.record_denial();
    assert!(g.circuit_breaker_tripped().is_none());
    g.record_denial();
    let trip = g.circuit_breaker_tripped();
    assert!(trip.is_some());
    assert_eq!(trip.unwrap().0, 3);
    assert!(g.circuit_breaker_tripped().is_none());
}

#[test]
fn circuit_breaker_trips_on_window_denials() {
    let mut g = test_guardrail(100, 3);
    for _ in 0..3 {
        g.record_denial();
    }
    assert!(g.circuit_breaker_tripped().is_some());
}

#[test]
fn circuit_breaker_resets_on_non_denial() {
    let mut g = test_guardrail(3, 10);
    g.record_denial();
    g.record_denial();
    g.record_non_denial();
    assert!(g.circuit_breaker_tripped().is_none());
}

#[test]
fn should_review_only_destructive_when_enabled() {
    let g = test_guardrail(3, 10);
    assert!(g.should_review("destructive"));
    assert!(!g.should_review("reversible"));
    assert!(!g.should_review("readonly"));
}

#[test]
fn should_not_review_when_disabled() {
    let mut g = test_guardrail(3, 10);
    g.config.enabled = false;
    assert!(!g.should_review("destructive"));
}

#[test]
fn bash_risk_valid_values() {
    assert_eq!(
        bash_risk(&json!({"risk": "readonly"})),
        Some("readonly".into())
    );
    assert_eq!(
        bash_risk(&json!({"risk": "reversible"})),
        Some("reversible".into())
    );
    assert_eq!(
        bash_risk(&json!({"risk": "destructive"})),
        Some("destructive".into())
    );
}

#[test]
fn bash_risk_invalid_value_returns_none() {
    assert_eq!(bash_risk(&json!({"risk": "dangerous"})), None);
    assert_eq!(bash_risk(&json!({"risk": ""})), None);
}

#[test]
fn bash_risk_missing_field_returns_none() {
    assert_eq!(bash_risk(&json!({"script": "ls"})), None);
    assert_eq!(bash_risk(&json!({})), None);
}
