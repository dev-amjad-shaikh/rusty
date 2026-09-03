//! The connector surface's server half (schema-driven configuration,
//! `docs/connector-surface-design.md`): the file layout behind
//! `server_store.rs`'s [`ConnectorPlane`], the `/connectors/*` HTTP
//! surface, the real check transport, and the secret-sealing bridge to
//! the credential broker.
//!
//! Layout under `{store_path}/connectors/` (the knowledge plane's
//! conventions exactly — one JSON file per record, tenant
//! subdirectories for named tenants, atomic temp-file-plus-rename
//! writes, corrupt-tolerant boot loads):
//!
//! ```text
//! connectors/
//!   manifests/{scoped_hash}.json       ConnectorManifest records
//!   instances/{scoped_id}.json         ConnectorInstance records
//! ```
//!
//! `scoped_*` keys are tenant-scoped (`{tenant}/{id}` for named tenants,
//! bare for the default tenant — [`crate::auth::scope_id`]), so the
//! surface is tenant-isolated at the storage layer: cross-tenant reads
//! are indistinguishable from absence, and the HTTP surface answers them
//! `404` — never `403`.
//!
//! **Secrets.** Registration validates the config against the manifest's
//! `connection_specification` (a rejection is a 422 naming the failing
//! schema path), then extracts every `rusty_secret` field and seals it
//! through the broker ([`Broker::seal_connector_secret`]) under the
//! tenant-scoped instance id as associated data. The persisted record
//! holds the non-secret config plus the sealed envelopes — ciphertext
//! only, so a store leak is not a credential leak. Secrets open
//! host-side at call time only (a live-instance check), into the
//! outbound request's auth material and nowhere else.
//!
//! **Check.** `POST /connectors/check` runs the manifest's check
//! operation either pre-save (`{manifest_hash, config}` — the setup
//! gate) or against a live instance (`{instance_id}` — the edit gate),
//! answering the Airbyte verdict contract `{"status", "message"?}`.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as AxumPath, State as AxumState};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use rusty_agent_runtime::connector::{
    execute_check, extract_secrets, insert_masked_secrets, insert_opened_secrets, validate_config,
    without_secrets, CheckRequest, CheckResponse, ConnectorInstance, ConnectorManifest,
    ConnectorTransport, INSTANCE_ID_PREFIX,
};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

// --------------------------------------------------------------------- //
// File layout and IO (the server_store persistence section's helpers)
// --------------------------------------------------------------------- //

/// The connectors directory under the store root. `connectors` is a
/// reserved layout name (see [`crate::RESERVED_NAMES`]): client-chosen
/// thread ids may not claim it.
pub(crate) fn dir(root: &Path) -> PathBuf {
    root.join("connectors")
}

fn manifests_dir(root: &Path) -> PathBuf {
    dir(root).join("manifests")
}

fn instances_dir(root: &Path) -> PathBuf {
    dir(root).join("instances")
}

/// Persist one JSON record atomically (temp file + rename — a crash
/// mid-write must never leave a truncated record behind). The scoped key
/// may carry a `{tenant}/` prefix, so the parent directory is created,
/// not just the flat dir.
async fn persist_json(
    dir: &Path,
    scoped_key: &str,
    record: &impl serde::Serialize,
) -> Result<(), String> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("create {}: {e}", dir.display()))?;
    let bytes =
        serde_json::to_vec_pretty(record).map_err(|e| format!("serialize {scoped_key}: {e}"))?;
    let path = dir.join(format!("{scoped_key}.json"));
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = dir.join(format!("{scoped_key}.tmp"));
    tokio::fs::write(&tmp, &bytes)
        .await
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    tokio::fs::rename(&tmp, &path)
        .await
        .map_err(|e| format!("rename {}: {e}", path.display()))
}

/// Load every record under `dir` (recursing into tenant subdirectories),
/// keyed by the `{tenant}/`-prefixed path relative to `dir`. Corrupt
/// files are skipped — a boot must not fail on one bad record (the
/// knowledge plane's corrupt-tolerant convention).
fn load_records<T: serde::de::DeserializeOwned>(dir: &Path) -> Vec<(String, T)> {
    fn walk(dir: &Path, prefix: &str, out: &mut Vec<(String, Value)>) -> io::Result<()> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(()); // absent plane directory: an empty plane
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                walk(&path, &format!("{prefix}{name}/"), out)?;
            } else if let Some(id) = name.strip_suffix(".json") {
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                        out.push((format!("{prefix}{id}"), value));
                    }
                }
            }
        }
        Ok(())
    }
    let mut raw = Vec::new();
    let _ = walk(dir, "", &mut raw);
    raw.into_iter()
        .filter_map(|(key, value)| serde_json::from_value(value).ok().map(|r| (key, r)))
        .collect()
}

