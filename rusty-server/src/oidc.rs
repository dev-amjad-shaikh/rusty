//! OIDC sign-in — a person signs in through the identity provider their
//! organisation already runs.
//!
//! The server is the relying party in the authorization-code flow with
//! PKCE: an administrator names the provider once (its issuer, a client
//! id and a sealed client secret, the role a newcomer gets), the server
//! reads the provider's discovery document, and *Sign in with …* on the
//! sign-in page sends the browser to the provider. The callback exchanges
//! the code at the token endpoint — a direct, server-to-server call —
//! reads the ID token's claims, checks issuer, audience, expiry and the
//! nonce it minted, and opens the same session a password sign-in opens.
//! The first time a person arrives, an account is made for them in the
//! default role, keyed by the provider's stable subject; from then on the
//! provider's subject is how they are known here, whatever their name.
//!
//! The ID token comes straight from the token endpoint over TLS, which is
//! the case the specification lets a client rely on in place of checking
//! the token's signature (OpenID Connect Core §3.1.3.7.6); this server
//! does not verify JWS signatures, so it never accepts an ID token from
//! anywhere but that endpoint. The provider's host has to be under the
//! egress ceiling: sign-in is one more thing the deployment talks to.
use std::sync::Arc;

use axum::extract::{Query, State as AxumState};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use rusty_agent_runtime::broker::SealedCredential;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::auth::{Role, TenantContext};
use crate::error::ApiError;
use crate::routes::AppState;

const NAMESPACE: &str = "oidc";
const KEY: &str = "provider";
/// How long a sign-in may take between leaving for the provider and
/// coming back.
const SIGN_IN_WINDOW_MINUTES: i64 = 10;
const MAX_PENDING: usize = 500;

/// The provider, as the store keeps it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    /// What the sign-in button says: "Sign in with Okta".
    pub name: String,
    /// The issuer URL; discovery is `{issuer}/.well-known/openid-configuration`.
    pub issuer: String,
    pub client_id: String,
    pub client_secret: SealedCredential,
    /// The role a person gets on their first sign-in.
    pub default_role: Role,
    /// Scopes asked for; `openid` is always among them.
    #[serde(default = "default_scopes")]
    pub scopes: String,
    pub discovery: Discovery,
    pub updated_by: Value,
    pub updated_at: DateTime<Utc>,
}

fn default_scopes() -> String {
    "openid profile email".to_owned()
}

/// What the provider's discovery document said.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub userinfo_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_session_endpoint: Option<String>,
    pub read_at: DateTime<Utc>,
}

/// A sign-in in flight: what the callback needs to finish it.
#[derive(Debug, Clone)]
pub struct PendingSignIn {
    pub nonce: String,
    pub code_verifier: String,
    pub started_at: DateTime<Utc>,
    pub return_to: Option<String>,
}

fn owner() -> String {
    "oidc:provider".to_owned()
}

fn random_token() -> String {
    use chacha20poly1305::aead::rand_core::RngCore;
    let mut bytes = [0u8; 32];
    chacha20poly1305::aead::OsRng.fill_bytes(&mut bytes);
    base64url(&bytes)
}

fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(TABLE[n as usize & 63] as char);
        }
    }
    out
}

fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for c in text.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'\n' | b'\r' => continue,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn urlencode(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The ID token's claims, without checking its signature (see the module
/// note): the payload of the JWS the token endpoint answered with.
fn claims_of(id_token: &str) -> Result<Value, String> {
    let mut parts = id_token.split('.');
    let (Some(_header), Some(payload), Some(_signature)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err("the ID token is not a JWT".to_owned());
    };
    let bytes = base64url_decode(payload)
        .ok_or_else(|| "the ID token's payload is not base64url".to_owned())?;
    serde_json::from_slice(&bytes).map_err(|e| format!("the ID token's payload is not JSON: {e}"))
}

pub(crate) async fn load(state: &AppState, tenant: &str) -> Result<Option<Provider>, ApiError> {
    let item = state
        .server_store
        .kv_get(&format!("{NAMESPACE}:{tenant}"), KEY)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(item.and_then(|i| serde_json::from_value(i.value).ok()))
}

async fn keep(state: &AppState, tenant: &str, provider: &Provider) -> Result<(), ApiError> {
    let value = serde_json::to_value(provider).map_err(|e| ApiError::internal(e.to_string()))?;
    state
        .server_store
        .kv_put(&format!("{NAMESPACE}:{tenant}"), KEY, value)
        .await
        .map(|_| ())
        .map_err(|e| ApiError::internal(e.to_string()))
}

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
}

