//! SCIM 2.0 — the identity provider provisions and deprovisions people.
//!
//! An organisation's directory (Okta, Entra, anything that speaks SCIM)
//! pushes its people here: created when they join, changed when they
//! move, deactivated or deleted when they leave. The value is the last
//! part: a person the directory deactivates is signed out everywhere and
//! refused from then on — before anyone here hears they left. Groups map
//! to roles by name, so the directory decides who is an administrator
//! and who is a builder.
//!
//! The provider authenticates with a bearer token an administrator mints
//! here (shown once, kept hashed). Users and Groups are the resources; a
//! user's SCIM id is the account's sign-in name; `externalId` is the
//! directory's own id; `userName` is the email or principal name, which
//! is also what an OIDC sign-in (Story 53) matches an existing account by.
//! Deactivation and deletion never forget a person (Story 51): erasure is
//! a decision an administrator makes, not a side effect of an IdP call.
use std::sync::Arc;

use axum::extract::{Path, Query, State as AxumState};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::auth::{Role, TenantContext};
use crate::error::ApiError;
use crate::routes::AppState;
use crate::users::UserRecord;

const NAMESPACE: &str = "scim";
const CONFIG_KEY: &str = "config";
const GROUPS_KEY: &str = "groups";
const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
const PATCH_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
const RUSTY_USER_EXT: &str = "urn:ietf:params:scim:schemas:extension:rusty:2.0:User";

/// What the store keeps: the token's hash, the group-to-role map, and
/// what the provider has done.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScimConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_minted_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_minted_by: Option<Value>,
    /// Directory group name → role here. A person in several mapped
    /// groups holds every mapped role.
    #[serde(default)]
    pub group_roles: Vec<GroupRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub provisioned: u64,
    #[serde(default)]
    pub deactivated: u64,
    #[serde(default)]
    pub deleted: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRole {
    pub group: String,
    pub role: Role,
}

/// A directory group, as the provider keeps it here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// Member account ids.
    #[serde(default)]
    pub members: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn namespace(tenant: &str) -> String {
    format!("{NAMESPACE}:{tenant}")
}

pub(crate) async fn load_config(state: &AppState, tenant: &str) -> ScimConfig {
    state
        .server_store
        .kv_get(&namespace(tenant), CONFIG_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|i| serde_json::from_value(i.value).ok())
        .unwrap_or_default()
}

async fn keep_config(state: &AppState, tenant: &str, config: &ScimConfig) -> Result<(), String> {
    let value = serde_json::to_value(config).map_err(|e| e.to_string())?;
    state.server_store.kv_put(&namespace(tenant), CONFIG_KEY, value).await.map(|_| ()).map_err(|e| e.to_string())
}

async fn load_groups(state: &AppState, tenant: &str) -> Vec<Group> {
    state
        .server_store
        .kv_get(&namespace(tenant), GROUPS_KEY)
        .await
        .ok()
        .flatten()
        .and_then(|i| serde_json::from_value(i.value).ok())
        .unwrap_or_default()
}

