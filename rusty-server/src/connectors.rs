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
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use rusty_agent_runtime::connector::{
    execute_check, extract_secrets, insert_masked_secrets, insert_opened_secrets, validate_config,
    without_secrets, CheckRequest, CheckResponse, ConnectorInstance, ConnectorManifest,
    ConnectorOperation, ConnectorTransport, HttpMethod, OperationAuth, OperationEffect,
    INSTANCE_ID_PREFIX,
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
pub(crate) async fn persist_json(
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

/// Remove one record. Absent is not an error: the record is gone either way,
/// and a revoke that races a revoke should not fail the second caller.
pub(crate) async fn remove_record(dir: &Path, scoped_key: &str) -> Result<(), String> {
    let path = dir.join(format!("{scoped_key}.json"));
    match tokio::fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("remove {}: {e}", path.display())),
    }
}

pub(crate) async fn remove_instance(root: &Path, scoped_id: &str) -> Result<(), String> {
    remove_record(&instances_dir(root), scoped_id).await
}

/// Load every record under `dir` (recursing into tenant subdirectories),
/// keyed by the `{tenant}/`-prefixed path relative to `dir`. Corrupt
/// files are skipped — a boot must not fail on one bad record (the
/// knowledge plane's corrupt-tolerant convention).
pub(crate) fn load_records<T: serde::de::DeserializeOwned>(dir: &Path) -> Vec<(String, T)> {
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

fn current_dir(root: &Path) -> PathBuf {
    dir(root).join("current")
}

/// The pointer from a connector's identity — `id@version` — to the content
/// hash that is current for it. A connector is a *thing in a library*: it has
/// one current definition and a history of what it used to be. Without this
/// the library shows one row per edit, which is a changelog, not a catalog.
pub(crate) fn load_current(root: &Path) -> Vec<(String, String)> {
    load_records(&current_dir(root))
}

pub(crate) async fn persist_current(
    root: &Path,
    scoped_identity: &str,
    hash: &str,
) -> Result<(), String> {
    persist_json(&current_dir(root), scoped_identity, &hash.to_owned()).await
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

/// What every connector call says it is. Sent unless the connector declares
/// its own User-Agent header.
pub const RUSTY_USER_AGENT: &str = concat!("rusty/", env!("CARGO_PKG_VERSION"));

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
pub struct ReqwestConnectorTransport {
    client: reqwest::Client,
    /// The deployment's L7 egress policy. `None` means open.
    policy: Option<Arc<rusty_agent_runtime::egress::EgressPolicy>>,
    /// The component identity attributed to this traffic per
    /// `contracts:turn-stamp`.
    originating_component: String,
    /// Maximum redirect hops before giving up.
    max_redirects: u8,
    /// The worlds a run may be routed into: a request addressed to the host
    /// a run's world stands in for is answered there, never sent.
    worlds: Option<Arc<crate::worlds::WorldPlane>>,
    /// A world this transport answers from outside any run — a
    /// connection's check proved against a stand-in: `(tenant, world_id)`.
    pinned_world: Option<(String, String)>,
}

impl ReqwestConnectorTransport {
    /// Create a transport with the given policy and component attribution.
    pub fn new(
        client: reqwest::Client,
        policy: Option<Arc<rusty_agent_runtime::egress::EgressPolicy>>,
        originating_component: impl Into<String>,
    ) -> Self {
        Self {
            client,
            policy,
            originating_component: originating_component.into(),
            max_redirects: 10,
            worlds: None,
            pinned_world: None,
        }
    }

    /// Route runs that name a world into it.
    pub fn with_worlds(mut self, worlds: Arc<crate::worlds::WorldPlane>) -> Self {
        self.worlds = Some(worlds);
        self
    }
    /// Answer from this world whether or not a run names one — the way a
    /// check is proved against a stand-in before a connection is saved.
    pub fn with_world(mut self, tenant: impl Into<String>, world_id: impl Into<String>) -> Self {
        self.pinned_world = Some((tenant.into(), world_id.into()));
        self
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

/// Faults an operator injects on purpose (`RUSTY_FAULTS`, dev and the
/// campaign only): `drop-response:POST:/api/now/table/incident` loses the
/// answer to every matching request *after* it was sent — the case the
/// F8 family measures. Parsed once; absent in production.
fn injected_faults() -> &'static [(String, String)] {
    static FAULTS: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    FAULTS.get_or_init(|| {
        let raw = std::env::var("RUSTY_FAULTS").unwrap_or_default();
        let faults: Vec<(String, String)> = raw
            .split(',')
            .filter_map(|entry| {
                let mut parts = entry.trim().splitn(3, ':');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some("drop-response"), Some(method), Some(path)) if !path.is_empty() => Some((method.to_ascii_uppercase(), path.to_owned())),
                    _ => None,
                }
            })
            .collect();
        if !faults.is_empty() {
            tracing::warn!(?faults, "RUSTY_FAULTS is set: the answers to matching requests are dropped after they are sent");
        }
        faults
    })
}

#[async_trait::async_trait]
impl ConnectorTransport for ReqwestConnectorTransport {
    async fn send(
        &self,
        mut request: CheckRequest,
    ) -> rusty_agent_runtime::error::Result<CheckResponse> {
        let mut redirect_count = 0u8;
        let fault_method = match request.method {
            rusty_agent_runtime::connector::HttpMethod::Get => "GET",
            rusty_agent_runtime::connector::HttpMethod::Post => "POST",
            rusty_agent_runtime::connector::HttpMethod::Patch => "PATCH",
            rusty_agent_runtime::connector::HttpMethod::Put => "PUT",
            rusty_agent_runtime::connector::HttpMethod::Delete => "DELETE",
        };
        let faulted = injected_faults().iter().any(|(m, p)| m == fault_method && request.url.contains(p.as_str()));
        // A run in a world: the world answers what is addressed to the host
        // it stands in for, in-process, before egress ever sees it.
        if let Some(plane) = &self.worlds {
            let run = rusty_agent_runtime::tool::current_run();
            let tenant = run.as_ref().and_then(|r| r.tenant().map(str::to_owned)).unwrap_or_else(|| crate::auth::DEFAULT_TENANT.to_owned());
            let execution = run.as_ref().and_then(|r| r.execution.as_ref());
            // The run's worlds: several when it touches several systems,
            // else the one; each answers what is addressed to the host it
            // stands in for, so the call finds its own stand-in.
            let mut named: Vec<(String, String)> = execution
                .and_then(|e| e.get("worlds"))
                .and_then(serde_json::Value::as_array)
                .map(|ws| ws.iter().filter_map(serde_json::Value::as_str).map(|w| (tenant.clone(), w.to_owned())).collect())
                .unwrap_or_default();
            if named.is_empty() {
                if let Some(one) = execution.and_then(|e| e.get("world")).and_then(serde_json::Value::as_str) {
                    named.push((tenant.clone(), one.to_owned()));
                } else if let Some(pinned) = self.pinned_world.clone() {
                    named.push(pinned);
                }
            }
            for (tenant, world_id) in named {
                if let Some(answer) = plane.answer(&tenant, &world_id, &request).await {
                    return answer;
                }
            }
        }

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

            let mut pinned: Option<std::net::SocketAddr> = None;
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
                // Pin the *socket* to the preflighted address, not the URL:
                // the hostname must stay in the request for SNI, certificate
                // verification and the Host header, or every https host fails
                // exactly when the policy is on. The connection still goes to
                // precisely the address preflight approved.
                let pinned_ip = self.preflight(policy, host, port).await?;
                let ip: std::net::IpAddr = pinned_ip.parse().map_err(|_| {
                    rusty_agent_runtime::error::RustyError::Tool(format!(
                        "egress: preflight returned a non-address `{pinned_ip}`"
                    ))
                })?;
                pinned = Some(std::net::SocketAddr::new(ip, port));
            }

            // -----------------------------------------------------------------
            // Issue the wire call.
            // -----------------------------------------------------------------
            // Whether the request may have reached the system: a connection
            // that never opened sent nothing; anything after that may have.
            let transport_err = |e: reqwest::Error| rusty_agent_runtime::error::RustyError::Transport {
                sent: !(e.is_connect() || e.is_builder()),
                detail: format!("connector transport: {e}"),
            };
            let client = match pinned {
                Some(addr) => reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .resolve(host, addr)
                    .build()
                    .map_err(transport_err)?,
                None => self.client.clone(),
            };
            let mut call = client
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
            // Every call identifies itself. Some APIs (GitHub among them)
            // refuse anonymous clients outright; a connector that declares
            // its own User-Agent overrides this below.
            call = call.header("User-Agent", RUSTY_USER_AGENT);

            for (name, value) in &request.headers {
                call = call.header(name, value);
            }
            if let Some(body) = &request.body {
                call = call
                    .header("content-type", "application/json")
                    .body(body.clone());
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
                    if faulted {
                        tracing::warn!(url = %request.url, status, "RUSTY_FAULTS: the answer was dropped after the request was sent");
                        return Err(rusty_agent_runtime::error::RustyError::Transport {
                            sent: true,
                            detail: "connector transport: (injected fault) the answer was lost after the request was sent".to_owned(),
                        });
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
            if faulted {
                tracing::warn!(url = %request.url, status, "RUSTY_FAULTS: the answer was dropped after the request was sent");
                return Err(rusty_agent_runtime::error::RustyError::Transport {
                    sent: true,
                    detail: "connector transport: (injected fault) the answer was lost after the request was sent".to_owned(),
                });
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
fn serve_instance(instance: &ConnectorInstance, manifest: Option<&ConnectorManifest>) -> Value {
    json!({
        "instance_id": instance.instance_id,
        "manifest_hash": instance.manifest_hash,
        // A connection is a connection *to something*, so it says what. The
        // manifest it was configured against may since have been superseded in
        // the library — resolving the hash on the client would then find
        // nothing, and the connection would read as "unknown".
        "connector": manifest.map(|m| json!({
            "id": m.id,
            "display_name": m.display_name,
            "version": m.version,
        })),
        // Whether this connection is usable, without anyone pressing a button.
        // A connector whose credentials are granted is not connected until a
        // person has approved it, and a screen that cannot say so leaves a
        // builder guessing why nothing works.
        "authorization": manifest.map(|m| grant_status(instance, m)),
        "config": insert_masked_secrets(instance.config.clone(), &instance.sealed),
        "created_at": instance.created_at,
    })
}

/// The served instance in full: the record, plus the tools it derives and
/// the agents whose charters name them — one shape wherever an instance
/// is served, so Catalog, Security and a client that created it read the
/// same thing.
async fn serve_instance_full(
    state: &AppState,
    tenant: &TenantContext,
    instance: &ConnectorInstance,
    manifest: Option<&ConnectorManifest>,
) -> Value {
    let mut one = serve_instance(instance, manifest);
    let tools: Vec<String> = state
        .connection_tools
        .as_ref()
        .map(|cell| cell.tools_of(&instance.instance_id))
        .unwrap_or_default();
    let assistants = state.server_store.list_assistants().await.unwrap_or_default();
    let agents: Vec<Value> = assistants
        .iter()
        .filter(|a| {
            a.config
                .pointer("/studio_intent/tools")
                .and_then(Value::as_array)
                .is_some_and(|ts| ts.iter().any(|t| t.get("name").and_then(Value::as_str).is_some_and(|n| tools.iter().any(|mine| mine == n))))
        })
        .map(|a| json!({"assistant_id": tenant.unscope(&a.assistant_id).unwrap_or(&a.assistant_id), "name": a.name}))
        .collect();
    one["tools"] = json!(tools);
    one["agents"] = json!(agents);
    one
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
    // What the vendor requires and what any credentialed connector must
    // do: a manifest that lacks it is refused here with the exact fix,
    // whether it was shipped, described, imported, or built by an agent.
    let findings = rusty_agent_runtime::connector::lint_manifest(&manifest);
    if !findings.is_empty() {
        let words: Vec<String> = findings.iter().map(ToString::to_string).collect();
        return Err(ApiError::unprocessable(format!(
            "manifest `{}` is not ready to register: {}",
            manifest.id,
            words.join("; ")
        )));
    }
    // A content hash is derived, not chosen. A manifest authored by hand —
    // in Studio's manifest field, in a catalog file, by any client that is
    // not this Rust workspace — arrives without one and is sealed here. A
    // manifest that carries a hash still has to prove it.
    let manifest = if manifest.hash.is_empty() {
        manifest
            .sealed()
            .map_err(|e| ApiError::bad_request(e.to_string()))?
    } else {
        manifest
    };
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

/// How a generic API authenticates. The four shapes a manifest can render
/// without a flow of its own — which is the honest boundary of "any HTTP API".
#[derive(Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GenericAuth {
    Bearer,
    Basic,
    Header,
    Query,
    /// A public API: no credential, nothing to seal, every call as is.
    None,
}

/// The generic-connector payload: who it is, where it lives, how it
/// authenticates, and an OpenAPI document to read its operations from.
#[derive(Deserialize)]
pub(crate) struct OpenApiPayload {
    id: String,
    #[serde(default = "one")]
    version: String,
    display_name: String,
    description: String,
    documentation_url: String,
    base_url: String,
    auth: GenericAuth,
    /// Header or query parameter name, for those two styles.
    #[serde(default)]
    auth_name: Option<String>,
    /// The name of the operation to use as the check, if the document has a
    /// parameterless read this can point at.
    #[serde(default)]
    check: Option<String>,
    /// The operations the builder chose to bring, by name. Absent means all
    /// of them — which a large API cannot fit (`MAX_OPERATIONS`), so then
    /// the answer is the list to choose from instead of a manifest.
    #[serde(default)]
    operations: Option<Vec<String>>,
    spec: Value,
}

fn one() -> String {
    "1".to_owned()
}

/// The configuration an auth style needs, as a specification the studio can
/// render — the same closed, secret-marked shape every catalog connector has.
/// A key sent in a header or the address is titled by the name the API uses
/// for it (`User-Agent`, `appid`), so a person knows what to put there.
fn generic_spec(auth: GenericAuth, name: Option<&str>) -> Value {
    if matches!(auth, GenericAuth::None) {
        return json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        });
    }
    let credentials = match auth {
        GenericAuth::Basic => json!({
            "type": "object", "title": "Authentication", "additionalProperties": false,
            "required": ["username", "password"], "rusty_order": 0,
            "properties": {
                "username": {"type": "string", "title": "Username", "rusty_order": 0},
                "password": {"type": "string", "title": "Password", "rusty_secret": true, "rusty_order": 1}
            }
        }),
        _ => json!({
            "type": "object", "title": "Authentication", "additionalProperties": false,
            "required": ["token"], "rusty_order": 0,
            "properties": {
                "token": {"type": "string", "title": match (auth, name) { (GenericAuth::Header | GenericAuth::Query, Some(name)) => name, _ => "Token" }, "rusty_secret": true, "rusty_order": 0}
            }
        }),
    };
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "additionalProperties": false,
        "required": ["credentials"],
        "properties": {"credentials": credentials}
    })
}

fn generic_auth(auth: GenericAuth, name: Option<&str>) -> Vec<OperationAuth> {
    match auth {
        GenericAuth::Bearer => vec![OperationAuth::Bearer {
            token: "{credentials.token}".to_owned(),
        }],
        GenericAuth::Basic => vec![OperationAuth::Basic {
            username: "{credentials.username}".to_owned(),
            password: "{credentials.password}".to_owned(),
        }],
        GenericAuth::Header => vec![OperationAuth::Header {
            name: name.unwrap_or("X-API-Key").to_owned(),
            value_template: "{credentials.token}".to_owned(),
        }],
        GenericAuth::Query => vec![OperationAuth::Query {
            name: name.unwrap_or("api_key").to_owned(),
            value_template: "{credentials.token}".to_owned(),
        }],
        GenericAuth::None => Vec::new(),
    }
}

/// `POST /connectors/openapi` — read an OpenAPI 3.x document into a manifest.
///
/// This is the answer to "the connector I need is not in the library": most
/// APIs publish a description of themselves, and a connector is exactly that
/// description plus how it authenticates. The result is a *draft* — it is
/// returned, not registered, with the operations that could not be mapped
/// listed beside it, so a builder registers something they have looked at.
pub(crate) async fn from_openapi(
    Json(payload): Json<OpenApiPayload>,
) -> Result<Json<Value>, ApiError> {
    // The document as published: a JSON object, or the text of a JSON or
    // YAML file — most published descriptions are YAML.
    let spec = match payload.spec {
        Value::String(text) => match serde_json::from_str::<Value>(&text) {
            Ok(value) => value,
            Err(_) => serde_yaml::from_str::<Value>(&text).map_err(|e| {
                ApiError::bad_request(format!("the document is neither JSON nor YAML: {e}"))
            })?,
        },
        value => value,
    };
    let imported = rusty_agent_runtime::connector::import_openapi(&spec)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    if imported.mapped.is_empty() {
        return Err(ApiError::bad_request(
            "the document described no operation this connector could call — an operation needs \
             a supported method (GET, POST, PUT, PATCH, DELETE) under `paths`"
                .to_owned(),
        ));
    }
    let auth = generic_auth(payload.auth, payload.auth_name.as_deref());
    let operations: Vec<ConnectorOperation> = imported
        .mapped
        .into_iter()
        .map(|mut operation| {
            operation.auth = auth.clone();
            operation
        })
        .collect();
    // Everything the document offers, for a builder choosing what agents
    // may do with it: name, what it does, and whether it changes anything.
    let available: Vec<Value> = operations
        .iter()
        .map(|op| json!({"name": op.name, "description": op.description, "method": op.method, "path": op.path, "effect": op.effect}))
        .collect();
    // Every connector needs one read that takes no arguments, because that is
    // what proving a configuration means: reach the system with these
    // credentials and nothing else. A published API rarely declares such an
    // operation, so one is derived — the same method and path as a listing
    // read, called with no parameters. Nothing is invented: the path is the
    // document's own.
    let candidate = payload
        .check
        .and_then(|named| operations.iter().find(|op| op.name == named).cloned())
        .or_else(|| {
            operations
                .iter()
                .find(|op| {
                    op.method == HttpMethod::Get
                        && op.effect == OperationEffect::ReadOnly
                        && !op.path.contains('{')
                })
                .cloned()
        })
        .ok_or_else(|| {
            ApiError::bad_request(
                "no operation can serve as the check — a connector needs one GET whose path takes \
                 no parameters, so a configuration can be proven before it is saved"
                    .to_owned(),
            )
        })?;
    let check = "check-connection".to_owned();
    let mut operations = operations;
    operations.retain(|op| op.name != check);
    if let Some(chosen) = &payload.operations {
        operations.retain(|op| chosen.iter().any(|name| name == &op.name));
    }
    // The check rides with the chosen ones; more than the cap cannot be one
    // connector, so the builder is handed the list to choose from.
    let cap = rusty_agent_runtime::connector::manifest::MAX_OPERATIONS - 1;
    if operations.len() > cap {
        return Ok(Json(json!({
            "manifest": null,
            "available": available,
            "choose": {"count": operations.len(), "cap": cap},
            "unmapped": imported.unmapped,
        })));
    }
    operations.push(ConnectorOperation {
        name: check.clone(),
        description: format!("Verify the API answers at {} and the credentials are accepted.", candidate.path),
        method: HttpMethod::Get,
        path: candidate.path.clone(),
        effect: OperationEffect::ReadOnly,
        params_schema: json!({"type": "object"}),
        headers: Vec::new(),
        auth: auth.clone(),
        max_response_bytes: None,
        reconcile: None,
    });

    let mut manifest = ConnectorManifest::new(
        payload.id,
        payload.version,
        payload.display_name,
        payload.description,
        payload.documentation_url,
        payload.base_url,
        generic_spec(payload.auth, payload.auth_name.as_deref()),
        operations,
        check,
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    // A write that would guess after a lost answer reads back through one
    // of the document's own reads when the document makes that possible
    // (a read on the same path that filters by the write's natural key):
    // adopted into the draft, and said so, with the writes that still
    // guess named beside it — nothing is invented, and a builder can undo
    // any of it in the editor before registering.
    let proposals = manifest.propose_read_backs();
    for proposal in &proposals {
        if let Some(op) = manifest.operations.iter_mut().find(|op| op.name == proposal.write) {
            op.reconcile = serde_json::from_value(json!({"operation": proposal.operation, "arguments": proposal.arguments})).ok();
        }
    }
    let manifest = ConnectorManifest::new(
        manifest.id.clone(),
        manifest.version.clone(),
        manifest.display_name.clone(),
        manifest.description.clone(),
        manifest.documentation_url.clone(),
        manifest.base_url.clone(),
        manifest.connection_specification.clone(),
        manifest.operations.clone(),
        manifest.check.clone(),
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let still_guessing: Vec<String> = manifest.writes_without_read_back().iter().map(|op| op.name.clone()).collect();

    Ok(Json(json!({
        "manifest": manifest,
        "available": available,
        "unmapped": imported.unmapped,
        "read_backs": {
            "adopted": proposals,
            "still_guessing": still_guessing,
        },
    })))
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
    under_ceiling(&state, &manifest, &payload.config)?;
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
    // The connection is a tool from now on.
    refresh_connection_tools(&state).await;
    Ok((StatusCode::CREATED, Json(serve_instance_full(&state, &tenant, &instance, Some(&manifest)).await)))
}

/// The rotate payload: a whole new config for an existing connection.
#[derive(Deserialize)]
pub(crate) struct RotatePayload {
    config: Value,
}

/// `PUT /connectors/instances/{id}` — rotate: new credentials, same
/// connection.
///
/// The instance id is what every agent's tool allow-list names, so rotating
/// a credential must not mint a new one. The config validates against the
/// same specification, its secrets seal the same way, and the record keeps
/// its id and its birthday. The tools refill, so the next call an agent makes
/// goes out with the new credential and nothing else changes.
pub(crate) async fn rotate_instance(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(instance_id): AxumPath<String>,
    Json(payload): Json<RotatePayload>,
) -> Result<Json<Value>, ApiError> {
    let existing = state
        .connectors
        .get_instance(tenant.tenant(), &instance_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found(format!("unknown connector instance `{instance_id}`")))?;
    let manifest = manifest_for(&state, &tenant, &existing.manifest_hash).await?;
    if let Err(rejection) = validate_config(&manifest.connection_specification, &payload.config) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_config",
            rejection,
        ));
    }
    under_ceiling(&state, &manifest, &payload.config)?;
    let scoped = crate::auth::scope_id(tenant.tenant(), &instance_id);
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
    let rotated = ConnectorInstance::new(
        &instance_id,
        &existing.manifest_hash,
        without_secrets(payload.config.clone(), &extracted),
        sealed,
        existing.created_at,
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    state
        .connectors
        .put_instance(tenant.tenant(), &rotated)
        .await
        .map_err(store_err)?;
    refresh_connection_tools(&state).await;
    Ok(Json(serve_instance_full(&state, &tenant, &rotated, Some(&manifest)).await))
}

/// `DELETE /connectors/instances/{id}` — revoke.
///
/// The record and its sealed envelopes go; the tools refill without it. A run
/// that names the connection from then on is refused at admission — the
/// allow-list validates against the graph's current catalog — and a call
/// already in flight fails closed at dispatch, because dispatch resolves the
/// tool by name at the moment of the call. That is what "revoke denies
/// pending" means here: not a flag checked somewhere, but the tool no longer
/// existing anywhere a run could find it.
#[derive(Debug, Deserialize)]
pub(crate) struct UpgradePayload {
    /// The manifest to move to — another version of the same connector.
    manifest_hash: String,
}

/// `POST /connectors/instances/{id}/upgrade` — a connection follows its
/// connector to another version: same id, same sealed credentials, the
/// new manifest's operations. The new version's connection spec must
/// accept the configuration already held, and the system must answer the
/// new version's check, before anything changes; otherwise the connection
/// stays exactly as it was and the reason comes back in words.
pub(crate) async fn upgrade_instance(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(instance_id): AxumPath<String>,
    Json(payload): Json<UpgradePayload>,
) -> Result<Json<Value>, ApiError> {
    let existing = state
        .connectors
        .get_instance(tenant.tenant(), &instance_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found(format!("unknown connector instance `{instance_id}`")))?;
    let current = manifest_for(&state, &tenant, &existing.manifest_hash).await?;
    let target = manifest_for(&state, &tenant, &payload.manifest_hash).await?;
    if target.id != current.id {
        return Err(ApiError::bad_request(format!(
            "`{}` is {}, not another version of {} — a connection follows its own connector",
            payload.manifest_hash, target.display_name, current.display_name
        )));
    }
    if target.hash == existing.manifest_hash {
        return Err(ApiError::bad_request(format!(
            "this connection already runs {} version {}",
            current.display_name, current.version
        )));
    }
    let config = opened_config(&state, &tenant, &existing).await?;
    if let Err(rejection) = validate_config(&target.connection_specification, &config) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "config_differs",
            format!(
                "version {} of {} needs a different configuration ({rejection}) — connect it again rather than upgrading",
                target.version, target.display_name
            ),
        ));
    }
    under_ceiling(&state, &target, &config)?;
    let candidate = rusty_agent_runtime::connector::render_template(&target.base_url, &config)
        .ok()
        .and_then(|url| host_of(&url));
    let policy = Some(Arc::new(effective_egress_policy(state.as_ref(), candidate.as_deref())));
    let outcome = execute_check(&target, &config, &transport(policy)).await;
    if !matches!(outcome.status, rusty_agent_runtime::connector::CheckStatus::Succeeded) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "check_failed",
            format!(
                "{} did not answer version {}'s check: {} — the connection stays on version {}",
                target.display_name,
                target.version,
                outcome.message.as_deref().unwrap_or("no reason given"),
                current.version
            ),
        ));
    }
    let upgraded = ConnectorInstance::new(
        &instance_id,
        &target.hash,
        existing.config.clone(),
        existing.sealed.clone(),
        existing.created_at,
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    state
        .connectors
        .put_instance(tenant.tenant(), &upgraded)
        .await
        .map_err(store_err)?;
    refresh_connection_tools(&state).await;
    tracing::info!(%instance_id, from = %current.version, to = %target.version, connector = %target.id, "connection upgraded");
    Ok(Json(serve_instance_full(&state, &tenant, &upgraded, Some(&target)).await))
}

