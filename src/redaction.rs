use std::collections::BTreeSet;

use crate::config::Config;

const SHORT_SECRET_LEN: usize = 8;

#[derive(Debug, Clone)]
struct Secret {
    name: String,
    value: String,
}

#[derive(Debug, Clone, Default)]
pub struct SecretRedactor {
    secrets: Vec<Secret>,
    max_secret_chars: usize,
    pending: String,
    redacted: bool,
    warnings: Vec<String>,
}

impl SecretRedactor {
    pub fn from_config(config: &Config) -> Self {
        let mut names = BTreeSet::new();
        names.insert(config.provider.api_key_env.clone());
        names.extend(config.redaction.env.iter().cloned());

        let mut warnings = Vec::new();
        let mut secrets = Vec::new();
        for name in names {
            let Some(value) = config.env.get(&name) else {
                continue;
            };
            if value.is_empty() {
                warnings.push(format!("redaction env var `{name}` is empty; ignoring it"));
                continue;
            }
            if value.chars().count() < SHORT_SECRET_LEN {
                warnings.push(format!(
                    "redaction env var `{name}` has a short value; it will still be redacted"
                ));
            }
            secrets.push(Secret {
                name,
                value: value.clone(),
            });
        }

        secrets.sort_by(|a, b| b.value.len().cmp(&a.value.len()));
        let max_secret_chars = secrets
            .iter()
            .map(|secret| secret.value.chars().count())
            .max()
            .unwrap_or_default();

        Self {
            secrets,
            max_secret_chars,
            pending: String::new(),
            redacted: false,
            warnings,
        }
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn did_redact(&self) -> bool {
        self.redacted
    }

    pub fn redact_chunk(&mut self, text: &str) -> String {
        if self.secrets.is_empty() {
            return text.to_string();
        }

        self.pending.push_str(text);
        let keep_chars = self.max_secret_chars.saturating_mul(2).saturating_sub(2);
        let pending_chars = self.pending.chars().count();
        if pending_chars <= keep_chars {
            return String::new();
        }

        let emit_chars = pending_chars - keep_chars;
        let split = byte_index_after_chars(&self.pending, emit_chars);
        let tail = self.pending.split_off(split);
        let emit = std::mem::replace(&mut self.pending, tail);
        self.redact_text(&emit)
    }

    pub fn finish(&mut self) -> String {
        let text = std::mem::take(&mut self.pending);
        self.redact_text(&text)
    }

    fn redact_text(&mut self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in &self.secrets {
            if out.contains(&secret.value) {
                out = out.replace(&secret.value, &format!("[redacted:{}]", secret.name));
                self.redacted = true;
            }
        }
        out
    }
}

fn byte_index_after_chars(text: &str, chars: usize) -> usize {
    if chars == 0 {
        return 0;
    }
    text.char_indices()
        .nth(chars)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::config::{
        CompactionConfig, Config, GuardrailConfig, LimitsConfig, ProviderConfig, RedactionConfig,
    };

    use super::*;

    fn config(env: &[(&str, &str)], redaction_env: &[&str]) -> Config {
        Config {
            provider: ProviderConfig {
                base_url: "https://example.test".into(),
                api_key_env: "OPENAI_API_KEY".into(),
            },
            default_model: "model".into(),
            default_effort: None,
            models: HashMap::new(),
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
    fn provider_key_is_implicitly_redacted_across_chunks() {
        let cfg = config(&[("OPENAI_API_KEY", "secret-value")], &[]);
        let mut redactor = SecretRedactor::from_config(&cfg);

        let mut out = String::new();
        out.push_str(&redactor.redact_chunk("before secret"));
        out.push_str(&redactor.redact_chunk("-value after"));
        out.push_str(&redactor.finish());

        assert_eq!(out, "before [redacted:OPENAI_API_KEY] after");
        assert!(redactor.did_redact());
    }

    #[test]
    fn empty_values_warn_and_are_ignored() {
        let cfg = config(&[("OPENAI_API_KEY", ""), ("TOKEN", "abc")], &["TOKEN"]);
        let mut redactor = SecretRedactor::from_config(&cfg);

        assert!(redactor
            .warnings()
            .iter()
            .any(|warning| warning.contains("OPENAI_API_KEY") && warning.contains("empty")));
        assert!(redactor
            .warnings()
            .iter()
            .any(|warning| warning.contains("TOKEN") && warning.contains("short")));

        let mut out = String::new();
        out.push_str(&redactor.redact_chunk("empty is  and token is abc"));
        out.push_str(&redactor.finish());
        assert_eq!(out, "empty is  and token is [redacted:TOKEN]");
    }
}
