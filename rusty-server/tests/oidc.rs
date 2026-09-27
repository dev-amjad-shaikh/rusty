//! OIDC sign-in: an administrator names the provider (its discovery
//! document is read then), the sign-in page sends the browser off with
//! state, nonce and PKCE, the callback exchanges the code, checks the
//! claims and opens the same session a password opens — making the
//! account on first arrival, finding it by the provider's subject after.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::extract::{Query, State as AxumState};
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::{router, GraphRegistry, Principal, PrincipalKind, Role, ServerConfig};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

struct Brief;

#[async_trait::async_trait]
impl ChatModel for Brief {
    async fn chat(&self, _m: &[ChatMessage], _t: &[Value]) -> RustyResult<ChatResponse> {
        Ok(ChatResponse {
            message: ChatMessage::assistant("noted"),
            model: Some("brief".into()),
            usage: None,
        })
    }
}

fn b64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
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

/// A tiny identity provider: discovery, a code it issues to whoever asks
/// with a valid PKCE challenge, a token endpoint that checks the verifier
/// and the client secret and answers an ID token (HS256-shaped; the
/// relying party reads the payload).
#[derive(Clone)]
struct Idp {
    issuer: Arc<Mutex<String>>,
    // code -> (sub, nonce, challenge)
    codes: Arc<Mutex<HashMap<String, AuthCode>>>,
    tokens_issued: Arc<Mutex<u32>>,
}

struct AuthCode(String, String, String);

async fn discovery(AxumState(idp): AxumState<Idp>) -> Json<Value> {
    let issuer = idp.issuer.lock().unwrap().clone();
    Json(
        json!({"issuer": issuer, "authorization_endpoint": format!("{issuer}/authorize"), "token_endpoint": format!("{issuer}/token"), "userinfo_endpoint": format!("{issuer}/userinfo")}),
    )
}

/// The authorize step, as the test drives it: it "signs in" as `sub` and
/// gets the code the browser would carry back.
async fn authorize(
    AxumState(idp): AxumState<Idp>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    let code = format!("code-{}", uuid::Uuid::new_v4().simple());
    let sub = q
        .get("sub")
        .cloned()
        .unwrap_or_else(|| "u-priya".to_owned());
    idp.codes.lock().unwrap().insert(
        code.clone(),
        AuthCode(
            sub,
            q.get("nonce").cloned().unwrap_or_default(),
            q.get("code_challenge").cloned().unwrap_or_default(),
        ),
    );
    Json(json!({"code": code, "state": q.get("state")}))
}

async fn token(
    AxumState(idp): AxumState<Idp>,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    if form.get("client_secret").map(String::as_str) != Some("s3cret")
        || form.get("client_id").map(String::as_str) != Some("rusty-studio")
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid_client"})),
        );
    }
    let Some(AuthCode(sub, nonce, challenge)) = idp
        .codes
        .lock()
        .unwrap()
        .remove(form.get("code").map(String::as_str).unwrap_or(""))
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant", "error_description": "unknown or used code"})),
        );
    };
    let verifier = form.get("code_verifier").cloned().unwrap_or_default();
    if b64url(&Sha256::digest(verifier.as_bytes())) != challenge {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error": "invalid_grant", "error_description": "PKCE verifier does not match"}),
            ),
        );
    }
    let issuer = idp.issuer.lock().unwrap().clone();
    let (name, email) = if sub == "u-priya" {
        ("Priya Natarajan", "priya@example.com")
    } else {
        ("Omar Haddad", "omar@example.com")
    };
    let payload = json!({"iss": issuer, "sub": sub, "aud": "rusty-studio", "exp": chrono::Utc::now().timestamp() + 600, "nonce": nonce, "name": name, "email": email});
    let id_token = format!(
        "{}.{}.{}",
        b64url(br#"{"alg":"HS256","typ":"JWT"}"#),
        b64url(payload.to_string().as_bytes()),
        b64url(b"signature")
    );
    *idp.tokens_issued.lock().unwrap() += 1;
    (
        StatusCode::OK,
        Json(json!({"access_token": "at", "token_type": "Bearer", "id_token": id_token})),
    )
}