/// What the sign-in page shows: whether a provider is set, and its name.
/// Public: the page is what a signed-out person sees.
pub(crate) async fn get_public(
    AxumState(state): AxumState<Arc<AppState>>,
) -> Result<Json<Value>, ApiError> {
    let provider = load(&state, crate::auth::DEFAULT_TENANT).await?;
    Ok(Json(match provider {
        Some(p) => {
            json!({"configured": true, "name": p.name, "issuer": p.issuer, "start": "/auth/oidc/start"})
        }
        None => json!({"configured": false}),
    }))
}

/// The provider as an administrator sees it: everything but the secret.
pub(crate) async fn get_provider(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let provider = load(&state, tenant.tenant()).await?;
    Ok(Json(json!({
        "provider": provider.as_ref().map(served),
        "callback_url": callback_url(&state),
    })))
}

fn served(p: &Provider) -> Value {
    json!({
        "name": p.name,
        "issuer": p.issuer,
        "client_id": p.client_id,
        "has_secret": true,
        "default_role": p.default_role,
        "scopes": p.scopes,
        "discovery": p.discovery,
        "updated_by": p.updated_by,
        "updated_at": p.updated_at,
    })
}

pub(crate) fn callback_url(state: &AppState) -> String {
    format!(
        "{}/auth/oidc/callback",
        state.config.public_url.trim_end_matches('/')
    )
}

#[derive(Debug, Deserialize)]
pub struct ProviderInput {
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    /// Sent once; absent on a later save keeps the sealed one.
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub default_role: Option<Role>,
    #[serde(default)]
    pub scopes: Option<String>,
}

/// `PUT /auth/oidc` — an administrator names the provider. The server reads
/// its discovery document now, so a wrong issuer is refused here rather
/// than at someone's sign-in.
pub(crate) async fn put_provider(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
    Json(input): Json<ProviderInput>,
) -> Result<Json<Value>, ApiError> {
    let name = input.name.trim().to_owned();
    let issuer = input.issuer.trim().trim_end_matches('/').to_owned();
    let client_id = input.client_id.trim().to_owned();
    if name.is_empty() || issuer.is_empty() || client_id.is_empty() {
        return Err(ApiError::bad_request(
            "a provider is a name, an issuer URL and a client id".to_owned(),
        ));
    }
    if !(issuer.starts_with("https://") || issuer.starts_with("http://")) {
        return Err(ApiError::bad_request(
            "the issuer is a URL, e.g. https://login.example.com/oauth2/default".to_owned(),
        ));
    }
    let host = host_of(&issuer)
        .ok_or_else(|| ApiError::bad_request("the issuer names no host".to_owned()))?;
    // The provider is one more host the deployment talks to: under the
    // ceiling, or refused by name so an administrator can allow it.
    let ceiling = crate::egress_ceiling::current(&state);
    if let Some(outside) = crate::egress_ceiling::outside(&ceiling, [host.as_str()]) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "egress_ceiling",
            format!("sign-in would call {outside}, which the egress ceiling does not allow — allow it in Settings → Security → Sites agents may reach, then save again"),
        ));
    }
    let existing = load(&state, tenant.tenant()).await?;
    let client_secret = match input.client_secret.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(secret) => state
            .broker
            .seal_connector_secret(&owner(), secret.as_bytes())
            .await
            .map_err(|e| ApiError::internal(format!("the client secret could not be sealed: {e}")))?,
        None => match existing.as_ref() {
            Some(p) if p.issuer == issuer && p.client_id == client_id => p.client_secret.clone(),
            _ => return Err(ApiError::bad_request("the client secret is needed the first time, and again whenever the issuer or client id change".to_owned())),
        },
    };
    let discovery = discover(&issuer)
        .await
        .map_err(|why| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "discovery_failed", why))?;
    let provider = Provider {
        name,
        issuer,
        client_id,
        client_secret,
        default_role: input.default_role.unwrap_or(Role::Builder),
        scopes: input
            .scopes
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s.split_whitespace().any(|w| w == "openid") {
                    s
                } else {
                    format!("openid {s}")
                }
            })
            .unwrap_or_else(default_scopes),
        discovery,
        updated_by: tenant.attribution(),
        updated_at: Utc::now(),
    };
    keep(&state, tenant.tenant(), &provider).await?;
    tracing::info!(issuer = %provider.issuer, name = %provider.name, "oidc provider set");
    Ok(Json(
        json!({"provider": served(&provider), "callback_url": callback_url(&state)}),
    ))
}

