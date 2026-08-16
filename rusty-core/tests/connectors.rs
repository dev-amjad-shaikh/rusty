//! Connector plane tests: manifests, credentials, lifecycle, providers,
//! catalog generations, and the registry — one coherent suite over the
//! public `connector` module surface.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusty_agent_runtime::connector::{
    ConnectorInstance, ConnectorManifest, ConnectorProvider, ConnectorRegistry,
    ConnectorSearchTool, CredentialHandle, CredentialSlot, HttpRequest,
    HttpResponse, HttpSearchProvider, HttpSearchSpec, HttpTransport, InMemoryCredentialBroker,
    LifecycleState, McpSession, McpStdioProvider, McpStdioSpec, ProviderKind, ProviderSession,
    SearchAuth, SearchRequest, MAX_SEARCH_RESPONSE_BYTES,
};
use rusty_agent_runtime::error::Result;
use rusty_agent_runtime::mcp::McpClient;
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::{Tool, ToolCapability};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn mcp_manifest(id: &str) -> ConnectorManifest {
    ConnectorManifest::new(
        id,
        "1.0.0",
        "Test MCP",
        "A test MCP connector.",
        ProviderKind::McpStdio(McpStdioSpec {
            command: "test-server".to_owned(),
            args: vec!["--stdio".to_owned()],
            env_allowlist: vec!["PATH".to_owned()],
        }),
        vec!["mcp tools".to_owned()],
        vec![],
    )
    .expect("valid mcp manifest")
}

fn http_manifest(id: &str) -> ConnectorManifest {
    ConnectorManifest::new(
        id,
        "1.0.0",
        "Test Search",
        "A test search connector.",
        ProviderKind::HttpSearch(HttpSearchSpec {
            base_url: "https://search.example.com/query".to_owned(),
            auth: Some(SearchAuth {
                header: "x-api-key".to_owned(),
                credential_slot: "api_key".to_owned(),
            }),
        }),
        vec!["web search".to_owned()],
        vec![CredentialSlot {
            name: "api_key".to_owned(),
            description: "Search API key.".to_owned(),
        }],
    )
    .expect("valid http manifest")
}

fn capability(name: &str) -> ToolCapability {
    ToolCapability {
        name: name.to_owned(),
        description: format!("The {name} tool."),
        parameters_schema: json!({"type": "object"}),
        effect: Effect::NonIdempotent,
    }
}

/// A scripted MCP server over an in-memory duplex transport — the same
/// pattern `mcp.rs`'s own tests use, re-implemented here because the
/// framing helpers are crate-private. The tool list is swappable and
/// `tools/list` can be flipped to fail, so one server exercises refresh,
/// degradation, and recovery.
fn fake_mcp_server(
    tools: Arc<Mutex<Vec<Value>>>,
    fail_list: Arc<AtomicBool>,
) -> (McpClient, JoinHandle<()>) {
    let (client_stream, server_stream) = tokio::io::duplex(64 * 1024);
    let handle = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(server_stream);
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await.expect("server read");
            if n == 0 {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Value = serde_json::from_str(trimmed).expect("request json");
            let id = msg.get("id").cloned();
            let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
            let response = match method {
                "notifications/initialized" => None,
                "initialize" => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fake-mcp", "version": "0.1.0"},
                    }
                })),
                "tools/list" if fail_list.load(Ordering::Relaxed) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32603, "message": "tools backend unavailable"}
                })),
                "tools/list" => {
                    let tools = tools.lock().expect("tools lock").clone();
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"tools": tools}
                    }))
                }
                _ => id.map(|i| {
                    json!({"jsonrpc": "2.0", "id": i, "error": {"code": -32601, "message": "method not found"}})
                }),
            };
            if let Some(resp) = response {
                let mut bytes = serde_json::to_vec(&resp).expect("encode response");
                bytes.push(b'\n');
                write.write_all(&bytes).await.expect("server write");
            }
        }
    });
    let (read, write) = tokio::io::split(client_stream);
    (McpClient::connect(read, write), handle)
}

/// A provider whose sessions ride scripted duplex servers. Each `connect`
/// spawns a *fresh* server/client pair — the real provider spawns a fresh
/// process per connection, and a shut-down client (like a dead child) is
/// never reused.
struct DuplexMcpProvider {
    tools: Arc<Mutex<Vec<Value>>>,
    fail_list: Arc<AtomicBool>,
}

impl std::fmt::Debug for DuplexMcpProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuplexMcpProvider").finish()
    }
}

#[async_trait]
impl ConnectorProvider for DuplexMcpProvider {
    async fn connect(
        &self,
        manifest: &ConnectorManifest,
        _credentials: &[CredentialHandle],
    ) -> Result<Box<dyn ProviderSession>> {
        let (client, _server) =
            fake_mcp_server(Arc::clone(&self.tools), Arc::clone(&self.fail_list));
        Ok(Box::new(McpSession::from_client(
            manifest.id.clone(),
            client,
        )))
    }
}

/// A scripted HTTP transport: replies are queued, requests are captured.
#[derive(Debug, Default)]
struct FakeTransport {
    replies: Mutex<VecDeque<HttpResponse>>,
    captured: Mutex<Vec<HttpRequest>>,
}

impl FakeTransport {
    fn push(&self, reply: HttpResponse) {
        self.replies.lock().expect("replies lock").push_back(reply);
    }

