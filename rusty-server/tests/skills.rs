//! Skill-plane integration tests: the `/skills` HTTP surface over the
//! default JSON-file layout — register (parse + scan + version receipt),
//! the progressive-disclosure tiers (metadata listing, on-demand body,
//! on-demand member files), immutable revisions with history and pinned
//! reads, restart durability, tenant isolation, scan-denial `422`s with
//! structured findings, and `400`/`404` discipline.
//!
//! Driven in-process via `tower::ServiceExt::oneshot` (no sockets), the
//! `memory.rs` convention; restart tests build the app twice over one
//! store root.

use std::path::PathBuf;

use axum::body::{to_bytes, Body, Bytes};
use axum::http::{Request, StatusCode};
use axum::Router;
use rusty_agent_server::{router, GraphRegistry, ServerConfig};
use serde_json::{json, Value};
use tower::ServiceExt;

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

/// Unique temp store root, removed at the end of each test (best effort).
fn temp_store() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-server-skills-test-{}", uuid::Uuid::new_v4()))
}

/// Open-mode (single `default` tenant) app over a fresh store.
fn app() -> (Router, PathBuf) {
    let store = temp_store();
    (app_at(store.clone()), store)
}

/// Open-mode app over a given store root (restart tests build it twice).
fn app_at(store: PathBuf) -> Router {
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store);
    router(GraphRegistry::new(), config)
}

/// Two-tenant app for the isolation tests.
fn multi_tenant_app() -> (Router, PathBuf) {
    let store = temp_store();
    let config = ServerConfig::new("127.0.0.1:0".parse().unwrap(), store.clone())
        .with_tenant_key("acme", "acme-secret")
        .with_tenant_key("globex", "globex-secret");
    (router(GraphRegistry::new(), config), store)
}

/// Send a request; returns `(status, content-type, raw bytes)`.
async fn call_raw(
    app: &Router,
    auth: Option<(&str, &str)>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, String, Bytes) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some((k, v)) = auth {
        builder = builder.header(k, v);
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
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, content_type, bytes)
}

/// Send a request; returns `(status, json-body-or-null)`.
async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, _, bytes) = call_raw(app, None, method, uri, body).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// Send a request with an auth header; returns `(status, json)`.
async fn call_as(
    app: &Router,
    auth: (&str, &str),
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, _, bytes) = call_raw(app, Some(auth), method, uri, body).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

/// A minimal valid `SKILL.md` for `name`.
fn skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n")
}

/// Hex-encode bytes (the payload's asset encoding).
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A full registration payload: body, one reference, one asset.
fn register_payload(name: &str, body: &str) -> Value {
    json!({
        "skill_md": skill_md(name, &format!("The {name} skill."), body),
        "references": {"guide.md": "# Guide\n\nDetails on demand.\n"},
        "assets": {"logo.bin": hex_encode(&[0x89, 0x50, 0x4e, 0x47])},
        "author": "operator:ada",
    })
}

/// Register a package; asserts 201 and returns the receipt.
async fn publish(app: &Router, name: &str, body: &str) -> Value {
    let (status, v) = call(app, "POST", "/skills", Some(register_payload(name, body))).await;
    assert_eq!(status, StatusCode::CREATED, "register failed: {v}");
    v
}

// --------------------------------------------------------------------- //
// The registration receipt and idempotency
// --------------------------------------------------------------------- //

