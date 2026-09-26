//! The models behind the platform, as configuration a person keeps: each
//! provider is an OpenAI-compatible endpoint with a model name, a sealed
//! API key, optional request extras and prices; one is primary, one may be
//! the fallback. The graph holds a [`SwappableChatModel`]; what is behind it
//! is built from this configuration at boot and on every change, so a
//! change takes effect without a restart. The environment (`RUSTY_LLM_*`,
//! `RUSTY_LLM_FALLBACK_*`) seeds the configuration once, when the store
//! holds none; from then on the store is the truth and the studio edits it.

use std::sync::Arc;

use axum::extract::{Path, State as AxumState};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use rusty_agent_runtime::broker::SealedCredential;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, FallbackChatModel, ModelPricing, OpenAiCompatibleClient};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

/// One provider: where, which model, how it authenticates (sealed), what
/// rides in every request, and what it costs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRecord {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<SealedCredential>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_body: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_input_per_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_output_per_m: Option<f64>,
    /// USD per million cache-served prompt tokens; without it, cached
    /// tokens bill at the full input rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cached_input_per_m: Option<f64>,
    pub created_at: DateTime<Utc>,
}

/// The configuration: the providers, which one answers first, which one
/// answers when it cannot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmConfig {
    #[serde(default)]
    pub providers: Vec<ProviderRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

impl LlmConfig {
    pub fn provider(&self, id: &str) -> Option<&ProviderRecord> {
        self.providers.iter().find(|p| p.id == id)
    }
}

fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.trim().to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_owned()
}

/// A provider's name from its endpoint when nobody named it: the host's
/// second-level label (`api.fireworks.ai` → `fireworks`), or the host.
fn name_from(base_url: &str) -> String {
    let host = base_url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_owned();
    let labels: Vec<&str> = host.split('.').collect();
    match labels.len() {
        0 => "provider".to_owned(),
        1 => labels[0].to_owned(),
        n => labels[n - 2].to_owned(),
    }
}

fn owner(id: &str) -> String {
    format!("llm:{id}")
}

/// What the environment says a provider is, before it is sealed.
struct EnvProvider {
    name: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    extra_body: Option<serde_json::Map<String, Value>>,
    price_input_per_m: Option<f64>,
    price_output_per_m: Option<f64>,
    price_cached_input_per_m: Option<f64>,
}

fn env_provider(prefix: &str) -> Option<EnvProvider> {
    let var = |suffix: &str| std::env::var(format!("{prefix}{suffix}")).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    let base_url = var("BASE_URL")?;
    let model = var("MODEL")?;
    let extra_body = var("EXTRA_BODY").and_then(|raw| serde_json::from_str::<serde_json::Map<String, Value>>(&raw).ok());
    let price = |suffix: &str| var(suffix).and_then(|v| v.parse::<f64>().ok()).filter(|v| *v >= 0.0);
    Some(EnvProvider {
        name: var("NAME").unwrap_or_else(|| name_from(&base_url)),
        base_url,
        model,
        api_key: var("API_KEY"),
        extra_body,
        price_input_per_m: price("PRICE_INPUT_PER_M"),
        price_output_per_m: price("PRICE_OUTPUT_PER_M"),
        price_cached_input_per_m: price("PRICE_CACHED_INPUT_PER_M"),
    })
}

async fn seal_key(state: &AppState, id: &str, key: &str) -> Result<SealedCredential, String> {
    state
        .broker
        .seal_connector_secret(&owner(id), key.as_bytes())
        .await
        .map_err(|e| format!("the key could not be sealed: {e}"))
}

