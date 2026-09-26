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

/// What a caller is. A key is a credential; a principal is *who* presented
/// it — a person or a service — and what they are for. Every run, every
/// version and every receipt is attributed to one, so "who did this" is
/// never answered with "whoever had the key".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Principal {
    /// Stable id, unique within the deployment (`amjad`, `svc-nightly-kb`).
    pub id: String,
    /// What to call them.
    pub name: String,
    pub kind: PrincipalKind,
    /// The roles they hold. Scopes are derived from these, never granted
    /// directly — a role is a thing an operator can reason about; a scope
    /// string is not.
    pub roles: Vec<Role>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalKind {
    /// A person at a keyboard.
    User,
    /// A program: a scheduler, a pipeline, another agent runtime.
    Service,
}

/// The four ways of being in this product. The studio has shown these names
/// since its first screen; they are now what the server enforces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Everything, including who else may do anything.
    Admin,
    /// Makes agents: assistants, skills, connectors and connections, and runs
    /// them to see. Cannot promote what runs for others.
    Builder,
    /// Runs what exists, approves what waits, decides what is live. Cannot
    /// change what an agent is.
    Operator,
    /// Reads everything, changes nothing. Can verify a receipt.
    Auditor,
}

impl Role {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "admin" => Some(Self::Admin),
            "builder" => Some(Self::Builder),
            "operator" => Some(Self::Operator),
            "auditor" => Some(Self::Auditor),
            _ => None,
        }
    }

    /// The scopes a role grants, in the route table's own grammar. Wildcards
    /// match per segment, so `assistants:*` is every assistant action and
    /// `*:read` is reading anything.
    pub fn scopes(self) -> Vec<Scope> {
        let names: &[&str] = match self {
            Self::Admin => &["*:*", "*:*:*"],
            Self::Builder => &[
                "assistants:read", "assistants:write",
                "skills:*", "connectors:*", "connections:read", "connections:write", "connections:consent",
                "threads:*", "runs:create", "runs:read", "runs:cancel", "runs:replay",
                "crons:*", "triggers:read", "triggers:write", "triggers:delete", "approvals:*", "plugins:*",
                // The board: a builder queues work for the agent they are
                // building and reads what became of it; claiming and
                // settling stay the workers' and the operator's.
                "tasks:read", "tasks:write",
                "memory:*", "knowledge:read", "datasets:*", "gates:read", "mcp:read",
                "receipts:verify", "health:read", "system:read", "registry:read",
            ],
            Self::Operator => &[
                "assistants:read", "assistants:activate",
                "threads:*", "runs:*", "tasks:*", "crons:*", "triggers:read", "triggers:replay", "approvals:*", "plugins:*",
                "connections:read", "connectors:read", "connectors:check",
                // Worlds, datasets and their evaluations, read: an operator
                // schedules an agent in a stand-in and reads how a suite
                // went; making either stays the builder's.
                "datasets:read",
                "gates:*", "deployments:read", "deployments:promote",
                "receipts:verify", "health:read", "system:read", "registry:read",
            ],
            Self::Auditor => &["*:read", "receipts:verify", "health:read", "system:read"],
        };
        names
            .iter()
            .map(|name| Scope::parse(name).expect("role scope tables are valid"))
            .collect()
    }
}

/// The scopes a set of roles grants together.
pub fn scopes_for_roles(roles: &[Role]) -> Vec<Scope> {
    let mut all: Vec<Scope> = roles.iter().flat_map(|role| role.scopes()).collect();
    all.dedup();
    all
}

/// Nobody: the identity a public route runs under when no one is signed in.
/// It holds no role and no scope, so it can sign in, sign out, or take an
/// OAuth redirect — and nothing else.
pub fn anonymous_principal() -> Principal {
    Principal {
        id: "anonymous".to_owned(),
        name: "Not signed in".to_owned(),
        kind: PrincipalKind::Service,
        roles: Vec::new(),
    }
}

/// The principal an open (dev-mode) server acts as. Named, so attribution is
/// never empty even where authentication is off.
pub fn dev_principal() -> Principal {
    Principal {
        id: "dev".to_owned(),
        name: "Developer (open mode)".to_owned(),
        kind: PrincipalKind::User,
        roles: vec![Role::Admin],
    }
}

/// The resolved tenant for one request, inserted into request extensions by
/// [`require_api_key`] and extracted by every tenant-scoped handler.
#[derive(Debug, Clone)]
pub(crate) struct TenantContext {
    tenant: String,
    scopes: Vec<Scope>,
    principal: Principal,
}

/// The deployment's own hand: the principal a context made *inside* the
/// server carries — a schedule firing, an evaluation running its cases, a
/// connector's own job. A service, nobody in particular: what it starts is
/// tenant-wide, as every key was before people existed, and stamps no
/// person's name on a thread or a run.
pub fn server_principal() -> Principal {
    Principal {
        id: "server".to_owned(),
        name: "the server".to_owned(),
        kind: PrincipalKind::Service,
        roles: vec![Role::Admin],
    }
}

impl TenantContext {
    /// A context the server makes for itself (no request behind it): the
    /// deployment's own hand, see [`server_principal`]. A request's context
    /// always sets its principal explicitly ([`require_api_key`]).
    pub(crate) fn new(tenant: String, scopes: Vec<Scope>) -> Self {
        Self { tenant, scopes, principal: server_principal() }
    }

    pub(crate) fn with_principal(mut self, principal: Principal) -> Self {
        self.principal = principal;
        self
    }