/// A connection follows its connector to another version outside the
/// route: the same checks as `upgrade_instance` — the configuration held
/// must satisfy the target's specification and the system must answer the
/// target's check — and the same write. The reason comes back in words
/// when it cannot. Used when the platform extends a connector on the
/// Composer's behalf and every connection to it must move.
pub(crate) async fn follow_version(
    state: &AppState,
    tenant: &TenantContext,
    existing: &ConnectorInstance,
    target: &ConnectorManifest,
) -> Result<ConnectorInstance, String> {
    let config = opened_config(state, tenant, existing).await.map_err(|e| e.to_string())?;
    if let Err(rejection) = validate_config(&target.connection_specification, &config) {
        return Err(format!(
            "version {} of {} needs a different configuration ({rejection})",
            target.version, target.display_name
        ));
    }
    let candidate = rusty_agent_runtime::connector::render_template(&target.base_url, &config)
        .ok()
        .and_then(|url| host_of(&url));
    let policy = Some(Arc::new(effective_egress_policy(state, candidate.as_deref())));
    let outcome = execute_check(target, &config, &transport(policy)).await;
    if !matches!(outcome.status, rusty_agent_runtime::connector::CheckStatus::Succeeded) {
        return Err(format!(
            "{} did not answer version {}'s check: {}",
            target.display_name,
            target.version,
            outcome.message.as_deref().unwrap_or("no reason given")
        ));
    }
    let moved = ConnectorInstance::new(
        &existing.instance_id,
        &target.hash,
        existing.config.clone(),
        existing.sealed.clone(),
        existing.created_at,
    )
    .map_err(|e| e.to_string())?;
    state
        .connectors
        .put_instance(tenant.tenant(), &moved)
        .await
        .map_err(|e| format!("connector store: {e}"))?;
    tracing::info!(instance = %existing.instance_id, to = %target.version, connector = %target.id, "connection followed its connector");
    Ok(moved)
}