async fn keep_groups(state: &AppState, tenant: &str, groups: &[Group]) -> Result<(), String> {
    let value = serde_json::to_value(groups).map_err(|e| e.to_string())?;
    state.server_store.kv_put(&namespace(tenant), GROUPS_KEY, value).await.map(|_| ()).map_err(|e| e.to_string())
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn base_url(state: &AppState) -> String {
    format!("{}/scim/v2", state.config.public_url.trim_end_matches('/'))
}

// ── The administrator's side ────────────────────────────────────────────────

fn served_config(state: &AppState, c: &ScimConfig) -> Value {
    json!({
        "base_url": base_url(state),
        "has_token": c.token_sha256.is_some(),
        "token_minted_at": c.token_minted_at,
        "token_minted_by": c.token_minted_by,
        "group_roles": c.group_roles,
        "last_seen_at": c.last_seen_at,
        "provisioned": c.provisioned,
        "deactivated": c.deactivated,
        "deleted": c.deleted,
    })
}

/// `GET /auth/scim` — the provisioning setup as an administrator sees it.
pub(crate) async fn get_config(AxumState(state): AxumState<Arc<AppState>>, Extension(tenant): Extension<TenantContext>) -> Result<Json<Value>, ApiError> {
    let config = load_config(&state, tenant.tenant()).await;
    let groups = load_groups(&state, tenant.tenant()).await;
    let mut out = served_config(&state, &config);
    out["groups"] = json!(groups.iter().map(|g| json!({"id": g.id, "display_name": g.display_name, "members": g.members.len()})).collect::<Vec<_>>());
    Ok(Json(out))
}

/// `POST /auth/scim/token` — mint the provisioning token: shown once, kept
/// hashed; a new one replaces the old.
pub(crate) async fn mint_token(AxumState(state): AxumState<Arc<AppState>>, Extension(tenant): Extension<TenantContext>) -> Result<Json<Value>, ApiError> {
    use chacha20poly1305::aead::rand_core::RngCore;
    let mut bytes = [0u8; 32];
    chacha20poly1305::aead::OsRng.fill_bytes(&mut bytes);
    let token = format!("scim_{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    let mut config = load_config(&state, tenant.tenant()).await;
    config.token_sha256 = Some(sha256_hex(&token));
    config.token_minted_at = Some(Utc::now());
    config.token_minted_by = Some(tenant.attribution());
    keep_config(&state, tenant.tenant(), &config).await.map_err(ApiError::internal)?;
    tracing::info!(by = %tenant.principal().id, "scim token minted");
    Ok(Json(json!({"token": token, "base_url": base_url(&state), "config": served_config(&state, &config)})))
}

/// `DELETE /auth/scim/token` — the provider is shut out; accounts stay.
pub(crate) async fn revoke_token(AxumState(state): AxumState<Arc<AppState>>, Extension(tenant): Extension<TenantContext>) -> Result<Json<Value>, ApiError> {
    let mut config = load_config(&state, tenant.tenant()).await;
    let had = config.token_sha256.take().is_some();
    config.token_minted_at = None;
    config.token_minted_by = None;
    keep_config(&state, tenant.tenant(), &config).await.map_err(ApiError::internal)?;
    Ok(Json(json!({"revoked": had, "config": served_config(&state, &config)})))
}

#[derive(Debug, Deserialize)]
pub struct GroupRolesInput {
    pub group_roles: Vec<GroupRole>,
}

/// `PUT /auth/scim/group-roles` — which directory groups mean which roles.
/// Every member of a mapped group gets its roles now.
pub(crate) async fn put_group_roles(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(input): Json<GroupRolesInput>,
) -> Result<Json<Value>, ApiError> {
    let mut config = load_config(&state, tenant.tenant()).await;
    let mut seen = std::collections::HashSet::new();
    let mut map = Vec::new();
    for entry in input.group_roles {
        let group = entry.group.trim().to_owned();
        if group.is_empty() {
            continue;
        }
        if seen.insert((group.to_ascii_lowercase(), entry.role)) {
            map.push(GroupRole { group, role: entry.role });
        }
    }
    config.group_roles = map;
    keep_config(&state, tenant.tenant(), &config).await.map_err(ApiError::internal)?;
    let groups = load_groups(&state, tenant.tenant()).await;
    let touched: std::collections::BTreeSet<String> = groups.iter().flat_map(|g| g.members.iter().cloned()).collect();
    for member in touched {
        apply_group_roles(&state, &config, &groups, &member).await;
    }
    Ok(Json(served_config(&state, &config)))
}

/// The roles a person holds from the groups they are in: the mapped
/// roles, when any group of theirs is mapped; otherwise what they have.
async fn apply_group_roles(state: &AppState, config: &ScimConfig, groups: &[Group], member: &str) {
    let mut roles: Vec<Role> = Vec::new();
    for group in groups.iter().filter(|g| g.members.iter().any(|m| m == member)) {
        for mapping in config.group_roles.iter().filter(|m| m.group.eq_ignore_ascii_case(&group.display_name)) {
            if !roles.contains(&mapping.role) {
                roles.push(mapping.role);
            }
        }
    }
    if roles.is_empty() {
        return;
    }
    if let Err(error) = state.users.set_roles(member, roles).await {
        tracing::warn!(%member, %error, "scim: roles not applied");
    }
}

// ── The provider's side ─────────────────────────────────────────────────────

fn scim_error(status: StatusCode, detail: impl Into<String>, scim_type: Option<&str>) -> Response {
    let mut body = json!({"schemas": [ERROR_SCHEMA], "status": status.as_u16().to_string(), "detail": detail.into()});
    if let Some(t) = scim_type {
        body["scimType"] = json!(t);
    }
    (status, [(header::CONTENT_TYPE, "application/scim+json")], body.to_string()).into_response()
}

fn scim_json(status: StatusCode, body: Value) -> Response {
    (status, [(header::CONTENT_TYPE, "application/scim+json")], body.to_string()).into_response()
}

/// The provider's bearer token, checked against the hash; the config it
/// unlocks, with `last_seen_at` moved on.
async fn admitted(state: &AppState, headers: &HeaderMap) -> Result<ScimConfig, Response> {
    let tenant = crate::auth::DEFAULT_TENANT;
    let mut config = load_config(state, tenant).await;
    let Some(expected) = config.token_sha256.clone() else {
        return Err(scim_error(StatusCode::UNAUTHORIZED, "no provisioning token is minted here", None));
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(str::trim)
        .unwrap_or("");
    if presented.is_empty() || sha256_hex(presented) != expected {
        return Err(scim_error(StatusCode::UNAUTHORIZED, "the provisioning token was not accepted", None));
    }
    config.last_seen_at = Some(Utc::now());
    let _ = keep_config(state, tenant, &config).await;
    Ok(config)
}

pub(crate) async fn service_provider_config(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(refused) = admitted(&state, &headers).await {
        return refused;
    }
    scim_json(StatusCode::OK, json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig"],
        "patch": {"supported": true},
        "bulk": {"supported": false, "maxOperations": 0, "maxPayloadSize": 0},
        "filter": {"supported": true, "maxResults": 200},
        "changePassword": {"supported": false},
        "sort": {"supported": false},
        "etag": {"supported": false},
        "authenticationSchemes": [{"type": "oauthbearertoken", "name": "Bearer token", "description": "A provisioning token minted in Config → Sign-in."}],
        "meta": {"resourceType": "ServiceProviderConfig", "location": format!("{}/ServiceProviderConfig", base_url(&state))},
    }))
}

pub(crate) async fn resource_types(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(refused) = admitted(&state, &headers).await {
        return refused;
    }
    let base = base_url(&state);
    scim_json(StatusCode::OK, json!({
        "schemas": [LIST_SCHEMA], "totalResults": 2, "startIndex": 1, "itemsPerPage": 2,
        "Resources": [
            {"schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"], "id": "User", "name": "User", "endpoint": "/Users", "schema": USER_SCHEMA, "schemaExtensions": [{"schema": RUSTY_USER_EXT, "required": false}], "meta": {"resourceType": "ResourceType", "location": format!("{base}/ResourceTypes/User")}},
            {"schemas": ["urn:ietf:params:scim:schemas:core:2.0:ResourceType"], "id": "Group", "name": "Group", "endpoint": "/Groups", "schema": GROUP_SCHEMA, "meta": {"resourceType": "ResourceType", "location": format!("{base}/ResourceTypes/Group")}},
        ],
    }))
}