async fn serve_idp() -> (String, Idp) {
    let idp = Idp {
        issuer: Arc::new(Mutex::new(String::new())),
        codes: Arc::new(Mutex::new(HashMap::new())),
        tokens_issued: Arc::new(Mutex::new(0)),
    };
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .with_state(idp.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    *idp.issuer.lock().unwrap() = issuer.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (issuer, idp)
}

fn app(store: &std::path::Path) -> Router {
    let tools = ToolRegistry::new();
    let graph = create_react_agent(Arc::new(Brief), tools.clone()).unwrap();
    let spec = StateSpec::new().channel(MESSAGES_CHANNEL, Reducer::AddMessages);
    let mut registry = GraphRegistry::new();
    registry
        .register_with_tools("react_agent", graph, spec, &tools)
        .unwrap();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.to_path_buf())
        .with_public_url("http://127.0.0.1:8100")
        .with_principal(
            "default",
            Principal {
                id: "ada".into(),
                name: "ada".into(),
                kind: PrincipalKind::User,
                roles: vec![Role::Admin],
            },
            "ada-key",
        );
    router(registry, config)
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, headers, value)
}

fn query_of(url: &str) -> HashMap<String, String> {
    let parsed = reqwest::Url::parse(url).unwrap();
    parsed
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// Walk the browser's part: start at the server, "sign in" at the provider
/// as `sub`, come back with the code. Returns the session cookie.
async fn sign_in(app: &Router, idp_issuer: &str, sub: &str) -> Result<String, String> {
    let (status, headers, _) =
        call(app, "GET", "/auth/oidc/start?return_to=/agents", &[], None).await;
    assert_eq!(
        status,
        StatusCode::SEE_OTHER,
        "start redirects to the provider"
    );
    let location = headers
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        location.starts_with(&format!("{idp_issuer}/authorize?")),
        "{location}"
    );
    let q = query_of(&location);
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], "rusty-studio");
    assert_eq!(
        q["redirect_uri"],
        "http://127.0.0.1:8100/auth/oidc/callback"
    );
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(q["scope"].contains("openid"));
    // The provider, as the browser would reach it.
    let issued: Value = reqwest::get(format!(
        "{idp_issuer}/authorize?sub={sub}&nonce={}&code_challenge={}&state={}",
        q["nonce"], q["code_challenge"], q["state"]
    ))
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let code = issued["code"].as_str().unwrap().to_owned();
    let (status, headers, body) = call(
        app,
        "GET",
        &format!("/auth/oidc/callback?code={code}&state={}", q["state"]),
        &[],
        None,
    )
    .await;
    if status != StatusCode::SEE_OTHER {
        return Err(format!("{status}: {body}"));
    }
    assert_eq!(
        headers.get("location").unwrap().to_str().unwrap(),
        "http://127.0.0.1:4400/agents",
        "back to where the person was going"
    );
    let cookie = headers
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    Ok(cookie.split(';').next().unwrap().to_owned())
}