async fn record_from_env(state: &AppState, env: EnvProvider, taken: &[String]) -> Result<ProviderRecord, String> {
    let mut id = slug(&env.name);
    if id.is_empty() {
        id = "provider".to_owned();
    }
    let base = id.clone();
    let mut n = 2;
    while taken.contains(&id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    let api_key = match &env.api_key {
        Some(key) => Some(seal_key(state, &id, key).await?),
        None => None,
    };
    Ok(ProviderRecord {
        id,
        name: env.name,
        base_url: env.base_url,
        model: env.model,
        api_key,
        extra_body: env.extra_body,
        price_input_per_m: env.price_input_per_m,
        price_output_per_m: env.price_output_per_m,
        price_cached_input_per_m: env.price_cached_input_per_m,
        created_at: Utc::now(),
    })
}

/// The configuration the environment describes: `RUSTY_LLM_*` as the
/// primary, `RUSTY_LLM_FALLBACK_*` as the fallback. `None` when the
/// environment names no model.
async fn config_from_env(state: &AppState) -> Result<Option<LlmConfig>, String> {
    let Some(primary) = env_provider("RUSTY_LLM_") else {
        return Ok(None);
    };
    let primary = record_from_env(state, primary, &[]).await?;
    let mut config = LlmConfig { primary: Some(primary.id.clone()), providers: vec![primary], fallback: None, updated_at: Some(Utc::now()) };
    if let Some(fallback) = env_provider("RUSTY_LLM_FALLBACK_") {
        let taken: Vec<String> = config.providers.iter().map(|p| p.id.clone()).collect();
        let fallback = record_from_env(state, fallback, &taken).await?;
        config.fallback = Some(fallback.id.clone());
        config.providers.push(fallback);
    }
    Ok(Some(config))
}

/// The client for one provider, its key opened for this process only.
async fn client_for(state: &AppState, p: &ProviderRecord) -> Result<OpenAiCompatibleClient, String> {
    let key = match &p.api_key {
        Some(envelope) => {
            let bytes = state
                .broker
                .open_connector_secret(&owner(&p.id), envelope)
                .await
                .map_err(|e| format!("the key of `{}` could not be opened: {e}", p.id))?;
            Some(String::from_utf8(bytes).map_err(|_| format!("the key of `{}` is not text", p.id))?)
        }
        None => None,
    };
    let mut client = OpenAiCompatibleClient::new(&p.base_url, key, p.model.clone());
    if let Some(extra) = &p.extra_body {
        client = client.with_extra_body(extra.clone());
    }
    if let (Some(input), Some(output)) = (p.price_input_per_m, p.price_output_per_m) {
        let mut pricing = ModelPricing::new(input, output);
        if let Some(cached) = p.price_cached_input_per_m {
            pricing = pricing.with_cached_input(cached);
        }
        client = client.with_pricing(pricing);
    }
    Ok(client)
}

fn label_of(p: &ProviderRecord) -> String {
    format!("{} ({} via {})", p.name, p.model, p.base_url)
}

/// The model the configuration describes: the primary, with the fallback
/// behind it when one is named. `None` when nothing is primary.
pub(crate) async fn build_model(state: &AppState, config: &LlmConfig) -> Result<Option<(Arc<dyn ChatModel>, String)>, String> {
    let Some(primary_id) = &config.primary else {
        return Ok(None);
    };
    let primary = config.provider(primary_id).ok_or_else(|| format!("primary `{primary_id}` is not among the providers"))?;
    let primary_client: Arc<dyn ChatModel> = Arc::new(client_for(state, primary).await?);
    let fallback = match &config.fallback {
        Some(id) if id != primary_id => Some(config.provider(id).ok_or_else(|| format!("fallback `{id}` is not among the providers"))?),
        _ => None,
    };
    match fallback {
        Some(f) => {
            let fallback_client: Arc<dyn ChatModel> = Arc::new(client_for(state, f).await?);
            let model = FallbackChatModel::new(primary_client, label_of(primary), fallback_client, label_of(f));
            Ok(Some((Arc::new(model), format!("{}, falling back to {}", label_of(primary), label_of(f)))))
        }
        None => Ok(Some((primary_client, label_of(primary)))),
    }
}

/// One provider as a run would call it: the client, with the deployment's
/// fallback behind it when the fallback is another provider.
async fn model_for(state: &AppState, config: &LlmConfig, id: &str) -> Result<(Arc<dyn ChatModel>, String), String> {
    let p = config.provider(id).ok_or_else(|| format!("`{id}` is not among the providers"))?;
    let client: Arc<dyn ChatModel> = Arc::new(client_for(state, p).await?);
    match &config.fallback {
        Some(f) if f != id => {
            let fp = config.provider(f).ok_or_else(|| format!("fallback `{f}` is not among the providers"))?;
            let fallback_client: Arc<dyn ChatModel> = Arc::new(client_for(state, fp).await?);
            Ok((Arc::new(FallbackChatModel::new(client, label_of(p), fallback_client, label_of(fp))), format!("{}, falling back to {}", label_of(p), label_of(fp))))
        }
        _ => Ok((client, label_of(p))),
    }
}

/// Build from the configuration and put it behind the handle: the primary
/// answers by default, and every provider stands ready under its id for an
/// agent that names it.
pub(crate) async fn apply(state: &AppState, config: &LlmConfig) -> Result<String, String> {
    let Some(handle) = &state.model_handle else {
        return Err("this server's model is fixed at boot; it holds no swappable handle".to_owned());
    };
    match build_model(state, config).await? {
        Some((model, label)) => {
            handle.swap(model, label.clone());
            handle.clear_named();
            handle.set_primary_id(config.primary.clone());
            for p in &config.providers {
                match model_for(state, config, &p.id).await {
                    Ok((m, l)) => handle.set_named(p.id.clone(), m, l),
                    Err(e) => tracing::warn!(id = %p.id, %e, "provider not available as an agent's model"),
                }
                // Bare as well, for an agent that pairs it with its own fallback.
                match client_for(state, p).await {
                    Ok(client) => handle.set_bare(p.id.clone(), Arc::new(client), label_of(p)),
                    Err(e) => tracing::warn!(id = %p.id, %e, "provider not available bare"),
                }
            }
            tracing::info!(%label, "model providers applied");
            Ok(label)
        }
        None => Err("no provider is primary".to_owned()),
    }
}

/// Boot: the stored configuration wins; the environment seeds the store
/// once when it holds none; then whatever is primary goes behind the handle.
pub(crate) fn restore(state: Arc<AppState>) {
    tokio::spawn(async move {
        if state.model_handle.is_none() {
            return;
        }
        let stored = match state.server_store.get_llm_config().await {
            Ok(stored) => stored,
            Err(error) => {
                tracing::warn!(%error, "model providers: the store could not be read; the boot model stays");
                return;
            }
        };
        let config = match stored {
            Some(config) if config.primary.is_some() => config,
            _ => match config_from_env(&state).await {
                Ok(Some(config)) => {
                    if let Err(error) = state.server_store.put_llm_config(&config).await {
                        tracing::warn!(%error, "model providers: seeded from the environment but not stored");
                    } else {
                        tracing::info!(providers = config.providers.len(), "model providers seeded from the environment");
                    }
                    config
                }
                Ok(None) => return,
                Err(error) => {
                    tracing::warn!(%error, "model providers: the environment's provider could not be kept");
                    return;
                }
            },
        };
        if let Err(error) = apply(&state, &config).await {
            tracing::warn!(%error, "model providers: the configuration could not be applied; the boot model stays");
        }
    });
}

fn served_provider(p: &ProviderRecord, config: &LlmConfig) -> Value {
    json!({
        "id": p.id,
        "name": p.name,
        "base_url": p.base_url,
        "model": p.model,
        "has_key": p.api_key.is_some(),
        "extra_body": p.extra_body,
        "price_input_per_m": p.price_input_per_m,
        "price_output_per_m": p.price_output_per_m,
        "price_cached_input_per_m": p.price_cached_input_per_m,
        "created_at": p.created_at,
        "role": if config.primary.as_deref() == Some(&p.id) { "primary" } else if config.fallback.as_deref() == Some(&p.id) { "fallback" } else { "" },
    })
}

pub(crate) fn served(config: &LlmConfig, state: &AppState) -> Value {
    json!({
        "providers": config.providers.iter().map(|p| served_provider(p, config)).collect::<Vec<_>>(),
        "primary": config.primary,
        "fallback": config.fallback,
        "updated_at": config.updated_at,
        "active": state.model_handle.as_ref().map(|h| h.label()),
    })
}

/// What `/info` says about the models: enough to name them, never a key.
pub(crate) async fn info(state: &AppState) -> Value {
    let config = state.server_store.get_llm_config().await.ok().flatten().unwrap_or_default();
    let brief = |id: &Option<String>| id.as_ref().and_then(|id| config.provider(id)).map(|p| json!({"id": p.id, "name": p.name, "model": p.model}));
    json!({
        "primary": brief(&config.primary),
        "fallback": brief(&config.fallback),
        "active": state.model_handle.as_ref().map(|h| h.label()),
    })
}

/// `GET /llm/providers` — the configuration, keys shown as held or not.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ProvidersQuery {
    /// `?cache=1` adds cache hits per model over the newest journalled runs —
    /// a scan of forty journals, so it is asked for, never served by default.
    #[serde(default)]
    cache: Option<String>,
}