pub(crate) async fn revoke_instance(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(instance_id): AxumPath<String>,
) -> Result<StatusCode, ApiError> {
    let removed = state
        .connectors
        .delete_instance(tenant.tenant(), &instance_id)
        .await
        .map_err(store_err)?;
    if !removed {
        return Err(ApiError::not_found(format!("unknown connector instance `{instance_id}`")));
    }
    refresh_connection_tools(&state).await;
    Ok(StatusCode::NO_CONTENT)
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
    // The tools each connection derives and the agents that name them: one
    // inventory, the same for Catalog and Security, the one agents run
    // through.
    let mut served = Vec::with_capacity(instances.len());
    for instance in &instances {
        let manifest = state
            .connectors
            .get_manifest(tenant.tenant(), &instance.manifest_hash)
            .await
            .map_err(store_err)?;
        served.push(serve_instance_full(&state, &tenant, instance, manifest.as_ref()).await);
    }
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
    /// A world (by name or id) to prove the configuration against instead
    /// of the live system: it must stand in for the host the configuration
    /// addresses. The check reaches the world and nothing else.
    #[serde(default)]
    world: Option<String>,
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
            // A granted connection whose token has expired refreshes before
            // the check runs — otherwise "does this still work" answers for
            // the clock rather than for the credentials.
            let instance = refreshed(&state, tenant.tenant(), &instance, &manifest)
                .await
                .unwrap_or(instance);
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
            // Outside the ceiling, the check would only fail on egress:
            // say which host, and where it is allowed, instead. Proved
            // against a world, nothing reaches the wire, so the ceiling is
            // not in the way — it is said on the outcome, since the
            // connection's live calls will meet it.
            if payload.world.as_deref().map(str::trim).filter(|w| !w.is_empty()).is_none() {
                under_ceiling(&state, &manifest, &config)?;
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
    // A pre-save check reaches the candidate's own host — the operator is
    // testing exactly that host before storing it — when the ceiling
    // admits it; a check never widens the ceiling.
    let candidate = rusty_agent_runtime::connector::render_template(&manifest.base_url, &config)
        .ok()
        .and_then(|url| host_of(&url));
    let policy = Some(Arc::new(effective_egress_policy(state.as_ref(), candidate.as_deref())));
    let mut wire = transport(policy);
    let mut in_world = None;
    if let Some(named) = payload.world.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
        let world = state
            .worlds
            .find(tenant.tenant(), named)
            .await
            .map_err(ApiError::internal)?
            .ok_or_else(|| ApiError::not_found(format!("unknown world `{named}` — make it in Evals → Worlds")))?;
        if candidate.as_deref().is_some_and(|host| !host.eq_ignore_ascii_case(&world.stands_for)) {
            return Err(ApiError::bad_request(format!(
                "world `{named}` stands in for {}, and this configuration addresses {} — a check proves the host it will call",
                world.stands_for,
                candidate.as_deref().unwrap_or("?")
            )));
        }
        wire = wire.with_worlds(Arc::clone(&state.worlds)).with_world(tenant.tenant(), world.world_id.clone());
        in_world = Some(world.name.clone());
    }
    let outcome = execute_check(&manifest, &config, &wire).await;
    let mut served = serde_json::to_value(outcome).expect("outcome serializes");
    if let Some(name) = in_world {
        served["world"] = json!(name);
        let ceiling = crate::egress_ceiling::current(&state);
        if let Some(host) = crate::egress_ceiling::outside(&ceiling, connection_hosts(&manifest, &config).iter().map(String::as_str)) {
            served["outside_ceiling"] = json!(host);
        }
    }
    Ok(Json(served))
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

/// Mints ServiceNow-style OAuth tokens for connector instances whose config
/// carries an `oauth` block. The client secret and any account password stay
/// here; only the bearer token reaches a request, and only while it is valid.
///
/// Two grants, chosen by the instance's `oauth.grant`:
///
/// - `client_credentials` — the OAuth app is linked to a user on the instance,
///   so no account password is stored anywhere. The better production choice.
/// - `password` — the resource-owner grant, which exchanges a service
///   account's username and password. The default, because it works on every
///   instance version.
#[derive(Debug)]
pub struct PasswordGrantTokenSource {
    client: reqwest::Client,
    token_url: String,
}

impl PasswordGrantTokenSource {
    pub fn new(client: reqwest::Client, token_url: impl Into<String>) -> Self {
        Self {
            client,
            token_url: token_url.into(),
        }
    }
}

#[async_trait::async_trait]
impl rusty_agent_runtime::connector::agent_tool::OAuthTokenSource for PasswordGrantTokenSource {
    async fn token(
        &self,
        config: &serde_json::Value,
    ) -> rusty_agent_runtime::error::Result<(String, u64)> {
        // The connector's own `connection_specification` says what a
        // credentials block looks like: flat fields under `credentials`,
        // discriminated by `auth`. This reads that, and nothing else — a
        // connector whose runtime needs a private shape is not a connector the
        // studio can configure.
        let oauth = config.get("credentials");
        let field = |name: &str| -> rusty_agent_runtime::error::Result<String> {
            oauth
                .and_then(|o| o.get(name))
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .ok_or_else(|| {
                    rusty_agent_runtime::error::RustyError::Tool(format!(
                        "the instance's oauth config has no `{name}`"
                    ))
                })
        };
        let grant = match oauth
            .and_then(|o| o.get("auth"))
            .and_then(|v| v.as_str())
            .unwrap_or("oauth_password")
        {
            "oauth_client_credentials" => "client_credentials",
            _ => "password",
        };
        let mut form = vec![
            ("grant_type".to_owned(), grant.to_owned()),
            ("client_id".to_owned(), field("client_id")?),
            ("client_secret".to_owned(), field("client_secret")?),
        ];
        // The client-credentials grant authenticates as the app itself; only
        // the resource-owner grant needs an account to speak for.
        if grant == "password" {
            form.push(("username".to_owned(), field("username")?));
            form.push(("password".to_owned(), field("password")?));
        }
        let response = self
            .client
            .post(&self.token_url)
            .form(&form)
            .send()
            .await
            .map_err(|e| {
                rusty_agent_runtime::error::RustyError::Tool(format!("token request failed: {e}"))
            })?;
        let status = response.status();
        if !status.is_success() {
            // The raw body of a refusal can quote the secret's neighborhood,
            // but a token failure with no detail is hours of guesswork — as
            // this one cost. ServiceNow answers with a structured
            // `{error, error_description}`; those two fields name the cause
            // (bad account, inactive app, wrong grant) and carry no secret, so
            // they travel and nothing else does.
            let detail = response
                .json::<serde_json::Value>()
                .await
                .ok()
                .map(|body| {
                    let code = body.get("error").and_then(|v| v.as_str()).unwrap_or("");
                    let described = body
                        .get("error_description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    match (code, described) {
                        ("", "") => String::new(),
                        (c, "") => format!(" — {c}"),
                        ("", d) => format!(" — {d}"),
                        (c, d) => format!(" — {c}: {d}"),
                    }
                })
                .unwrap_or_default();
            return Err(rusty_agent_runtime::error::RustyError::Tool(format!(
                "the token endpoint answered {status}{detail} (grant `{grant}`)"
            )));
        }
        let body: serde_json::Value = response.json().await.map_err(|e| {
            rusty_agent_runtime::error::RustyError::Tool(format!("token response unreadable: {e}"))
        })?;
        let token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                rusty_agent_runtime::error::RustyError::Tool(
                    "the token response carried no access_token".to_owned(),
                )
            })?;
        let expires_in = body.get("expires_in").and_then(|v| v.as_u64()).unwrap_or(1800);
        Ok((token.to_owned(), expires_in))
    }
}