/// `DELETE /auth/oidc` — password sign-in only from here; accounts stay.
pub(crate) async fn delete_provider(
    AxumState(state): AxumState<Arc<AppState>>,
    Extension(tenant): Extension<TenantContext>,
) -> Result<Json<Value>, ApiError> {
    let removed = state
        .server_store
        .kv_delete(&format!("{NAMESPACE}:{}", tenant.tenant()), KEY)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(json!({"removed": removed})))
}

/// The provider's discovery document, read now.
async fn discover(issuer: &str) -> Result<Discovery, String> {
    let url = format!("{issuer}/.well-known/openid-configuration");
    let response = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("the issuer's discovery document could not be read ({url}): {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "the issuer's discovery document answered {} ({url})",
            response.status()
        ));
    }
    let doc: Value = response
        .json()
        .await
        .map_err(|e| format!("the discovery document is not JSON: {e}"))?;
    let said = doc
        .get("issuer")
        .and_then(Value::as_str)
        .map(|s| s.trim_end_matches('/'));
    if said.is_some_and(|s| s != issuer) {
        return Err(format!(
            "the discovery document names issuer {}, not {issuer}",
            said.unwrap_or("?")
        ));
    }
    let endpoint = |name: &str| doc.get(name).and_then(Value::as_str).map(str::to_owned);
    Ok(Discovery {
        authorization_endpoint: endpoint("authorization_endpoint")
            .ok_or("the discovery document names no authorization_endpoint")?,
        token_endpoint: endpoint("token_endpoint")
            .ok_or("the discovery document names no token_endpoint")?,
        userinfo_endpoint: endpoint("userinfo_endpoint"),
        end_session_endpoint: endpoint("end_session_endpoint"),
        read_at: Utc::now(),
    })
}

#[derive(Debug, Deserialize)]
pub struct StartQuery {
    #[serde(default)]
    pub return_to: Option<String>,
}

/// `GET /auth/oidc/start` — off to the provider, with state, nonce and PKCE.
pub(crate) async fn start(
    AxumState(state): AxumState<Arc<AppState>>,
    Query(query): Query<StartQuery>,
) -> Response {
    let provider = match load(&state, crate::auth::DEFAULT_TENANT).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return page(
                "No identity provider is set up here — sign in with a name and password.",
                false,
            )
        }
        Err(e) => return page(&e.to_string(), false),
    };
    let sign_in_state = random_token();
    let nonce = random_token();
    let code_verifier = format!("{}{}", random_token(), random_token());
    let challenge = base64url(&Sha256::digest(code_verifier.as_bytes()));
    // Only a same-site path is a place to return to.
    let return_to = query
        .return_to
        .filter(|r| r.starts_with('/') && !r.starts_with("//"));
    {
        let mut pending = state.oidc_pending.lock().await;
        let now = Utc::now();
        pending.retain(|_, p| (now - p.started_at).num_minutes() <= SIGN_IN_WINDOW_MINUTES);
        if pending.len() >= MAX_PENDING {
            return page(
                "Too many sign-ins are in flight; try again in a minute.",
                false,
            );
        }
        pending.insert(
            sign_in_state.clone(),
            PendingSignIn {
                nonce: nonce.clone(),
                code_verifier,
                started_at: now,
                return_to,
            },
        );
    }
    let url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&nonce={}&code_challenge={}&code_challenge_method=S256",
        provider.discovery.authorization_endpoint,
        urlencode(&provider.client_id),
        urlencode(&callback_url(&state)),
        urlencode(&provider.scopes),
        urlencode(&sign_in_state),
        urlencode(&nonce),
        urlencode(&challenge),
    );
    Redirect::to(&url).into_response()
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_description: Option<String>,
}

