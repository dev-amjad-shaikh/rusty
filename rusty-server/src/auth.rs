//! API-key authentication middleware (`X-Api-Key` header), RBAC scope
//! enforcement, and the tenant model: every configured API key maps to exactly
//! one tenant and a set of scopes, and every request resolves to a
//! [`TenantContext`] injected into the request extensions.
//!
//! ## Tenancy model
//!
//! - `ServerConfig::with_api_key(k)` (legacy) maps `k` to the [`DEFAULT_TENANT`]
//!   with the super-user scope `*:*:*`.
//! - `ServerConfig::with_tenant_key(tenant, k)` maps `k` to `tenant` with the
//!   super-user scope.
//! - `ServerConfig::api_key_scopes` overrides the default scope set for a key.
//! - No keys configured at all: open (dev) mode — no header required and
//!   every request runs as the default tenant with super-user scopes,
//!   preserving v0.2/v0.3 behavior bit-for-bit.
//!
//! ## Isolation scheme
//!
//! Tenant-scoped resources are namespaced at the handler layer by prefixing
//! internal ids / KV namespaces with `{tenant}/` (see
//! [`TenantContext::scope`]). The default tenant is **unprefixed** so
//! existing deployments keep their flat on-disk layout
//! (`{store_path}/{thread_id}/…`, `assistants/{id}.json`, `store/{ns}/…`)
//! and open mode behaves exactly as before; named tenants get their own
//! subtrees (`{store_path}/{tenant}/{thread_id}/…`,
//! `assistants/{tenant}/{id}.json`, `store/{tenant}/{ns}/…`) and their own
//! Postgres rows (thread ids / namespaces carry the prefix, primary keys
//! separate naturally).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use rusty_agent_runtime::scope::{Scope, scope_authorizes};

use crate::error::AdmissionReason;
use crate::routes::AppState;

/// The tenant every request resolves to when no tenant keys are configured
/// (open mode) or when the legacy single `api_key` matches. The default
/// tenant keeps the legacy unprefixed storage layout.
pub(crate) const DEFAULT_TENANT: &str = "default";

/// The resolved tenant for one request, inserted into request extensions by
/// [`require_api_key`] and extracted by every tenant-scoped handler.
#[derive(Debug, Clone)]
pub(crate) struct TenantContext {
    tenant: String,
    scopes: Vec<Scope>,
}

impl TenantContext {
    pub(crate) fn new(tenant: String, scopes: Vec<Scope>) -> Self {
        Self { tenant, scopes }
    }

    pub(crate) fn tenant(&self) -> &str {
        &self.tenant
    }

    /// The granted scopes for this caller.
    pub(crate) fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    /// The internal id for a tenant-scoped resource: `{tenant}/{id}` for
    /// named tenants, plain `id` for the default tenant (legacy layout).
    pub(crate) fn scope(&self, id: &str) -> String {
        scope_id(&self.tenant, id)
    }

    /// Strip this tenant's prefix from an internal id, or `None` when the
    /// id belongs to a different tenant (callers answer 404 — never 403 —
    /// so one tenant cannot probe another tenant's resources).
    pub(crate) fn unscope<'a>(&self, internal_id: &'a str) -> Option<&'a str> {
        strip_owned(&self.tenant, internal_id)
    }

    /// `true` when `internal_id` lives in this tenant's namespace.
    pub(crate) fn owns(&self, internal_id: &str) -> bool {
        self.unscope(internal_id).is_some()
    }
}

/// The internal id for `id` under `tenant`: `{tenant}/{id}` for named
/// tenants, plain `id` for the default tenant.
pub(crate) fn scope_id(tenant: &str, id: &str) -> String {
    if tenant == DEFAULT_TENANT {
        id.to_string()
    } else {
        format!("{tenant}/{id}")
    }
}

/// Strip `tenant`'s prefix from an internal id (`None` when the id belongs
/// to another tenant). Default-tenant ids are exactly the unprefixed ones —
/// a default-tenant request never matches a `{other}/{id}` internal id.
pub(crate) fn strip_owned<'a>(tenant: &str, internal_id: &'a str) -> Option<&'a str> {
    if tenant == DEFAULT_TENANT {
        if internal_id.contains('/') {
            None
        } else {
            Some(internal_id)
        }
    } else {
        internal_id
            .strip_prefix(tenant)
            .and_then(|rest| rest.strip_prefix('/'))
    }
}