// --------------------------------------------------------------------- //
// The grant: a connection that is authorized, not typed
// --------------------------------------------------------------------- //
//
// Some systems issue no credential a person can type. What a builder has is a
// client id and secret; the credential itself is minted after a *person*
// approves the request at the provider. Slack, Google, Microsoft and Atlassian
// all work this way, and this is the shape of it:
//
//   configure  →  authorize  →  connected
//
// Configure saves the connection with whatever it can (client id and secret,
// the instance host). Authorize sends the person to the provider and takes the
// code back. What comes back is written into the connection as sealed
// credentials, so from that moment the connector is an ordinary bearer-token
// connector and nothing downstream knows the difference.

/// What is still needed before a connection can be used, and for how long what
/// it has will last. A connection that says `connected` has been granted; one
/// that says `needs_auth` has been configured and never approved.
pub(crate) fn grant_status(instance: &ConnectorInstance, manifest: &ConnectorManifest) -> Value {
    if manifest.authorization.is_none() {
        return json!({"kind": "not_required"});
    }
    let granted = instance.sealed.contains_key(GRANT_ACCESS_TOKEN);
    if !granted {
        return json!({"kind": "needs_auth"});
    }
    let expires_at = instance
        .config
        .pointer("/credentials/expires_at")
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|t| t.with_timezone(&Utc));
    let refreshable = instance.sealed.contains_key(GRANT_REFRESH_TOKEN);
    match expires_at {
        Some(expiry) if expiry <= Utc::now() && !refreshable => {
            json!({"kind": "expired", "expires_at": expiry})
        }
        Some(expiry) => json!({"kind": "connected", "expires_at": expiry, "refreshable": refreshable}),
        None => json!({"kind": "connected", "refreshable": refreshable}),
    }
}

const GRANT_ACCESS_TOKEN: &str = "credentials.access_token";
const GRANT_REFRESH_TOKEN: &str = "credentials.refresh_token";

/// One consent in flight. The state is the only thing tying a provider's
/// redirect back to a connection, so it is unguessable and single-use.
pub(crate) struct PendingGrant {
    pub(crate) tenant: String,
    pub(crate) instance_id: String,
    pub(crate) started_at: DateTime<Utc>,
}

/// How long a consent may take before the state is refused. Long enough for a
/// person to read a scope list and log in; short enough that an abandoned one
/// does not linger.
const GRANT_WINDOW_MINUTES: i64 = 15;

#[derive(Deserialize)]
pub(crate) struct AuthorizePayload {
    /// Where to return the person once the provider redirects back.
    #[serde(default)]
    return_to: Option<String>,
}