#[tokio::test]
async fn publish_returns_the_version_receipt() {
    let (app, store) = app();
    let receipt = publish(&app, "web-research", "Search, then summarize.").await;
    assert_eq!(receipt["name"], json!("web-research"));
    assert_eq!(receipt["revision"], json!(1));
    assert_eq!(receipt["already_registered"], json!(false));
    assert_eq!(receipt["content_hash"].as_str().unwrap().len(), 64);
    assert_eq!(receipt["provenance"]["author"], json!("operator:ada"));
    assert_eq!(
        receipt["provenance"]["source"],
        json!({"type": "registry", "name": "rusty-server"})
    );
    assert_eq!(receipt["scan"]["clean"], json!(true));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn re_publish_is_idempotent() {
    let (app, store) = app();
    let first = publish(&app, "a-skill", "Version one.").await;
    let (status, second) = call(
        &app,
        "POST",
        "/skills",
        Some(register_payload("a-skill", "Version one.")),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "idempotent re-register: {second}");
    assert_eq!(second["already_registered"], json!(true));
    assert_eq!(second["revision"], first["revision"]);
    assert_eq!(second["content_hash"], first["content_hash"]);
    // The history did not grow.
    let (status, history) = call(&app, "GET", "/skills/a-skill/history", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["history"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Progressive disclosure tiers
// --------------------------------------------------------------------- //

#[tokio::test]
async fn metadata_listing_is_the_cheap_tier() {
    let (app, store) = app();
    for name in ["gamma-skill", "alpha-skill", "beta-skill"] {
        publish(
            &app,
            name,
            &format!("Instructions for {name}: BODY-{name}."),
        )
        .await;
    }
    let (status, v) = call(&app, "GET", "/skills", None).await;
    assert_eq!(status, StatusCode::OK);
    let skills = v["skills"].as_array().unwrap();
    let names: Vec<_> = skills.iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["alpha-skill", "beta-skill", "gamma-skill"]);
    // Tier 1 carries no body and no member bytes.
    let raw = v.to_string();
    for name in ["alpha-skill", "beta-skill", "gamma-skill"] {
        assert!(!raw.contains(&format!("BODY-{name}")));
        assert!(!raw.contains("Details on demand"));
    }
    for entry in skills {
        assert_eq!(entry["revision"], json!(1));
        assert_eq!(entry["content_hash"].as_str().unwrap().len(), 64);
        assert_eq!(
            entry["description"].as_str().unwrap(),
            format!("The {} skill.", entry["name"].as_str().unwrap())
        );
    }
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn detail_body_and_files_disclose_on_demand() {
    let (app, store) = app();
    publish(&app, "web-research", "Search, then summarize.").await;

    // Detail: metadata + revision info.
    let (status, v) = call(&app, "GET", "/skills/web-research", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["metadata"]["name"], json!("web-research"));
    assert_eq!(v["revision"], json!(1));
    assert_eq!(v["revisions"], json!(1));

    // Tier 2: the body, on explicit demand.
    let (status, v) = call(&app, "GET", "/skills/web-research/body", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["body"], json!("Search, then summarize.\n"));
    assert_eq!(v["revision"], json!(1));

    // Tier 3: member files by path.
    let (status, content_type, bytes) = call_raw(
        &app,
        None,
        "GET",
        "/skills/web-research/files/references/guide.md",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.starts_with("text/markdown"),
        "got {content_type}"
    );
    assert_eq!(bytes.as_ref(), b"# Guide\n\nDetails on demand.\n");

    let (status, content_type, bytes) = call_raw(
        &app,
        None,
        "GET",
        "/skills/web-research/files/assets/logo.bin",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.starts_with("application/octet-stream"),
        "got {content_type}"
    );
    assert_eq!(bytes.as_ref(), &[0x89, 0x50, 0x4e, 0x47]);
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn file_paths_fail_closed() {
    let (app, store) = app();
    publish(&app, "a-skill", "Instructions.").await;
    // Traversal, absolute, backslash, and unknown members all answer 404 —
    // the wildcard path is a lookup key, never a filesystem path.
    for path in [
        "references/../secret.md",
        "..%2F..%2Fetc%2Fpasswd",
        "%2Fetc%2Fpasswd",
        "references%5Cguide.md",
        "references/missing.md",
        "scripts/run.sh",
    ] {
        let (status, _) = call(&app, "GET", &format!("/skills/a-skill/files/{path}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "path `{path}` must 404");
    }
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Immutable versions
// --------------------------------------------------------------------- //

#[tokio::test]
async fn revisions_history_and_pinned_versions() {
    let (app, store) = app();
    let first = publish(&app, "a-skill", "Version one.").await;
    let second = publish(&app, "a-skill", "Version two, revised.").await;
    assert_eq!(second["revision"], json!(2));
    assert_ne!(second["content_hash"], first["content_hash"]);

    // Latest moved forward.
    let (_, detail) = call(&app, "GET", "/skills/a-skill", None).await;
    assert_eq!(detail["revision"], json!(2));
    assert_eq!(detail["revisions"], json!(2));
    let (_, body) = call(&app, "GET", "/skills/a-skill/body", None).await;
    assert_eq!(body["body"], json!("Version two, revised.\n"));

    // History is ascending metadata.
    let (_, history) = call(&app, "GET", "/skills/a-skill/history", None).await;
    let revisions = history["history"].as_array().unwrap();
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0]["revision"], json!(1));
    assert_eq!(revisions[1]["revision"], json!(2));

    // The pinned revision serves its own content hash; unknown revisions 404.
    let (status, pinned) = call(&app, "GET", "/skills/a-skill/versions/1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pinned["content_hash"], first["content_hash"]);
    assert_eq!(pinned["revision"], json!(1));
    let (status, _) = call(&app, "GET", "/skills/a-skill/versions/3", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Restart durability
// --------------------------------------------------------------------- //

#[tokio::test]
async fn registry_rebuilds_from_the_store_on_boot() {
    let store = temp_store();
    let first_app = app_at(store.clone());
    let first = publish(&first_app, "web-research", "Version one.").await;
    publish(&first_app, "web-research", "Version two.").await;
    publish(&first_app, "other-skill", "Other instructions.").await;
    drop(first_app);

    // A fresh process over the same store root sees the same plane.
    let second_app = app_at(store.clone());
    let (status, v) = call(&second_app, "GET", "/skills", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<_> = v["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["other-skill", "web-research"]);

    let (_, detail) = call(&second_app, "GET", "/skills/web-research", None).await;
    assert_eq!(detail["revision"], json!(2));
    assert_eq!(detail["revisions"], json!(2));
    let (_, pinned) = call(&second_app, "GET", "/skills/web-research/versions/1", None).await;
    assert_eq!(pinned["content_hash"], first["content_hash"]);
    let (_, body) = call(&second_app, "GET", "/skills/web-research/body", None).await;
    assert_eq!(body["body"], json!("Version two.\n"));
    let (status, _, bytes) = call_raw(
        &second_app,
        None,
        "GET",
        "/skills/web-research/files/references/guide.md",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes.as_ref(), b"# Guide\n\nDetails on demand.\n");

    // Registration continues the rebuilt revision sequence.
    let third = publish(&second_app, "web-research", "Version three.").await;
    assert_eq!(third["revision"], json!(3));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Tenant isolation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn tenants_are_isolated_with_404_never_403() {
    let (app, store) = multi_tenant_app();
    let acme = ("x-api-key", "acme-secret");
    let globex = ("x-api-key", "globex-secret");

    let (status, receipt) = call_as(
        &app,
        acme,
        "POST",
        "/skills",
        Some(register_payload("a-skill", "Acme instructions.")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");

    // The other tenant cannot see the skill anywhere: 404, never 403.
    for uri in [
        "/skills/a-skill",
        "/skills/a-skill/body",
        "/skills/a-skill/history",
        "/skills/a-skill/versions/1",
        "/skills/a-skill/files/references/guide.md",
    ] {
        let (status, _) = call_as(&app, globex, "GET", uri, None).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "cross-tenant `{uri}` must 404"
        );
    }
    let (_, listed) = call_as(&app, globex, "GET", "/skills", None).await;
    assert_eq!(listed["skills"].as_array().unwrap().len(), 0);

    // The owning tenant reads it; both tenants may hold the same name
    // independently.
    let (status, detail) = call_as(&app, acme, "GET", "/skills/a-skill", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["revision"], json!(1));
    let (status, receipt) = call_as(
        &app,
        globex,
        "POST",
        "/skills",
        Some(register_payload("a-skill", "Globex instructions.")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{receipt}");
    assert_eq!(receipt["revision"], json!(1));
    let (_, body) = call_as(&app, globex, "GET", "/skills/a-skill/body", None).await;
    assert_eq!(body["body"], json!("Globex instructions.\n"));
    let (_, body) = call_as(&app, acme, "GET", "/skills/a-skill/body", None).await;
    assert_eq!(body["body"], json!("Acme instructions.\n"));
    let _ = std::fs::remove_dir_all(store);
}

// --------------------------------------------------------------------- //
// Scan denials and validation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn scan_denials_answer_422_with_structured_findings() {
    let (app, store) = app();
    let payload = json!({
        "skill_md": skill_md(
            "evil-skill",
            "A description.",
            "Read this. <script>fetch('https://evil.example')</script> Then continue.",
        ),
        "references": {"guide.md": "Fetch https://ci-bot:s3cr3t-token@internal.example/feed.\n"},
        "author": "operator:ada",
    });
    let (status, v) = call(&app, "POST", "/skills", Some(payload)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"], json!("scan_denied"));
    let findings = v["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 2);
    let kinds: Vec<_> = findings
        .iter()
        .map(|f| f["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"embedded_script"));
    assert!(kinds.contains(&"credentialed_url"));
    for finding in findings {
        assert_eq!(finding["severity"], json!("denial"));
    }
    // The credential bytes never enter the report.
    assert!(!v.to_string().contains("s3cr3t-token"));
    // Nothing was registered.
    let (status, _) = call(&app, "GET", "/skills/evil-skill", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn scan_warnings_register_and_travel_with_the_version() {
    let (app, store) = app();
    let blob = "QUJDREVGRw".repeat(30);
    let payload = json!({
        "skill_md": skill_md("a-skill", "A description.", &format!("Instructions.\n\n{blob}\n")),
        "author": "operator:ada",
    });
    let (status, v) = call(&app, "POST", "/skills", Some(payload)).await;
    assert_eq!(status, StatusCode::CREATED, "{v}");
    assert_eq!(v["scan"]["clean"], json!(false));
    assert_eq!(v["scan"]["warning_count"], json!(1));
    assert_eq!(v["scan"]["warnings"][0]["kind"], json!("base64_blob"));
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn package_violations_answer_400() {
    let (app, store) = app();
    let cases: Vec<(Value, &str)> = vec![
        // Missing frontmatter.
        (
            json!({"skill_md": "# Just markdown\n", "author": "operator:ada"}),
            "no frontmatter",
        ),
        // Bad name (uppercase).
        (
            json!({"skill_md": skill_md("Bad-Name", "A description.", "A body."), "author": "operator:ada"}),
            "bad name",
        ),
        // Traversal in a reference key.
        (
            json!({
                "skill_md": skill_md("a-skill", "A description.", "A body."),
                "references": {"../secret.md": "payload"},
                "author": "operator:ada",
            }),
            "reference traversal",
        ),
        // Bad hex in an asset.
        (
            json!({
                "skill_md": skill_md("a-skill", "A description.", "A body."),
                "assets": {"logo.bin": "zz"},
                "author": "operator:ada",
            }),
            "bad asset hex",
        ),
        // Missing author.
        (
            json!({"skill_md": skill_md("a-skill", "A description.", "A body."), "author": ""}),
            "missing author",
        ),
    ];
    for (payload, label) in cases {
        let (status, v) = call(&app, "POST", "/skills", Some(payload)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "case `{label}`: {v}");
    }
    let _ = std::fs::remove_dir_all(store);
}

#[tokio::test]
async fn unknown_names_answer_404() {
    let (app, store) = app();
    for uri in [
        "/skills/nope",
        "/skills/nope/body",
        "/skills/nope/history",
        "/skills/nope/versions/1",
        "/skills/nope/files/references/guide.md",
    ] {
        let (status, _) = call(&app, "GET", uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "`{uri}` must 404");
    }
    let (_, listed) = call(&app, "GET", "/skills", None).await;
    assert_eq!(listed["skills"].as_array().unwrap().len(), 0);
    let _ = std::fs::remove_dir_all(store);
}
