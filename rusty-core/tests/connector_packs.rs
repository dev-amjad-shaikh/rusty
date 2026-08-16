//! Service pack tests: every built-in pack constructs and validates,
//! derives an effect-honest catalog, and produces the wire shape the real
//! service's documentation describes — against a scripted transport,
//! never the network.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use rusty_agent_runtime::connector::packs;
use rusty_agent_runtime::connector::{
    ConnectorManifest, CredentialHandle, HttpApiProvider, HttpApiRequest, HttpApiTransport,
    HttpMethod, HttpResponse, ProviderKind,
};
use rusty_agent_runtime::error::Result;
use rusty_agent_runtime::record::Effect;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A scripted transport: replies are queued, requests are captured.
#[derive(Debug, Default)]
struct FakeTransport {
    replies: Mutex<VecDeque<HttpResponse>>,
    captured: Mutex<Vec<HttpApiRequest>>,
}

impl FakeTransport {
    fn push_json(&self, status: u16, body: Value) {
        self.push(HttpResponse {
            status,
            body: serde_json::to_vec(&body).expect("encode reply"),
        });
    }

    fn push(&self, reply: HttpResponse) {
        self.replies.lock().expect("replies lock").push_back(reply);
    }

    fn captured(&self) -> Vec<HttpApiRequest> {
        self.captured.lock().expect("captured lock").clone()
    }
}

#[async_trait]
impl HttpApiTransport for FakeTransport {
    async fn send(&self, request: HttpApiRequest) -> Result<HttpResponse> {
        self.captured.lock().expect("captured lock").push(request);
        self.replies
            .lock()
            .expect("replies lock")
            .pop_front()
            .ok_or_else(|| {
                rusty_agent_runtime::error::RustyError::Tool(
                    "fake transport: no scripted reply".to_owned(),
                )
            })
    }
}

fn handle(slot: &str, secret: &str) -> CredentialHandle {
    CredentialHandle::new("acme", slot, secret).expect("valid handle")
}

fn header<'a>(request: &'a HttpApiRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn request_body(request: &HttpApiRequest) -> Value {
    serde_json::from_slice(&request.body).expect("body is JSON")
}

fn provider(manifest: &ConnectorManifest) -> HttpApiProvider {
    HttpApiProvider::from_manifest(manifest).expect("provider from manifest")
}

/// The derived catalog as `name → wire effect` pairs, sorted by name.
fn catalog_effects(manifest: &ConnectorManifest) -> Vec<(String, Effect)> {
    provider(manifest)
        .catalog(&manifest.id)
        .expect("catalog")
        .into_iter()
        .map(|capability| (capability.name, capability.effect))
        .collect()
}

fn http_api_spec(manifest: &ConnectorManifest) -> &rusty_agent_runtime::connector::HttpApiSpec {
    let ProviderKind::HttpApi(spec) = &manifest.provider else {
        panic!("pack manifests are http-api")
    };
    spec
}

/// Execute one operation against a scripted reply and return the projected
/// result plus the captured request.
async fn call(
    manifest: &ConnectorManifest,
    credentials: &[CredentialHandle],
    operation: &str,
    args: Value,
    reply: Value,
) -> (Value, HttpApiRequest) {
    let transport = FakeTransport::default();
    transport.push_json(200, reply);
    let result = provider(manifest)
        .execute(&transport, credentials, "inst-000001", operation, &args)
        .await
        .expect("execute");
    let captured = transport.captured();
    assert_eq!(captured.len(), 1);
    (result, captured.into_iter().next().expect("one request"))
}

// ---------------------------------------------------------------------------
// Cross-pack construction
// ---------------------------------------------------------------------------

#[test]
fn all_packs_construct_and_verify() {
    let manifests = vec![
        packs::servicenow("acme").expect("servicenow"),
        packs::gmail().expect("gmail"),
        packs::slack().expect("slack"),
        packs::linear().expect("linear"),
        packs::notion().expect("notion"),
        packs::google_calendar().expect("google calendar"),
    ];
    let ids: Vec<&str> = manifests.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "servicenow",
            "gmail",
            "slack",
            "linear",
            "notion",
            "google-calendar"
        ]
    );
    for manifest in &manifests {
        assert_eq!(manifest.version, "1");
        assert!(manifest.verify_hash(), "{} hash must verify", manifest.id);
        // One derived tool per declared operation, namespaced by id.
        let catalog = provider(manifest).catalog(&manifest.id).expect("catalog");
        let operations = &http_api_spec(manifest).operations;
        assert_eq!(catalog.len(), operations.len(), "{}", manifest.id);
        for capability in &catalog {
            assert!(
                capability.name.starts_with(&format!("{}/", manifest.id)),
                "{}",
                capability.name
            );
        }
    }
}