/// The owning tenant of an internal id: the segment before the first `/`,
/// or [`DEFAULT_TENANT`] for unprefixed (legacy / default-tenant) ids. Used
/// by the cron scheduler, which lists crons across all tenants and must
/// fire each one inside its own tenant namespace.
pub(crate) fn tenant_of_internal(internal_id: &str) -> &str {
    match internal_id.split_once('/') {
        Some((tenant, _)) => tenant,
        None => DEFAULT_TENANT,
    }
}

/// Resolve the request's tenant and scopes: with keys configured, the
/// `X-Api-Key` header must match a configured key (401 otherwise) and
/// selects that key's tenant and scope set; with no keys configured the
/// server is in open (dev) mode and every request runs as the default
/// tenant with super-user scopes. The resolved [`TenantContext`] is
/// inserted into the request extensions.
pub(crate) async fn require_api_key(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let (tenant, scopes) = if state.config.auth_enabled() {
        let provided = request
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok());
        match provided {
            Some(key) => match state.config.tenant_for_key(key) {
                Some(tenant) => {
                    let scopes = state.config.scopes_for_key(key);
                    (tenant.to_string(), scopes)
                }
                None => {
                    return AdmissionReason::Unauthorized.into_response(StatusCode::UNAUTHORIZED);
                }
            },
            None => {
                return AdmissionReason::Unauthorized.into_response(StatusCode::UNAUTHORIZED);
            }
        }
    } else {
        (
            DEFAULT_TENANT.to_string(),
            vec![
                Scope::parse("*:*").expect("super-user collection scope is valid"),
                Scope::parse("*:*:*").expect("super-user instance scope is valid"),
            ],
        )
    };
    request
        .extensions_mut()
        .insert(TenantContext::new(tenant, scopes));
    next.run(request).await
}

/// Scope-enforcement middleware: runs after [`require_api_key`] and before
/// handler logic. Looks up the required scope for the route from the
/// [`ScopeTable`] and verifies the caller's granted scopes authorize it.
///
/// If the route has no declared scope, the request is allowed (backward
/// compatibility during transition). If the caller lacks the required scope,
/// returns [`AdmissionReason::Unauthorized`] with **403 Forbidden** —
/// enumeration-safe because the check happens before any handler logic can
/// probe resource existence.
pub(crate) async fn require_scope(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let tenant = request.extensions().get::<TenantContext>();
    let method = request.method().as_str();
    let path = request.uri().path();

    if let Some(required) = state.scope_table.required_scope(method, path) {
        let authorized = tenant.is_some_and(|t| scope_authorizes(t.scopes(), required));
        if !authorized {
            return AdmissionReason::Unauthorized.into_response(StatusCode::FORBIDDEN);
        }
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tenant_is_unprefixed() {
        assert_eq!(scope_id(DEFAULT_TENANT, "t1"), "t1");
        assert_eq!(scope_id("acme", "t1"), "acme/t1");
    }

    #[test]
    fn strip_owned_respects_tenant_boundaries() {
        assert_eq!(strip_owned("acme", "acme/t1"), Some("t1"));
        assert_eq!(strip_owned("acme", "globex/t1"), None);
        assert_eq!(strip_owned("acme", "t1"), None);
        // The default tenant owns exactly the unprefixed ids.
        assert_eq!(strip_owned(DEFAULT_TENANT, "t1"), Some("t1"));
        assert_eq!(strip_owned(DEFAULT_TENANT, "acme/t1"), None);
        // A prefix alone is not ownership ("acm" must not match "acme/…").
        assert_eq!(strip_owned("acm", "acme/t1"), None);
    }

    #[test]
    fn tenant_of_internal_reads_the_prefix() {
        assert_eq!(tenant_of_internal("acme/t1"), "acme");
        assert_eq!(tenant_of_internal("t1"), DEFAULT_TENANT);
    }
}