pub(crate) fn load_manifests(root: &Path) -> Vec<(String, ConnectorManifest)> {
    load_records(&manifests_dir(root))
}

pub(crate) fn load_instances(root: &Path) -> Vec<(String, ConnectorInstance)> {
    load_records(&instances_dir(root))
}

pub(crate) async fn persist_manifest(
    root: &Path,
    scoped_hash: &str,
    manifest: &ConnectorManifest,
) -> Result<(), String> {
    persist_json(&manifests_dir(root), scoped_hash, manifest).await
}

pub(crate) async fn persist_instance(
    root: &Path,
    scoped_id: &str,
    instance: &ConnectorInstance,
) -> Result<(), String> {
    persist_json(&instances_dir(root), scoped_id, instance).await
}

// --------------------------------------------------------------------- //
// The real check transport
// --------------------------------------------------------------------- //

/// The real HTTP transport: reqwest behind the core
/// [`ConnectorTransport`] seam. The response body is read as a stream
/// with the request's byte ceiling enforced *during* the read — the
/// ceiling trips before the allocation grows past it, so a hostile or
/// buggy endpoint cannot make the server buffer unbounded bytes.
///
/// EP-11-S03: when an egress policy is configured, every outbound
/// request is evaluated before the wire call; a denial returns a typed
/// error without issuing the request.
///
/// EP-11-S04: DNS preflight runs before connect; the connection uses
/// the exact IP the preflight pinned; HTTP redirects are re-evaluated
/// against the full policy before they are followed.
#[derive(Debug)]
pub(crate) struct ReqwestConnectorTransport {
    client: reqwest::Client,
    /// The deployment's L7 egress policy. `None` means open.
    policy: Option<Arc<rusty_agent_runtime::egress::EgressPolicy>>,
    /// The component identity attributed to this traffic per
    /// `contracts:turn-stamp`.
    originating_component: String,
    /// Maximum redirect hops before giving up.
    max_redirects: u8,
}

impl ReqwestConnectorTransport {
    /// Create a transport with the given policy and component attribution.
    fn new(
        client: reqwest::Client,
        policy: Option<Arc<rusty_agent_runtime::egress::EgressPolicy>>,
        originating_component: impl Into<String>,
    ) -> Self {
        Self {
            client,
            policy,
            originating_component: originating_component.into(),
            max_redirects: 10,
        }
    }

    /// EP-11-S04: resolve the hostname, run preflight against the
    /// matching endpoint, and return the pinned IP or a typed denial.
    async fn preflight(
        &self,
        policy: &rusty_agent_runtime::egress::EgressPolicy,
        host: &str,
        port: u16,
    ) -> rusty_agent_runtime::error::Result<String> {
        let resolved: Vec<String> =
            match tokio::net::lookup_host(format!("{}:{}", host, port)).await {
                Ok(addrs) => addrs.map(|sa| sa.ip().to_string()).collect(),
                Err(e) => {
                    return Err(rusty_agent_runtime::error::RustyError::Tool(format!(
                        "egress: DNS resolution failed for {}: {e}",
                        host
                    )));
                }
            };

        let endpoint_policy = rusty_agent_runtime::egress::find_endpoint_policy(
            policy,
            host,
            port,
            rusty_agent_runtime::egress::EgressProtocol::Rest,
        );

        let Some(ep) = endpoint_policy else {
            // Defensive: evaluate_egress should have already found this,
            // but if it didn't, deny rather than proceed unchecked.
            return Err(rusty_agent_runtime::error::RustyError::Tool(format!(
                "egress: no endpoint policy for {}:{}",
                host, port
            )));
        };

        match rusty_agent_runtime::egress::preflight_egress(&ep.endpoint, &resolved) {
            rusty_agent_runtime::egress::PreflightResult::Allowed { ip } => Ok(ip),
            rusty_agent_runtime::egress::PreflightResult::Denied { reason, detail } => {
                tracing::info!(
                    url_host = %host,
                    component = %self.originating_component,
                    reason = ?reason,
                    detail = %detail,
                    "egress preflight denial"
                );
                Err(rusty_agent_runtime::error::RustyError::Tool(format!(
                    "egress denied: {reason:?} — {detail}"
                )))
            }
        }
    }
}

