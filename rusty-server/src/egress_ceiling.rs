//! The operator's egress ceiling: the hosts this deployment may reach at
//! all. Connections fit under it or are not made; the OAuth token
//! exchange, `web.fetch` and a skills import ride the same bound. The
//! environment (`RUSTY_EGRESS_ALLOW`) seeds it once, together with the
//! hosts the connections in use already call; from then on the store is
//! the truth and an administrator edits it under Settings → Security → Sites agents may reach. A
//! deployment booted with no allow-list is *open* — it says so, and one
//! action closes it to the hosts in use. A connector narrows what is
//! reachable (its own host, its token endpoint); it never widens it.

use std::sync::{Arc, RwLock};

use axum::extract::State as AxumState;
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::TenantContext;
use crate::error::ApiError;
use crate::routes::AppState;

/// One allowed host: exact (`api.example.com`) or a wildcard over
/// subdomains (`*.service-now.com`), with who allowed it and when.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CeilingHost {
    pub host: String,
    pub added_by: Value,
    pub added_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The ceiling: open (anything a connection names is reachable — the
/// state of a deployment nobody has bounded yet) or the listed hosts.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EgressCeiling {
    #[serde(default)]
    pub open: bool,
    #[serde(default)]
    pub hosts: Vec<CeilingHost>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// The cell the server and the OAuth provider share.
pub type SharedCeiling = Arc<RwLock<EgressCeiling>>;

impl EgressCeiling {
    /// What a deployment boots with: open when the environment set no
    /// allow-list, otherwise closed to the environment's hosts.
    pub fn boot(policy: Option<&rusty_agent_runtime::egress::EgressPolicy>) -> Self {
        let Some(policy) = policy else {
            return Self {
                open: true,
                ..Self::default()
            };
        };
        let now = Utc::now();
        Self {
            open: false,
            hosts: policy
                .policies
                .iter()
                .map(|p| CeilingHost {
                    host: p.endpoint.host.to_ascii_lowercase(),
                    added_by: json!({"kind": "environment"}),
                    added_at: now,
                    note: Some("RUSTY_EGRESS_ALLOW".to_owned()),
                })
                .collect(),
            updated_by: None,
            updated_at: None,
        }
    }

    /// Whether `host` is reachable under this ceiling.
    pub fn fits(&self, host: &str) -> bool {
        self.open || self.hosts.iter().any(|h| host_matches(&h.host, host))
    }

    /// The listed hosts that are concrete (no wildcard) — the ones an
    /// endpoint policy can name outright.
    pub fn concrete_hosts(&self) -> impl Iterator<Item = &str> {
        self.hosts
            .iter()
            .map(|h| h.host.as_str())
            .filter(|h| !h.starts_with("*."))
    }
}

/// `pattern` is an exact host or `*.suffix`; a wildcard matches any host
/// with at least one label before the suffix, never the bare suffix.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let pattern = pattern.to_ascii_lowercase();
    match pattern.strip_prefix("*.") {
        Some(suffix) => host
            .strip_suffix(suffix)
            .is_some_and(|head| head.len() > 1 && head.ends_with('.')),
        None => host == pattern,
    }
}

/// A host as an administrator may type it: lowercased, no scheme, path or
/// port, an optional leading `*.`.
pub fn normalize_host(raw: &str) -> Result<String, String> {
    let host = raw.trim().to_ascii_lowercase();
    if host.is_empty() {
        return Err("a host is empty".to_owned());
    }
    if host.contains("://") || host.contains('/') {
        return Err(format!("`{host}` is a URL — a ceiling entry is a host name, such as api.example.com or *.example.com"));
    }
    if host.contains(':') || host.contains(char::is_whitespace) {
        return Err(format!("`{host}` is not a host name — no port, no spaces"));
    }
    let body = host.strip_prefix("*.").unwrap_or(&host);
    if body.is_empty()
        || body.starts_with('.')
        || body.ends_with('.')
        || body.contains("..")
        || body.contains('*')
    {
        return Err(format!("`{host}` is not a host name"));
    }
    if !body
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(format!("`{host}` is not a host name"));
    }
    Ok(host)
}

/// The first of `hosts` the ceiling does not admit.
pub fn outside<'a>(
    ceiling: &EgressCeiling,
    hosts: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    hosts
        .into_iter()
        .find(|h| !ceiling.fits(h))
        .map(str::to_owned)
}

/// The ceiling in force now.
pub(crate) fn current(state: &AppState) -> EgressCeiling {
    state.ceiling.read().map(|c| c.clone()).unwrap_or_default()
}