pub(crate) async fn schemas(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(refused) = admitted(&state, &headers).await {
        return refused;
    }
    scim_json(StatusCode::OK, json!({
        "schemas": [LIST_SCHEMA], "totalResults": 3, "startIndex": 1, "itemsPerPage": 3,
        "Resources": [
            {"id": USER_SCHEMA, "name": "User", "attributes": [
                {"name": "userName", "type": "string", "multiValued": false, "required": true, "uniqueness": "server"},
                {"name": "name", "type": "complex", "multiValued": false, "subAttributes": [{"name": "formatted", "type": "string"}, {"name": "givenName", "type": "string"}, {"name": "familyName", "type": "string"}]},
                {"name": "displayName", "type": "string", "multiValued": false},
                {"name": "active", "type": "boolean", "multiValued": false},
                {"name": "emails", "type": "complex", "multiValued": true, "subAttributes": [{"name": "value", "type": "string"}, {"name": "primary", "type": "boolean"}]},
                {"name": "externalId", "type": "string", "multiValued": false},
            ]},
            {"id": GROUP_SCHEMA, "name": "Group", "attributes": [
                {"name": "displayName", "type": "string", "required": true},
                {"name": "members", "type": "complex", "multiValued": true, "subAttributes": [{"name": "value", "type": "string"}, {"name": "display", "type": "string"}]},
            ]},
            {"id": RUSTY_USER_EXT, "name": "Rusty user", "attributes": [{"name": "roles", "type": "string", "multiValued": true, "mutability": "readOnly", "description": "The roles the person holds here: from the mapped groups, or as set in Security → People."}]},
        ],
    }))
}