    /// Who is calling.
    pub(crate) fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The attribution stamped on what this caller creates.
    pub(crate) fn attribution(&self) -> serde_json::Value {
        serde_json::json!({
            "principal_id": self.principal.id,
            "name": self.principal.name,
            "kind": self.principal.kind,
        })
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

    /// Whether the caller is bound to their own person: a signed-in user
    /// without the Admin role. What belongs to a person — `user`-scoped
    /// memory, a connection with a subject — is theirs alone; an
    /// administrator acts for anyone, and a service key (an integration,
    /// the deployment's own hand, nobody in particular) stays tenant-wide,
    /// as every key was before people existed.
    pub(crate) fn person_bound(&self) -> bool {
        self.principal.kind == PrincipalKind::User && !self.principal.roles.contains(&Role::Admin)
    }

    /// Whether the caller may act on what belongs to `subject`: always,
    /// unless the caller is person-bound and `subject` is someone else.
    pub(crate) fn may_act_for(&self, subject: &str) -> bool {
        !self.person_bound() || subject == self.principal.id
    }

    /// Whether the caller may *read* what was done for `subject`: whoever
    /// may act for them, and an auditor — who reads everyone's and changes
    /// nothing. A person's runs and conversations are theirs; a builder
    /// or an operator sees their own and what acted for nobody.
    pub(crate) fn may_read_for(&self, subject: &str) -> bool {
        self.may_act_for(subject) || self.principal.roles.contains(&Role::Auditor)
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
    // A browser's session cookie is one way of being somebody; a key is the
    // other. Either resolves to a principal; a server that requires one
    // refuses a request that has neither.
    let session = crate::users::session_cookie(request.headers());
    // A session is only as good as the user is now: gone, or revoked (a
    // newer security epoch), and the session ends here; roles are the
    // user's current ones, never the ones the session was issued with.
    let mut renewed: Option<chrono::DateTime<chrono::Utc>> = None;
    let signed_in = match &session {
        Some(token) => match state.sessions.resolve(token).await {
            Some((principal, epoch)) => match state.users.get(&principal.id) {
                Some(user) if user.security_epoch == epoch && user.active => {
                    // A session in use stays in use: renewed once it is an
                    // hour old, and the response's cookie says the new end.
                    renewed = state.sessions.renew_if_stale(token).await;
                    Some(user.principal())
                }
                Some(_) | None if !state.users.is_empty() => {
                    state.sessions.close(token).await;
                    None
                }
                _ => Some(principal),
            },
            None => None,
        },
        None => None,
    };
    let (tenant, scopes, principal) = if let Some(principal) = signed_in {
        (
            DEFAULT_TENANT.to_string(),
            scopes_for_roles(&principal.roles),
            principal,
        )
    } else if state.auth_required {
        let provided = request
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok());
        match provided {
            // A key bound to a principal: the tenant is the principal's, and
            // the scopes are what their roles grant — nothing else.
            Some(key) if state.config.principal_for_key(key).is_some() => {
                let bound = state.config.principal_for_key(key).expect("checked");
                (
                    bound.tenant.clone(),
                    scopes_for_roles(&bound.principal.roles),
                    bound.principal.clone(),
                )
            }
            // A legacy tenant key: a tenant and a scope set, but nobody in
            // particular. Attributed to the key itself so it is never blank.
            Some(key) => match state.config.tenant_for_key(key) {
                Some(tenant) => {
                    let scopes = state.config.scopes_for_key(key);
                    let principal = Principal {
                        id: format!("key:{}", &key[..key.len().min(8)]),
                        name: "API key (no principal)".to_owned(),
                        kind: PrincipalKind::Service,
                        roles: Vec::new(),
                    };
                    (tenant.to_string(), scopes, principal)
                }
                None => {
                    return AdmissionReason::Unauthorized.into_response(StatusCode::UNAUTHORIZED);
                }
            },
            // Nobody at the door. A public route — signing in, signing out,
            // an OAuth redirect — still opens, as nobody; everything else
            // is refused.
            None if state
                .scope_table
                .is_public(request.method().as_str(), request.uri().path()) =>
            {
                (DEFAULT_TENANT.to_string(), Vec::new(), anonymous_principal())
            }
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
            dev_principal(),
        )
    };
    // A forgotten person is nobody here, whatever key or session names them.
    if state.users.is_forgotten(&principal.id) {
        return AdmissionReason::Unauthorized.into_response(StatusCode::UNAUTHORIZED);
    }
    request
        .extensions_mut()
        .insert(TenantContext::new(tenant, scopes).with_principal(principal));
    let mut response = next.run(request).await;
    // A renewed session tells the browser its new end; a header that does
    // not parse (never, for what session_cookie_header writes) is dropped.
    if let (Some(token), Some(until)) = (&session, renewed) {
        if let Ok(value) = axum::http::HeaderValue::from_str(&crate::routes::session_cookie_header(token, Some(until))) {
            response.headers_mut().append(axum::http::header::SET_COOKIE, value);
        }
    }
    response
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

    // Routes declared public are served without a scope check — a decision
    // recorded in the table, not an omission in it.
    if state.scope_table.is_public(method, path) {
        return next.run(request).await;
    }

    // Fail closed. A route the scope table does not declare has not been
    // reviewed for who may call it, so it is refused rather than allowed
    // through — the census test keeps the table complete (G12).
    let Some(required) = state.scope_table.required_scope(method, path) else {
        return AdmissionReason::Unauthorized.into_response(StatusCode::FORBIDDEN);
    };
    if !tenant.is_some_and(|t| scope_authorizes(t.scopes(), required)) {
        return AdmissionReason::Unauthorized.into_response(StatusCode::FORBIDDEN);
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