// ---------------------------------------------------------------------------
// ServiceNow
// ---------------------------------------------------------------------------

fn servicenow() -> ConnectorManifest {
    packs::servicenow("acme").expect("servicenow manifest")
}

fn servicenow_credentials() -> Vec<CredentialHandle> {
    vec![handle("username", "ada"), handle("password", "p@ss")]
}

#[test]
fn servicenow_rejects_bad_instance_labels() {
    let long = "a".repeat(64);
    for bad in [
        "",                 // empty
        "-acme",            // leading hyphen
        "acme-",            // trailing hyphen
        "ACME",             // uppercase
        "ac_me",            // underscore is not a label character
        "ac me",            // whitespace
        "acme.example.com", // dots: the constructor takes the label only
        long.as_str(),      // over 63 bytes
    ] {
        let error = packs::servicenow(bad)
            .expect_err("bad instance must be rejected")
            .to_string();
        assert!(error.contains("DNS label"), "{bad}: {error}");
    }
    for good in ["a", "acme", "ac-me", "acme2", "a1-b2"] {
        packs::servicenow(good).expect("valid instance");
    }
}

#[test]
fn servicenow_catalog_is_effect_honest() {
    let manifest = servicenow();
    assert_eq!(
        catalog_effects(&manifest),
        vec![
            ("servicenow/create-record".to_owned(), Effect::Compensatable),
            ("servicenow/delete-record".to_owned(), Effect::NonIdempotent),
            ("servicenow/get-record".to_owned(), Effect::ReadOnly),
            ("servicenow/list-records".to_owned(), Effect::ReadOnly),
            ("servicenow/update-record".to_owned(), Effect::Compensatable),
        ]
    );
    // No parameterless read exists in this set, so no health check.
    assert_eq!(http_api_spec(&manifest).health_check, None);
}