fn user_resource(state: &AppState, u: &UserRecord) -> Value {
    let (given, family) = match u.name.split_once(' ') {
        Some((g, f)) => (g.to_owned(), f.to_owned()),
        None => (u.name.clone(), String::new()),
    };
    json!({
        "schemas": [USER_SCHEMA, RUSTY_USER_EXT],
        "id": u.id,
        "externalId": u.external_id,
        "userName": u.id,
        "name": {"formatted": u.name, "givenName": given, "familyName": family},
        "displayName": u.name,
        "active": u.active,
        "emails": if u.id.contains('@') { json!([{"value": u.id, "primary": true}]) } else { json!([]) },
        RUSTY_USER_EXT: {"roles": u.roles},
        "meta": {"resourceType": "User", "created": u.created_at, "lastModified": u.updated_at.unwrap_or(u.created_at), "location": format!("{}/Users/{}", base_url(state), u.id)},
    })
}

fn group_resource(state: &AppState, g: &Group, users: &[UserRecord]) -> Value {
    json!({
        "schemas": [GROUP_SCHEMA],
        "id": g.id,
        "externalId": g.external_id,
        "displayName": g.display_name,
        "members": g.members.iter().map(|m| json!({"value": m, "display": users.iter().find(|u| &u.id == m).map(|u| u.name.clone()).unwrap_or_default(), "$ref": format!("{}/Users/{m}", base_url(state))})).collect::<Vec<_>>(),
        "meta": {"resourceType": "Group", "created": g.created_at, "lastModified": g.updated_at, "location": format!("{}/Groups/{}", base_url(state), g.id)},
    })
}

/// The one filter shape a directory uses to look people up before it
/// creates them: `attr eq "value"`.
fn parse_eq_filter(filter: &str) -> Option<(String, String)> {
    let mut parts = filter.trim().splitn(3, ' ');
    let attr = parts.next()?.trim();
    let op = parts.next()?.trim();
    let value = parts.next()?.trim();
    if !op.eq_ignore_ascii_case("eq") {
        return None;
    }
    let value = value.trim_matches('"').to_owned();
    Some((attr.to_ascii_lowercase(), value))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub filter: Option<String>,
    #[serde(default, rename = "startIndex")]
    pub start_index: Option<usize>,
    #[serde(default)]
    pub count: Option<usize>,
}

fn list_response(resources: Vec<Value>, query: &ListQuery) -> Value {
    let total = resources.len();
    let start = query.start_index.unwrap_or(1).max(1);
    let count = query.count.unwrap_or(100).min(200);
    let page: Vec<Value> = resources.into_iter().skip(start - 1).take(count).collect();
    json!({"schemas": [LIST_SCHEMA], "totalResults": total, "startIndex": start, "itemsPerPage": page.len(), "Resources": page})
}

pub(crate) async fn list_users(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap, Query(query): Query<ListQuery>) -> Response {
    if let Err(refused) = admitted(&state, &headers).await {
        return refused;
    }
    let users = state.users.list().await;
    let filter = query.filter.as_deref().map(parse_eq_filter);
    let matching: Vec<Value> = users
        .iter()
        .filter(|u| match &filter {
            Some(Some((attr, value))) => match attr.as_str() {
                "username" => u.id.eq_ignore_ascii_case(value),
                "externalid" => u.external_id.as_deref() == Some(value.as_str()),
                "id" => u.id == *value,
                _ => false,
            },
            Some(None) => false,
            None => true,
        })
        .map(|u| user_resource(&state, u))
        .collect();
    if matches!(filter, Some(None)) {
        return scim_error(StatusCode::BAD_REQUEST, "only `attribute eq \"value\"` filters are read here", Some("invalidFilter"));
    }
    scim_json(StatusCode::OK, list_response(matching, &query))
}

pub(crate) async fn get_user(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(refused) = admitted(&state, &headers).await {
        return refused;
    }
    match state.users.get(&id) {
        Some(u) => scim_json(StatusCode::OK, user_resource(&state, &u)),
        None => scim_error(StatusCode::NOT_FOUND, format!("no user {id}"), None),
    }
}

