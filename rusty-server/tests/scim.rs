//! SCIM: the directory provisions a person, changes them, maps groups to
//! roles, deactivates them — signed out everywhere, refused from then on
//! — and deletes them. The token is minted by an administrator, shown
//! once, and is the only key the directory has.
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_runtime::error::Result as RustyResult;
use rusty_agent_runtime::llm::{ChatMessage, ChatModel, ChatResponse};
use rusty_agent_runtime::react::{create_react_agent, MESSAGES_CHANNEL};
use rusty_agent_runtime::state::{Reducer, StateSpec};
use rusty_agent_runtime::tool::ToolRegistry;
use rusty_agent_server::{router, GraphRegistry, Principal, PrincipalKind, Role, ServerConfig};
use serde_json::{json, Value};
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
            builder = builder.header("content-type", "application/scim+json");
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

#[tokio::test]
async fn the_directory_provisions_maps_groups_to_roles_deactivates_and_deletes_people() {
    let store = std::env::temp_dir().join(format!("rusty-scim-{}", uuid::Uuid::new_v4()));
    let app = app(&store);
    let admin = [("x-api-key", "ada-key")];

    // No token yet: the directory is refused with a SCIM error; the
    // administrator mints one and sees it once.
    let (status, _, refused) = call(
        &app,
        "GET",
        "/scim/v2/Users",
        &[("authorization", "Bearer nothing")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        refused["schemas"][0], "urn:ietf:params:scim:api:messages:2.0:Error",
        "{refused}"
    );
    let (status, _, minted) = call(&app, "POST", "/auth/scim/token", &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{minted}");
    let token = minted["token"].as_str().unwrap().to_owned();
    assert!(token.starts_with("scim_"));
    assert_eq!(minted["base_url"], "http://127.0.0.1:8100/scim/v2");
    let (_, _, shown) = call(&app, "GET", "/auth/scim", &admin, None).await;
    assert_eq!(shown["has_token"], true);
    assert!(
        shown.get("token").is_none(),
        "the token is shown once: {shown}"
    );
    let bearer = format!("Bearer {token}");
    let directory = [("authorization", bearer.as_str())];
    let (status, _, _) = call(
        &app,
        "GET",
        "/scim/v2/Users",
        &[("authorization", "Bearer scim_wrong")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Discovery, as a directory reads it before it starts.
    let (status, headers, spc) = call(
        &app,
        "GET",
        "/scim/v2/ServiceProviderConfig",
        &directory,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{spc}");
    assert_eq!(
        headers.get("content-type").unwrap().to_str().unwrap(),
        "application/scim+json"
    );
    assert_eq!(spc["patch"]["supported"], true);
    let (_, _, types) = call(&app, "GET", "/scim/v2/ResourceTypes", &directory, None).await;
    assert_eq!(types["totalResults"], 2);

    // The group → role map: directory admins are administrators here.
    let (status, _, mapped) = call(&app, "PUT", "/auth/scim/group-roles", &admin, Some(json!({"group_roles": [{"group": "Rusty Admins", "role": "admin"}, {"group": "Rusty Builders", "role": "builder"}]}))).await;
    assert_eq!(status, StatusCode::OK, "{mapped}");
    assert_eq!(mapped["group_roles"].as_array().unwrap().len(), 2);

    // Provisioning: a person is created, a builder (the default), active.
    let (status, _, priya) = call(&app, "POST", "/scim/v2/Users", &directory, Some(json!({
        "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"],
        "externalId": "00u-priya", "userName": "priya@example.com", "active": true,
        "name": {"givenName": "Priya", "familyName": "Natarajan"}, "emails": [{"value": "priya@example.com", "primary": true}]
    }))).await;
    assert_eq!(status, StatusCode::CREATED, "{priya}");
    assert_eq!(priya["id"], "priya@example.com");
    assert_eq!(priya["externalId"], "00u-priya");
    assert_eq!(priya["name"]["formatted"], "Priya Natarajan");
    assert_eq!(priya["active"], true);
    assert_eq!(
        priya["urn:ietf:params:scim:schemas:extension:rusty:2.0:User"]["roles"],
        json!(["builder"])
    );
    // Looked up the way a directory does before creating twice.
    let (_, _, found) = call(
        &app,
        "GET",
        "/scim/v2/Users?filter=userName%20eq%20%22priya%40example.com%22",
        &directory,
        None,
    )
    .await;
    assert_eq!(found["totalResults"], 1, "{found}");
    assert_eq!(found["Resources"][0]["id"], "priya@example.com");
    let (_, _, by_ext) = call(
        &app,
        "GET",
        "/scim/v2/Users?filter=externalId%20eq%20%2200u-priya%22",
        &directory,
        None,
    )
    .await;
    assert_eq!(by_ext["totalResults"], 1);
    let (status, _, bad) = call(
        &app,
        "GET",
        "/scim/v2/Users?filter=userName%20co%20%22pri%22",
        &directory,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    assert_eq!(bad["scimType"], "invalidFilter");
    // Security → People sees the same account, with what the directory said.
    let (_, _, users) = call(&app, "GET", "/users", &admin, None).await;
    let listed = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == "priya@example.com")
        .cloned()
        .unwrap();
    assert_eq!(listed["active"], true);
    assert_eq!(listed["external_id"], "00u-priya");

    // A group with her in it: the map makes her an administrator.
    let (status, _, group) = call(&app, "POST", "/scim/v2/Groups", &directory, Some(json!({"schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"], "displayName": "Rusty Admins", "members": [{"value": "priya@example.com"}]}))).await;
    assert_eq!(status, StatusCode::CREATED, "{group}");
    let group_id = group["id"].as_str().unwrap().to_owned();
    let (_, _, after) = call(
        &app,
        "GET",
        "/scim/v2/Users/priya@example.com",
        &directory,
        None,
    )
    .await;
    assert_eq!(
        after["urn:ietf:params:scim:schemas:extension:rusty:2.0:User"]["roles"],
        json!(["admin"]),
        "{after}"
    );
    // Moved to the builders' group: a builder again.
    let (_, _, builders) = call(&app, "POST", "/scim/v2/Groups", &directory, Some(json!({"schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"], "displayName": "Rusty Builders"}))).await;
    let builders_id = builders["id"].as_str().unwrap().to_owned();
    let (status, _, _) = call(&app, "PATCH", &format!("/scim/v2/Groups/{group_id}"), &directory, Some(json!({"schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": [{"op": "remove", "path": "members[value eq \"priya@example.com\"]"}]}))).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(&app, "PATCH", &format!("/scim/v2/Groups/{builders_id}"), &directory, Some(json!({"schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": [{"op": "add", "path": "members", "value": [{"value": "priya@example.com"}]}]}))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, after) = call(
        &app,
        "GET",
        "/scim/v2/Users/priya@example.com",
        &directory,
        None,
    )
    .await;
    assert_eq!(
        after["urn:ietf:params:scim:schemas:extension:rusty:2.0:User"]["roles"],
        json!(["builder"]),
        "{after}"
    );

    // Deactivation, the shape Entra sends: signed out everywhere, refused.
    let (status, _, patched) = call(&app, "PATCH", "/scim/v2/Users/priya@example.com", &directory, Some(json!({"schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": [{"op": "Replace", "value": {"active": false}}]}))).await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["active"], false);
    let (_, _, users) = call(&app, "GET", "/users", &admin, None).await;
    let listed = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == "priya@example.com")
        .cloned()
        .unwrap();
    assert_eq!(listed["active"], false);
    let (_, _, config) = call(&app, "GET", "/auth/scim", &admin, None).await;
    assert_eq!(config["provisioned"], 1);
    assert_eq!(config["deactivated"], 1);
    assert!(config["last_seen_at"].is_string());
    // …and back, the Okta shape.
    let (status, _, patched) = call(&app, "PATCH", "/scim/v2/Users/priya@example.com", &directory, Some(json!({"schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": [{"op": "replace", "path": "active", "value": true}, {"op": "replace", "path": "name.givenName", "value": "Priyanka"}]}))).await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["active"], true);
    assert_eq!(patched["name"]["formatted"], "Priyanka Natarajan");
    // The sign-in name does not change through the directory.
    let (status, _, refused) = call(&app, "PATCH", "/scim/v2/Users/priya@example.com", &directory, Some(json!({"schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": [{"op": "replace", "path": "userName", "value": "p.natarajan@example.com"}]}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{refused}");
    assert_eq!(refused["scimType"], "mutability");

    // Deletion: the account goes, the groups drop her, the count moves.
    let (status, _, _) = call(
        &app,
        "DELETE",
        "/scim/v2/Users/priya@example.com",
        &directory,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = call(
        &app,
        "GET",
        "/scim/v2/Users/priya@example.com",
        &directory,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, _, group) = call(
        &app,
        "GET",
        &format!("/scim/v2/Groups/{builders_id}"),
        &directory,
        None,
    )
    .await;
    assert_eq!(group["members"].as_array().unwrap().len(), 0, "{group}");
    let (_, _, config) = call(&app, "GET", "/auth/scim", &admin, None).await;
    assert_eq!(config["deleted"], 1);

    // The token revoked: the directory is shut out; the map stays.
    let (status, _, revoked) = call(&app, "DELETE", "/auth/scim/token", &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["revoked"], true);
    let (status, _, _) = call(&app, "GET", "/scim/v2/Users", &directory, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let _ = std::fs::remove_dir_all(store);
}
