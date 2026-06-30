pub mod prompt;

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::config::{Config, GuardrailConfig};
use crate::models::RequestOptions;
use crate::provider::{Message, Provider, ProviderError};

const MAX_ATTEMPTS: u32 = 3;
const MAX_MESSAGE_TRANSCRIPT_TOKENS: usize = 10_000;
const MAX_TOOL_TRANSCRIPT_TOKENS: usize = 10_000;
const MAX_MESSAGE_ENTRY_TOKENS: usize = 2_000;
const MAX_TOOL_ENTRY_TOKENS: usize = 1_000;
const RECENT_ENTRY_LIMIT: usize = 40;
const MAX_ACTION_STRING_TOKENS: usize = 16_000;
const TRUNCATION_TAG: &str = "truncated";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

impl RiskLevel {
    /// Ordinal rank. The gap between `High`(2) and `Critical`(4) ensures
    /// only `Explicit`(4) authorization can approve critical-risk actions.
    pub fn rank(&self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::Critical => 4,
        }
    }
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Critical => write!(f, "critical"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UserAuthLevel {
    Unknown,
    Low,
    Medium,
    High,
    Explicit,
}

impl UserAuthLevel {
    pub fn rank(&self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
            Self::Explicit => 4,
        }
    }
}

impl fmt::Display for UserAuthLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => write!(f, "unknown"),
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Explicit => write!(f, "explicit"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Assessment {
    pub risk_level: RiskLevel,
    pub user_auth_level: UserAuthLevel,
    pub reason: String,
}

impl Assessment {
    /// Execute only if `user_auth_level >= risk_level` on the ordinal scale.
    pub fn is_allowed(&self) -> bool {
        self.user_auth_level.rank() >= self.risk_level.rank()
    }

    pub fn outcome(&self) -> &'static str {
        if self.is_allowed() { "allow" } else { "deny" }
    }
}

pub enum GuardrailOutcome {
    Allow(Assessment),
    Deny(Assessment),
    Failed(anyhow::Error),
}

pub struct Guardrail {
    config: GuardrailConfig,
    runtime: Config,
    consecutive_denials: u32,
    recent_denials: VecDeque<bool>,
    interrupt_triggered: bool,
}

impl Guardrail {
    pub fn new(config: &Config, _provider: Arc<dyn Provider>) -> Self {
        Self {
            config: config.guardrail.clone(),
            runtime: config.clone(),
            consecutive_denials: 0,
            recent_denials: VecDeque::new(),
            interrupt_triggered: false,
        }
    }

    /// Whether the guardrail should review a bash call with the given risk.
    pub fn should_review(&self, risk: &str) -> bool {
        self.config.enabled && risk == "destructive"
    }

    /// Assess a planned action. Returns `Allow`, `Deny`, or `Failed` (which
    /// should abort the turn — re-authorizing would likely fail again since
    /// the reviewer itself is malfunctioning).
    pub async fn assess(&mut self, action: &Value, context: &[Message]) -> GuardrailOutcome {
        crate::tools::bash::install_signal_forwarder();
        let model_ref = self
            .config
            .review_model
            .as_deref()
            .unwrap_or(&self.runtime.default_model);
        let request_model = match crate::models::resolve_model_ref(&self.runtime, model_ref) {
            Ok(model) => model,
            Err(error) => return GuardrailOutcome::Failed(error),
        };
        let provider =
            match crate::provider::build_provider(&self.runtime, &request_model.provider_id) {
                Ok(provider) => provider,
                Err(error) => return GuardrailOutcome::Failed(error),
            };

        let system_prompt = prompt::policy_prompt().to_string();
        let user_content = prompt::build_reviewer_user_content(context, action);

        let msgs = vec![
            Message::System {
                content: system_prompt,
            },
            Message::User {
                content: user_content.into(),
            },
        ];

        let timeout = Duration::from_millis(self.config.timeout_ms);
        let mut last_error = String::new();

        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                let backoff = Duration::from_secs(1 << (attempt - 1));
                tokio::time::sleep(backoff).await;
            }

            let mut ignore_event = |_event: crate::provider::StreamEvent| Ok(());
            let result = tokio::time::timeout(timeout, async {
                provider
                    .stream_chat(
                        &RequestOptions {
                            model: request_model.clone(),
                        },
                        &msgs,
                        &[],
                        &mut ignore_event,
                    )
                    .await
            })
            .await;

            match result {
                Err(_elapsed) => {
                    last_error = format!("reviewer timed out after {}ms", self.config.timeout_ms);
                    continue;
                }
                Ok(Err(ProviderError::ContextLength)) => {
                    return GuardrailOutcome::Failed(anyhow::anyhow!(
                        "reviewer context length exceeded"
                    ));
                }
                Ok(Err(ProviderError::Other(e))) => {
                    last_error = e;
                    continue;
                }
                Ok(Ok(stream_result)) => {
                    let content = match &stream_result.message {
                        Message::Assistant {
                            content: Some(c), ..
                        } => c.as_str(),
                        _ => "",
                    };
                    match prompt::parse_assessment(content) {
                        Ok(assessment) => {
                            if assessment.is_allowed() {
                                self.record_non_denial();
                                return GuardrailOutcome::Allow(assessment);
                            } else {
                                self.record_denial();
                                return GuardrailOutcome::Deny(assessment);
                            }
                        }
                        Err(e) => {
                            last_error = format!("parse error: {e}");
                            continue;
                        }
                    }
                }
            }
        }

        GuardrailOutcome::Failed(anyhow::anyhow!(
            "reviewer failed after {MAX_ATTEMPTS} attempts: {last_error}"
        ))
    }

    fn record_denial(&mut self) {
        self.consecutive_denials = self.consecutive_denials.saturating_add(1);
        self.push_recent(true);
    }

    fn record_non_denial(&mut self) {
        self.consecutive_denials = 0;
        self.push_recent(false);
    }

    fn push_recent(&mut self, denied: bool) {
        self.recent_denials.push_back(denied);
        if self.recent_denials.len() > self.config.circuit_breaker.window {
            self.recent_denials.pop_front();
        }
    }

    /// Check whether the circuit breaker has tripped after recent denials.
    /// Returns `Some((consecutive, recent))` with the counts that triggered it,
    /// or `None` if the breaker has not tripped (or already tripped earlier).
    pub fn circuit_breaker_tripped(&mut self) -> Option<(u32, u32)> {
        if self.interrupt_triggered {
            return None;
        }
        let cb = &self.config.circuit_breaker;
        let recent = self.recent_denials.iter().filter(|d| **d).count() as u32;

        if self.consecutive_denials >= cb.consecutive || recent >= cb.window_denials {
            self.interrupt_triggered = true;
            Some((self.consecutive_denials, recent))
        } else {
            None
        }
    }
}

/// Extract and validate the risk field from bash tool arguments.
/// Returns `Some(value)` only if the field is present and one of the three
/// valid enum strings. An invalid or missing value returns `None`.
pub fn bash_risk(args: &Value) -> Option<String> {
    let risk = args.get("risk")?.as_str()?;
    match risk {
        "readonly" | "reversible" | "destructive" => Some(risk.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
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
    fn ordinal_scale_critical_denied_by_high() {
        let a = Assessment {
            risk_level: RiskLevel::Critical,
            user_auth_level: UserAuthLevel::High,
            reason: "".into(),
        };
        assert!(!a.is_allowed());
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
}