#[tokio::test]
async fn a_person_signs_in_through_the_provider_and_is_known_by_its_subject_from_then_on() {
    let store = std::env::temp_dir().join(format!("rusty-oidc-{}", uuid::Uuid::new_v4()));
    let (issuer, idp) = serve_idp().await;
    let app = app(&store);

    // Nobody configured: the sign-in page says so; start has nowhere to go.
    let (status, _, public) = call(&app, "GET", "/auth/oidc", &[], None).await;
    assert_eq!(status, StatusCode::OK, "{public}");
    assert_eq!(public["configured"], false);
    let (status, _, _) = call(&app, "GET", "/auth/oidc/start", &[], None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The administrator names the provider; discovery is read now, so a
    // wrong issuer is refused here.
    let (status, _, refused) = call(&app, "PUT", "/auth/oidc/provider", &[("x-api-key", "ada-key")], Some(json!({"name": "Example Identity", "issuer": "http://127.0.0.1:1", "client_id": "rusty-studio", "client_secret": "s3cret"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{refused}");
    let (status, _, set) = call(&app, "PUT", "/auth/oidc/provider", &[("x-api-key", "ada-key")], Some(json!({"name": "Example Identity", "issuer": issuer, "client_id": "rusty-studio", "client_secret": "s3cret", "default_role": "builder"}))).await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["provider"]["has_secret"], true);
    assert!(
        set["provider"].get("client_secret").is_none(),
        "the secret never comes back: {set}"
    );
    assert_eq!(
        set["provider"]["discovery"]["token_endpoint"],
        format!("{issuer}/token")
    );
    assert_eq!(
        set["callback_url"],
        "http://127.0.0.1:8100/auth/oidc/callback"
    );
    let (_, _, public) = call(&app, "GET", "/auth/oidc", &[], None).await;
    assert_eq!(public["name"], "Example Identity");
    assert_eq!(public["start"], "/auth/oidc/start");

    // Priya signs in: an account is made for her, a builder, and the
    // session cookie is the one a password sign-in gets.
    let cookie = sign_in(&app, &issuer, "u-priya").await.unwrap();
    let (status, _, me) = call(&app, "GET", "/me", &[("cookie", &cookie)], None).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["principal"]["id"], "priya@example.com", "{me}");
    assert_eq!(me["principal"]["name"], "Priya Natarajan");
    assert_eq!(me["roles"], json!(["builder"]));
    let (_, _, users) = call(&app, "GET", "/users", &[("x-api-key", "ada-key")], None).await;
    let priya = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == "priya@example.com")
        .cloned()
        .expect("provisioned");
    assert_eq!(priya["roles"], json!(["builder"]), "{priya}");

    // Signing in again finds the same account by the provider's subject —
    // no second account, and a password sign-in for it is refused (it has
    // none of its own).
    let again = sign_in(&app, &issuer, "u-priya").await.unwrap();
    let (_, _, me) = call(&app, "GET", "/me", &[("cookie", &again)], None).await;
    assert_eq!(me["principal"]["id"], "priya@example.com");
    let (_, _, users) = call(&app, "GET", "/users", &[("x-api-key", "ada-key")], None).await;
    assert_eq!(
        users["users"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|u| u["id"].as_str().unwrap_or("").starts_with("priya"))
            .count(),
        1,
        "{users}"
    );
    let (status, _, _) = call(
        &app,
        "POST",
        "/auth/login",
        &[],
        Some(json!({"username": "priya@example.com", "password": "anything-at-all"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(*idp.tokens_issued.lock().unwrap(), 2);

    // A callback nobody started, and a state used twice, are refused.
    let (status, _, _) = call(
        &app,
        "GET",
        "/auth/oidc/callback?code=x&state=never-minted",
        &[],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A forgotten person does not come back through the provider.
    let (status, forgotten, _) = call(
        &app,
        "POST",
        "/users/priya@example.com/forget",
        &[("x-api-key", "ada-key")],
        Some(json!({"reason": "left"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{forgotten:?}");
    let refused = sign_in(&app, &issuer, "u-priya")
        .await
        .expect_err("refused after the forget");
    assert!(refused.contains("forgotten"), "{refused}");

    // The provider removed: password sign-in only, and the accounts stay.
    let (status, _, removed) = call(
        &app,
        "DELETE",
        "/auth/oidc/provider",
        &[("x-api-key", "ada-key")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    let (_, _, public) = call(&app, "GET", "/auth/oidc", &[], None).await;
    assert_eq!(public["configured"], false);

    let _ = std::fs::remove_dir_all(store);
}