#[async_trait::async_trait]
impl ConnectorTransport for ReqwestConnectorTransport {
    async fn send(
        &self,
        mut request: CheckRequest,
    ) -> rusty_agent_runtime::error::Result<CheckResponse> {
        let mut redirect_count = 0u8;

        loop {
            // -----------------------------------------------------------------
            // EP-11-S03: evaluate egress before the wire call.
            // -----------------------------------------------------------------
            let url = match reqwest::Url::parse(&request.url) {
                Ok(u) => u,
                Err(e) => {
                    return Err(rusty_agent_runtime::error::RustyError::Tool(format!(
                        "egress: malformed URL `{}`: {e}",
                        request.url
                    )));
                }
            };
            let host = url.host_str().unwrap_or("");
            let port = url.port().unwrap_or(443);
            let path = url.path();
            let method_str = match request.method {
                rusty_agent_runtime::connector::HttpMethod::Get => "GET",
                rusty_agent_runtime::connector::HttpMethod::Post => "POST",
                rusty_agent_runtime::connector::HttpMethod::Patch => "PATCH",
                rusty_agent_runtime::connector::HttpMethod::Put => "PUT",
                rusty_agent_runtime::connector::HttpMethod::Delete => "DELETE",
            };

            if let Some(policy) = &self.policy {
                let decision = rusty_agent_runtime::egress::evaluate_egress(
                    policy,
                    host,
                    port,
                    rusty_agent_runtime::egress::EgressProtocol::Rest,
                    method_str,
                    path,
                    None, // tool_name: not applicable for connector checks
                    &self.originating_component,
                );
                match decision {
                    rusty_agent_runtime::egress::EgressDecision::Allow => {}
                    rusty_agent_runtime::egress::EgressDecision::Deny {
                        reason,
                        policy_name,
                    } => {
                        let msg = format!(
                            "egress denied: {reason:?} (policy: {})",
                            policy_name.as_deref().unwrap_or("<none>")
                        );
                        tracing::info!(%msg, url = %request.url, component = %self.originating_component, "egress denial");
                        return Err(rusty_agent_runtime::error::RustyError::Tool(msg));
                    }
                    rusty_agent_runtime::egress::EgressDecision::Audit {
                        policy_name,
                        rule_index,
                    } => {
                        tracing::info!(
                            url = %request.url,
                            component = %self.originating_component,
                            policy = %policy_name,
                            rule_index,
                            "egress audit-mode hit"
                        );
                    }
                }

                // ---------------------------------------------------------
                // EP-11-S04: DNS preflight + pinned-IP connect.
                // ---------------------------------------------------------
                let pinned_ip = self.preflight(policy, host, port).await?;

                // Rewrite URL to use the pinned IP, preserving the Host header.
                let mut pinned_url = url.clone();
                pinned_url.set_host(Some(&pinned_ip)).map_err(|_| {
                    rusty_agent_runtime::error::RustyError::Tool(format!(
                        "egress: cannot pin URL to {pinned_ip}"
                    ))
                })?;
                request.url = pinned_url.to_string();
            }

            // -----------------------------------------------------------------
            // Issue the wire call.
            // -----------------------------------------------------------------
            let transport_err = |e: reqwest::Error| {
                rusty_agent_runtime::error::RustyError::Tool(format!(
                    "connector: check transport failed: {e}"
                ))
            };
            let mut call = self
                .client
                .request(
                    match request.method {
                        rusty_agent_runtime::connector::HttpMethod::Get => reqwest::Method::GET,
                        rusty_agent_runtime::connector::HttpMethod::Post => reqwest::Method::POST,
                        rusty_agent_runtime::connector::HttpMethod::Patch => reqwest::Method::PATCH,
                        rusty_agent_runtime::connector::HttpMethod::Put => reqwest::Method::PUT,
                        rusty_agent_runtime::connector::HttpMethod::Delete => {
                            reqwest::Method::DELETE
                        }
                    },
                    &request.url,
                )
                .timeout(request.timeout);

            // Preserve the original hostname for SNI and the Host header.
            call = call.header("Host", host);

            for (name, value) in &request.headers {
                call = call.header(name, value);
            }
            let response = call.send().await.map_err(transport_err)?;
            let status = response.status().as_u16();

            // -----------------------------------------------------------------
            // EP-11-S04: redirect re-evaluation.
            // -----------------------------------------------------------------
            if (300..400).contains(&status) {
                if redirect_count >= self.max_redirects {
                    return Err(rusty_agent_runtime::error::RustyError::Tool(
                        "egress: redirect limit exceeded".into(),
                    ));
                }
                redirect_count += 1;

                let location = response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");

                if location.is_empty() {
                    // No Location header — return the redirect response as-is
                    // (the caller will see a 3xx status and treat it as failed).
                    use futures::StreamExt;
                    let mut body = Vec::new();
                    let mut stream = response.bytes_stream();
                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk.map_err(transport_err)?;
                        if body.len() + chunk.len() > request.max_response_bytes {
                            return Err(rusty_agent_runtime::error::RustyError::Tool(format!(
                                "connector: check response exceeds the {}-byte ceiling",
                                request.max_response_bytes
                            )));
                        }
                        body.extend_from_slice(&chunk);
                    }
                    return Ok(CheckResponse { status, body });
                }

                let redirect_url =
                    if location.starts_with("http://") || location.starts_with("https://") {
                        match reqwest::Url::parse(location) {
                            Ok(u) => u,
                            Err(e) => {
                                return Err(rusty_agent_runtime::error::RustyError::Tool(format!(
                                    "egress: malformed redirect location `{location}`: {e}"
                                )));
                            }
                        }
                    } else {
                        // Relative URL — resolve against the original request URL.
                        match url.join(location) {
                            Ok(u) => u,
                            Err(e) => {
                                return Err(rusty_agent_runtime::error::RustyError::Tool(format!(
                                "egress: malformed relative redirect location `{location}`: {e}"
                            )));
                            }
                        }
                    };

                let redirect_host = redirect_url.host_str().unwrap_or("");
                let redirect_port = redirect_url.port().unwrap_or(443);
                let redirect_path = redirect_url.path();

                if let Some(policy) = &self.policy {
                    let redirect_decision = rusty_agent_runtime::egress::evaluate_redirect(
                        policy,
                        redirect_host,
                        redirect_port,
                        rusty_agent_runtime::egress::EgressProtocol::Rest,
                        method_str,
                        redirect_path,
                        None,
                        &self.originating_component,
                    );
                    match redirect_decision {
                        rusty_agent_runtime::egress::EgressDecision::Allow => {}
                        rusty_agent_runtime::egress::EgressDecision::Deny {
                            reason,
                            policy_name,
                        } => {
                            let msg = format!(
                                "egress denied: {reason:?} (policy: {}) — redirect to {location}",
                                policy_name.as_deref().unwrap_or("<none>")
                            );
                            tracing::info!(%msg, url = %request.url, component = %self.originating_component, "egress redirect denial");
                            return Err(rusty_agent_runtime::error::RustyError::Tool(msg));
                        }
                        rusty_agent_runtime::egress::EgressDecision::Audit {
                            policy_name,
                            rule_index,
                        } => {
                            tracing::info!(
                                url = %request.url,
                                redirect = %location,
                                component = %self.originating_component,
                                policy = %policy_name,
                                rule_index,
                                "egress redirect audit-mode hit"
                            );
                        }
                    }
                }

                // Follow the redirect: update the request URL and loop.
                request.url = redirect_url.to_string();
                request.method = match method_str {
                    "GET" => rusty_agent_runtime::connector::HttpMethod::Get,
                    "POST" => rusty_agent_runtime::connector::HttpMethod::Post,
                    "PATCH" => rusty_agent_runtime::connector::HttpMethod::Patch,
                    "PUT" => rusty_agent_runtime::connector::HttpMethod::Put,
                    "DELETE" => rusty_agent_runtime::connector::HttpMethod::Delete,
                    _ => request.method,
                };
                continue;
            }

            // Not a redirect — read body and return.
            use futures::StreamExt;
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(transport_err)?;
                if body.len() + chunk.len() > request.max_response_bytes {
                    return Err(rusty_agent_runtime::error::RustyError::Tool(format!(
                        "connector: check response exceeds the {}-byte ceiling",
                        request.max_response_bytes
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(CheckResponse { status, body });
        }
    }
}

fn transport(
    policy: Option<Arc<rusty_agent_runtime::egress::EgressPolicy>>,
) -> ReqwestConnectorTransport {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client builds");
    ReqwestConnectorTransport::new(client, policy, "connector-check")
}

// --------------------------------------------------------------------- //
// Handlers
// --------------------------------------------------------------------- //

fn store_err(e: String) -> ApiError {
    ApiError::internal(format!("connector store: {e}"))
}

/// The served shape of one instance: the record with each sealed field
/// re-inserted as `{"rusty_secret": true}` — "set, never rendered".
fn serve_instance(instance: &ConnectorInstance) -> Value {
    json!({
        "instance_id": instance.instance_id,
        "manifest_hash": instance.manifest_hash,
        "config": insert_masked_secrets(instance.config.clone(), &instance.sealed),
        "created_at": instance.created_at,
    })
}

/// Look up the caller's manifest by content hash, 404 on
/// unknown/cross-tenant (the indistinguishability rule).
async fn manifest_for(
    state: &AppState,
    tenant: &TenantContext,
    hash: &str,
) -> Result<ConnectorManifest, ApiError> {
    state
        .connectors
        .get_manifest(tenant.tenant(), hash)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found(format!("unknown connector manifest `{hash}`")))
}

/// `POST /connectors` — register a manifest. The manifest validates and
/// its hash re-verifies (a tampered or malformed declaration is a 400);
/// an identical re-registration converges with `200` and
/// `registered: false` (content addressing).
pub(crate) async fn register_manifest(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(manifest): Json<ConnectorManifest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if let Err(e) = manifest.validate() {
        return Err(ApiError::bad_request(e.to_string()));
    }
    if !manifest.verify_hash() {
        return Err(ApiError::bad_request(format!(
            "manifest `{}` hash does not match its content — the hash is computed at \
             construction (`ConnectorManifest::new`), not chosen",
            manifest.id
        )));
    }
    let registered = state
        .connectors
        .put_manifest(tenant.tenant(), &manifest)
        .await
        .map_err(store_err)?;
    let status = if registered {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(json!({"hash": manifest.hash, "registered": registered})),
    ))
}

/// `GET /connectors` — the tenant's manifests, sorted by connector id.
pub(crate) async fn list_manifests(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let manifests = state
        .connectors
        .list_manifests(tenant.tenant())
        .await
        .map_err(store_err)?;
    Ok(Json(json!({"manifests": manifests})))
}

/// The instantiation payload: a manifest hash and one config object.
#[derive(Deserialize)]
pub(crate) struct InstantiatePayload {
    manifest_hash: String,
    config: Value,
}

/// `POST /connectors/instances` — schema-validated config → 201
/// instance. A schema rejection is a 422 whose message names the failing
/// schema path (`credentials.username: required property missing` — the
/// format Studio pins field errors from). Secrets extract and seal
/// through the broker before anything persists.
pub(crate) async fn register_instance(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<InstantiatePayload>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let manifest = manifest_for(&state, &tenant, &payload.manifest_hash).await?;
    if let Err(rejection) = validate_config(&manifest.connection_specification, &payload.config) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_config",
            rejection,
        ));
    }
    let instance_id = format!(
        "{INSTANCE_ID_PREFIX}{}",
        &uuid::Uuid::new_v4().simple().to_string()[..16]
    );
    let scoped = crate::auth::scope_id(tenant.tenant(), &instance_id);
    // Extract and seal before anything persists: the record holds the
    // non-secret config plus ciphertext envelopes, never plaintext.
    let extracted = extract_secrets(&manifest.connection_specification, &payload.config);
    let mut sealed = BTreeMap::new();
    for (path, secret) in &extracted {
        let plaintext =
            serde_json::to_vec(secret).map_err(|e| ApiError::internal(e.to_string()))?;
        let envelope = state
            .broker
            .seal_connector_secret(&scoped, &plaintext)
            .await
            .map_err(store_err)?;
        sealed.insert(path.clone(), envelope);
    }
    let instance = ConnectorInstance::new(
        &instance_id,
        &manifest.hash,
        without_secrets(payload.config.clone(), &extracted),
        sealed,
        Utc::now(),
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    state
        .connectors
        .put_instance(tenant.tenant(), &instance)
        .await
        .map_err(store_err)?;
    Ok((StatusCode::CREATED, Json(serve_instance(&instance))))
}