/// `POST /connectors/instances/{id}/authorize` — begin the consent.
///
/// Returns the URL to send the person to. Nothing is granted by this call: it
/// mints the state that will identify the answer, and renders the provider's
/// own authorize URL from the connection's config.
pub(crate) async fn authorize(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(instance_id): AxumPath<String>,
    Json(payload): Json<AuthorizePayload>,
) -> Result<Json<Value>, ApiError> {
    let instance = state
        .connectors
        .get_instance(tenant.tenant(), &instance_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found(format!("unknown connector instance `{instance_id}`")))?;
    let manifest = manifest_for(&state, &tenant, &instance.manifest_hash).await?;
    let authorization = manifest.authorization.as_ref().ok_or_else(|| {
        ApiError::bad_request(format!(
            "connector `{}` declares no grant flow — its credentials are configured, not granted",
            manifest.id
        ))
    })?;

    // The client id is not a secret and lives in the config; the secret stays
    // sealed until the exchange, which is why only the id is rendered here.
    let config = opened_config(&state, &tenant, &instance).await?;
    let client_id = rusty_agent_runtime::connector::render_template(&authorization.client_id, &config)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let authorize_url =
        rusty_agent_runtime::connector::render_template(&authorization.authorize_url, &config)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;

    let grant_state = format!("gs-{}", uuid::Uuid::new_v4().simple());
    state.pending_grants.lock().await.insert(
        grant_state.clone(),
        PendingGrant {
            tenant: tenant.tenant().to_owned(),
            instance_id: instance_id.clone(),
            started_at: Utc::now(),
        },
    );

    let mut url = format!(
        "{authorize_url}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={grant_state}",
        urlencode(&client_id),
        urlencode(&callback_url(&state)),
        urlencode(&authorization.scopes),
    );
    for (key, template) in &authorization.extra_params {
        let value = rusty_agent_runtime::connector::render_template(template, &config)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        url.push_str(&format!("&{}={}", urlencode(key), urlencode(&value)));
    }
    let _ = payload.return_to;
    Ok(Json(json!({
        "url": url,
        "state": grant_state,
        "expires_in_minutes": GRANT_WINDOW_MINUTES,
    })))
}

#[derive(Deserialize)]
pub(crate) struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// `GET /connectors/oauth/callback` — the provider's redirect.
///
/// Public, because a provider redirects a browser here with no credential of
/// ours; the `state` is what makes the answer trustworthy, and it is
/// single-use. The exchanged tokens are sealed into the connection before this
/// returns, so the page the person lands on is the truth.
pub(crate) async fn oauth_callback(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<CallbackQuery>,
) -> axum::response::Response {
    if let Some(error) = query.error {
        let detail = query.error_description.unwrap_or_default();
        return grant_page(&format!("The provider refused: {error}. {detail}"), false);
    }
    let (Some(code), Some(grant_state)) = (query.code, query.state) else {
        return grant_page("That redirect carried no authorization code.", false);
    };
    let Some(pending) = state.pending_grants.lock().await.remove(&grant_state) else {
        return grant_page(
            "That consent is not one this server started, or it has already been used.",
            false,
        );
    };
    if (Utc::now() - pending.started_at).num_minutes() > GRANT_WINDOW_MINUTES {
        return grant_page("That consent took too long — start it again.", false);
    }
    match complete_grant(&state, &pending, &code).await {
        Ok(connector) => grant_page(
            &format!("{connector} is connected. You can close this tab."),
            true,
        ),
        Err(why) => grant_page(&why, false),
    }
}

/// Exchange the code and seal what comes back onto the connection.
async fn complete_grant(
    state: &AppState,
    pending: &PendingGrant,
    code: &str,
) -> std::result::Result<String, String> {
    let tenant = TenantContext::new(pending.tenant.clone(), Vec::new());
    let instance = state
        .connectors
        .get_instance(&pending.tenant, &pending.instance_id)
        .await?
        .ok_or_else(|| "that connection no longer exists".to_owned())?;
    let manifest = state
        .connectors
        .get_manifest(&pending.tenant, &instance.manifest_hash)
        .await?
        .ok_or_else(|| "that connection's connector no longer exists".to_owned())?;
    let authorization = manifest
        .authorization
        .as_ref()
        .ok_or_else(|| "that connector declares no grant flow".to_owned())?;

    let config = opened_config(state, &tenant, &instance)
        .await
        .map_err(|e| e.to_string())?;
    let render = |template: &str| {
        rusty_agent_runtime::connector::render_template(template, &config).map_err(|e| e.to_string())
    };
    let token_url = render(&authorization.token_url)?;
    let client_id = render(&authorization.client_id)?;
    let client_secret = render(&authorization.client_secret)?;

    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", &callback_url(state)),
        ("client_id", &client_id),
        ("client_secret", &client_secret),
    ];
    let response = reqwest::Client::new()
        .post(&token_url)
        .form(&form)
        .send()
        .await
        .map_err(|e| format!("the token endpoint could not be reached: {e}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("the token endpoint answered something that is not JSON: {e}"))?;
    let tokens = read_token_response(&body).ok_or_else(|| {
        let said = body
            .get("error_description")
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("no access token and no error field");
        format!("the token endpoint refused the exchange ({status}) — {said}")
    })?;

    seal_grant(state, &pending.tenant, &instance, tokens).await?;
    Ok(manifest.display_name)
}

/// What a token response carries that a connection needs to keep.
struct GrantTokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
}

fn read_token_response(body: &Value) -> Option<GrantTokens> {
    // Slack answers `{"ok": true, "access_token": …}` at the top level for a
    // user token and under `authed_user` for a bot one; both are read.
    let token = body
        .get("access_token")
        .and_then(Value::as_str)
        .or_else(|| body.pointer("/authed_user/access_token").and_then(Value::as_str))?;
    let expires_at = body
        .get("expires_in")
        .and_then(Value::as_i64)
        .map(|seconds| Utc::now() + chrono::Duration::seconds(seconds));
    Some(GrantTokens {
        access_token: token.to_owned(),
        refresh_token: body
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_at,
    })
}

/// Write the granted credentials into the connection: tokens sealed, expiry in
/// the clear so a screen can say when it runs out.
async fn seal_grant(
    state: &AppState,
    tenant: &str,
    instance: &ConnectorInstance,
    tokens: GrantTokens,
) -> std::result::Result<(), String> {
    let scoped = crate::auth::scope_id(tenant, &instance.instance_id);
    let mut sealed = instance.sealed.clone();
    for (path, secret) in [
        (GRANT_ACCESS_TOKEN, Some(tokens.access_token)),
        (GRANT_REFRESH_TOKEN, tokens.refresh_token),
    ] {
        let Some(secret) = secret else { continue };
        let plaintext = serde_json::to_vec(&Value::String(secret)).map_err(|e| e.to_string())?;
        let envelope = state
            .broker
            .seal_connector_secret(&scoped, &plaintext)
            .await?;
        sealed.insert(path.to_owned(), envelope);
    }
    let mut config = instance.config.clone();
    if let Some(expiry) = tokens.expires_at {
        if let Some(credentials) = config
            .as_object_mut()
            .and_then(|o| o.entry("credentials").or_insert_with(|| json!({})).as_object_mut())
        {
            credentials.insert("expires_at".to_owned(), json!(expiry.to_rfc3339()));
        }
    }
    let granted = ConnectorInstance::new(
        &instance.instance_id,
        &instance.manifest_hash,
        config,
        sealed,
        instance.created_at,
    )
    .map_err(|e| e.to_string())?;
    state.connectors.put_instance(tenant, &granted).await?;
    // Granted or refreshed: the credential the tools call with has changed.
    refresh_connection_tools(state).await;
    Ok(())
}

/// The instance's config with its sealed values opened, for this call only.
pub(crate) async fn opened_config(
    state: &AppState,
    tenant: &TenantContext,
    instance: &ConnectorInstance,
) -> Result<Value, ApiError> {
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
    Ok(insert_opened_secrets(instance.config.clone(), &opened))
}

/// Where a provider redirects back to. Configurable because it must be a URL
/// the provider can reach and the operator has registered.
fn callback_url(state: &AppState) -> String {
    format!(
        "{}/connectors/oauth/callback",
        state.config.public_url.trim_end_matches('/')
    )
}

fn urlencode(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b' ' => "%20".to_owned(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// The page a person lands on after consent. It is the last thing they see, so
/// it says what happened and nothing else — no branding, no redirect they did
/// not ask for.
fn grant_page(message: &str, ok: bool) -> axum::response::Response {
    let escaped = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Connector authorization</title>\
         <body style=\"font:15px/1.6 system-ui,sans-serif;margin:0;display:grid;place-items:center;height:100vh\">\
         <main style=\"max-width:32rem;padding:2rem;text-align:center\">\
         <p style=\"font-size:1.05rem\">{escaped}</p></main></body>"
    );
    (
        if ok {
            axum::http::StatusCode::OK
        } else {
            axum::http::StatusCode::BAD_REQUEST
        },
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Exchange a refresh token for a fresh access token when the one on record
/// has run out. Returns the updated instance, or `None` when nothing was done
/// — there is nothing to refresh, or the provider refused, and in both cases
/// the caller carries on with what it has and the failure surfaces as itself.
async fn refreshed(
    state: &AppState,
    tenant: &str,
    instance: &ConnectorInstance,
    manifest: &ConnectorManifest,
) -> Option<ConnectorInstance> {
    let authorization = manifest.authorization.as_ref()?;
    let expires_at = instance
        .config
        .pointer("/credentials/expires_at")
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())?
        .with_timezone(&Utc);
    // A minute of headroom: a token that expires during the call is expired.
    if expires_at > Utc::now() + chrono::Duration::seconds(60) {
        return None;
    }
    let context = TenantContext::new(tenant.to_owned(), Vec::new());
    let config = opened_config(state, &context, instance).await.ok()?;
    let refresh_token = config
        .pointer("/credentials/refresh_token")
        .and_then(Value::as_str)?;
    let render = |template: &str| {
        rusty_agent_runtime::connector::render_template(template, &config).ok()
    };
    let response = reqwest::Client::new()
        .post(render(&authorization.token_url)?)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &render(&authorization.client_id)?),
            ("client_secret", &render(&authorization.client_secret)?),
        ])
        .send()
        .await
        .ok()?;
    let body: Value = response.json().await.ok()?;
    let mut tokens = read_token_response(&body)?;
    // A provider that does not reissue the refresh token means the one on
    // record still stands.
    if tokens.refresh_token.is_none() {
        tokens.refresh_token = Some(refresh_token.to_owned());
    }
    seal_grant(state, tenant, instance, tokens).await.ok()?;
    state.connectors.get_instance(tenant, &instance.instance_id).await.ok()?
}