pub(crate) fn set(state: &AppState, ceiling: EgressCeiling) {
    if let Ok(mut cell) = state.ceiling.write() {
        *cell = ceiling;
    }
}

/// Every connection with the hosts it calls, for the default tenant — the
/// ceiling is deployment-wide.
async fn connections_in_use(state: &AppState) -> Vec<(String, String, Vec<String>)> {
    let tenant = crate::auth::DEFAULT_TENANT;
    let context = TenantContext::new(tenant.to_owned(), Vec::new());
    let mut out = Vec::new();
    let Ok(instances) = state.connectors.list_instances(tenant).await else {
        return out;
    };
    for instance in instances {
        let Ok(Some(manifest)) = state
            .connectors
            .get_manifest(tenant, &instance.manifest_hash)
            .await
        else {
            continue;
        };
        let Ok(config) = crate::connectors::opened_config(state, &context, &instance).await else {
            continue;
        };
        out.push((
            instance.instance_id.clone(),
            manifest.display_name.clone(),
            crate::connectors::connection_hosts(&manifest, &config),
        ));
    }
    out
}

/// At boot: the stored ceiling wins; with none stored, what is reachable
/// today — the environment's hosts and the hosts of every connection in
/// use — becomes the ceiling and is kept.
pub(crate) async fn restore(state: &AppState) {
    match state.server_store.get_egress_ceiling().await {
        Ok(Some(stored)) => {
            tracing::info!(
                open = stored.open,
                hosts = stored.hosts.len(),
                "egress ceiling restored"
            );
            set(state, stored);
            return;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(%error, "egress ceiling: the store could not be read; the boot ceiling stays");
            return;
        }
    }
    let mut ceiling = current(state);
    if !ceiling.open {
        let now = Utc::now();
        for (_, name, hosts) in connections_in_use(state).await {
            for host in hosts {
                if !ceiling.fits(&host) {
                    ceiling.hosts.push(CeilingHost {
                        host,
                        added_by: json!({"kind": "boot"}),
                        added_at: now,
                        note: Some(format!("in use by {name} when the ceiling was first kept")),
                    });
                }
            }
        }
    }
    ceiling.updated_at = Some(Utc::now());
    match state.server_store.put_egress_ceiling(&ceiling).await {
        Ok(()) => tracing::info!(
            open = ceiling.open,
            hosts = ceiling.hosts.len(),
            "egress ceiling seeded and kept"
        ),
        Err(error) => tracing::warn!(%error, "egress ceiling: seeded but not kept"),
    }
    set(state, ceiling);
}

async fn served(state: &AppState, ceiling: &EgressCeiling) -> Value {
    let in_use = connections_in_use(state).await;
    let used_by = |host: &str| -> Vec<Value> {
        in_use
            .iter()
            .filter(|(_, _, hosts)| hosts.iter().any(|h| host_matches(host, h)))
            .map(|(id, name, _)| json!({"instance_id": id, "name": name}))
            .collect()
    };
    let mut unlisted: Vec<Value> = Vec::new();
    for (id, name, hosts) in &in_use {
        for host in hosts {
            if !ceiling.hosts.iter().any(|h| host_matches(&h.host, host))
                && !unlisted.iter().any(|u| u["host"] == *host)
            {
                unlisted.push(
                    json!({"host": host, "connections": [{"instance_id": id, "name": name}]}),
                );
            }
        }
    }
    json!({
        "open": ceiling.open,
        "hosts": ceiling.hosts.iter().map(|h| json!({
            "host": h.host, "added_by": h.added_by, "added_at": h.added_at, "note": h.note, "used_by": used_by(&h.host),
        })).collect::<Vec<_>>(),
        "unlisted": unlisted,
        "updated_by": ceiling.updated_by,
        "updated_at": ceiling.updated_at,
    })
}

