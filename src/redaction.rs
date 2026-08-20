use std::collections::{BTreeMap, BTreeSet};

use aho_corasick::{AhoCorasick, MatchKind};
use anyhow::{Context, Result};

use crate::config::{Config, redaction_suffix};

const SHORT_SECRET_LEN: usize = 8;

#[derive(Debug, Clone)]
struct Secret {
    name: String,
    value: String,
    replacement: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct SecretRedactor {
    secrets: Vec<Secret>,
    matcher: Option<AhoCorasick>,
    max_secret_bytes: usize,
    pending: Vec<u8>,
    utf8_pending: Vec<u8>,
    redacted: bool,
    warnings: Vec<String>,
}

impl SecretRedactor {
    pub fn from_config(config: &Config) -> Result<Self> {
        let mut names = BTreeSet::new();
        for provider in config.providers.values() {
            names.insert(provider.api_key_env.clone());
        }
        for selector in &config.redaction.env {
            match redaction_suffix(selector)? {
                Some(suffix) => names.extend(
                    config
                        .env
                        .keys()
                        .filter(|name| name.ends_with(suffix))
                        .cloned(),
                ),
                None => {
                    names.insert(selector.clone());
                }
            }
        }

        let mut warnings = Vec::new();
        let mut values = BTreeMap::new();
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
            values.entry(value.clone()).or_insert_with(|| name.clone());
        }

        let mut secrets: Vec<_> = values
            .into_iter()
            .map(|(value, name)| Secret {
                replacement: format!("[redacted:{name}]").into_bytes(),
                name,
                value,
            })
            .collect();
        secrets.sort_by(|a, b| {
            b.value
                .len()
                .cmp(&a.value.len())
                .then_with(|| a.name.cmp(&b.name))
        });