fn text_at<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    v.pointer(path).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn name_of(body: &Value) -> String {
    text_at(body, "/name/formatted")
        .map(str::to_owned)
        .or_else(|| match (text_at(body, "/name/givenName"), text_at(body, "/name/familyName")) {
            (Some(g), Some(f)) => Some(format!("{g} {f}")),
            (Some(g), None) => Some(g.to_owned()),
            (None, Some(f)) => Some(f.to_owned()),
            (None, None) => None,
        })
        .or_else(|| text_at(body, "/displayName").map(str::to_owned))
        .unwrap_or_default()
}

/// `POST /Users` — the directory creates a person: the account is made
/// (or, when an account with that sign-in name exists, linked), in the
/// default role of the OIDC provider or builder, active as the directory
/// says.
pub(crate) async fn create_user(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let mut config = match admitted(&state, &headers).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    let tenant = crate::auth::DEFAULT_TENANT;
    let Some(user_name) = text_at(&body, "/userName").map(|s| s.to_ascii_lowercase()) else {
        return scim_error(StatusCode::BAD_REQUEST, "userName is required", Some("invalidValue"));
    };
    let name = name_of(&body);
    let active = body.get("active").and_then(Value::as_bool).unwrap_or(true);
    let external_id = text_at(&body, "/externalId").map(str::to_owned);
    if let Some(existing) = state.users.get(&user_name) {
        // Already here (a password account, or one an OIDC sign-in made):
        // the directory takes it over — linked, not duplicated.
        if let Err(error) = state.users.provisioned(&existing.id, external_id.as_deref(), &name, active).await {
            return scim_error(StatusCode::INTERNAL_SERVER_ERROR, error, None);
        }
        let user = state.users.get(&existing.id).unwrap_or(existing);
        return scim_json(StatusCode::CREATED, user_resource(&state, &user));
    }
    let default_role = crate::oidc::load(&state, tenant).await.ok().flatten().map(|p| p.default_role).unwrap_or(Role::Builder);
    // No provider identity yet: the person's first OIDC sign-in links the
    // subject to this account by its sign-in name.
    let user = match state.users.provision_external(&user_name, &name, vec![default_role], None).await {
        Ok(u) => u,
        Err(error) => return scim_error(StatusCode::BAD_REQUEST, error, Some("invalidValue")),
    };
    if let Err(error) = state.users.provisioned(&user.id, external_id.as_deref(), &name, active).await {
        return scim_error(StatusCode::INTERNAL_SERVER_ERROR, error, None);
    }
    config.provisioned += 1;
    let _ = keep_config(&state, tenant, &config).await;
    let user = state.users.get(&user.id).unwrap_or(user);
    tracing::info!(id = %user.id, active, "scim: user provisioned");
    scim_json(StatusCode::CREATED, user_resource(&state, &user))
}

/// `PUT /Users/{id}` — the whole resource as the directory has it.
pub(crate) async fn replace_user(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let mut config = match admitted(&state, &headers).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    let Some(user) = state.users.get(&id) else {
        return scim_error(StatusCode::NOT_FOUND, format!("no user {id}"), None);
    };
    let name = { let n = name_of(&body); if n.is_empty() { user.name.clone() } else { n } };
    let active = body.get("active").and_then(Value::as_bool).unwrap_or(user.active);
    let external_id = text_at(&body, "/externalId").map(str::to_owned).or(user.external_id.clone());
    if let Err(error) = apply_active(&state, &mut config, &user, active).await {
        return scim_error(StatusCode::INTERNAL_SERVER_ERROR, error, None);
    }
    if let Err(error) = state.users.provisioned(&user.id, external_id.as_deref(), &name, active).await {
        return scim_error(StatusCode::INTERNAL_SERVER_ERROR, error, None);
    }
    let user = state.users.get(&id).unwrap_or(user);
    scim_json(StatusCode::OK, user_resource(&state, &user))
}

/// Deactivation is the point: sessions end everywhere now, and nothing
/// starts in their name; reactivation lets them back in.
async fn apply_active(state: &AppState, config: &mut ScimConfig, user: &UserRecord, active: bool) -> Result<(), String> {
    if user.active && !active {
        let _ = state.users.revoke_sessions(&user.id).await;
        config.deactivated += 1;
        let _ = keep_config(state, crate::auth::DEFAULT_TENANT, config).await;
        tracing::warn!(id = %user.id, "scim: user deactivated — signed out everywhere");
    } else if !user.active && active {
        tracing::info!(id = %user.id, "scim: user reactivated");
    }
    Ok(())
}

