use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::paths;

const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
const MODELS_DEV_MODELS_URL: &str = "https://models.dev/models.json";
const MODEL_CATALOG_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl EffortLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn canonical() -> &'static [Self] {
        &[Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max]
    }
}

impl fmt::Display for EffortLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EffortLevel {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            other => bail!("unsupported effort level `{other}`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOptions {
    pub model: String,
    pub effort: Option<EffortLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub version: u32,
    pub fetched_at: String,
    pub provider: CachedProviderInfo,
    pub models: BTreeMap<String, CachedModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedProviderInfo {
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedModel {
    pub id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort_levels: Option<Vec<EffortLevel>>,
    pub reasoning_effort_levels_source: EffortLevelsSource,
    pub metadata_source: CachedMetadataSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffortLevelsSource {
    Explicit,
    Inferred,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedMetadataSource {
    pub kind: CachedMetadataSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CachedMetadataSourceKind {
    ProviderModelsEndpoint,
    ModelsDevApi,
    ModelsDevModels,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMetadataSource {
    Cache,
    FallbackInference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportedEffortSource {
    Explicit,
    Inferred,
    FallbackInference,
}

#[derive(Debug, Clone)]
pub struct ResolvedModelInfo {
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub reasoning: Option<bool>,
    pub supported_effort_levels: Vec<EffortLevel>,
    pub metadata_source: ModelMetadataSource,
    pub supported_effort_source: SupportedEffortSource,
    validation_mode: EffortValidationMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffortValidationMode {
    ExplicitList,
    KnownNonReasoning,
    Inferred,
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderModelsResponse {
    #[serde(default)]
    data: Vec<RemoteModelRecord>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RemoteModelRecord {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    reasoning_options: Option<Vec<RawReasoningOption>>,
    #[serde(default)]
    limit: Option<RemoteModelLimit>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RemoteModelLimit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default)]
    output: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RawReasoningOption {
    #[serde(rename = "type", default)]
    option_type: String,
    #[serde(default)]
    values: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ModelsDevProviderRecord {
    #[serde(default)]
    id: String,
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, RemoteModelRecord>,
}

#[derive(Debug, Clone)]
enum ModelMetadataRecord<'a> {
    Provider(&'a RemoteModelRecord),
    ModelsDevApi {
        provider_id: &'a str,
        api_url: Option<&'a str>,
        model: &'a RemoteModelRecord,
    },
    ModelsDevModels(&'a RemoteModelRecord),
}

impl ModelCatalog {
    pub fn cache_path() -> PathBuf {
        paths::global_dir().join("models.json")
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading model catalog {}", path.display()))?;
        let catalog = serde_json::from_str(&raw)
            .with_context(|| format!("parsing model catalog {}", path.display()))?;
        Ok(Some(catalog))
    }

    pub fn load_matching(base_url: &str) -> Result<Option<Self>> {
        let Some(catalog) = Self::load(&Self::cache_path())? else {
            return Ok(None);
        };
        if normalize_base_url(&catalog.provider.base_url) == normalize_base_url(base_url) {
            Ok(Some(catalog))
        } else {
            Ok(None)
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self)?;
        std::fs::write(path, body)
            .with_context(|| format!("writing model catalog {}", path.display()))?;
        Ok(())
    }
}

pub async fn refresh_model_catalog(base_url: &str, api_key: &str) -> Result<ModelCatalog> {
    let client = Client::new();
    let provider_models = fetch_provider_models(&client, base_url, api_key).await?;

    let base_url = normalize_base_url(base_url);
    let models_dev_api = fetch_models_dev_api(&client).await.ok();
    let matched_provider = models_dev_api.as_ref().and_then(|providers| {
        providers.values().find(|provider| {
            provider.api.as_deref().map(normalize_base_url) == Some(base_url.clone())
        })
    });
    let models_dev_models = if matched_provider.is_none() {
        fetch_models_dev_models(&client).await.ok()
    } else {
        None
    };

    Ok(build_catalog(
        &base_url,
        provider_models,
        matched_provider,
        models_dev_models.as_ref(),
    ))
}

pub fn resolve_model_info(
    config: &Config,
    catalog: Option<&ModelCatalog>,
    model_id: &str,
) -> ResolvedModelInfo {
    if let Some(cached) = catalog.and_then(|catalog| catalog.models.get(model_id)) {
        let supported_effort_levels = cached
            .reasoning_effort_levels
            .clone()
            .unwrap_or_else(|| inferred_effort_levels(cached.reasoning));
        let validation_mode = match cached.reasoning_effort_levels_source {
            EffortLevelsSource::Explicit => EffortValidationMode::ExplicitList,
            EffortLevelsSource::Inferred => {
                if cached.reasoning == Some(false) {
                    EffortValidationMode::KnownNonReasoning
                } else {
                    EffortValidationMode::Inferred
                }
            }
        };

        return ResolvedModelInfo {
            context_window: cached
                .context_window
                .or_else(|| config.context_window(model_id)),
            max_output_tokens: cached.max_output_tokens,
            reasoning: cached.reasoning,
            supported_effort_levels,
            metadata_source: ModelMetadataSource::Cache,
            supported_effort_source: match cached.reasoning_effort_levels_source {
                EffortLevelsSource::Explicit => SupportedEffortSource::Explicit,
                EffortLevelsSource::Inferred => SupportedEffortSource::Inferred,
            },
            validation_mode,
        };
    }

    ResolvedModelInfo {
        context_window: config.context_window(model_id),
        max_output_tokens: None,
        reasoning: None,
        supported_effort_levels: EffortLevel::canonical().to_vec(),
        metadata_source: ModelMetadataSource::FallbackInference,
        supported_effort_source: SupportedEffortSource::FallbackInference,
        validation_mode: EffortValidationMode::Inferred,
    }
}

pub fn validate_effort_support(
    model_id: &str,
    effort: Option<&EffortLevel>,
    info: &ResolvedModelInfo,
) -> Result<()> {
    let Some(effort) = effort else {
        return Ok(());
    };

    match info.validation_mode {
        EffortValidationMode::Inferred => Ok(()),
        EffortValidationMode::KnownNonReasoning => bail!(
            "reasoning effort `{effort}` is not supported by model `{model_id}`; cached metadata marks it as non-reasoning"
        ),
        EffortValidationMode::ExplicitList => {
            if info.supported_effort_levels.contains(effort) {
                Ok(())
            } else if info.supported_effort_levels.is_empty() {
                bail!(
                    "reasoning effort `{effort}` is not supported by model `{model_id}`; cached metadata reports no supported effort levels"
                )
            } else {
                let supported = info
                    .supported_effort_levels
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "reasoning effort `{effort}` is not supported by model `{model_id}`; supported levels: {supported}"
                )
            }
        }
    }
}

fn build_catalog(
    base_url: &str,
    provider_models: Vec<RemoteModelRecord>,
    matched_provider: Option<&ModelsDevProviderRecord>,
    models_dev_models: Option<&BTreeMap<String, RemoteModelRecord>>,
) -> ModelCatalog {
    let mut models = BTreeMap::new();

    for model in provider_models {
        let metadata = matched_provider
            .and_then(|provider| {
                provider
                    .models
                    .get(&model.id)
                    .map(|matched| ModelMetadataRecord::ModelsDevApi {
                        provider_id: provider.id.as_str(),
                        api_url: provider.api.as_deref(),
                        model: matched,
                    })
            })
            .or_else(|| {
                matched_provider
                    .is_none()
                    .then(|| {
                        models_dev_models
                            .and_then(|all| all.get(&model.id))
                            .map(ModelMetadataRecord::ModelsDevModels)
                    })
                    .flatten()
            })
            .unwrap_or(ModelMetadataRecord::Provider(&model));

        let cached = cached_model_from_metadata(&model.id, &metadata);
        models.insert(model.id.clone(), cached);
    }

    ModelCatalog {
        version: MODEL_CATALOG_VERSION,
        fetched_at: chrono::Utc::now().to_rfc3339(),
        provider: CachedProviderInfo {
            base_url: base_url.to_string(),
        },
        models,
    }
}

fn cached_model_from_metadata(id: &str, metadata: &ModelMetadataRecord<'_>) -> CachedModel {
    let record = match metadata {
        ModelMetadataRecord::Provider(record) => record,
        ModelMetadataRecord::ModelsDevApi { model, .. } => model,
        ModelMetadataRecord::ModelsDevModels(model) => model,
    };
    let (reasoning_effort_levels, reasoning_effort_levels_source) = extract_effort_levels(record);

    let metadata_source = match metadata {
        ModelMetadataRecord::Provider(_) => CachedMetadataSource {
            kind: CachedMetadataSourceKind::ProviderModelsEndpoint,
            provider_id: None,
            api_url: None,
        },
        ModelMetadataRecord::ModelsDevApi {
            provider_id,
            api_url,
            ..
        } => CachedMetadataSource {
            kind: CachedMetadataSourceKind::ModelsDevApi,
            provider_id: Some((*provider_id).to_string()),
            api_url: api_url.map(str::to_string),
        },
        ModelMetadataRecord::ModelsDevModels(_) => CachedMetadataSource {
            kind: CachedMetadataSourceKind::ModelsDevModels,
            provider_id: None,
            api_url: None,
        },
    };

    CachedModel {
        id: id.to_string(),
        display_name: record
            .display_name
            .clone()
            .or_else(|| record.name.clone())
            .unwrap_or_else(|| id.to_string()),
        context_window: record
            .context_window
            .or_else(|| record.limit.as_ref().and_then(|limit| limit.context)),
        max_output_tokens: record
            .max_output_tokens
            .or_else(|| record.limit.as_ref().and_then(|limit| limit.output)),
        reasoning: record.reasoning,
        reasoning_effort_levels: Some(reasoning_effort_levels),
        reasoning_effort_levels_source,
        metadata_source,
    }
}

fn extract_effort_levels(record: &RemoteModelRecord) -> (Vec<EffortLevel>, EffortLevelsSource) {
    let Some(options) = record.reasoning_options.as_ref() else {
        return (
            inferred_effort_levels(record.reasoning),
            EffortLevelsSource::Inferred,
        );
    };

    let levels = EffortLevel::canonical()
        .iter()
        .copied()
        .filter(|level| {
            options.iter().any(|option| {
                option.option_type == "effort"
                    && option.values.iter().any(|value| value == level.as_str())
            })
        })
        .collect();

    (levels, EffortLevelsSource::Explicit)
}

fn inferred_effort_levels(reasoning: Option<bool>) -> Vec<EffortLevel> {
    match reasoning {
        Some(false) => vec![],
        Some(true) | None => EffortLevel::canonical().to_vec(),
    }
}

fn normalize_base_url(value: &str) -> String {
    value.trim_end_matches('/').to_string()
}

async fn fetch_provider_models(
    client: &Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<RemoteModelRecord>> {
    let url = format!("{}/models", normalize_base_url(base_url));
    let response = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("provider models request failed with HTTP {status}: {body}");
    }

    let parsed: ProviderModelsResponse = response
        .json()
        .await
        .with_context(|| format!("parsing provider models response from {url}"))?;
    Ok(parsed.data)
}

async fn fetch_models_dev_api(
    client: &Client,
) -> Result<BTreeMap<String, ModelsDevProviderRecord>> {
    client
        .get(MODELS_DEV_API_URL)
        .send()
        .await
        .context("requesting models.dev api.json")?
        .error_for_status()
        .context("models.dev api.json returned an error status")?
        .json()
        .await
        .context("parsing models.dev api.json")
}

async fn fetch_models_dev_models(client: &Client) -> Result<BTreeMap<String, RemoteModelRecord>> {
    client
        .get(MODELS_DEV_MODELS_URL)
        .send()
        .await
        .context("requesting models.dev models.json")?
        .error_for_status()
        .context("models.dev models.json returned an error status")?
        .json()
        .await
        .context("parsing models.dev models.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_model(id: &str) -> RemoteModelRecord {
        RemoteModelRecord {
            id: id.to_string(),
            display_name: Some(format!("Provider {id}")),
            ..Default::default()
        }
    }

    #[test]
    fn catalog_uses_exact_ids_only() {
        let provider_models = vec![provider_model("alpha"), provider_model("beta")];
        let provider = ModelsDevProviderRecord {
            id: "provider".into(),
            api: Some("https://example.test/v1".into()),
            models: BTreeMap::from([
                (
                    "alpha".into(),
                    RemoteModelRecord {
                        id: "alpha".into(),
                        name: Some("Exact Alpha".into()),
                        ..Default::default()
                    },
                ),
                (
                    "gamma".into(),
                    RemoteModelRecord {
                        id: "gamma".into(),
                        name: Some("Should Not Appear".into()),
                        ..Default::default()
                    },
                ),
            ]),
        };

        let catalog = build_catalog(
            "https://example.test/v1",
            provider_models,
            Some(&provider),
            None,
        );

        assert_eq!(catalog.models.len(), 2);
        assert_eq!(
            catalog.models.get("alpha").unwrap().display_name,
            "Exact Alpha"
        );
        assert!(catalog.models.get("gamma").is_none());
    }

    #[test]
    fn exact_api_match_prefers_models_dev_api() {
        let provider_models = vec![provider_model("alpha")];
        let provider = ModelsDevProviderRecord {
            id: "provider".into(),
            api: Some("https://example.test/v1".into()),
            models: BTreeMap::from([(
                "alpha".into(),
                RemoteModelRecord {
                    id: "alpha".into(),
                    name: Some("API Alpha".into()),
                    reasoning: Some(true),
                    limit: Some(RemoteModelLimit {
                        context: Some(123),
                        output: Some(456),
                    }),
                    ..Default::default()
                },
            )]),
        };
        let global = BTreeMap::from([(
            "alpha".into(),
            RemoteModelRecord {
                id: "alpha".into(),
                name: Some("Global Alpha".into()),
                ..Default::default()
            },
        )]);

        let catalog = build_catalog(
            "https://example.test/v1",
            provider_models,
            Some(&provider),
            Some(&global),
        );
        let model = catalog.models.get("alpha").unwrap();

        assert_eq!(model.display_name, "API Alpha");
        assert_eq!(model.context_window, Some(123));
        assert_eq!(
            model.metadata_source.kind,
            CachedMetadataSourceKind::ModelsDevApi
        );
    }

    #[test]
    fn falls_back_to_provider_agnostic_models_when_api_url_does_not_match() {
        let provider_models = vec![provider_model("alpha")];
        let global = BTreeMap::from([(
            "alpha".into(),
            RemoteModelRecord {
                id: "alpha".into(),
                name: Some("Global Alpha".into()),
                reasoning: Some(false),
                ..Default::default()
            },
        )]);

        let catalog = build_catalog(
            "https://other.test/v1",
            provider_models,
            None,
            Some(&global),
        );
        let model = catalog.models.get("alpha").unwrap();

        assert_eq!(model.display_name, "Global Alpha");
        assert_eq!(
            model.metadata_source.kind,
            CachedMetadataSourceKind::ModelsDevModels
        );
    }

    #[test]
    fn reasoning_options_keep_only_canonical_effort_levels() {
        let record = RemoteModelRecord {
            id: "alpha".into(),
            reasoning: Some(true),
            reasoning_options: Some(vec![
                RawReasoningOption {
                    option_type: "toggle".into(),
                    values: vec![],
                },
                RawReasoningOption {
                    option_type: "effort".into(),
                    values: vec![
                        "none".into(),
                        "low".into(),
                        "medium".into(),
                        "minimal".into(),
                        "xhigh".into(),
                        "max".into(),
                    ],
                },
                RawReasoningOption {
                    option_type: "budget_tokens".into(),
                    values: vec![],
                },
            ]),
            ..Default::default()
        };

        let (levels, source) = extract_effort_levels(&record);
        assert_eq!(
            levels,
            vec![
                EffortLevel::Low,
                EffortLevel::Medium,
                EffortLevel::Xhigh,
                EffortLevel::Max
            ]
        );
        assert_eq!(source, EffortLevelsSource::Explicit);
    }

    #[test]
    fn saves_and_loads_catalog() {
        let tmp = std::env::temp_dir().join(format!("mu-models-{}", uuid::Uuid::new_v4()));
        let path = tmp.join("models.json");
        let catalog = ModelCatalog {
            version: 1,
            fetched_at: "2026-06-26T00:00:00Z".into(),
            provider: CachedProviderInfo {
                base_url: "https://example.test/v1".into(),
            },
            models: BTreeMap::from([(
                "alpha".into(),
                CachedModel {
                    id: "alpha".into(),
                    display_name: "Alpha".into(),
                    context_window: Some(1),
                    max_output_tokens: Some(2),
                    reasoning: Some(true),
                    reasoning_effort_levels: Some(vec![EffortLevel::Low]),
                    reasoning_effort_levels_source: EffortLevelsSource::Explicit,
                    metadata_source: CachedMetadataSource {
                        kind: CachedMetadataSourceKind::ProviderModelsEndpoint,
                        provider_id: None,
                        api_url: None,
                    },
                },
            )]),
        };

        catalog.save(&path).unwrap();
        let loaded = ModelCatalog::load(&path).unwrap().unwrap();

        assert_eq!(loaded.provider.base_url, "https://example.test/v1");
        assert_eq!(loaded.models["alpha"].display_name, "Alpha");
        let _ = std::fs::remove_dir_all(tmp);
    }
}
