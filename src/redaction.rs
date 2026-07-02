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
        for provider in config.providers.values() {
            names.insert(provider.api_key_env.clone());
        }
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
#[path = "redaction_tests.rs"]
mod tests;