/// `GET /connectors/instances` — the tenant's instances, secrets masked.
pub(crate) async fn list_instances(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let instances = state
        .connectors
        .list_instances(tenant.tenant())
        .await
        .map_err(store_err)?;
    let served: Vec<Value> = instances.iter().map(serve_instance).collect();
    Ok(Json(json!({"instances": served})))
}

/// The check payload: pre-save (`manifest_hash` + `config`) or against a
/// live instance (`instance_id`).
#[derive(Deserialize)]
pub(crate) struct CheckPayload {
    #[serde(default)]
    manifest_hash: Option<String>,
    #[serde(default)]
    config: Option<Value>,
    #[serde(default)]
    instance_id: Option<String>,
}

/// `POST /connectors/check` — execute the manifest's check operation
/// with the candidate config (the setup gate) or a live instance's
/// stored config (the edit gate). The verdict is the Airbyte contract:
/// `{"status": "succeeded"}` or `{"status": "failed", "message"}`.
/// Pre-save configs validate against the schema first — a rejection is
/// the same 422 the instantiation door returns.
pub(crate) async fn check(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(payload): Json<CheckPayload>,
) -> Result<Json<Value>, ApiError> {
    let (manifest, config) = match (payload.instance_id, payload.manifest_hash, payload.config) {
        (Some(instance_id), None, None) => {
            let instance = state
                .connectors
                .get_instance(tenant.tenant(), &instance_id)
                .await
                .map_err(store_err)?
                .ok_or_else(|| {
                    ApiError::not_found(format!("unknown connector instance `{instance_id}`"))
                })?;
            let manifest = manifest_for(&state, &tenant, &instance.manifest_hash).await?;
            // Open the sealed secrets host-side, for this call only.
            let scoped = crate::auth::scope_id(tenant.tenant(), &instance.instance_id);
            let mut opened = Vec::with_capacity(instance.sealed.len());
            for (path, envelope) in &instance.sealed {
                let plaintext = state
                    .broker
                    .open_connector_secret(&scoped, envelope)
                    .await
                    .map_err(store_err)?;
                let secret: Value = serde_json::from_slice(&plaintext)
                    .map_err(|e| ApiError::internal(format!("corrupt sealed secret: {e}")))?;
                opened.push((path.clone(), secret));
            }
            (
                manifest,
                insert_opened_secrets(instance.config.clone(), &opened),
            )
        }
        (None, Some(hash), Some(config)) => {
            let manifest = manifest_for(&state, &tenant, &hash).await?;
            if let Err(rejection) = validate_config(&manifest.connection_specification, &config) {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_config",
                    rejection,
                ));
            }
            (manifest, config)
        }
        _ => {
            return Err(ApiError::bad_request(
                "check takes either `instance_id` (a live instance) or `manifest_hash` + \
                 `config` (a pre-save candidate), not both and not neither"
                    .to_owned(),
            ));
        }
    };
    let policy = state.config.egress_policy.clone().map(std::sync::Arc::new);
    let outcome = execute_check(&manifest, &config, &transport(policy)).await;
    Ok(Json(
        serde_json::to_value(outcome).expect("outcome serializes"),
    ))
}

/// `GET /connectors/instances/{id}/catalog` — the instance's derived
/// tool catalog: one tool per manifest operation, namespaced
/// `<connector-id>/<operation>`.
pub(crate) async fn instance_catalog(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(instance_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let instance = state
        .connectors
        .get_instance(tenant.tenant(), &instance_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| {
            ApiError::not_found(format!("unknown connector instance `{instance_id}`"))
        })?;
    let manifest = manifest_for(&state, &tenant, &instance.manifest_hash).await?;
    let tools = manifest
        .derive_catalog()
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({
        "instance_id": instance.instance_id,
        "manifest_hash": instance.manifest_hash,
        "tools": tools,
    })))
}