// --------------------------------------------------------------------- //
// Connections become tools
// --------------------------------------------------------------------- //
//
// A connection a builder configured this afternoon has to be callable this
// afternoon. The graph's registry is built once; this is the live part of
// it. The cell holds the tools every stored connection currently derives,
// and the server refills it at the moments a connection changes — created,
// granted, refreshed — and once at boot. Reads are a lock and a clone.

/// One binding in the history: which connection a tool ran through, from
/// when until when. Active while `unbound_at` is `None`. The one record
/// Catalog, Security, an approval and a run's evidence all read — and the
/// one that outlives the connection, so a run that ran through a connection
/// since revoked is still named honestly.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Binding {
    pub tool: String,
    pub instance_id: String,
    /// The connection's display name at binding time.
    pub connection: String,
    pub bound_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unbound_at: Option<DateTime<Utc>>,
}

impl Binding {
    fn active(&self) -> bool {
        self.unbound_at.is_none()
    }
    fn covers(&self, at: DateTime<Utc>) -> bool {
        self.bound_at <= at && self.unbound_at.is_none_or(|end| end > at)
    }
    /// The connection as a client reads it.
    pub fn served(&self) -> Value {
        json!({
            "instance_id": self.instance_id,
            "name": self.connection,
            "revoked_at": self.unbound_at,
        })
    }
}

/// The tools of every configured connection, as a live [`ToolSource`] the
/// `react_agent` registry is built over. Empty until the server fills it.
#[derive(Default)]
pub struct ConnectionTools {
    tools: std::sync::RwLock<Vec<Arc<dyn rusty_agent_runtime::tool::Tool>>>,
    /// The binding history: every (tool, connection) range there has been.
    /// Restored at boot, kept on every change.
    bindings: std::sync::RwLock<Vec<Binding>>,
}

impl ConnectionTools {
    fn history(&self) -> Vec<Binding> {
        self.bindings.read().map(|b| b.clone()).unwrap_or_default()
    }

    /// The tools a connection derives, by name (active bindings).
    pub fn tools_of(&self, instance_id: &str) -> Vec<String> {
        self.history().into_iter().filter(|b| b.active() && b.instance_id == instance_id).map(|b| b.tool).collect()
    }

    /// The connection a tool runs through now, when it is a connection's.
    pub fn instance_of(&self, tool: &str) -> Option<String> {
        self.active_binding(tool).map(|b| b.instance_id)
    }

    /// The active binding of a tool.
    pub fn active_binding(&self, tool: &str) -> Option<Binding> {
        self.history().into_iter().find(|b| b.active() && b.tool == tool)
    }

    /// The binding a tool ran through at `at`: the range covering it, else
    /// the latest one bound before it (a stub's refusal after a revoke maps
    /// to the connection that was revoked).
    pub fn binding_at(&self, tool: &str, at: DateTime<Utc>) -> Option<Binding> {
        let mine: Vec<Binding> = self.history().into_iter().filter(|b| b.tool == tool).collect();
        mine.iter()
            .find(|b| b.covers(at))
            .cloned()
            .or_else(|| mine.into_iter().filter(|b| b.bound_at <= at).max_by_key(|b| b.bound_at))
    }

    /// The binding of `tool` to `instance_id`, latest first — how an
    /// approval learns the connection it named was revoked while it waited.
    pub fn binding_of(&self, tool: &str, instance_id: &str) -> Option<Binding> {
        self.history().into_iter().filter(|b| b.tool == tool && b.instance_id == instance_id).max_by_key(|b| b.bound_at)
    }

    /// Tools that have no active binding but had one: the revoked ones. A
    /// connection still bound under another tool name was renamed, not
    /// revoked (a second connection of its connector arrived or left), so
    /// its old name does not answer as a revoked tool.
    fn revoked(&self) -> Vec<Binding> {
        let history = self.history();
        let mut out: Vec<Binding> = Vec::new();
        for b in history.iter().filter(|b| !b.active()) {
            if history.iter().any(|a| a.active() && (a.tool == b.tool || a.instance_id == b.instance_id))
                || out.iter().any(|o| o.tool == b.tool)
            {
                continue;
            }
            let latest = history.iter().filter(|x| x.tool == b.tool && !x.active()).max_by_key(|x| x.bound_at).cloned().expect("at least this one");
            out.push(latest);
        }
        out
    }

    /// Record which connection a tool runs through now (tests).
    pub fn bind(&self, tool: &str, instance_id: &str) {
        if let Ok(mut b) = self.bindings.write() {
            b.push(Binding { tool: tool.to_owned(), instance_id: instance_id.to_owned(), connection: instance_id.to_owned(), bound_at: Utc::now(), unbound_at: None });
        }
    }

    /// Replace the history wholesale (boot restore).
    pub fn load(&self, history: Vec<Binding>) {
        if let Ok(mut b) = self.bindings.write() {
            *b = history;
        }
    }

    /// Reconcile the history with what is bound now: a binding that is
    /// gone closes, a new one opens, one that stands is untouched. Returns
    /// the history to keep.
    pub fn reconcile(&self, now_bound: &[(String, String, String)], at: DateTime<Utc>) -> Vec<Binding> {
        let mut history = self.history();
        for b in history.iter_mut().filter(|b| b.active()) {
            if !now_bound.iter().any(|(tool, id, _)| *tool == b.tool && *id == b.instance_id) {
                b.unbound_at = Some(at);
            }
        }
        for (tool, id, name) in now_bound {
            if !history.iter().any(|b| b.active() && b.tool == *tool && b.instance_id == *id) {
                history.push(Binding { tool: tool.clone(), instance_id: id.clone(), connection: name.clone(), bound_at: at, unbound_at: None });
            }
        }
        self.load(history.clone());
        history
    }
}

/// What answers for a tool whose connection was revoked: the name still
/// resolves — a run resumed after the revoke, an agent that names it — but
/// nothing is sent; the call is refused with the connection named. Pure,
/// so no approval is minted for a call that cannot reach anything.
struct RevokedConnectionTool {
    binding: Binding,
    description: String,
}

#[async_trait::async_trait]
impl rusty_agent_runtime::tool::Tool for RevokedConnectionTool {
    fn name(&self) -> &str {
        &self.binding.tool
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn call(&self, _args: Value) -> rusty_agent_runtime::error::Result<Value> {
        Err(rusty_agent_runtime::error::RustyError::Tool(format!(
            "the {} connection ({}) this tool ran through was revoked at {}; nothing was sent — an admin reconnects it under Catalog → Connections",
            self.binding.connection,
            self.binding.instance_id,
            self.binding.unbound_at.map(|t| t.to_rfc3339()).unwrap_or_default()
        )))
    }
}

impl std::fmt::Debug for ConnectionTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.tools.read().map(|tools| tools.len()).unwrap_or(0);
        f.debug_struct("ConnectionTools").field("tools", &count).finish()
    }
}