pub(crate) async fn get_providers(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(_tenant): Extension<TenantContext>,
    axum::extract::Query(query): axum::extract::Query<ProvidersQuery>,
) -> Result<Json<Value>, ApiError> {
    let config = state
        .server_store
        .get_llm_config()
        .await
        .map_err(|e| ApiError::internal(format!("model providers: {e}")))?
        .unwrap_or_default();
    let mut body = served(&config, &state);
    // Cache hits per model over the newest journaled runs, so a provider's
    // row can say how much of its prompts it served from cache — on request:
    // the scan takes seconds on a full store, and the list must not wait.
    if matches!(query.cache.as_deref(), Some("1" | "true")) {
        body["cache"] = cache_stats(&state, 40).await;
    }
    Ok(Json(body))
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProviderPayload {
    #[serde(default)]
    id: Option<String>,
    name: String,
    base_url: String,
    model: String,
    /// The key, when set or changed; omitted keeps the one held.
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    extra_body: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    price_input_per_m: Option<f64>,
    #[serde(default)]
    price_output_per_m: Option<f64>,
    /// USD per million cache-served prompt tokens; without it, cached
    /// tokens bill at the full input rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    price_cached_input_per_m: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PutProvidersPayload {
    providers: Vec<ProviderPayload>,
    primary: Option<String>,
    #[serde(default)]
    fallback: Option<String>,
}

/// `PUT /llm/providers` — the whole configuration, as the studio holds it:
/// every provider (a new key seals; an omitted key keeps the one held), the
/// primary and the fallback. Stored, then applied: the next model call goes
/// to the new primary.
pub(crate) async fn put_providers(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(_tenant): Extension<TenantContext>,
    Json(payload): Json<PutProvidersPayload>,
) -> Result<Json<Value>, ApiError> {
    let existing = state
        .server_store
        .get_llm_config()
        .await
        .map_err(|e| ApiError::internal(format!("model providers: {e}")))?
        .unwrap_or_default();
    if payload.providers.is_empty() {
        return Err(ApiError::bad_request("name at least one provider".to_owned()));
    }
    let mut providers: Vec<ProviderRecord> = Vec::new();
    for p in &payload.providers {
        let name = p.name.trim();
        if name.is_empty() {
            return Err(ApiError::bad_request("every provider needs a name".to_owned()));
        }
        let base_url = p.base_url.trim().trim_end_matches('/').to_owned();
        if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
            return Err(ApiError::bad_request(format!("`{name}`: the API root must be an http(s) URL")));
        }
        if p.model.trim().is_empty() {
            return Err(ApiError::bad_request(format!("`{name}`: name the model")));
        }
        let id = p.id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).unwrap_or_else(|| slug(name));
        if id.is_empty() || providers.iter().any(|q| q.id == id) {
            return Err(ApiError::bad_request(format!("`{name}`: the id `{id}` is empty or taken")));
        }
        let held = existing.provider(&id);
        let api_key = match p.api_key.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(key) => Some(seal_key(&state, &id, key).await.map_err(ApiError::internal)?),
            None => held.and_then(|h| h.api_key.clone()),
        };
        providers.push(ProviderRecord {
            id,
            name: name.to_owned(),
            base_url,
            model: p.model.trim().to_owned(),
            api_key,
            extra_body: p.extra_body.clone().filter(|m| !m.is_empty()),
            price_input_per_m: p.price_input_per_m.filter(|v| *v >= 0.0),
            price_output_per_m: p.price_output_per_m.filter(|v| *v >= 0.0),
            price_cached_input_per_m: p.price_cached_input_per_m.filter(|v| *v >= 0.0),
            created_at: held.map(|h| h.created_at).unwrap_or_else(Utc::now),
        });
    }
    let primary = payload.primary.clone().or_else(|| providers.first().map(|p| p.id.clone()));
    if let Some(id) = &primary {
        if !providers.iter().any(|p| &p.id == id) {
            return Err(ApiError::bad_request(format!("primary `{id}` is not among the providers")));
        }
    }
    let fallback = payload.fallback.clone().filter(|id| Some(id) != primary.as_ref());
    if let Some(id) = &fallback {
        if !providers.iter().any(|p| &p.id == id) {
            return Err(ApiError::bad_request(format!("fallback `{id}` is not among the providers")));
        }
    }
    let config = LlmConfig { providers, primary, fallback, updated_at: Some(Utc::now()) };
    state
        .server_store
        .put_llm_config(&config)
        .await
        .map_err(|e| ApiError::internal(format!("model providers: {e}")))?;
    let applied = apply(&state, &config).await;
    let mut body = served(&config, &state);
    match applied {
        Ok(label) => body["applied"] = json!(label),
        Err(error) => body["applied_error"] = json!(error),
    }
    Ok(Json(body))
}