#[tokio::test]
async fn servicenow_wire_shapes() {
    let manifest = servicenow();
    let credentials = servicenow_credentials();

    // list-records: GET with sysparm query params, `/result` projection,
    // Basic auth (base64("ada:p@ss") per RFC 4648).
    let (result, request) = call(
        &manifest,
        &credentials,
        "list-records",
        json!({"table": "incident", "sysparm_query": "active=true", "sysparm_limit": 10}),
        json!({"result": [{"sys_id": "abc", "short_description": "VPN down"}]}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Get);
    assert_eq!(
        request.url,
        "https://acme.service-now.com/api/now/table/incident?sysparm_limit=10&sysparm_query=active%3Dtrue"
    );
    assert_eq!(
        header(&request, "authorization"),
        Some("Basic YWRhOnBAc3M=")
    );
    assert!(request.body.is_empty());
    assert_eq!(
        result,
        json!([{"sys_id": "abc", "short_description": "VPN down"}]),
        "the /result envelope is projected away"
    );

    // get-record: both path parameters render.
    let (_, request) = call(
        &manifest,
        &credentials,
        "get-record",
        json!({"table": "incident", "sys_id": "abc 123"}),
        json!({"result": {"sys_id": "abc 123"}}),
    )
    .await;
    assert_eq!(
        request.url,
        "https://acme.service-now.com/api/now/table/incident/abc%20123"
    );

    // create-record: the path parameter stays out of the JSON body.
    let (result, request) = call(
        &manifest,
        &credentials,
        "create-record",
        json!({"table": "incident", "short_description": "VPN down", "urgency": "1"}),
        json!({"result": {"sys_id": "new1"}}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Post);
    assert_eq!(
        request.url,
        "https://acme.service-now.com/api/now/table/incident"
    );
    assert_eq!(header(&request, "content-type"), Some("application/json"));
    assert_eq!(
        request_body(&request),
        json!({"short_description": "VPN down", "urgency": "1"})
    );
    assert_eq!(result, json!({"sys_id": "new1"}));

    // update-record: PATCH with the same field set.
    let (_, request) = call(
        &manifest,
        &credentials,
        "update-record",
        json!({"table": "incident", "sys_id": "abc", "state": "2", "work_notes": "investigating"}),
        json!({"result": {"sys_id": "abc"}}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Patch);
    assert_eq!(
        request.url,
        "https://acme.service-now.com/api/now/table/incident/abc"
    );
    assert_eq!(
        request_body(&request),
        json!({"state": "2", "work_notes": "investigating"})
    );
}

#[tokio::test]
async fn servicenow_delete_record_is_bodyless() {
    let manifest = servicenow();
    let transport = FakeTransport::default();
    // The Table API answers a delete with an empty 204.
    transport.push(HttpResponse {
        status: 204,
        body: Vec::new(),
    });
    provider(&manifest)
        .execute(
            &transport,
            &servicenow_credentials(),
            "inst-000001",
            "delete-record",
            &json!({"table": "incident", "sys_id": "abc"}),
        )
        .await
        .expect("execute");
    let request = &transport.captured()[0];
    assert_eq!(request.method, HttpMethod::Delete);
    assert_eq!(
        request.url,
        "https://acme.service-now.com/api/now/table/incident/abc"
    );
    assert!(request.body.is_empty());
    assert!(header(request, "content-type").is_none());
}

#[tokio::test]
async fn servicenow_rejects_bad_arguments() {
    let manifest = servicenow();
    let credentials = servicenow_credentials();
    let transport = FakeTransport::default();

    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "list-records",
            &json!({"table": "incident", "sysparm_qurey": "typo"}),
        )
        .await
        .expect_err("unexpected argument must fail")
        .to_string();
    assert!(
        error.contains("unexpected argument `sysparm_qurey`"),
        "{error}"
    );

    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "get-record",
            &json!({"table": "incident"}),
        )
        .await
        .expect_err("missing required argument must fail")
        .to_string();
    assert!(
        error.contains("missing required argument `sys_id`"),
        "{error}"
    );
    assert!(transport.captured().is_empty(), "no request went out");
}

// ---------------------------------------------------------------------------
// Gmail
// ---------------------------------------------------------------------------

fn gmail() -> ConnectorManifest {
    packs::gmail().expect("gmail manifest")
}

fn gmail_credentials() -> Vec<CredentialHandle> {
    vec![handle("access_token", "ya29.test-token")]
}

#[test]
fn gmail_catalog_and_health_check() {
    let manifest = gmail();
    assert_eq!(
        catalog_effects(&manifest),
        vec![
            ("gmail/get-message".to_owned(), Effect::ReadOnly),
            ("gmail/get-profile".to_owned(), Effect::ReadOnly),
            ("gmail/list-messages".to_owned(), Effect::ReadOnly),
            ("gmail/modify-message".to_owned(), Effect::Compensatable),
            ("gmail/send-message".to_owned(), Effect::NonIdempotent),
        ]
    );
    assert_eq!(
        http_api_spec(&manifest).health_check.as_deref(),
        Some("get-profile")
    );
}

#[tokio::test]
async fn gmail_wire_shapes() {
    let manifest = gmail();
    let credentials = gmail_credentials();

    // get-profile: parameterless, bearer auth, response passed through
    // (no projection declared).
    let (result, request) = call(
        &manifest,
        &credentials,
        "get-profile",
        json!({}),
        json!({"emailAddress": "ada@example.com", "messagesTotal": 42, "threadsTotal": 7}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Get);
    assert_eq!(
        request.url,
        "https://gmail.googleapis.com/gmail/v1/users/me/profile"
    );
    assert_eq!(
        header(&request, "authorization"),
        Some("Bearer ya29.test-token")
    );
    assert_eq!(result["emailAddress"], json!("ada@example.com"));

    // list-messages: query params route to the query string under their
    // real (camelCase) wire names.
    let (_, request) = call(
        &manifest,
        &credentials,
        "list-messages",
        json!({"q": "is:unread", "maxResults": 5}),
        json!({"messages": [{"id": "m1", "threadId": "t1"}], "nextPageToken": "tok"}),
    )
    .await;
    assert_eq!(
        request.url,
        "https://gmail.googleapis.com/gmail/v1/users/me/messages?maxResults=5&q=is%3Aunread"
    );

    // get-message: path parameter plus the `format` query.
    let (_, request) = call(
        &manifest,
        &credentials,
        "get-message",
        json!({"message_id": "m1", "format": "full"}),
        json!({"id": "m1", "threadId": "t1", "labelIds": ["INBOX"]}),
    )
    .await;
    assert_eq!(
        request.url,
        "https://gmail.googleapis.com/gmail/v1/users/me/messages/m1?format=full"
    );

    // modify-message: label id arrays in the body, id in the path.
    let (_, request) = call(
        &manifest,
        &credentials,
        "modify-message",
        json!({"message_id": "m1", "addLabelIds": ["STARRED"], "removeLabelIds": ["INBOX"]}),
        json!({"id": "m1", "threadId": "t1", "labelIds": ["STARRED"]}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Post);
    assert_eq!(
        request.url,
        "https://gmail.googleapis.com/gmail/v1/users/me/messages/m1/modify"
    );
    assert_eq!(
        request_body(&request),
        json!({"addLabelIds": ["STARRED"], "removeLabelIds": ["INBOX"]})
    );

    // send-message: the raw RFC 2822 payload, and nothing else.
    let (_, request) = call(
        &manifest,
        &credentials,
        "send-message",
        json!({"raw": "RnJvbTogYWRhQGV4YW1wbGUuY29t"}),
        json!({"id": "m2", "threadId": "t2", "labelIds": ["SENT"]}),
    )
    .await;
    assert_eq!(
        request.url,
        "https://gmail.googleapis.com/gmail/v1/users/me/messages/send"
    );
    assert_eq!(
        request_body(&request),
        json!({"raw": "RnJvbTogYWRhQGV4YW1wbGUuY29t"})
    );
}

#[tokio::test]
async fn gmail_rejects_bad_arguments() {
    let manifest = gmail();
    let credentials = gmail_credentials();
    let transport = FakeTransport::default();

    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "send-message",
            &json!({"raw": "abc", "to": "bob@example.com"}),
        )
        .await
        .expect_err("unexpected argument must fail")
        .to_string();
    assert!(error.contains("unexpected argument `to`"), "{error}");

    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "get-message",
            &json!({}),
        )
        .await
        .expect_err("missing required argument must fail")
        .to_string();
    assert!(
        error.contains("missing required argument `message_id`"),
        "{error}"
    );
    assert!(transport.captured().is_empty());
}

// ---------------------------------------------------------------------------
// Slack
// ---------------------------------------------------------------------------

fn slack() -> ConnectorManifest {
    packs::slack().expect("slack manifest")
}

fn slack_credentials() -> Vec<CredentialHandle> {
    vec![handle("bot_token", "xoxb-test-token")]
}

#[test]
fn slack_catalog_and_health_check() {
    let manifest = slack();
    assert_eq!(
        catalog_effects(&manifest),
        vec![
            ("slack/add-reaction".to_owned(), Effect::Compensatable),
            ("slack/channel-history".to_owned(), Effect::ReadOnly),
            ("slack/list-channels".to_owned(), Effect::ReadOnly),
            ("slack/list-users".to_owned(), Effect::ReadOnly),
            ("slack/post-message".to_owned(), Effect::Compensatable),
        ]
    );
    assert_eq!(
        http_api_spec(&manifest).health_check.as_deref(),
        Some("list-channels")
    );
}

#[tokio::test]
async fn slack_wire_shapes() {
    let manifest = slack();
    let credentials = slack_credentials();

    // list-channels: all query params optional; Slack's `{"ok": true, …}`
    // envelope passes through (no projection declared).
    let (result, request) = call(
        &manifest,
        &credentials,
        "list-channels",
        json!({"limit": 10, "types": "public_channel"}),
        json!({"ok": true, "channels": [{"id": "C0123", "name": "general"}]}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Get);
    assert_eq!(
        request.url,
        "https://slack.com/api/conversations.list?limit=10&types=public_channel"
    );
    assert_eq!(
        header(&request, "authorization"),
        Some("Bearer xoxb-test-token")
    );
    assert_eq!(result["ok"], json!(true));

    // channel-history: `channel` is required.
    let (_, request) = call(
        &manifest,
        &credentials,
        "channel-history",
        json!({"channel": "C0123", "oldest": "1700000000.000000"}),
        json!({"ok": true, "messages": [{"ts": "1700000001.000100", "text": "hi"}]}),
    )
    .await;
    assert_eq!(
        request.url,
        "https://slack.com/api/conversations.history?channel=C0123&oldest=1700000000.000000"
    );

    // list-users: pagination params only.
    let (_, request) = call(
        &manifest,
        &credentials,
        "list-users",
        json!({"cursor": "dXNlcjpVMDYxTkZU", "limit": 100}),
        json!({"ok": true, "members": [{"id": "U0123", "name": "ada"}]}),
    )
    .await;
    assert_eq!(
        request.url,
        "https://slack.com/api/users.list?cursor=dXNlcjpVMDYxTkZU&limit=100"
    );

    // post-message: channel and text required, thread_ts optional.
    let (_, request) = call(
        &manifest,
        &credentials,
        "post-message",
        json!({"channel": "C0123", "text": "deploy done", "thread_ts": "1700000001.000100"}),
        json!({"ok": true, "ts": "1700000002.000200"}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Post);
    assert_eq!(request.url, "https://slack.com/api/chat.postMessage");
    assert_eq!(header(&request, "content-type"), Some("application/json"));
    assert_eq!(
        request_body(&request),
        json!({"channel": "C0123", "text": "deploy done", "thread_ts": "1700000001.000100"})
    );

    // add-reaction: all three fields required.
    let (_, request) = call(
        &manifest,
        &credentials,
        "add-reaction",
        json!({"channel": "C0123", "timestamp": "1700000001.000100", "name": "rocket"}),
        json!({"ok": true}),
    )
    .await;
    assert_eq!(request.url, "https://slack.com/api/reactions.add");
    assert_eq!(
        request_body(&request),
        json!({"channel": "C0123", "timestamp": "1700000001.000100", "name": "rocket"})
    );
}

#[tokio::test]
async fn slack_rejects_bad_arguments() {
    let manifest = slack();
    let credentials = slack_credentials();
    let transport = FakeTransport::default();

    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "channel-history",
            &json!({"limit": 5}),
        )
        .await
        .expect_err("missing required argument must fail")
        .to_string();
    assert!(
        error.contains("missing required argument `channel`"),
        "{error}"
    );

    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "post-message",
            &json!({"channel": "C0123", "text": "hi", "blocks": []}),
        )
        .await
        .expect_err("unexpected argument must fail")
        .to_string();
    assert!(error.contains("unexpected argument `blocks`"), "{error}");
    assert!(transport.captured().is_empty());
}

// ---------------------------------------------------------------------------
// Linear
// ---------------------------------------------------------------------------

fn linear() -> ConnectorManifest {
    packs::linear().expect("linear manifest")
}

fn linear_credentials() -> Vec<CredentialHandle> {
    vec![handle("api_key", "lin_api_test")]
}

#[test]
fn linear_reads_are_post_and_not_declared_read_only() {
    let manifest = linear();
    assert_eq!(
        catalog_effects(&manifest),
        vec![
            ("linear/create-issue".to_owned(), Effect::Compensatable),
            ("linear/get-issue".to_owned(), Effect::Compensatable),
            ("linear/list-issues".to_owned(), Effect::Compensatable),
            ("linear/list-teams".to_owned(), Effect::Compensatable),
            ("linear/update-issue".to_owned(), Effect::Compensatable),
        ]
    );
    // The taxonomy honesty: Linear serves reads over POST /graphql, and
    // `read_only` is GET-only, so nothing here may claim it.
    for operation in &http_api_spec(&manifest).operations {
        assert_eq!(operation.method, HttpMethod::Post);
        assert_ne!(operation.effect.wire_effect(), Effect::ReadOnly);
    }
    // POST-only means no health check.
    assert_eq!(http_api_spec(&manifest).health_check, None);
}

#[tokio::test]
async fn linear_graphql_wire_shapes() {
    let manifest = linear();
    let credentials = linear_credentials();

    // list-teams: a parameterless query — braces collapse, no holes.
    let (result, request) = call(
        &manifest,
        &credentials,
        "list-teams",
        json!({}),
        json!({"data": {"teams": {"nodes": [{"id": "t1", "name": "Eng", "key": "ENG"}]}}}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Post);
    assert_eq!(request.url, "https://api.linear.app/graphql");
    assert_eq!(
        header(&request, "authorization"),
        Some("Bearer lin_api_test")
    );
    assert_eq!(header(&request, "content-type"), Some("application/json"));
    assert_eq!(
        request_body(&request),
        json!({"query": "query { teams { nodes { id name key } } }"})
    );
    assert_eq!(
        result,
        json!({"teams": {"nodes": [{"id": "t1", "name": "Eng", "key": "ENG"}]}}),
        "the /data envelope is projected away"
    );

    // list-issues: the integer argument interpolates bare.
    let (_, request) = call(
        &manifest,
        &credentials,
        "list-issues",
        json!({"first": 25}),
        json!({"data": {"issues": {"nodes": []}}}),
    )
    .await;
    assert_eq!(
        request_body(&request),
        json!({"query": "query { issues(first: 25) { nodes { id identifier title } } }"})
    );

    // get-issue: the string argument arrives quoted and escaped.
    let (_, request) = call(
        &manifest,
        &credentials,
        "get-issue",
        json!({"id": "ENG-123 \"quoted\""}),
        json!({"data": {"issue": {"id": "i1"}}}),
    )
    .await;
    assert_eq!(
        request_body(&request),
        json!({"query": "query { issue(id: \"ENG-123 \\\"quoted\\\"\") { id identifier title description } }"})
    );

    // create-issue: the mutation nests input braces correctly.
    let (_, request) = call(
        &manifest,
        &credentials,
        "create-issue",
        json!({"title": "Fix login", "description": "redirect loop", "team_id": "t1"}),
        json!({"data": {"issueCreate": {"success": true, "issue": {"id": "i2", "identifier": "ENG-124"}}}}),
    )
    .await;
    assert_eq!(
        request_body(&request),
        json!({"query": "mutation { issueCreate(input: { title: \"Fix login\", description: \"redirect loop\", teamId: \"t1\" }) { success issue { id identifier } } }"})
    );

    // update-issue: id outside the input object, fields inside.
    let (_, request) = call(
        &manifest,
        &credentials,
        "update-issue",
        json!({"id": "i2", "title": "Fix login redirect", "description": "", "state_id": "s1"}),
        json!({"data": {"issueUpdate": {"success": true}}}),
    )
    .await;
    assert_eq!(
        request_body(&request),
        json!({"query": "mutation { issueUpdate(id: \"i2\", input: { title: \"Fix login redirect\", description: \"\", stateId: \"s1\" }) { success } }"})
    );
}

#[tokio::test]
async fn linear_rejects_bad_arguments() {
    let manifest = linear();
    let credentials = linear_credentials();
    let transport = FakeTransport::default();

    // `first` is required even though Linear would default it: the
    // template interpolates every declared parameter.
    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "list-issues",
            &json!({}),
        )
        .await
        .expect_err("missing required argument must fail")
        .to_string();
    assert!(
        error.contains("missing required argument `first`"),
        "{error}"
    );

    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "get-issue",
            &json!({"id": "i1", "extra": true}),
        )
        .await
        .expect_err("unexpected argument must fail")
        .to_string();
    assert!(error.contains("unexpected argument `extra`"), "{error}");
    assert!(transport.captured().is_empty());
}