impl ConnectionTools {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl rusty_agent_runtime::tool::ToolSource for ConnectionTools {
    fn tools(&self) -> Vec<Arc<dyn rusty_agent_runtime::tool::Tool>> {
        let mut tools = self.tools.read().map(|tools| tools.clone()).unwrap_or_default();
        for binding in self.revoked() {
            let description = format!(
                "Unavailable: the {} connection ({}) was revoked on {}. Nothing sent through it reaches the system; an admin reconnects it under Catalog → Connections.",
                binding.connection,
                binding.instance_id,
                binding.unbound_at.map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default()
            );
            tools.push(Arc::new(RevokedConnectionTool { binding, description }));
        }
        tools
    }
}

/// Restore the binding history at boot, before the first refresh closes
/// or opens anything.
pub(crate) async fn restore_bindings(state: &AppState) {
    let Some(cell) = &state.connection_tools else {
        return;
    };
    match state.server_store.get_bindings().await {
        Ok(Some(history)) => {
            tracing::info!(bindings = history.len(), "connection bindings restored");
            cell.load(history);
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "connection bindings: the store could not be read"),
    }
}

/// The connections a run's tool calls went through, from its journal and
/// the binding history: `[{instance_id, name, revoked_at, tools: [{tool,
/// calls}]}]`. A tool with no binding (the platform's own) is not listed.
pub(crate) async fn run_connections(state: &AppState, tenant: &TenantContext, run_id: &str) -> Vec<Value> {
    let Some(cell) = &state.connection_tools else {
        return Vec::new();
    };
    let Ok(evidence) = crate::routes::run_evidence(state, tenant, run_id).await else {
        return Vec::new();
    };
    let Some(snapshot) = evidence.journal else {
        return Vec::new();
    };
    let payload = |reference: &rusty_agent_runtime::record::PayloadRef| -> Option<Value> {
        match reference {
            rusty_agent_runtime::record::PayloadRef::Inline(value) => Some(value.clone()),
            rusty_agent_runtime::record::PayloadRef::Artifact(artifact) => snapshot.artifacts.get(&artifact.sha256).cloned(),
        }
    };
    // (instance_id, name, revoked_at) → tool → calls, in first-seen order.
    let mut out: Vec<(Binding, Vec<(String, usize)>)> = Vec::new();
    for event in snapshot.events.iter().filter(|e| e.kind == rusty_agent_runtime::record::RunEventKind::ToolCall) {
        let Some(tool) = event.input.as_ref().and_then(&payload).and_then(|v| v.get("tool").and_then(Value::as_str).map(str::to_owned)) else {
            continue;
        };
        let Some(binding) = cell.binding_at(&tool, event.recorded_at) else {
            continue;
        };
        let entry = match out.iter_mut().find(|(b, _)| b.instance_id == binding.instance_id) {
            Some(entry) => entry,
            None => {
                out.push((binding.clone(), Vec::new()));
                out.last_mut().expect("just pushed")
            }
        };
        match entry.1.iter_mut().find(|(t, _)| *t == tool) {
            Some(t) => t.1 += 1,
            None => entry.1.push((tool, 1)),
        }
    }
    out.into_iter()
        .map(|(b, tools)| {
            let mut served = b.served();
            served["tools"] = json!(tools.iter().map(|(t, n)| json!({"tool": t, "calls": n})).collect::<Vec<_>>());
            served
        })
        .collect()
}

/// Rebuild the connection tools from what the store holds.
///
/// Every instance's manifest is read, its sealed secrets opened for this
/// process only, and each read operation becomes a [`ConnectorMethodTool`]
/// named `{connector}.{operation}` — the same name the connection's catalog
/// shows a builder, so what is picked is what runs. A second connection to
/// the same connector is named with its instance so the two stay distinct.
///
/// Only the default tenant's connections are sourced today: the registry is
/// one per process, not one per tenant, and giving each tenant its own tool
/// view is authorization work (G13), not connector work.
pub(crate) async fn refresh_connection_tools(state: &AppState) {
    let Some(cell) = &state.connection_tools else {
        return;
    };
    let tenant = crate::auth::DEFAULT_TENANT;
    let context = TenantContext::new(tenant.to_owned(), Vec::new());
    let instances = match state.connectors.list_instances(tenant).await {
        Ok(instances) => instances,
        Err(error) => {
            tracing::warn!(%error, "connection tools: could not list instances");
            return;
        }
    };
    // Rebuild the policy in force first: every configured connection's host
    // is reachable, nothing else is, and the tools built below call through
    // it. Egress is on by default because the policy always exists.
    let policy = Arc::new(refresh_egress_policy(state).await);
    let policy = Some(policy);
    let transport: Arc<dyn ConnectorTransport> = Arc::new(
        ReqwestConnectorTransport::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client builds"),
            policy,
            "connection-tool",
        )
        .with_worlds(Arc::clone(&state.worlds)),
    );

    // How many connections each connector has: one gets the plain name, more
    // than one get the instance in the name.
    let mut per_connector: BTreeMap<String, usize> = BTreeMap::new();
    let mut resolved = Vec::with_capacity(instances.len());
    for instance in instances {
        let Ok(Some(manifest)) = state
            .connectors
            .get_manifest(tenant, &instance.manifest_hash)
            .await
        else {
            continue;
        };
        *per_connector.entry(manifest.id.clone()).or_default() += 1;
        resolved.push((instance, manifest));
    }

    let mut tools: Vec<Arc<dyn rusty_agent_runtime::tool::Tool>> = Vec::new();
    let mut bindings: Vec<(String, String, String)> = Vec::new();
    for (instance, manifest) in resolved {
        // A granted connection with nothing granted yet has no credential to
        // call with; it is a connection in name only until someone approves.
        if manifest.authorization.is_some() && !instance.sealed.contains_key(GRANT_ACCESS_TOKEN) {
            continue;
        }
        let config = match opened_config(state, &context, &instance).await {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(instance = %instance.instance_id, error = ?error, "connection tools: secrets could not be opened");
                continue;
            }
        };
        // A second connection of one connector names its tools by the
        // connection too, in the grammar a tool name allows: a hyphen and
        // the instance's short id, never a character the catalog refuses —
        // one refused name empties the whole catalog.
        let shared = per_connector.get(&manifest.id).copied().unwrap_or(1) > 1;
        let suffix = shared.then(|| instance.instance_id.trim_start_matches(INSTANCE_ID_PREFIX)[..8].to_owned());
        for tool in rusty_agent_runtime::connector::ConnectorMethodTool::for_manifest(
            Arc::new(manifest.clone()),
            Arc::new(config),
            Arc::clone(&transport),
        ) {
            let operation = tool_operation(&tool);
            let tool: Arc<dyn rusty_agent_runtime::tool::Tool> = match &suffix {
                Some(short) => Arc::new(tool.renamed(format!("{}-{short}.{operation}", manifest.id))),
                None => Arc::new(tool),
            };
            bindings.push((tool.name().to_owned(), instance.instance_id.clone(), manifest.display_name.clone()));
            tools.push(tool);
        }
    }
    let count = tools.len();
    if let Ok(mut cell) = cell.tools.write() {
        *cell = tools;
    }
    // The binding history: what closed, what opened, kept in the store so
    // a run's evidence names its connection after the connection is gone.
    let history = cell.reconcile(&bindings, Utc::now());
    if let Err(error) = state.server_store.put_bindings(&history).await {
        tracing::warn!(%error, "connection bindings: not kept");
    }
    tracing::info!(count, bindings = history.len(), "connection tools refreshed");
    // A gap filed on one of these operations closes now.
    crate::routes::close_gaps_on_capabilities(state).await;
}

fn tool_operation(tool: &rusty_agent_runtime::connector::ConnectorMethodTool) -> String {
    use rusty_agent_runtime::tool::Tool as _;
    tool.name().rsplit('.').next().unwrap_or_default().to_owned()
}

// --------------------------------------------------------------------- //
// Egress, on by default
// --------------------------------------------------------------------- //
//
// The engine is an allow-list: a host with no endpoint policy is denied, and
// a host that resolves to a private, loopback or link-local address is
// refused at preflight unless pinned. What was missing was a policy at all —
// with none configured, nothing was checked. Now one always exists: every
// configured connection's host, plus whatever the operator allow-listed,
// and nothing else. A connection is an operator's decision to reach a host;
// that decision is the policy.

/// The host of an https URL, or `None` for anything else.
pub(crate) fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .filter(|u| u.scheme() == "https")
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
}

/// An endpoint policy that reaches one host, wherever its paths go.
pub(crate) fn reach(name: &str, host: &str) -> rusty_agent_runtime::egress::EgressEndpointPolicy {
    use rusty_agent_runtime::egress::*;
    EgressEndpointPolicy {
        name: name.to_owned(),
        endpoint: EgressEndpoint {
            host: host.to_owned(),
            port: 443,
            protocol: EgressProtocol::Rest,
            tls: true,
            rewrite: EgressRewrite::default(),
            allowed_ips: Vec::new(),
            allow_encoded_slashes: false,
        },
        rules: vec![EgressRule {
            methods: Vec::new(),
            path_pattern: "/**".to_owned(),
            mode: EgressRuleMode::Enforce,
            tool_names: None,
        }],
        originating: Vec::new(),
    }
}

/// The policy in force right now, optionally widened by one candidate host
/// for the duration of a pre-save check.
pub(crate) fn effective_egress_policy(
    state: &AppState,
    candidate: Option<&str>,
) -> rusty_agent_runtime::egress::EgressPolicy {
    let mut policy = state
        .egress
        .read()
        .map(|p| p.clone())
        .unwrap_or(rusty_agent_runtime::egress::EgressPolicy { policies: Vec::new() });
    if let Some(host) = candidate {
        let host = host.to_ascii_lowercase();
        if crate::egress_ceiling::current(state).fits(&host) && !policy.policies.iter().any(|p| p.endpoint.host == host) {
            policy.policies.push(reach(&format!("candidate:{host}"), &host));
        }
    }
    policy
}

/// The hosts one connection calls: its API host, and the token endpoint's
/// host for a granted or client-credentials connector.
pub(crate) fn connection_hosts(manifest: &ConnectorManifest, config: &Value) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    let mut add = |url: Option<String>| {
        if let Some(host) = url.as_deref().and_then(host_of) {
            let host = host.to_ascii_lowercase();
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
    };
    add(rusty_agent_runtime::connector::render_template(&manifest.base_url, config).ok());
    if let Some(auth) = &manifest.authorization {
        add(rusty_agent_runtime::connector::render_template(&auth.token_url, config).ok());
    }
    for op in &manifest.operations {
        for alt in &op.auth {
            if let rusty_agent_runtime::connector::OperationAuth::OAuth2ClientCredentials { token_url, .. } = alt {
                add(rusty_agent_runtime::connector::render_template(token_url, config).ok());
            }
        }
    }
    hosts
}

/// The refusal a connection outside the ceiling gets, at creation,
/// rotation and upgrade alike: the host, and where it is allowed.
pub(crate) fn under_ceiling(state: &AppState, manifest: &ConnectorManifest, config: &Value) -> Result<(), ApiError> {
    let ceiling = crate::egress_ceiling::current(state);
    let hosts = connection_hosts(manifest, config);
    match crate::egress_ceiling::outside(&ceiling, hosts.iter().map(String::as_str)) {
        None => Ok(()),
        Some(host) => Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "egress_outside_ceiling",
            format!(
                "{} would call {host}, which is outside this deployment's egress ceiling — an administrator allows the host under Settings → Security → Sites agents may reach, or the connection is not made",
                manifest.display_name
            ),
        )
        .with("host", json!(host))),
    }
}