/// `PATCH /Users/{id}` — RFC 7644 §3.5.2: `replace`/`add` on `active`,
/// `name`, `displayName`, `externalId`, `userName` (read-only here:
/// refused), with or without a `path`.
pub(crate) async fn patch_user(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let mut config = match admitted(&state, &headers).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    let Some(user) = state.users.get(&id) else {
        return scim_error(StatusCode::NOT_FOUND, format!("no user {id}"), None);
    };
    if !body.get("schemas").and_then(Value::as_array).is_some_and(|s| s.iter().any(|x| x.as_str() == Some(PATCH_SCHEMA))) {
        return scim_error(StatusCode::BAD_REQUEST, "a PatchOp names its schema", Some("invalidSyntax"));
    }
    let mut active = user.active;
    let mut name = user.name.clone();
    let mut external_id = user.external_id.clone();
    for op in body.get("Operations").and_then(Value::as_array).cloned().unwrap_or_default() {
        let kind = op.get("op").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        if kind != "replace" && kind != "add" {
            return scim_error(StatusCode::BAD_REQUEST, format!("op `{kind}` is not applied to a user here"), Some("invalidValue"));
        }
        let path = op.get("path").and_then(Value::as_str).map(|p| p.to_ascii_lowercase());
        let value = op.get("value").cloned().unwrap_or(Value::Null);
        // Either a path with a scalar value, or no path with an object of
        // attributes (the shape Entra sends).
        let mut fields: Map<String, Value> = Map::new();
        match path {
            Some(p) => {
                fields.insert(p, value);
            }
            None => {
                if let Value::Object(map) = value {
                    for (k, v) in map {
                        fields.insert(k.to_ascii_lowercase(), v);
                    }
                }
            }
        }
        for (key, v) in fields {
            match key.as_str() {
                "active" => {
                    active = match &v {
                        Value::Bool(b) => *b,
                        Value::String(s) => s.eq_ignore_ascii_case("true"),
                        _ => active,
                    }
                }
                "name.formatted" | "displayname" => {
                    if let Some(s) = v.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                        name = s.to_owned();
                    }
                }
                "name" => {
                    let n = name_of(&json!({"name": v}));
                    if !n.is_empty() {
                        name = n;
                    }
                }
                "name.givenname" => {
                    if let Some(g) = v.as_str() {
                        let family = name.split_once(' ').map(|(_, f)| f.to_owned()).unwrap_or_default();
                        name = if family.is_empty() { g.to_owned() } else { format!("{g} {family}") };
                    }
                }
                "name.familyname" => {
                    if let Some(f) = v.as_str() {
                        let given = name.split_once(' ').map(|(g, _)| g.to_owned()).unwrap_or_else(|| name.clone());
                        name = format!("{given} {f}");
                    }
                }
                "externalid" => external_id = v.as_str().map(str::to_owned),
                "username" => {
                    if v.as_str().is_some_and(|s| !s.eq_ignore_ascii_case(&user.id)) {
                        return scim_error(StatusCode::BAD_REQUEST, "userName is the sign-in name here and does not change; delete and create the person", Some("mutability"));
                    }
                }
                "emails" | "emails[primary eq true].value" | "title" | "phonenumbers" | "addresses" | "locale" | "timezone" | "nickname" | "profileurl" | "preferredlanguage" | "usertype" | "roles" | "groups" => {}
                other => {
                    tracing::debug!(attribute = other, "scim: attribute not kept here");
                }
            }
        }
    }
    if let Err(error) = apply_active(&state, &mut config, &user, active).await {
        return scim_error(StatusCode::INTERNAL_SERVER_ERROR, error, None);
    }
    if let Err(error) = state.users.provisioned(&user.id, external_id.as_deref(), &name, active).await {
        return scim_error(StatusCode::INTERNAL_SERVER_ERROR, error, None);
    }
    let user = state.users.get(&id).unwrap_or(user);
    scim_json(StatusCode::OK, user_resource(&state, &user))
}