// ---------------------------------------------------------------------------
// Notion
// ---------------------------------------------------------------------------

fn notion() -> ConnectorManifest {
    packs::notion().expect("notion manifest")
}

fn notion_credentials() -> Vec<CredentialHandle> {
    vec![handle("integration_token", "ntn_test_token")]
}

#[test]
fn notion_catalog_and_version_header() {
    let manifest = notion();
    assert_eq!(
        catalog_effects(&manifest),
        vec![
            ("notion/create-page".to_owned(), Effect::Compensatable),
            ("notion/get-database".to_owned(), Effect::ReadOnly),
            ("notion/get-page".to_owned(), Effect::ReadOnly),
            ("notion/list-block-children".to_owned(), Effect::ReadOnly),
            ("notion/query-database".to_owned(), Effect::Compensatable),
            ("notion/search".to_owned(), Effect::Compensatable),
            ("notion/update-page".to_owned(), Effect::Compensatable),
        ]
    );
    // The POST-shaped reads write nothing but cannot claim `read_only`.
    for name in ["search", "query-database"] {
        let operation = http_api_spec(&manifest)
            .operations
            .iter()
            .find(|op| op.name == name)
            .expect(name);
        assert_eq!(operation.method, HttpMethod::Post);
        assert_ne!(operation.effect.wire_effect(), Effect::ReadOnly);
    }
    let spec = http_api_spec(&manifest);
    assert_eq!(
        spec.default_headers,
        vec![("Notion-Version".to_owned(), "2022-06-28".to_owned())]
    );
    assert_eq!(spec.health_check, None);
}