/// Recompute the policy in force from the ceiling and the connections
/// that exist, store it, and return it. Open ceiling: the operator's
/// allow-list plus every connection's hosts — what an unbounded
/// deployment always was. Closed: the ceiling's own hosts, and the
/// connections' hosts that fit under it (a wildcard entry admits them
/// without naming them).
pub(crate) async fn refresh_egress_policy(
    state: &AppState,
) -> rusty_agent_runtime::egress::EgressPolicy {
    let ceiling = crate::egress_ceiling::current(state);
    let mut policy = if ceiling.open {
        state
            .config
            .egress_policy
            .clone()
            .unwrap_or(rusty_agent_runtime::egress::EgressPolicy { policies: Vec::new() })
    } else {
        rusty_agent_runtime::egress::EgressPolicy {
            policies: ceiling.concrete_hosts().map(|host| reach(&format!("ceiling:{host}"), host)).collect(),
        }
    };
    let tenant = crate::auth::DEFAULT_TENANT;
    let context = TenantContext::new(tenant.to_owned(), Vec::new());
    if let Ok(instances) = state.connectors.list_instances(tenant).await {
        for instance in instances {
            let Ok(Some(manifest)) = state
                .connectors
                .get_manifest(tenant, &instance.manifest_hash)
                .await
            else {
                continue;
            };
            let Ok(config) = opened_config(state, &context, &instance).await else {
                continue;
            };
            for host in connection_hosts(&manifest, &config) {
                if !ceiling.fits(&host) {
                    tracing::warn!(host, instance = %instance.instance_id, "connection host outside the egress ceiling — unreachable until allowed");
                    continue;
                }
                if !policy.policies.iter().any(|p| p.endpoint.host == host) {
                    policy
                        .policies
                        .push(reach(&format!("connection:{}:{host}", instance.instance_id), &host));
                }
            }
        }
    }
    if let Ok(mut cell) = state.egress.write() {
        *cell = policy.clone();
    }
    tracing::info!(hosts = policy.policies.len(), open = ceiling.open, "egress policy in force");
    policy
}

#[cfg(test)]
mod binding_tests {
    #[test]
    fn a_renamed_connection_is_not_a_revoked_one() {
        use rusty_agent_runtime::tool::ToolSource as _;
        let cell = ConnectionTools::new();
        let t0 = chrono::Utc::now();
        // One connection, plain name.
        cell.reconcile(&[("ticketing.create".into(), "inst-a".into(), "Ticketing".into())], t0);
        // A second connection arrives: both are renamed by instance.
        cell.reconcile(
            &[
                ("ticketing-aaaaaaaa.create".into(), "inst-a".into(), "Ticketing".into()),
                ("ticketing-bbbbbbbb.create".into(), "inst-b".into(), "Ticketing 2".into()),
            ],
            t0,
        );
        let names: Vec<String> = cell.tools().iter().map(|t| t.name().to_owned()).collect();
        assert!(!names.iter().any(|n| n == "ticketing.create"), "the old name is a rename, not a revoke: {names:?}");
        // The second connection goes: its tool is revoked; the first keeps
        // its plain name back, and the renamed one is not revoked either.
        cell.reconcile(&[("ticketing.create".into(), "inst-a".into(), "Ticketing".into())], t0);
        let names: Vec<String> = cell.tools().iter().map(|t| t.name().to_owned()).collect();
        assert!(names.contains(&"ticketing-bbbbbbbb.create".to_owned()), "{names:?}");
        assert!(!names.contains(&"ticketing-aaaaaaaa.create".to_owned()), "{names:?}");
    }

    use super::ConnectionTools;

    #[test]
    fn a_tool_binds_to_the_connection_it_runs_through() {
        let cell = ConnectionTools::new();
        cell.bind("servicenow.list-records", "inst-a");
        cell.bind("servicenow.create-incident", "inst-a");
        cell.bind("slack.post-message", "inst-b");
        assert_eq!(cell.tools_of("inst-a"), ["servicenow.list-records", "servicenow.create-incident"]);
        assert_eq!(cell.instance_of("slack.post-message").as_deref(), Some("inst-b"));
        assert_eq!(cell.instance_of("echo"), None);
        assert!(cell.tools_of("inst-c").is_empty());
    }
}

// ── Read-backs proposed and adopted ─────────────────────────────────────────

/// `GET /connectors/{hash}/read-backs` — the writes of this connector
/// version that would guess after a lost answer, the read-backs its own
/// reads make possible for them, and the ones already declared.
pub(crate) async fn read_back_proposals(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(hash): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let manifest = manifest_for(&state, &tenant, &hash).await?;
    let proposals = manifest.propose_read_backs();
    let unproposable: Vec<Value> = manifest
        .writes_without_read_back()
        .iter()
        .filter(|w| !proposals.iter().any(|p| p.write == w.name))
        .map(|w| json!({"write": w.name, "why": format!("no read on {} takes a filter over one of `{}`'s fields — add one by hand, or extend the connector with a read that does", w.path, w.name)}))
        .collect();
    let declared: Vec<Value> = manifest.operations.iter().filter_map(|op| op.reconcile.as_ref().map(|r| json!({"write": op.name, "operation": r.operation}))).collect();
    Ok(Json(json!({"hash": manifest.hash, "id": manifest.id, "version": manifest.version, "proposals": proposals, "unproposable": unproposable, "declared": declared})))
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct AdoptReadBacks {
    pub adopt: Vec<rusty_agent_runtime::connector::manifest::ReadBack>,
    #[serde(default)]
    pub writes: Vec<String>,
    /// A connection on this version to move to the new one. A read-back
    /// changes no request the connection's check would make and no
    /// configuration it needs, so the move needs no check: the credential
    /// and the address are the ones already proven.
    #[serde(default)]
    pub instance_id: Option<String>,
}

/// The next version after `version`: a number counts up, a dotted form
/// counts its last part up, anything else gains `.1`.
fn next_version(version: &str) -> String {
    if let Ok(n) = version.trim().parse::<u64>() {
        return (n + 1).to_string();
    }
    if let Some((head, last)) = version.rsplit_once('.') {
        if let Ok(n) = last.parse::<u64>() {
            return format!("{head}.{}", n + 1);
        }
    }
    format!("{version}.1")
}

/// `POST /connectors/{hash}/read-backs` — adopt read-backs for named
/// writes: `{"writes": ["create"], "adopt": [{"operation": "list",
/// "arguments": {…}}]}`, one per write in order. The result is a new
/// version of the connector in the library, hashed like any other; a
/// connection moves to it by upgrading, as to any version.
pub(crate) async fn adopt_read_backs(
    AxumState(state): AxumState<std::sync::Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    AxumPath(hash): AxumPath<String>,
    Json(payload): Json<AdoptReadBacks>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let mut manifest = manifest_for(&state, &tenant, &hash).await?;
    if payload.writes.is_empty() || payload.writes.len() != payload.adopt.len() {
        return Err(ApiError::bad_request("name each write and its read-back, one to one: `writes` and `adopt` of the same length".to_owned()));
    }
    for (write, read_back) in payload.writes.iter().zip(payload.adopt.iter()) {
        let read_exists = manifest.operations.iter().any(|op| op.name == read_back.operation && op.method == rusty_agent_runtime::connector::HttpMethod::Get);
        if !read_exists {
            return Err(ApiError::bad_request(format!("`{}` is not a read of this connector; a read-back is one of its own GET operations", read_back.operation)));
        }
        let op = manifest
            .operations
            .iter_mut()
            .find(|op| &op.name == write && op.method != rusty_agent_runtime::connector::HttpMethod::Get)
            .ok_or_else(|| ApiError::bad_request(format!("`{write}` is not a write of this connector")))?;
        op.reconcile = Some(read_back.clone());
    }
    manifest.version = next_version(&manifest.version);
    manifest.hash = String::new();
    let manifest = manifest.sealed().map_err(|e| ApiError::bad_request(e.to_string()))?;
    let registered = state.connectors.put_manifest(tenant.tenant(), &manifest).await.map_err(store_err)?;
    tracing::info!(connector = %manifest.id, version = %manifest.version, "read-backs adopted as a new version");
    let mut moved = None;
    if let Some(instance_id) = &payload.instance_id {
        let existing = state
            .connectors
            .get_instance(tenant.tenant(), instance_id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| ApiError::not_found(format!("unknown connector instance `{instance_id}`")))?;
        if existing.manifest_hash != hash {
            return Err(ApiError::bad_request(format!("connection `{instance_id}` is not on version {} of {}; move it there first", hash, manifest.id)));
        }
        let upgraded = ConnectorInstance::new(instance_id, &manifest.hash, existing.config.clone(), existing.sealed.clone(), existing.created_at)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        state.connectors.put_instance(tenant.tenant(), &upgraded).await.map_err(store_err)?;
        refresh_connection_tools(&state).await;
        tracing::info!(%instance_id, to = %manifest.version, connector = %manifest.id, "connection moved to the version with read-backs");
        moved = Some(instance_id.clone());
    }
    Ok((if registered { StatusCode::CREATED } else { StatusCode::OK }, Json(json!({"hash": manifest.hash, "id": manifest.id, "version": manifest.version, "registered": registered, "moved": moved}))))
}

#[cfg(test)]
mod read_back_version_tests {
    #[test]
    fn the_next_version_counts_up_in_the_form_the_connector_uses() {
        assert_eq!(super::next_version("5"), "6");
        assert_eq!(super::next_version("0.1.0"), "0.1.1");
        assert_eq!(super::next_version("2026-09"), "2026-09.1");
    }
}