/// `POST /llm/providers/{id}/test` — one small call to the provider, with
/// its held key: does it answer, with which model, how fast.
pub(crate) async fn test_provider(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(_tenant): Extension<TenantContext>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let config = state
        .server_store
        .get_llm_config()
        .await
        .map_err(|e| ApiError::internal(format!("model providers: {e}")))?
        .unwrap_or_default();
    let provider = config.provider(&id).ok_or_else(|| ApiError::not_found(format!("no provider `{id}`")))?;
    let client = client_for(&state, provider).await.map_err(ApiError::internal)?;
    let started = std::time::Instant::now();
    let messages = vec![ChatMessage::system("Answer with the single word: ok"), ChatMessage::user("Are you there?")];
    match client.chat(&messages, &[]).await {
        Ok(response) => Ok(Json(json!({
            "ok": true,
            "id": id,
            "model": response.model.unwrap_or_else(|| provider.model.clone()),
            "latency_ms": started.elapsed().as_millis() as u64,
            "said": response.message.content.unwrap_or_default().chars().take(120).collect::<String>(),
        }))),
        Err(error) => Ok(Json(json!({
            "ok": false,
            "id": id,
            "latency_ms": started.elapsed().as_millis() as u64,
            "error": error.to_string().chars().take(400).collect::<String>(),
        }))),
    }
}