pub(crate) async fn get_ceiling(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(_tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let ceiling = current(&state);
    Ok(Json(served(&state, &ceiling).await))
}

#[derive(Debug, Deserialize)]
pub struct CeilingHostInput {
    pub host: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CeilingInput {
    #[serde(default)]
    pub open: bool,
    #[serde(default)]
    pub hosts: Vec<CeilingHostInput>,
}

/// Replace the ceiling. Hosts already listed keep who allowed them; a
/// closed ceiling must still admit every connection in use — revoke the
/// connection first, then narrow.
pub(crate) async fn put_ceiling(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(input): Json<CeilingInput>,
) -> Result<Json<Value>, ApiError> {
    let before = current(&state);
    let now = Utc::now();
    let by = tenant.attribution();
    let mut hosts: Vec<CeilingHost> = Vec::new();
    for entry in &input.hosts {
        let host = normalize_host(&entry.host)
            .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_host", e))?;
        if hosts.iter().any(|h| h.host == host) {
            continue;
        }
        let note = entry
            .note
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned);
        match before.hosts.iter().find(|h| h.host == host) {
            Some(kept) => hosts.push(CeilingHost {
                note: note.or_else(|| kept.note.clone()),
                ..kept.clone()
            }),
            None => hosts.push(CeilingHost {
                host,
                added_by: by.clone(),
                added_at: now,
                note,
            }),
        }
    }
    let ceiling = EgressCeiling {
        open: input.open,
        hosts,
        updated_by: Some(by),
        updated_at: Some(now),
    };
    if !ceiling.open {
        for (_, name, used) in connections_in_use(&state).await {
            if let Some(host) = outside(&ceiling, used.iter().map(String::as_str)) {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "ceiling_below_connections",
                    format!("{name} calls {host}, which this ceiling would not admit — revoke the connection first, or keep the host"),
                )
                .with("host", json!(host))
                .with("connection", json!(name)));
            }
        }
    }
    state
        .server_store
        .put_egress_ceiling(&ceiling)
        .await
        .map_err(ApiError::internal)?;
    set(&state, ceiling.clone());
    // The policy in force follows the ceiling, and the connection tools
    // with it.
    crate::connectors::refresh_connection_tools(&state).await;
    tracing::info!(
        open = ceiling.open,
        hosts = ceiling.hosts.len(),
        "egress ceiling changed"
    );
    Ok(Json(served(&state, &ceiling).await))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(hosts: &[&str]) -> EgressCeiling {
        EgressCeiling {
            open: false,
            hosts: hosts
                .iter()
                .map(|h| CeilingHost {
                    host: (*h).to_owned(),
                    added_by: json!({"kind": "test"}),
                    added_at: Utc::now(),
                    note: None,
                })
                .collect(),
            updated_by: None,
            updated_at: None,
        }
    }

    #[test]
    fn a_listed_ceiling_admits_exact_hosts_and_wildcard_subdomains_only() {
        let ceiling = listed(&["api.example.com", "*.service-now.com"]);
        assert!(ceiling.fits("api.example.com"));
        assert!(ceiling.fits("API.Example.COM"));
        assert!(!ceiling.fits("example.com"));
        assert!(!ceiling.fits("evil-api.example.com"));
        assert!(ceiling.fits("dev12345.service-now.com"));
        assert!(ceiling.fits("a.b.service-now.com"));
        assert!(
            !ceiling.fits("service-now.com"),
            "the bare suffix is not a subdomain"
        );
        assert!(!ceiling.fits("notservice-now.com"));
        assert_eq!(
            outside(&ceiling, ["api.example.com", "hooks.slack.com", "x.y"]),
            Some("hooks.slack.com".to_owned())
        );
        assert_eq!(
            ceiling.concrete_hosts().collect::<Vec<_>>(),
            vec!["api.example.com"]
        );
    }

    #[test]
    fn an_open_ceiling_admits_everything_and_boot_follows_the_environment() {
        assert!(EgressCeiling::boot(None).open);
        assert!(EgressCeiling::boot(None).fits("anything.example"));
        let env = rusty_agent_runtime::egress::EgressPolicy {
            policies: vec![crate::connectors::reach(
                "operator:docs.example.com",
                "Docs.Example.com",
            )],
        };
        let booted = EgressCeiling::boot(Some(&env));
        assert!(!booted.open);
        assert_eq!(booted.hosts.len(), 1);
        assert_eq!(booted.hosts[0].host, "docs.example.com");
        assert!(booted.fits("docs.example.com"));
        assert!(!booted.fits("api.example.com"));
    }

    #[test]
    fn a_host_is_typed_as_a_host_name() {
        assert_eq!(
            normalize_host("  API.Example.com "),
            Ok("api.example.com".to_owned())
        );
        assert_eq!(
            normalize_host("*.service-now.com"),
            Ok("*.service-now.com".to_owned())
        );
        assert!(normalize_host("https://api.example.com/v1")
            .unwrap_err()
            .contains("URL"));
        assert!(normalize_host("api.example.com:8443").is_err());
        assert!(normalize_host("").is_err());
        assert!(normalize_host("*.").is_err());
        assert!(normalize_host("a..b").is_err());
        assert!(normalize_host("a.*.b").is_err());
    }
}