/// `DELETE /Users/{id}` — the account goes and its sessions end; what the
/// person did stays on the record (forgetting is an administrator's act).
pub(crate) async fn delete_user(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let mut config = match admitted(&state, &headers).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    let Some(user) = state.users.get(&id) else {
        return scim_error(StatusCode::NOT_FOUND, format!("no user {id}"), None);
    };
    let _ = state.users.revoke_sessions(&user.id).await;
    match state.users.delete(&user.id).await {
        Ok(true) => {}
        Ok(false) => return scim_error(StatusCode::NOT_FOUND, format!("no user {id}"), None),
        Err(error) => return scim_error(StatusCode::INTERNAL_SERVER_ERROR, error, None),
    }
    let tenant = crate::auth::DEFAULT_TENANT;
    let mut groups = load_groups(&state, tenant).await;
    for g in &mut groups {
        g.members.retain(|m| m != &user.id);
    }
    let _ = keep_groups(&state, tenant, &groups).await;
    config.deleted += 1;
    let _ = keep_config(&state, tenant, &config).await;
    tracing::warn!(id = %user.id, "scim: user deleted — signed out everywhere, account removed");
    StatusCode::NO_CONTENT.into_response()
}

// ── Groups ──────────────────────────────────────────────────────────────────

pub(crate) async fn list_groups(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap, Query(query): Query<ListQuery>) -> Response {
    if let Err(refused) = admitted(&state, &headers).await {
        return refused;
    }
    let groups = load_groups(&state, crate::auth::DEFAULT_TENANT).await;
    let users = state.users.list().await;
    let filter = query.filter.as_deref().map(parse_eq_filter);
    if matches!(filter, Some(None)) {
        return scim_error(StatusCode::BAD_REQUEST, "only `attribute eq \"value\"` filters are read here", Some("invalidFilter"));
    }
    let matching: Vec<Value> = groups
        .iter()
        .filter(|g| match &filter {
            Some(Some((attr, value))) => match attr.as_str() {
                "displayname" => g.display_name.eq_ignore_ascii_case(value),
                "externalid" => g.external_id.as_deref() == Some(value.as_str()),
                "id" => g.id == *value,
                _ => false,
            },
            _ => true,
        })
        .map(|g| group_resource(&state, g, &users))
        .collect();
    scim_json(StatusCode::OK, list_response(matching, &query))
}