/// The token usage of one run's model calls, summed from its journal:
/// prompt, of which cached; completion; the number of calls. Absent
/// (null) when the run left no journal.
pub(crate) async fn run_usage(state: &AppState, run_id: &str) -> Value {
    let Ok(Some(snapshot)) = state.server_store.get_journal(run_id).await else {
        return Value::Null;
    };
    usage_of(&snapshot.events)
}

pub(crate) fn usage_of(events: &[rusty_agent_runtime::record::RunEvent]) -> Value {
    let (mut prompt, mut cached, mut completion, mut calls, mut reported) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for event in events.iter().filter(|e| e.kind == rusty_agent_runtime::record::RunEventKind::ModelCall) {
        calls += 1;
        if let Some(usage) = &event.tokens {
            prompt += usage.prompt_tokens;
            completion += usage.completion_tokens;
            if let Some(c) = usage.cached_tokens {
                cached += c;
                reported += 1;
            }
        }
    }
    json!({
        "model_calls": calls,
        "prompt_tokens": prompt,
        "cached_tokens": cached,
        "completion_tokens": completion,
        // How many calls reported a cache figure at all: a provider that
        // reports none shows 0 hits and 0 reporting, not a 0 % hit rate.
        "cache_reported_calls": reported,
        "cache_hit_rate": if prompt > 0 && reported > 0 { Some(cached as f64 / prompt as f64) } else { None },
    })
}

/// Cache hits per model over the newest journaled runs: prompt tokens,
/// cached tokens, the hit rate, the calls — the number a provider's row in
/// Config shows. Read from the journals (the newest `runs` of them).
pub(crate) async fn cache_stats(state: &AppState, runs: usize) -> Value {
    let Ok(mut journals) = state.server_store.list_journals().await else {
        return json!({});
    };
    journals.sort_by(|a, b| b.events.first().map(|e| e.recorded_at).cmp(&a.events.first().map(|e| e.recorded_at)));
    journals.truncate(runs);
    let mut per_model: std::collections::BTreeMap<String, (u64, u64, u64, u64)> = std::collections::BTreeMap::new();
    for snapshot in &journals {
        for event in snapshot.events.iter().filter(|e| e.kind == rusty_agent_runtime::record::RunEventKind::ModelCall) {
            let Some(usage) = &event.tokens else { continue };
            let model = crate::replay::resolve(snapshot, event.output.as_ref())
                .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_owned());
            let entry = per_model.entry(model).or_insert((0, 0, 0, 0));
            entry.0 += 1;
            entry.1 += usage.prompt_tokens;
            if let Some(c) = usage.cached_tokens {
                entry.2 += c;
                entry.3 += 1;
            }
        }
    }
    let models: serde_json::Map<String, Value> = per_model
        .into_iter()
        .map(|(model, (calls, prompt, cached, reported))| {
            (model, json!({
                "calls": calls,
                "prompt_tokens": prompt,
                "cached_tokens": cached,
                "cache_reported_calls": reported,
                "cache_hit_rate": if prompt > 0 && reported > 0 { Some(cached as f64 / prompt as f64) } else { None },
            }))
        })
        .collect();
    json!({"runs": journals.len(), "models": models})
}

#[cfg(test)]
mod tests {
    use super::{name_from, slug};

    #[test]
    fn a_provider_is_named_from_its_host_and_slugged() {
        assert_eq!(name_from("https://api.fireworks.ai/inference/v1"), "fireworks");
        assert_eq!(name_from("http://100.123.104.44:8000/v1"), "104");
        assert_eq!(name_from("https://api.moonshot.ai/v1"), "moonshot");
        assert_eq!(slug("GPU box (Qwen)"), "gpu-box-qwen");
    }
}