/// `GET /auth/oidc/callback` — back from the provider: the code is
/// exchanged, the claims checked, the session opened, the browser sent to
/// the studio.
pub(crate) async fn callback(
    AxumState(state): AxumState<Arc<AppState>>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    if let Some(error) = query.error {
        return page(
            &format!(
                "The identity provider refused the sign-in: {} {}",
                error,
                query.error_description.unwrap_or_default()
            ),
            false,
        );
    }
    let (Some(code), Some(sign_in_state)) = (query.code, query.state) else {
        return page("The identity provider sent no code back.", false);
    };
    let Some(pending) = state.oidc_pending.lock().await.remove(&sign_in_state) else {
        return page(
            "This sign-in was not started here, or was already finished — start again.",
            false,
        );
    };
    if (Utc::now() - pending.started_at).num_minutes() > SIGN_IN_WINDOW_MINUTES {
        return page("That sign-in took too long — start it again.", false);
    }
    match finish(&state, &pending, &code).await {
        Ok((token, expires, principal)) => {
            let to = pending.return_to.unwrap_or_else(|| "/".to_owned());
            tracing::info!(principal = %principal.id, "oidc sign-in");
            (
                StatusCode::SEE_OTHER,
                [
                    (
                        header::SET_COOKIE,
                        crate::routes::session_cookie_header(&token, Some(expires)),
                    ),
                    (header::LOCATION, studio_url(&state, &to)),
                ],
            )
                .into_response()
        }
        Err(why) => {
            tracing::warn!(%why, "oidc sign-in refused");
            page(&why, false)
        }
    }
}

/// Where the studio is: the same origin as the server's public URL, port
/// 4400 in the dev layout; a deployment sets `RUSTY_STUDIO_URL`.
fn studio_url(state: &AppState, path: &str) -> String {
    let base = std::env::var("RUSTY_STUDIO_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            let public = state.config.public_url.trim_end_matches('/');
            match public.rsplit_once(':') {
                Some((head, port))
                    if port.chars().all(|c| c.is_ascii_digit())
                        && !head.ends_with("http")
                        && !head.ends_with("https") =>
                {
                    format!("{head}:4400")
                }
                _ => public.to_owned(),
            }
        });
    format!("{}{}", base.trim_end_matches('/'), path)
}