fn members_of(value: &Value) -> Vec<String> {
    value.as_array().map(|list| list.iter().filter_map(|m| m.get("value").and_then(Value::as_str).map(|s| s.trim().to_ascii_lowercase())).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
}

pub(crate) async fn create_group(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let config = match admitted(&state, &headers).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    let tenant = crate::auth::DEFAULT_TENANT;
    let Some(display_name) = text_at(&body, "/displayName").map(str::to_owned) else {
        return scim_error(StatusCode::BAD_REQUEST, "displayName is required", Some("invalidValue"));
    };
    let mut groups = load_groups(&state, tenant).await;
    if let Some(existing) = groups.iter().find(|g| g.display_name.eq_ignore_ascii_case(&display_name)) {
        return scim_error(StatusCode::CONFLICT, format!("group {} exists as {}", display_name, existing.id), Some("uniqueness"));
    }
    let now = Utc::now();
    let group = Group {
        id: format!("g-{}", uuid::Uuid::new_v4().simple()),
        display_name,
        external_id: text_at(&body, "/externalId").map(str::to_owned),
        members: members_of(body.get("members").unwrap_or(&Value::Null)),
        created_at: now,
        updated_at: now,
    };
    groups.push(group.clone());
    if let Err(error) = keep_groups(&state, tenant, &groups).await {
        return scim_error(StatusCode::INTERNAL_SERVER_ERROR, error, None);
    }
    for member in &group.members {
        apply_group_roles(&state, &config, &groups, member).await;
    }
    let users = state.users.list().await;
    scim_json(StatusCode::CREATED, group_resource(&state, &group, &users))
}

pub(crate) async fn get_group(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(refused) = admitted(&state, &headers).await {
        return refused;
    }
    let groups = load_groups(&state, crate::auth::DEFAULT_TENANT).await;
    let users = state.users.list().await;
    match groups.iter().find(|g| g.id == id) {
        Some(g) => scim_json(StatusCode::OK, group_resource(&state, g, &users)),
        None => scim_error(StatusCode::NOT_FOUND, format!("no group {id}"), None),
    }
}

/// `PATCH /Groups/{id}` — members added, removed or replaced; the display
/// name replaced. Every member whose group changed gets their roles
/// recomputed from the map.
pub(crate) async fn patch_group(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let config = match admitted(&state, &headers).await {
        Ok(c) => c,
        Err(refused) => return refused,
    };
    let tenant = crate::auth::DEFAULT_TENANT;
    let mut groups = load_groups(&state, tenant).await;
    let Some(at) = groups.iter().position(|g| g.id == id) else {
        return scim_error(StatusCode::NOT_FOUND, format!("no group {id}"), None);
    };
    let before: Vec<String> = groups[at].members.clone();
    for op in body.get("Operations").and_then(Value::as_array).cloned().unwrap_or_default() {
        let kind = op.get("op").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        let path = op.get("path").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
        let value = op.get("value").cloned().unwrap_or(Value::Null);
        let group = &mut groups[at];
        match (kind.as_str(), path.as_str()) {
            ("add", "members") | ("add", "") => {
                for m in members_of(value.get("members").unwrap_or(&value)) {
                    if !group.members.contains(&m) {
                        group.members.push(m);
                    }
                }
            }
            ("replace", "members") => group.members = members_of(&value),
            ("remove", p) if p.starts_with("members") => {
                // `members[value eq "id"]`, or a value list.
                if let Some(start) = p.find("eq") {
                    let target = p[start + 2..].trim().trim_matches(|c| c == '"' || c == ']' || c == ' ').to_ascii_lowercase();
                    group.members.retain(|m| m != &target);
                } else {
                    for m in members_of(&value) {
                        group.members.retain(|x| x != &m);
                    }
                }
            }
            ("replace", "displayname") => {
                if let Some(n) = value.as_str() {
                    group.display_name = n.trim().to_owned();
                }
            }
            ("replace", "") => {
                if let Some(n) = text_at(&value, "/displayName") {
                    group.display_name = n.to_owned();
                }
                if let Some(members) = value.get("members") {
                    group.members = members_of(members);
                }
            }
            (k, p) => return scim_error(StatusCode::BAD_REQUEST, format!("op `{k}` on `{p}` is not applied to a group here"), Some("invalidValue")),
        }
        group.updated_at = Utc::now();
    }
    if let Err(error) = keep_groups(&state, tenant, &groups).await {
        return scim_error(StatusCode::INTERNAL_SERVER_ERROR, error, None);
    }
    let after = groups[at].members.clone();
    let touched: std::collections::BTreeSet<String> = before.iter().chain(after.iter()).cloned().collect();
    for member in touched {
        // A member removed from every mapped group keeps their last roles
        // unless another mapped group still names them: the map, not the
        // absence of one, decides.
        apply_group_roles(&state, &config, &groups, &member).await;
    }
    let users = state.users.list().await;
    scim_json(StatusCode::OK, group_resource(&state, &groups[at], &users))
}

pub(crate) async fn delete_group(AxumState(state): AxumState<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(refused) = admitted(&state, &headers).await {
        return refused;
    }
    let tenant = crate::auth::DEFAULT_TENANT;
    let mut groups = load_groups(&state, tenant).await;
    let before = groups.len();
    groups.retain(|g| g.id != id);
    if groups.len() == before {
        return scim_error(StatusCode::NOT_FOUND, format!("no group {id}"), None);
    }
    if let Err(error) = keep_groups(&state, tenant, &groups).await {
        return scim_error(StatusCode::INTERNAL_SERVER_ERROR, error, None);
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_eq_filter_is_read_and_others_are_not() {
        assert_eq!(parse_eq_filter(r#"userName eq "priya@example.com""#), Some(("username".to_owned(), "priya@example.com".to_owned())));
        assert_eq!(parse_eq_filter(r#"externalId eq "abc-1""#), Some(("externalid".to_owned(), "abc-1".to_owned())));
        assert_eq!(parse_eq_filter(r#"userName co "pri""#), None);
    }

    #[test]
    fn a_name_is_read_from_whatever_the_directory_sends() {
        assert_eq!(name_of(&json!({"name": {"formatted": "Priya Natarajan"}})), "Priya Natarajan");
        assert_eq!(name_of(&json!({"name": {"givenName": "Priya", "familyName": "Natarajan"}})), "Priya Natarajan");
        assert_eq!(name_of(&json!({"displayName": "Priya N."})), "Priya N.");
        assert_eq!(name_of(&json!({})), "");
    }
}