        let max_secret_bytes = secrets
            .iter()
            .map(|secret| secret.value.len())
            .max()
            .unwrap_or_default();
        let matcher = if secrets.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .match_kind(MatchKind::LeftmostLongest)
                    .build(secrets.iter().map(|secret| secret.value.as_bytes()))
                    .context("building secret redaction matcher")?,
            )
        };

        Ok(Self {
            secrets,
            matcher,
            max_secret_bytes,
            pending: Vec::new(),
            utf8_pending: Vec::new(),
            redacted: false,
            warnings,
        })
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn did_redact(&self) -> bool {
        self.redacted
    }

    pub fn redact_chunk(&mut self, bytes: &[u8]) -> String {
        if self.matcher.is_none() {
            return self.decode_utf8(bytes, false);
        }

        self.pending.extend_from_slice(bytes);
        let keep_bytes = self.max_secret_bytes.saturating_sub(1);
        if self.pending.len() <= keep_bytes {
            return String::new();
        }

        let safe_start_limit = self.pending.len() - keep_bytes;
        let (redacted, consumed) = self.redact_before(safe_start_limit);
        self.pending.drain(..consumed);
        self.decode_utf8(&redacted, false)
    }

    pub fn finish(&mut self) -> String {
        let redacted = if self.matcher.is_some() {
            let limit = self.pending.len();
            let (redacted, consumed) = self.redact_before(limit);
            debug_assert_eq!(consumed, self.pending.len());
            self.pending.clear();
            redacted
        } else {
            Vec::new()
        };
        self.decode_utf8(&redacted, true)
    }

    fn redact_before(&mut self, safe_start_limit: usize) -> (Vec<u8>, usize) {
        let matcher = self.matcher.as_ref().expect("matcher present");
        let mut output = Vec::new();
        let mut cursor = 0;
        let mut consumed = safe_start_limit;

        for matched in matcher.find_iter(&self.pending) {
            if matched.start() >= safe_start_limit {
                break;
            }
            output.extend_from_slice(&self.pending[cursor..matched.start()]);
            output.extend_from_slice(&self.secrets[matched.pattern()].replacement);
            cursor = matched.end();
            consumed = consumed.max(cursor);
            self.redacted = true;
        }
        output.extend_from_slice(&self.pending[cursor..consumed]);
        (output, consumed)
    }

    fn decode_utf8(&mut self, bytes: &[u8], finish: bool) -> String {
        let mut data = std::mem::take(&mut self.utf8_pending);
        data.extend_from_slice(bytes);
        let mut remaining = data.as_slice();
        let mut output = String::new();

        while !remaining.is_empty() {
            match std::str::from_utf8(remaining) {
                Ok(valid) => {
                    output.push_str(valid);
                    break;
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    output.push_str(
                        std::str::from_utf8(&remaining[..valid_up_to])
                            .expect("UTF-8 validator reported a valid prefix"),
                    );
                    remaining = &remaining[valid_up_to..];
                    match error.error_len() {
                        Some(invalid_len) => {
                            output.push(char::REPLACEMENT_CHARACTER);
                            remaining = &remaining[invalid_len..];
                        }
                        None if finish => {
                            output.push_str(&String::from_utf8_lossy(remaining));
                            break;
                        }
                        None => {
                            self.utf8_pending.extend_from_slice(remaining);
                            break;
                        }
                    }
                }
            }
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        CompactionConfig, Config, GuardrailConfig, LimitsConfig, OrderedMap, ProviderConfig,
        RedactionConfig,
    };

    fn config(env: &[(&str, &str)], redaction_env: &[&str]) -> Config {
        Config {
            providers: OrderedMap::from_iter([(
                "test".into(),
                ProviderConfig {
                    endpoint: "https://example.test/chat/completions".into(),
                    api_key_env: "OPENAI_API_KEY".into(),
                    models: OrderedMap::default(),
                },
            )]),
            output: Default::default(),
            auto_resume: false,
            soft_interrupt: crate::config::bundled_test_default("/soft_interrupt"),
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

    fn redact_chunks(config: &Config, chunks: &[&[u8]]) -> (String, bool) {
        let mut redactor = SecretRedactor::from_config(config).unwrap();
        let mut output = String::new();
        for chunk in chunks {
            output.push_str(&redactor.redact_chunk(chunk));
        }
        output.push_str(&redactor.finish());
        (output, redactor.did_redact())
    }

    #[test]
    fn secrets_are_redacted_across_every_chunk_boundary() {
        for (secret, input, expected) in [
            (
                "secret-value",
                "before secret-value after",
                "before [redacted:OPENAI_API_KEY] after",
            ),
            (
                "abc",
                "before-abc-after",
                "before-[redacted:OPENAI_API_KEY]-after",
            ),
            ("秘密🔑", "前-秘密🔑-後", "前-[redacted:OPENAI_API_KEY]-後"),
        ] {
            let cfg = config(&[("OPENAI_API_KEY", secret)], &[]);
            let bytes = input.as_bytes();
            for split in 0..=bytes.len() {
                let (output, redacted) = redact_chunks(&cfg, &[&bytes[..split], &bytes[split..]]);
                assert_eq!(output, expected);
                assert!(redacted);
            }

            let chunks = bytes.chunks(1).collect::<Vec<_>>();
            assert_eq!(redact_chunks(&cfg, &chunks).0, expected);
        }

        let cfg = config(&[("OPENAI_API_KEY", "abc")], &[]);
        assert_eq!(
            redact_chunks(&cfg, &[b"abcde"]).0,
            "[redacted:OPENAI_API_KEY]de"
        );
    }

    #[test]
    fn suffix_selectors_expand_environment_names() {
        let cfg = config(
            &[
                ("SERVICE_TOKEN", "service-secret"),
                ("SECOND_TOKEN", "second-secret"),
                ("TOKEN_OTHER", "visible"),
            ],
            &["*_TOKEN"],
        );
        let (output, _) = redact_chunks(&cfg, &[b"service-secret|second-secret|visible"]);

        assert_eq!(
            output,
            "[redacted:SERVICE_TOKEN]|[redacted:SECOND_TOKEN]|visible"
        );
    }

    #[test]
    fn longest_match_wins_and_replacements_are_not_rescanned() {
        let cfg = config(
            &[("LONG", "abcd"), ("SHORT", "b"), ("LETTER", "a")],
            &["LONG", "SHORT", "LETTER"],
        );
        let (output, _) = redact_chunks(&cfg, &[b"abcd a"]);

        assert_eq!(output, "[redacted:LONG] [redacted:LETTER]");
    }

    #[test]
    fn utf8_output_is_decoded_across_chunks_without_secrets() {
        let cfg = config(&[], &[]);
        let input = "前🔑後".as_bytes();
        let chunks: Vec<_> = input.chunks(1).collect();
        let (output, redacted) = redact_chunks(&cfg, &chunks);

        assert_eq!(output, "前🔑後");
        assert!(!redacted);
    }

    #[test]
    fn invalid_utf8_is_replaced_without_corrupting_split_valid_text() {
        let cfg = config(&[], &[]);
        let (output, _) = redact_chunks(&cfg, &[b"before-\xf0\x9f", b"\x94\x91-\xff-after"]);

        assert_eq!(output, "before-🔑-�-after");
    }

    #[test]
    fn empty_values_warn_and_are_ignored() {
        let cfg = config(&[("OPENAI_API_KEY", ""), ("TOKEN", "abc")], &["TOKEN"]);
        let redactor = SecretRedactor::from_config(&cfg).unwrap();

        assert!(
            redactor
                .warnings()
                .iter()
                .any(|warning| warning.contains("OPENAI_API_KEY") && warning.contains("empty"))
        );
        assert!(
            redactor
                .warnings()
                .iter()
                .any(|warning| warning.contains("TOKEN") && warning.contains("short"))
        );
    }
}