    fn push_json(&self, status: u16, body: Value) {
        self.push(HttpResponse {
            status,
            body: serde_json::to_vec(&body).expect("encode reply"),
        });
    }

    fn captured(&self) -> Vec<HttpRequest> {
        self.captured.lock().expect("captured lock").clone()
    }
}

#[async_trait]
impl HttpTransport for FakeTransport {
    async fn post(&self, request: HttpRequest) -> Result<HttpResponse> {
        self.captured
            .lock()
            .expect("captured lock")
            .push(request);
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

fn search_provider() -> HttpSearchProvider {
    HttpSearchProvider::new(
        "https://search.example.com/query",
        Some(SearchAuth {
            header: "x-api-key".to_owned(),
            credential_slot: "api_key".to_owned(),
        }),
    )
    .expect("valid provider")
}

// ---------------------------------------------------------------------------
// Manifest validation matrix
// ---------------------------------------------------------------------------

#[test]
fn valid_manifests_construct_and_verify() {
    let mcp = mcp_manifest("test-conn");
    assert!(mcp.verify_hash());
    assert_eq!(mcp.hash.len(), 64);

    let http = http_manifest("web-search");
    assert!(http.verify_hash());
}

#[test]
fn manifest_rejects_bad_ids() {
    for bad in [
        "",
        "Upper",
        "under_score",
        "-leading",
        "trailing-",
        "double--dash",
        "has space",
        &"x".repeat(65),
    ] {
        let result = ConnectorManifest::new(
            bad,
            "1.0.0",
            "Name",
            "Description.",
            ProviderKind::McpStdio(McpStdioSpec {
                command: "server".to_owned(),
                args: vec![],
                env_allowlist: vec![],
            }),
            vec![],
            vec![],
        );
        assert!(result.is_err(), "id `{bad}` must be rejected");
    }
}

#[test]
fn manifest_rejects_bad_versions_and_text_fields() {
    let spec = || {
        ProviderKind::McpStdio(McpStdioSpec {
            command: "server".to_owned(),
            args: vec![],
            env_allowlist: vec![],
        })
    };
    // Empty and over-long versions.
    for bad in ["", &"1".repeat(33), "1.0 beta"] {
        assert!(ConnectorManifest::new("ok-id", bad, "Name", "Desc.", spec(), vec![], vec![])
            .is_err());
    }
    // Empty, untrimmed, and oversized display names / descriptions.
    assert!(
        ConnectorManifest::new("ok-id", "1.0.0", "", "Desc.", spec(), vec![], vec![]).is_err()
    );
    assert!(
        ConnectorManifest::new("ok-id", "1.0.0", " padded", "Desc.", spec(), vec![], vec![])
            .is_err()
    );
    assert!(ConnectorManifest::new(
        "ok-id",
        "1.0.0",
        "Name",
        "d".repeat(4097),
        spec(),
        vec![],
        vec![]
    )
    .is_err());
}

#[test]
fn manifest_rejects_bad_mcp_stdio_specs() {
    let manifest_with = |spec: McpStdioSpec| {
        ConnectorManifest::new(
            "ok-id",
            "1.0.0",
            "Name",
            "Desc.",
            ProviderKind::McpStdio(spec),
            vec![],
            vec![],
        )
    };
    // Missing command.
    assert!(manifest_with(McpStdioSpec {
        command: String::new(),
        args: vec![],
        env_allowlist: vec![],
    })
    .is_err());
    // Control character in an argument.
    assert!(manifest_with(McpStdioSpec {
        command: "server".to_owned(),
        args: vec!["bad\narg".to_owned()],
        env_allowlist: vec![],
    })
    .is_err());
    // Too many arguments.
    assert!(manifest_with(McpStdioSpec {
        command: "server".to_owned(),
        args: vec!["a".to_owned(); 65],
        env_allowlist: vec![],
    })
    .is_err());
    // Invalid env names: leading digit, dash.
    for bad in ["1PATH", "HAS-DASH"] {
        assert!(manifest_with(McpStdioSpec {
            command: "server".to_owned(),
            args: vec![],
            env_allowlist: vec![bad.to_owned()],
        })
        .is_err());
    }
    // Over-long allowlist.
    assert!(manifest_with(McpStdioSpec {
        command: "server".to_owned(),
        args: vec![],
        env_allowlist: (0..33).map(|i| format!("VAR_{i}")).collect(),
    })
    .is_err());
}

#[test]
fn manifest_rejects_bad_http_search_specs() {
    let manifest_with = |spec: HttpSearchSpec, slots: Vec<CredentialSlot>| {
        ConnectorManifest::new(
            "ok-id",
            "1.0.0",
            "Name",
            "Desc.",
            ProviderKind::HttpSearch(spec),
            vec![],
            slots,
        )
    };
    let api_key = || {
        vec![CredentialSlot {
            name: "api_key".to_owned(),
            description: String::new(),
        }]
    };
    // Plaintext transport is rejected.
    assert!(manifest_with(
        HttpSearchSpec {
            base_url: "http://search.example.com".to_owned(),
            auth: None,
        },
        vec![],
    )
    .is_err());
    // Missing scheme entirely.
    assert!(manifest_with(
        HttpSearchSpec {
            base_url: "search.example.com".to_owned(),
            auth: None,
        },
        vec![],
    )
    .is_err());
    // Whitespace in the URL.
    assert!(manifest_with(
        HttpSearchSpec {
            base_url: "https://search.example.com/has space".to_owned(),
            auth: None,
        },
        vec![],
    )
    .is_err());
    // Invalid header name.
    assert!(manifest_with(
        HttpSearchSpec {
            base_url: "https://search.example.com".to_owned(),
            auth: Some(SearchAuth {
                header: "Bad Header".to_owned(),
                credential_slot: "api_key".to_owned(),
            }),
        },
        api_key(),
    )
    .is_err());
    // Auth referencing an undeclared slot.
    assert!(manifest_with(
        HttpSearchSpec {
            base_url: "https://search.example.com".to_owned(),
            auth: Some(SearchAuth {
                header: "x-api-key".to_owned(),
                credential_slot: "api_key".to_owned(),
            }),
        },
        vec![],
    )
    .is_err());
}

#[test]
fn manifest_rejects_capability_and_slot_overflow() {
    let spec = || {
        ProviderKind::McpStdio(McpStdioSpec {
            command: "server".to_owned(),
            args: vec![],
            env_allowlist: vec![],
        })
    };
    // Too many (distinct) capability entries — duplicates are deduped at
    // construction, so the overflow needs distinct strings.
    assert!(ConnectorManifest::new(
        "ok-id",
        "1.0.0",
        "Name",
        "Desc.",
        spec(),
        (0..65).map(|i| format!("cap-{i}")).collect(),
        vec![],
    )
    .is_err());
    // Duplicate slot names.
    assert!(ConnectorManifest::new(
        "ok-id",
        "1.0.0",
        "Name",
        "Desc.",
        spec(),
        vec![],
        vec![
            CredentialSlot {
                name: "api_key".to_owned(),
                description: String::new(),
            },
            CredentialSlot {
                name: "api_key".to_owned(),
                description: "again".to_owned(),
            },
        ],
    )
    .is_err());
    // Bad slot name.
    assert!(ConnectorManifest::new(
        "ok-id",
        "1.0.0",
        "Name",
        "Desc.",
        spec(),
        vec![],
        vec![CredentialSlot {
            name: "ApiKey".to_owned(),
            description: String::new(),
        }],
    )
    .is_err());
}

#[test]
fn unknown_provider_kind_fails_deserialization() {
    let value = json!({
        "id": "ok-id",
        "version": "1.0.0",
        "display_name": "Name",
        "description": "Desc.",
        "provider": {"kind": "carrier-pigeon"},
        "capabilities": [],
        "credential_slots": [],
        "hash": "whatever",
    });
    assert!(serde_json::from_value::<ConnectorManifest>(value).is_err());
}

// ---------------------------------------------------------------------------
// Content hash
// ---------------------------------------------------------------------------

#[test]
fn content_hash_is_stable_order_insensitive_and_content_sensitive() {
    let a = mcp_manifest("test-conn");
    let b = mcp_manifest("test-conn");
    assert_eq!(a.hash, b.hash);

    // Capability and env-allowlist order do not change identity.
    let ordered = ConnectorManifest::new(
        "test-conn",
        "1.0.0",
        "Test MCP",
        "A test MCP connector.",
        ProviderKind::McpStdio(McpStdioSpec {
            command: "test-server".to_owned(),
            args: vec!["--stdio".to_owned()],
            env_allowlist: vec!["PATH".to_owned()],
        }),
        vec!["mcp tools".to_owned()],
        vec![],
    )
    .expect("manifest");
    assert_eq!(ordered.hash, a.hash);

    let shuffled = ConnectorManifest::new(
        "test-conn",
        "1.0.0",
        "Test MCP",
        "A test MCP connector.",
        ProviderKind::McpStdio(McpStdioSpec {
            command: "test-server".to_owned(),
            args: vec!["--stdio".to_owned()],
            env_allowlist: vec!["Z_LAST".to_owned(), "A_FIRST".to_owned()],
        }),
        vec!["beta cap".to_owned(), "alpha cap".to_owned()],
        vec![],
    )
    .expect("manifest");
    let reordered = ConnectorManifest::new(
        "test-conn",
        "1.0.0",
        "Test MCP",
        "A test MCP connector.",
        ProviderKind::McpStdio(McpStdioSpec {
            command: "test-server".to_owned(),
            args: vec!["--stdio".to_owned()],
            env_allowlist: vec!["A_FIRST".to_owned(), "Z_LAST".to_owned()],
        }),
        vec!["alpha cap".to_owned(), "beta cap".to_owned()],
        vec![],
    )
    .expect("manifest");
    assert_eq!(shuffled.hash, reordered.hash);

    // Content changes change the hash.
    let mut changed = mcp_manifest("test-conn");
    assert_eq!(changed.hash, a.hash);
    changed = ConnectorManifest::new(
        "test-conn",
        "1.0.0",
        "Test MCP",
        "A different description.",
        changed.provider.clone(),
        changed.capabilities.clone(),
        changed.credential_slots.clone(),
    )
    .expect("manifest");
    assert_ne!(changed.hash, a.hash);
}

#[test]
fn registration_is_idempotent_by_hash_and_rejects_tampering() {
    let provider = || Arc::new(McpStdioProvider) as Arc<dyn ConnectorProvider>;
    let mut registry = ConnectorRegistry::new();
    let first = registry
        .register_manifest(mcp_manifest("test-conn"), provider())
        .expect("register");
    let second = registry
        .register_manifest(mcp_manifest("test-conn"), provider())
        .expect("re-register");
    assert_eq!(first, second);
    assert_eq!(registry.list_manifests().len(), 1);

    // A manifest edited after construction no longer matches its hash.
    let mut tampered = mcp_manifest("test-conn");
    tampered.description = "Tampered.".to_owned();
    assert!(!tampered.verify_hash());
    let err = registry
        .register_manifest(tampered, provider())
        .expect_err("tampered manifest must be rejected");
    assert!(err.to_string().contains("hash does not match"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

#[test]
fn credential_handle_never_leaks_secret_bytes() {
    let secret = "super-secret-value";
    let handle = CredentialHandle::new("acme", "api_key", secret).expect("handle");

    let debug = format!("{handle:?}");
    assert!(!debug.contains(secret), "Debug leaked: {debug}");
    assert!(debug.contains("[redacted]"));

    let serialized = serde_json::to_string(&handle).expect("serialize");
    assert!(!serialized.contains(secret), "Serialize leaked: {serialized}");
    assert!(serialized.contains("[redacted]"));
    assert!(serialized.contains("acme"));
    assert!(serialized.contains("api_key"));

    // The broker's Debug lists slots, never values.
    let mut broker = InMemoryCredentialBroker::new();
    broker.insert("acme", "api_key", secret);
    let debug = format!("{broker:?}");
    assert!(!debug.contains(secret), "broker Debug leaked: {debug}");

    // A search tool holding the handle stays redacted too.
    let transport = Arc::new(FakeTransport::default());
    let tool = ConnectorSearchTool::new("web-search", search_provider(), transport, Some(handle));
    let debug = format!("{tool:?}");
    assert!(!debug.contains(secret), "tool Debug leaked: {debug}");
}

#[test]
fn credential_handles_reject_empty_and_oversized_secrets() {
    assert!(CredentialHandle::new("acme", "api_key", "").is_err());
    assert!(CredentialHandle::new("acme", "api_key", "x".repeat(4097)).is_err());
    assert!(CredentialHandle::new("", "api_key", "fine").is_err());
}

#[test]
fn missing_credential_lands_instance_in_failed_with_reason() {
    let mut registry = ConnectorRegistry::new();
    let hash = registry
        .register_manifest_with_default(http_manifest("web-search"))
        .expect("register");
    let broker = InMemoryCredentialBroker::new();
    let id = registry
        .instantiate(&hash, "acme", &broker)
        .expect("instantiate succeeds; the instance carries the failure");

    match registry.instance(&id).expect("instance").state() {
        LifecycleState::Failed { reason } => {
            assert!(reason.contains("api_key"), "got: {reason}");
            assert!(reason.contains("acme"), "got: {reason}");
        }
        other => panic!("expected failed, got {other:?}"),
    }

    // With the slot filled the same manifest instantiates pending.
    let mut broker = InMemoryCredentialBroker::new();
    broker.insert("acme", "api_key", "sekret");
    let id = registry
        .instantiate(&hash, "acme", &broker)
        .expect("instantiate");
    assert!(matches!(
        registry.instance(&id).expect("instance").state(),
        LifecycleState::Pending
    ));

    // Unknown manifest hashes error the call itself.
    assert!(registry.instantiate("no-such-hash", "acme", &broker).is_err());
}

// ---------------------------------------------------------------------------
// MCP provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mcp_catalog_is_namespaced_sorted_and_validated() {
    let tools = Arc::new(Mutex::new(vec![
        json!({"name": "zeta", "description": "Last.", "inputSchema": {"type": "object"}}),
        json!({
            "name": "alpha",
            "description": "First.",
            "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}
        }),
    ]));
    let fail = Arc::new(AtomicBool::new(false));
    let (client, _server) = fake_mcp_server(tools, fail);

    let catalog = McpStdioProvider::catalog_from_client("test-conn", &client)
        .await
        .expect("catalog");
    assert_eq!(
        catalog.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["test-conn/alpha", "test-conn/zeta"]
    );
    assert_eq!(catalog[0].effect, Effect::NonIdempotent);
    assert!(catalog[0].parameters_schema["properties"]["q"].is_object());
    client.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn mcp_catalog_fails_closed_on_invalid_server_tools() {
    // Empty description.
    let tools = Arc::new(Mutex::new(vec![
        json!({"name": "nodesc", "description": "", "inputSchema": {"type": "object"}}),
    ]));
    let (client, _server) = fake_mcp_server(tools, Arc::new(AtomicBool::new(false)));
    let err = McpStdioProvider::catalog_from_client("test-conn", &client)
        .await
        .expect_err("empty description must fail derivation");
    assert!(err.to_string().contains("description"), "got: {err}");
    client.shutdown().await.expect("shutdown");

    // A name that maps outside the catalog charset.
    let tools = Arc::new(Mutex::new(vec![
        json!({"name": "has space", "description": "Valid.", "inputSchema": {"type": "object"}}),
    ]));
    let (client, _server) = fake_mcp_server(tools, Arc::new(AtomicBool::new(false)));
    let err = McpStdioProvider::catalog_from_client("test-conn", &client)
        .await
        .expect_err("invalid name must fail derivation");
    assert!(err.to_string().contains("invalid catalog name"), "got: {err}");
    client.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn registry_connects_mcp_instance_to_healthy_catalog() {
    let tools = Arc::new(Mutex::new(vec![
        json!({"name": "echo", "description": "Echoes.", "inputSchema": {"type": "object"}}),
    ]));
    let fail_list = Arc::new(AtomicBool::new(false));

    let mut registry = ConnectorRegistry::new();
    let hash = registry
        .register_manifest(
            mcp_manifest("test-conn"),
            Arc::new(DuplexMcpProvider {
                tools: Arc::clone(&tools),
                fail_list: Arc::clone(&fail_list),
            }),
        )
        .expect("register");
    let broker = InMemoryCredentialBroker::new();
    let id = registry.instantiate(&hash, "acme", &broker).expect("instantiate");
    registry.connect(&id, 1_000).await.expect("connect");

    let instance = registry.instance(&id).expect("instance");
    assert!(matches!(instance.state(), LifecycleState::Healthy));
    assert_eq!(instance.last_health_check_ms(), Some(1_000));
    let catalog = registry.catalog(&id).expect("catalog");
    assert_eq!(catalog.generation, 1);
    assert_eq!(catalog.tools[0].name, "test-conn/echo");
}

#[tokio::test]
async fn mcp_spawn_failure_lands_instance_in_failed() {
    let manifest = ConnectorManifest::new(
        "broken-conn",
        "1.0.0",
        "Broken MCP",
        "Spawns a command that does not exist.",
        ProviderKind::McpStdio(McpStdioSpec {
            command: "rusty-no-such-mcp-server-zzz".to_owned(),
            args: vec![],
            env_allowlist: vec![],
        }),
        vec![],
        vec![],
    )
    .expect("manifest");

    let mut registry = ConnectorRegistry::new();
    let hash = registry
        .register_manifest_with_default(manifest)
        .expect("register");
    let broker = InMemoryCredentialBroker::new();
    let id = registry.instantiate(&hash, "acme", &broker).expect("instantiate");
    registry.connect(&id, 1_000).await.expect("connect returns Ok");

    match registry.instance(&id).expect("instance").state() {
        LifecycleState::Failed { reason } => {
            assert!(reason.contains("failed to spawn"), "got: {reason}");
        }
        other => panic!("expected failed, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// HTTP search provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_search_returns_ranked_hits_and_sends_auth_header() {
    let transport = Arc::new(FakeTransport::default());
    transport.push_json(
        200,
        json!({"results": [
            {"title": "One", "url": "https://a.example/1", "snippet": "first"},
            {"title": "Two", "url": "https://a.example/2", "snippet": "second"},
        ]}),
    );
    let handle = CredentialHandle::new("acme", "api_key", "sekret-token").expect("handle");
    let hits = search_provider()
        .search(
            transport.as_ref(),
            Some(&handle),
            &SearchRequest::new("rusty harness").expect("request"),
        )
        .await
        .expect("search");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].title, "One");
    assert_eq!(hits[1].url, "https://a.example/2");

    let captured = transport.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].url, "https://search.example.com/query");
    assert!(captured[0]
        .headers
        .contains(&("content-type".to_owned(), "application/json".to_owned())));
    assert!(captured[0]
        .headers
        .contains(&("x-api-key".to_owned(), "sekret-token".to_owned())));
    let body: Value = serde_json::from_slice(&captured[0].body).expect("request json");
    assert_eq!(body["query"], json!("rusty harness"));
}

#[tokio::test]
async fn http_search_enforces_byte_and_count_ceilings() {
    let provider = search_provider();
    let handle = CredentialHandle::new("acme", "api_key", "sekret-token").expect("handle");

    // Response body above the byte ceiling is rejected before parsing.
    let transport = Arc::new(FakeTransport::default());
    transport.push(HttpResponse {
        status: 200,
        body: serde_json::to_vec(&json!({
            "results": [],
            "pad": "x".repeat(MAX_SEARCH_RESPONSE_BYTES),
        }))
        .expect("encode"),
    });
    let err = provider
        .search(
            transport.as_ref(),
            Some(&handle),
            &SearchRequest::new("q").expect("request"),
        )
        .await
        .expect_err("oversized body must fail");
    assert!(err.to_string().contains("ceiling"), "got: {err}");

    // Over-long rankings truncate to the requested count.
    let transport = Arc::new(FakeTransport::default());
    let results: Vec<Value> = (0..25)
        .map(|i| {
            json!({"title": format!("t{i}"), "url": format!("https://a.example/{i}"), "snippet": "s"})
        })
        .collect();
    transport.push_json(200, json!({"results": results}));
    let hits = provider
        .search(
            transport.as_ref(),
            Some(&handle),
            &SearchRequest::new("q")
                .expect("request")
                .with_max_results(20)
                .expect("count"),
        )
        .await
        .expect("search");
    assert_eq!(hits.len(), 20);
    assert_eq!(hits[0].title, "t0");
    assert_eq!(hits[19].title, "t19");

    // One oversized field fails the whole call.
    let transport = Arc::new(FakeTransport::default());
    transport.push_json(
        200,
        json!({"results": [
            {"title": "x".repeat(513), "url": "https://a.example/1", "snippet": "s"},
        ]}),
    );
    let err = provider
        .search(
            transport.as_ref(),
            Some(&handle),
            &SearchRequest::new("q").expect("request"),
        )
        .await
        .expect_err("oversized field must fail");
    assert!(err.to_string().contains("title"), "got: {err}");
}

#[tokio::test]
async fn http_search_maps_failures_without_leaking() {
    let provider = search_provider();
    let handle = CredentialHandle::new("acme", "api_key", "sekret-token").expect("handle");

    // Auth failures are named as such; the body is never quoted.
    let transport = Arc::new(FakeTransport::default());
    transport.push(HttpResponse {
        status: 401,
        body: b"the token sekret-token is wrong".to_vec(),
    });
    let err = provider
        .search(
            transport.as_ref(),
            Some(&handle),
            &SearchRequest::new("q").expect("request"),
        )
        .await
        .expect_err("401 must fail");
    assert!(err.to_string().contains("rejected the credential"), "got: {err}");
    assert!(!err.to_string().contains("sekret-token"), "leaked: {err}");

    // Server failures carry the status.
    let transport = Arc::new(FakeTransport::default());
    transport.push_json(500, json!({"error": "boom"}));
    let err = provider
        .search(
            transport.as_ref(),
            Some(&handle),
            &SearchRequest::new("q").expect("request"),
        )
        .await
        .expect_err("500 must fail");
    assert!(err.to_string().contains("status 500"), "got: {err}");

    // Malformed shapes fail closed.
    let transport = Arc::new(FakeTransport::default());
    transport.push_json(200, json!({"not_results": []}));
    let err = provider
        .search(
            transport.as_ref(),
            Some(&handle),
            &SearchRequest::new("q").expect("request"),
        )
        .await
        .expect_err("missing results must fail");
    assert!(err.to_string().contains("results"), "got: {err}");

    // Missing and mismatched credentials are rejected before any request.
    let transport = Arc::new(FakeTransport::default());
    let err = provider
        .search(
            transport.as_ref(),
            None,
            &SearchRequest::new("q").expect("request"),
        )
        .await
        .expect_err("missing credential must fail");
    assert!(err.to_string().contains("api_key"), "got: {err}");
    let wrong_slot = CredentialHandle::new("acme", "other", "x").expect("handle");
    let err = provider
        .search(
            transport.as_ref(),
            Some(&wrong_slot),
            &SearchRequest::new("q").expect("request"),
        )
        .await
        .expect_err("slot mismatch must fail");
    assert!(err.to_string().contains("does not match"), "got: {err}");
    assert!(transport.captured().is_empty(), "no request may leave");
}

#[test]
fn search_request_validates_query_and_count() {
    assert!(SearchRequest::new("").is_err());
    assert!(SearchRequest::new("  padded").is_err());
    assert!(SearchRequest::new("x".repeat(1025)).is_err());
    assert!(SearchRequest::new("ok")
        .expect("request")
        .with_max_results(0)
        .is_err());
    assert!(SearchRequest::new("ok")
        .expect("request")
        .with_max_results(21)
        .is_err());
    let request = SearchRequest::new("ok").expect("request");
    assert_eq!(request.max_results(), 5);
}

#[tokio::test]
async fn search_tool_executes_through_the_provider_contract() {
    let transport = Arc::new(FakeTransport::default());
    transport.push_json(
        200,
        json!({"results": [
            {"title": "One", "url": "https://a.example/1", "snippet": "first"},
            {"title": "Two", "url": "https://a.example/2", "snippet": "second"},
        ]}),
    );
    let handle = CredentialHandle::new("acme", "api_key", "sekret-token").expect("handle");
    let tool = ConnectorSearchTool::new("web-search", search_provider(), transport, Some(handle));

    assert_eq!(tool.name(), "web-search/search");
    assert_eq!(tool.effect(), Effect::ReadOnly);
    assert!(tool.parameters_schema()["properties"]["query"].is_object());

    let out = tool
        .call(json!({"query": "harness", "max_results": 1}))
        .await
        .expect("call");
    assert_eq!(out["results"].as_array().expect("array").len(), 1);
    assert_eq!(out["results"][0]["title"], json!("One"));

    // Missing query is a tool error, not a panic.
    let err = tool.call(json!({})).await.expect_err("query required");
    assert!(err.to_string().contains("query"), "got: {err}");
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[test]
fn lifecycle_transitions_and_guards() {
    let mut instance =
        ConnectorInstance::new("inst-000001", "test-conn", "hash", "acme").expect("instance");
    assert!(matches!(instance.state(), LifecycleState::Pending));

    // Success out of `connecting` only.
    assert!(instance
        .record_connect_success(1_000, vec![capability("test-conn/a")])
        .is_err());

    // pending → connecting → healthy.
    instance.begin_connect().expect("begin");
    assert!(instance.begin_connect().is_err(), "already connecting");
    instance
        .record_connect_success(1_000, vec![capability("test-conn/a")])
        .expect("success");
    assert!(matches!(instance.state(), LifecycleState::Healthy));
    assert!(instance.begin_connect().is_err(), "healthy reconnects via sweep");

    // Health failures degrade at the threshold.
    assert!(!instance
        .record_health_failure("flaky", 2_000, 3)
        .expect("failure"));
    assert!(matches!(instance.state(), LifecycleState::Healthy));
    assert_eq!(instance.consecutive_failures(), 1);
    instance.record_health_failure("flaky", 2_100, 3).expect("failure");
    assert!(instance
        .record_health_failure("flaky", 2_200, 3)
        .expect("failure"), "third failure degrades");
    match instance.state() {
        LifecycleState::Degraded { reason } => assert_eq!(reason, "flaky"),
        other => panic!("expected degraded, got {other:?}"),
    }

    // Recovery resets the counter.
    instance
        .record_health_success(3_000, vec![capability("test-conn/a")])
        .expect("recovery");
    assert!(matches!(instance.state(), LifecycleState::Healthy));
    assert_eq!(instance.consecutive_failures(), 0);
    assert_eq!(instance.last_health_check_ms(), Some(3_000));

    // disabled rejects connections; enable returns to pending.
    instance.disable().expect("disable");
    assert!(instance.disable().is_err(), "already disabled");
    let err = instance.begin_connect().expect_err("disabled cannot connect");
    assert!(err.to_string().contains("disabled"), "got: {err}");
    assert!(instance.record_health_success(4_000, vec![]).is_err());
    instance.enable().expect("enable");
    assert!(matches!(instance.state(), LifecycleState::Pending));

    // failed carries the bounded reason and permits retry.
    instance.begin_connect().expect("begin");
    instance
        .record_connect_failure("spawn exploded")
        .expect("failure");
    match instance.state() {
        LifecycleState::Failed { reason } => assert_eq!(reason, "spawn exploded"),
        other => panic!("expected failed, got {other:?}"),
    }
    instance.begin_connect().expect("retry from failed");
    assert!(matches!(instance.state(), LifecycleState::Connecting));
}

#[test]
fn failure_reasons_are_bounded() {
    let mut instance =
        ConnectorInstance::new("inst-000001", "test-conn", "hash", "acme").expect("instance");
    instance.begin_connect().expect("begin");
    instance
        .record_connect_failure("x".repeat(2_000))
        .expect("failure");
    match instance.state() {
        LifecycleState::Failed { reason } => {
            assert!(reason.len() <= 512, "len: {}", reason.len());
            assert!(reason.ends_with("[truncated]"), "got: {reason}");
        }
        other => panic!("expected failed, got {other:?}"),
    }
}

#[test]
fn fail_pending_covers_pre_connect_failures_once() {
    let mut instance =
        ConnectorInstance::new("inst-000001", "test-conn", "hash", "acme").expect("instance");
    instance
        .fail_pending("credential slot `api_key` unresolved for tenant `acme`")
        .expect("fail");
    assert!(instance.fail_pending("again").is_err(), "not pending anymore");
    assert!(matches!(instance.state(), LifecycleState::Failed { .. }));

    // Tenant ids are validated at construction.
    assert!(ConnectorInstance::new("inst-000002", "c", "h", "").is_err());
    assert!(ConnectorInstance::new("inst-000002", "c", "h", "t".repeat(129)).is_err());
}

// ---------------------------------------------------------------------------
// Catalog generations
// ---------------------------------------------------------------------------

#[test]
fn catalog_generations_advance_only_when_bytes_change() {
    let mut instance =
        ConnectorInstance::new("inst-000001", "test-conn", "hash", "acme").expect("instance");
    assert!(instance.catalog_pin().is_none());

    instance.begin_connect().expect("begin");
    instance
        .record_connect_success(1_000, vec![capability("test-conn/a")])
        .expect("success");
    let pin = instance.catalog_pin().expect("pin");
    assert_eq!(pin.generation, 1);
    assert!(instance.verify_pin(&pin));
    let produced_at = instance.catalog().expect("catalog").produced_at_ms;

    // Same bytes: the generation — production time included — is untouched.
    let bumped = instance
        .record_health_success(2_000, vec![capability("test-conn/a")])
        .expect("refresh");
    assert!(!bumped);
    let catalog = instance.catalog().expect("catalog");
    assert_eq!(catalog.generation, 1);
    assert_eq!(catalog.produced_at_ms, produced_at);
    assert!(instance.verify_pin(&pin));

    // Changed bytes bump the generation and stale pins stop verifying.
    let bumped = instance
        .record_health_success(3_000, vec![capability("test-conn/b")])
        .expect("refresh");
    assert!(bumped);
    let catalog = instance.catalog().expect("catalog");
    assert_eq!(catalog.generation, 2);
    assert_ne!(catalog.hash, pin.hash);
    assert!(!instance.verify_pin(&pin), "stale pin must not verify");
    assert!(instance.verify_pin(&instance.catalog_pin().expect("new pin")));

    // A pin naming another instance never verifies.
    let mut foreign = pin.clone();
    foreign.instance_id = "inst-999999".to_owned();
    assert!(!instance.verify_pin(&foreign));
}

// ---------------------------------------------------------------------------
// Registry: sweep, listing, enable/disable
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_sweep_degrades_recovers_and_tracks_generations() {
    let tools = Arc::new(Mutex::new(vec![
        json!({"name": "echo", "description": "Echoes.", "inputSchema": {"type": "object"}}),
    ]));
    let fail_list = Arc::new(AtomicBool::new(false));

    let mut registry = ConnectorRegistry::new();
    let hash = registry
        .register_manifest(
            mcp_manifest("test-conn"),
            Arc::new(DuplexMcpProvider {
                tools: Arc::clone(&tools),
                fail_list: Arc::clone(&fail_list),
            }),
        )
        .expect("register");
    let broker = InMemoryCredentialBroker::new();
    let id = registry.instantiate(&hash, "acme", &broker).expect("instantiate");
    registry.connect(&id, 1_000).await.expect("connect");

    // Unchanged catalog: no new generation.
    let outcomes = registry.health_sweep(2_000).await;
    assert_eq!(outcomes.len(), 1);
    assert!(!outcomes[0].catalog_bumped);
    assert_eq!(outcomes[0].instance_id, id);
    assert_eq!(registry.catalog(&id).expect("catalog").generation, 1);

    // Changed catalog: one new generation.
    tools.lock().expect("tools lock").push(
        json!({"name": "extra", "description": "Added.", "inputSchema": {"type": "object"}}),
    );
    let outcomes = registry.health_sweep(2_500).await;
    assert!(outcomes[0].catalog_bumped);
    let catalog = registry.catalog(&id).expect("catalog");
    assert_eq!(catalog.generation, 2);
    assert_eq!(catalog.tools.len(), 2);

    // Three consecutive failures degrade (default threshold).
    fail_list.store(true, Ordering::Relaxed);
    for step in 1..=3u64 {
        let outcomes = registry.health_sweep(3_000 + step).await;
        let instance = registry.instance(&id).expect("instance");
        if step < 3 {
            assert!(matches!(instance.state(), LifecycleState::Healthy));
            assert_eq!(instance.consecutive_failures(), step as u32);
        } else {
            assert!(matches!(outcomes[0].current, LifecycleState::Degraded { .. }));
            match instance.state() {
                LifecycleState::Degraded { reason } => {
                    assert!(reason.contains("tools backend unavailable"), "got: {reason}");
                }
                other => panic!("expected degraded, got {other:?}"),
            }
        }
    }

    // Success recovers the instance and resets the counter.
    fail_list.store(false, Ordering::Relaxed);
    let outcomes = registry.health_sweep(4_000).await;
    assert!(matches!(outcomes[0].previous, LifecycleState::Degraded { .. }));
    assert!(matches!(outcomes[0].current, LifecycleState::Healthy));
    let instance = registry.instance(&id).expect("instance");
    assert_eq!(instance.consecutive_failures(), 0);
    assert_eq!(instance.last_health_check_ms(), Some(4_000));
}

#[tokio::test]
async fn registry_disable_shuts_down_and_enable_returns_to_pending() {
    let tools = Arc::new(Mutex::new(vec![
        json!({"name": "echo", "description": "Echoes.", "inputSchema": {"type": "object"}}),
    ]));
    let fail_list = Arc::new(AtomicBool::new(false));

    let mut registry = ConnectorRegistry::new();
    let hash = registry
        .register_manifest(
            mcp_manifest("test-conn"),
            Arc::new(DuplexMcpProvider {
                tools: Arc::clone(&tools),
                fail_list: Arc::clone(&fail_list),
            }),
        )
        .expect("register");
    let broker = InMemoryCredentialBroker::new();
    let id = registry.instantiate(&hash, "acme", &broker).expect("instantiate");
    registry.connect(&id, 1_000).await.expect("connect");

    registry.disable(&id).await.expect("disable");
    assert!(matches!(
        registry.instance(&id).expect("instance").state(),
        LifecycleState::Disabled
    ));
    // A disabled instance is out of the sweep and cannot connect.
    assert!(registry.health_sweep(2_000).await.is_empty());
    assert!(registry.connect(&id, 2_000).await.is_err());

    registry.enable(&id).expect("enable");
    assert!(matches!(
        registry.instance(&id).expect("instance").state(),
        LifecycleState::Pending
    ));
    registry.connect(&id, 3_000).await.expect("reconnect");
    assert!(matches!(
        registry.instance(&id).expect("instance").state(),
        LifecycleState::Healthy
    ));
}

#[test]
fn listings_are_deterministic() {
    let provider = || Arc::new(McpStdioProvider) as Arc<dyn ConnectorProvider>;
    let mut registry = ConnectorRegistry::new();
    let zeta = registry
        .register_manifest(mcp_manifest("zeta-conn"), provider())
        .expect("register");
    let alpha = registry
        .register_manifest(mcp_manifest("alpha-conn"), provider())
        .expect("register");

    let ids: Vec<&str> = registry
        .list_manifests()
        .iter()
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(ids, ["alpha-conn", "zeta-conn"]);
    assert!(registry.manifest(&zeta).is_some());
    assert!(registry.manifest("no-such-hash").is_none());

    let broker = InMemoryCredentialBroker::new();
    let first = registry.instantiate(&alpha, "beta", &broker).expect("instantiate");
    let second = registry.instantiate(&alpha, "acme", &broker).expect("instantiate");
    let third = registry.instantiate(&alpha, "acme", &broker).expect("instantiate");
    assert_eq!(first, "inst-000001");
    assert_eq!(second, "inst-000002");
    assert_eq!(third, "inst-000003");

    let ordered: Vec<(&str, &str)> = registry
        .list_instances(None)
        .iter()
        .map(|i| (i.tenant_id.as_str(), i.instance_id.as_str()))
        .collect();
    assert_eq!(
        ordered,
        [
            ("acme", "inst-000002"),
            ("acme", "inst-000003"),
            ("beta", "inst-000001"),
        ]
    );

    let acme_only: Vec<&str> = registry
        .list_instances(Some("acme"))
        .iter()
        .map(|i| i.instance_id.as_str())
        .collect();
    assert_eq!(acme_only, ["inst-000002", "inst-000003"]);
}