#[tokio::test]
async fn notion_wire_shapes() {
    let manifest = notion();
    let credentials = notion_credentials();

    // search: POST with the query DSL in the body; the version header
    // rides every call.
    let (_, request) = call(
        &manifest,
        &credentials,
        "search",
        json!({"query": "roadmap", "page_size": 10}),
        json!({"object": "list", "results": [{"object": "page", "id": "p1"}]}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Post);
    assert_eq!(request.url, "https://api.notion.com/v1/search");
    assert_eq!(header(&request, "notion-version"), Some("2022-06-28"));
    assert_eq!(
        header(&request, "authorization"),
        Some("Bearer ntn_test_token")
    );
    assert_eq!(
        request_body(&request),
        json!({"query": "roadmap", "page_size": 10})
    );

    // get-page and get-database: plain path-parameter GETs.
    let (_, request) = call(
        &manifest,
        &credentials,
        "get-page",
        json!({"page_id": "p1"}),
        json!({"object": "page", "id": "p1"}),
    )
    .await;
    assert_eq!(request.url, "https://api.notion.com/v1/pages/p1");
    let (_, request) = call(
        &manifest,
        &credentials,
        "get-database",
        json!({"database_id": "d1"}),
        json!({"object": "database", "id": "d1"}),
    )
    .await;
    assert_eq!(request.url, "https://api.notion.com/v1/databases/d1");

    // query-database: POST body beside the path parameter.
    let (_, request) = call(
        &manifest,
        &credentials,
        "query-database",
        json!({"database_id": "d1", "filter": {"property": "Done", "checkbox": {"equals": false}}}),
        json!({"object": "list", "results": []}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Post);
    assert_eq!(request.url, "https://api.notion.com/v1/databases/d1/query");
    assert_eq!(
        request_body(&request),
        json!({"filter": {"property": "Done", "checkbox": {"equals": false}}})
    );

    // list-block-children: pagination in the query string.
    let (_, request) = call(
        &manifest,
        &credentials,
        "list-block-children",
        json!({"block_id": "b1", "page_size": 50}),
        json!({"object": "list", "results": [{"object": "block", "id": "b2"}]}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Get);
    assert_eq!(
        request.url,
        "https://api.notion.com/v1/blocks/b1/children?page_size=50"
    );

    // create-page: structured parent and properties pass through as JSON.
    let parent = json!({"database_id": "d1"});
    let properties = json!({"Name": {"title": [{"text": {"content": "Spec"}}]}});
    let (_, request) = call(
        &manifest,
        &credentials,
        "create-page",
        json!({"parent": parent, "properties": properties}),
        json!({"object": "page", "id": "p2"}),
    )
    .await;
    assert_eq!(request.url, "https://api.notion.com/v1/pages");
    assert_eq!(
        request_body(&request),
        json!({"parent": {"database_id": "d1"}, "properties": {"Name": {"title": [{"text": {"content": "Spec"}}]}}})
    );

    // update-page: PATCH with properties and the archive flag.
    let (_, request) = call(
        &manifest,
        &credentials,
        "update-page",
        json!({"page_id": "p2", "archived": true}),
        json!({"object": "page", "id": "p2", "archived": true}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Patch);
    assert_eq!(request.url, "https://api.notion.com/v1/pages/p2");
    assert_eq!(request_body(&request), json!({"archived": true}));
}

#[tokio::test]
async fn notion_rejects_bad_arguments() {
    let manifest = notion();
    let credentials = notion_credentials();
    let transport = FakeTransport::default();

    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "query-database",
            &json!({"page_size": 10}),
        )
        .await
        .expect_err("missing required argument must fail")
        .to_string();
    assert!(
        error.contains("missing required argument `database_id`"),
        "{error}"
    );

    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "get-page",
            &json!({"page_id": "p1", "filter_properties": "x"}),
        )
        .await
        .expect_err("unexpected argument must fail")
        .to_string();
    assert!(
        error.contains("unexpected argument `filter_properties`"),
        "{error}"
    );
    assert!(transport.captured().is_empty());
}

// ---------------------------------------------------------------------------
// Google Calendar
// ---------------------------------------------------------------------------

fn google_calendar() -> ConnectorManifest {
    packs::google_calendar().expect("google calendar manifest")
}

fn google_calendar_credentials() -> Vec<CredentialHandle> {
    vec![handle("access_token", "ya29.calendar-token")]
}

#[test]
fn google_calendar_catalog_and_health_check() {
    let manifest = google_calendar();
    assert_eq!(
        catalog_effects(&manifest),
        vec![
            (
                "google-calendar/create-event".to_owned(),
                Effect::Compensatable
            ),
            (
                "google-calendar/delete-event".to_owned(),
                Effect::NonIdempotent
            ),
            ("google-calendar/get-event".to_owned(), Effect::ReadOnly),
            (
                "google-calendar/list-calendars".to_owned(),
                Effect::ReadOnly
            ),
            ("google-calendar/list-events".to_owned(), Effect::ReadOnly),
            (
                "google-calendar/update-event".to_owned(),
                Effect::Compensatable
            ),
        ]
    );
    assert_eq!(
        http_api_spec(&manifest).health_check.as_deref(),
        Some("list-calendars")
    );
}

#[tokio::test]
async fn google_calendar_wire_shapes() {
    let manifest = google_calendar();
    let credentials = google_calendar_credentials();

    // list-calendars: parameterless; the `calendar#…` envelope passes
    // through (no projection declared).
    let (result, request) = call(
        &manifest,
        &credentials,
        "list-calendars",
        json!({}),
        json!({"kind": "calendar#calendarList", "items": [{"id": "primary", "summary": "Ada"}]}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Get);
    assert_eq!(
        request.url,
        "https://www.googleapis.com/calendar/v3/users/me/calendarList"
    );
    assert_eq!(
        header(&request, "authorization"),
        Some("Bearer ya29.calendar-token")
    );
    assert_eq!(result["kind"], json!("calendar#calendarList"));

    // list-events: RFC 3339 timestamps percent-encode their colons, and
    // the query params carry their real camelCase wire names.
    let (_, request) = call(
        &manifest,
        &credentials,
        "list-events",
        json!({
            "calendar_id": "primary",
            "timeMin": "2024-06-01T00:00:00Z",
            "timeMax": "2024-06-30T23:59:59Z",
            "singleEvents": true,
        }),
        json!({"kind": "calendar#events", "items": [{"id": "e1", "summary": "standup"}]}),
    )
    .await;
    assert_eq!(
        request.url,
        "https://www.googleapis.com/calendar/v3/calendars/primary/events?singleEvents=true&timeMax=2024-06-30T23%3A59%3A59Z&timeMin=2024-06-01T00%3A00%3A00Z"
    );

    // get-event: two path parameters.
    let (_, request) = call(
        &manifest,
        &credentials,
        "get-event",
        json!({"calendar_id": "primary", "event_id": "e1"}),
        json!({"kind": "calendar#event", "id": "e1"}),
    )
    .await;
    assert_eq!(
        request.url,
        "https://www.googleapis.com/calendar/v3/calendars/primary/events/e1"
    );

    // create-event: start/end objects and the attendees array pass
    // through as structured JSON.
    let (_, request) = call(
        &manifest,
        &credentials,
        "create-event",
        json!({
            "calendar_id": "primary",
            "summary": "standup",
            "start": {"dateTime": "2024-06-03T09:30:00Z", "timeZone": "UTC"},
            "end": {"dateTime": "2024-06-03T09:45:00Z", "timeZone": "UTC"},
            "attendees": [{"email": "bob@example.com"}],
        }),
        json!({"kind": "calendar#event", "id": "e2", "summary": "standup"}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Post);
    assert_eq!(
        request.url,
        "https://www.googleapis.com/calendar/v3/calendars/primary/events"
    );
    assert_eq!(
        request_body(&request),
        json!({
            "summary": "standup",
            "start": {"dateTime": "2024-06-03T09:30:00Z", "timeZone": "UTC"},
            "end": {"dateTime": "2024-06-03T09:45:00Z", "timeZone": "UTC"},
            "attendees": [{"email": "bob@example.com"}],
        })
    );

    // update-event: PATCH, path parameters out of the body.
    let (_, request) = call(
        &manifest,
        &credentials,
        "update-event",
        json!({"calendar_id": "primary", "event_id": "e2", "location": "room 4"}),
        json!({"kind": "calendar#event", "id": "e2", "location": "room 4"}),
    )
    .await;
    assert_eq!(request.method, HttpMethod::Patch);
    assert_eq!(
        request.url,
        "https://www.googleapis.com/calendar/v3/calendars/primary/events/e2"
    );
    assert_eq!(request_body(&request), json!({"location": "room 4"}));
}

#[tokio::test]
async fn google_calendar_delete_event_is_bodyless() {
    let manifest = google_calendar();
    let transport = FakeTransport::default();
    // A delete answers 204 with an empty body.
    transport.push(HttpResponse {
        status: 204,
        body: Vec::new(),
    });
    provider(&manifest)
        .execute(
            &transport,
            &google_calendar_credentials(),
            "inst-000001",
            "delete-event",
            &json!({"calendar_id": "primary", "event_id": "e2"}),
        )
        .await
        .expect("execute");
    let request = &transport.captured()[0];
    assert_eq!(request.method, HttpMethod::Delete);
    assert_eq!(
        request.url,
        "https://www.googleapis.com/calendar/v3/calendars/primary/events/e2"
    );
    assert!(request.body.is_empty());
}

#[tokio::test]
async fn google_calendar_rejects_bad_arguments() {
    let manifest = google_calendar();
    let credentials = google_calendar_credentials();
    let transport = FakeTransport::default();

    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "list-events",
            &json!({"timeMin": "2024-06-01T00:00:00Z"}),
        )
        .await
        .expect_err("missing required argument must fail")
        .to_string();
    assert!(
        error.contains("missing required argument `calendar_id`"),
        "{error}"
    );

    let error = provider(&manifest)
        .execute(
            &transport,
            &credentials,
            "inst-000001",
            "create-event",
            &json!({
                "calendar_id": "primary",
                "start": {"dateTime": "2024-06-03T09:30:00Z"},
                "end": {"dateTime": "2024-06-03T09:45:00Z"},
                "colorId": "7",
            }),
        )
        .await
        .expect_err("unexpected argument must fail")
        .to_string();
    assert!(error.contains("unexpected argument `colorId`"), "{error}");
    assert!(transport.captured().is_empty());
}