async fn finish(
    state: &AppState,
    pending: &PendingSignIn,
    code: &str,
) -> Result<(String, DateTime<Utc>, crate::auth::Principal), String> {
    let tenant = crate::auth::DEFAULT_TENANT;
    let provider = load(state, tenant)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no identity provider is set up")?;
    let secret = state
        .broker
        .open_connector_secret(&owner(), &provider.client_secret)
        .await
        .map_err(|e| format!("the client secret could not be opened: {e}"))?;
    let secret = String::from_utf8_lossy(&secret).to_string();
    let redirect_uri = callback_url(state);
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", provider.client_id.as_str()),
        ("client_secret", secret.as_str()),
        ("code_verifier", pending.code_verifier.as_str()),
    ];
    let response = reqwest::Client::new()
        .post(&provider.discovery.token_endpoint)
        .form(&form)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("the token endpoint could not be reached: {e}"))?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|e| format!("the token endpoint answered something that is not JSON: {e}"))?;
    if !status.is_success() {
        let said = body
            .get("error_description")
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("no reason given");
        return Err(format!(
            "the token endpoint refused the code ({status}): {said}"
        ));
    }
    let id_token = body
        .get("id_token")
        .and_then(Value::as_str)
        .ok_or("the token endpoint answered no ID token")?;
    let claims = claims_of(id_token)?;
    // The checks the flow rests on: this provider, for this client, still
    // valid, and the nonce this sign-in minted.
    let issuer_claim = claims
        .get("iss")
        .and_then(Value::as_str)
        .map(|s| s.trim_end_matches('/'))
        .unwrap_or("");
    if issuer_claim != provider.issuer.trim_end_matches('/') {
        return Err(format!(
            "the ID token names issuer {issuer_claim}, not {}",
            provider.issuer
        ));
    }
    let audience_ok = match claims.get("aud") {
        Some(Value::String(a)) => a == &provider.client_id,
        Some(Value::Array(list)) => list
            .iter()
            .any(|a| a.as_str() == Some(provider.client_id.as_str())),
        _ => false,
    };
    if !audience_ok {
        return Err("the ID token is not for this client".to_owned());
    }
    let exp = claims.get("exp").and_then(Value::as_i64).unwrap_or(0);
    if exp < Utc::now().timestamp() - 60 {
        return Err("the ID token has expired".to_owned());
    }
    if claims.get("nonce").and_then(Value::as_str) != Some(pending.nonce.as_str()) {
        return Err("the ID token's nonce is not the one this sign-in minted".to_owned());
    }
    let subject = claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("the ID token names no subject")?
        .to_owned();
    let name = claims
        .get("name")
        .or_else(|| claims.get("preferred_username"))
        .or_else(|| claims.get("email"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&subject)
        .to_owned();
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // A person forgotten here does not come back through the provider.
    if state
        .users
        .is_forgotten(&crate::users::external_key(&provider.issuer, &subject))
    {
        return Err("this person was forgotten here; nothing signs in as them".to_owned());
    }
    // The person: known by the provider's subject, made on first arrival.
    let by_name = email
        .as_deref()
        .map(|e| e.to_ascii_lowercase())
        .and_then(|e| state.users.get(&e))
        .filter(|u| u.external.is_none());
    let user = match state.users.find_external(&provider.issuer, &subject) {
        Some(user) => {
            if user.name != name {
                state
                    .users
                    .rename(&user.id, &name)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            user
        }
        None if by_name.is_some() => {
            // An account with this sign-in name that no provider subject
            // owns yet — one the directory made, or a password one: the
            // person's, linked from now on.
            let existing = by_name.expect("checked");
            state
                .users
                .set_external(&existing.id, &provider.issuer, &subject)
                .await
                .map_err(|e| e.to_string())?;
            state.users.get(&existing.id).unwrap_or(existing)
        }
        None => {
            let wanted = email
                .as_deref()
                .map(|e| e.to_ascii_lowercase())
                .filter(|e| !e.is_empty())
                .unwrap_or_else(|| {
                    format!(
                        "oidc-{}",
                        subject
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric())
                            .take(12)
                            .collect::<String>()
                            .to_ascii_lowercase()
                    )
                });
            state
                .users
                .provision_external(
                    &wanted,
                    &name,
                    vec![provider.default_role],
                    Some((&provider.issuer, &subject)),
                )
                .await
                .map_err(|e| format!("the account could not be made: {e}"))?
        }
    };
    if state.users.is_forgotten(&user.id) {
        return Err(format!(
            "`{}` was forgotten here; nothing signs in as them",
            user.id
        ));
    }
    if !user.active {
        return Err(format!(
            "`{}` was deactivated by the directory; sign-in is refused until it says otherwise",
            user.id
        ));
    }
    let principal = user.principal();
    let (token, expires) = state
        .sessions
        .open(principal.clone(), user.security_epoch)
        .await;
    Ok((token, expires, principal))
}

fn page(message: &str, ok: bool) -> Response {
    let tone = if ok { "#1c7c4a" } else { "#b3261e" };
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Sign in</title></head><body style=\"font-family: system-ui, sans-serif; padding: 40px; color: #222\"><p style=\"color: {tone}; font-size: 16px\">{}</p><p><a href=\"/\">Back to the studio</a></p></body></html>",
        html_escape(message)
    );
    (
        if ok {
            StatusCode::OK
        } else {
            StatusCode::BAD_REQUEST
        },
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_and_pkce_challenge_is_s256() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let text = base64url(&bytes);
        assert!(!text.contains('=') && !text.contains('+') && !text.contains('/'));
        assert_eq!(base64url_decode(&text).unwrap(), bytes);
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64url(&Sha256::digest(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn claims_come_from_the_payload_alone() {
        let payload = base64url(br#"{"iss":"https://idp.example","sub":"u-1","aud":"rusty","exp":4102444800,"nonce":"n"}"#);
        let token = format!("eyJhbGciOiJub25lIn0.{payload}.sig");
        let claims = claims_of(&token).unwrap();
        assert_eq!(claims["sub"], "u-1");
        assert!(claims_of("not-a-jwt").is_err());
    }
}
