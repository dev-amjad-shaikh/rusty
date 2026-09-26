//! Credential/connection broker integration tests (R0.11 Extension
//! Plane, wave 3).
//!
//! Four test groups:
//!
//! - **Golden files** — the serialized shapes of the connection record,
//!   the stored (sealed) envelope, the handle claims, every denial
//!   reason, the six journaled payloads (wave 4 adds
//!   `connection_needs_reauth`), and the broker's eight additive
//!   `RunEventKind` wire names are pinned against checked-in JSON under
//!   `tests/golden/`. Any accidental contract drift fails here;
//!   `UPDATE_GOLDEN=1` blesses an intentional change, the
//!   `tests/registry.rs` discipline.
//! - **Redaction** — the secret-bearing types (`TokenMaterial`,
//!   `ResolvedCredential`, `CredentialHandle`) prove by assertion that
//!   their `Debug` cannot render credential bytes or signatures: the
//!   accidental-log-line failure mode the design calls out, closed by
//!   construction and pinned by test.
//! - **Tool mediation** — the wave's first exit criterion, in-process:
//!   a `CredentialTool` wrapped by a `CredentialMediator` and dispatched
//!   through an ordinary `ToolExecutor` authenticates a provider call
//!   through a handle, and the credential bytes are observed *only* at
//!   the host-side connector double — never in the tool's own code path
//!   (structural: `call_authenticated` receives `&CredentialHandle` and
//!   nothing else). Issuance reuse across the TTL, re-issue after
//!   expiry, scope-escalation refusal at issuance, and revocation
//!   failing closed at the *next* call against a scripted live-state
//!   broker.
//! - **Capsule mediation** (feature `wasm`) — the R0.9 secret-handle
//!   precedent extended: a manifest's `Secret` grants are brokered into
//!   issued handle tokens the guest receives under the reserved
//!   `secrets` input key (proven with an echo guest), an issuance
//!   denial refuses the invocation before guest code runs, and an input
//!   smuggling its own `secrets` key is refused at admission.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use rusty_agent_runtime::broker::{
    scopes_missing, BrokerDenial, BrokerDenialReason, ClassifiedFailure, ConnectionConsent,
    ConnectionHealth, ConnectionProvider, ConnectionReauthRequired, ConnectionRecord,
    ConnectionRefresh, ConnectionRevocation, ConnectionStatus, CredentialBroker, CredentialHandle,
    CredentialMediator, CredentialRequirement, CredentialTool, CredentialUse, HandleClaims,
    HandleIssuance, IssueRequest, ProbeLedger, ResolvedCredential, ScriptedProbeLedger,
    ScriptedSecretResolver, SealedCredential, SecretRef, SecretRefParseError, SecretResolver,
    StoredConnection, TokenMaterial, WireProbeOutcome, WireProbeRecord, CONNECTION_ID_PREFIX,
    HANDLE_ID_PREFIX, SEALED_FORMAT_VERSION,
};
use rusty_agent_runtime::durable::ErrorClass;
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::llm::ToolCall;
use rusty_agent_runtime::record::{Effect, RunEventKind};
use rusty_agent_runtime::tool::{Tool, ToolExecutor, ToolRegistry};

// ---------- golden-file machinery (the tests/registry.rs discipline) ----------

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Assert the pretty-printed serialization of `value` equals the golden
/// file's content exactly. `UPDATE_GOLDEN=1` rewrites the file instead —
/// the diff is then the contract change under review.
fn assert_golden(name: &str, value: &impl Serialize) {
    let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        rendered,
        expected,
        "contract drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff",
        path.display()
    );
}

// ---------- shared fixtures ----------

fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

fn connection_id() -> String {
    format!("{}{}", CONNECTION_ID_PREFIX, "a".repeat(32))
}

fn handle_id() -> String {
    format!("{}{}", HANDLE_ID_PREFIX, "b".repeat(32))
}

fn connection() -> ConnectionRecord {
    ConnectionRecord {
        connection_id: connection_id(),
        provider: ConnectionProvider::Oauth2AuthorizationCode,
        subject: Some("user-7".into()),
        scopes: BTreeSet::from(["drive.readonly".to_owned(), "drive.write".to_owned()]),
        status: ConnectionStatus::Active,
        health: ConnectionHealth {
            last_refresh_at: Some(ts(1_800_000_100_000)),
            last_failure: Some(ClassifiedFailure {
                class: ErrorClass::Transient,
                detail: "provider 503".into(),
                at: ts(1_800_000_050_000),
            }),
            consecutive_failures: 1,
        },
        created_at: ts(1_800_000_000_000),
        updated_at: ts(1_800_000_100_000),
    }
}

fn claims() -> HandleClaims {
    HandleClaims {
        handle_id: handle_id(),
        connection_id: connection_id(),
        tenant: "acme".into(),
        run_id: Some("run-9".into()),
        scopes: BTreeSet::from(["drive.readonly".to_owned()]),
        issued_at: ts(1_800_000_200_000),
        expires_at: ts(1_800_000_500_000),
    }
}

// ---------- golden pins ----------

#[test]
fn golden_connection_record_shape() {
    assert_golden("broker_connection.json", &connection());
}

#[test]
fn golden_stored_connection_shape() {
    // The store shape both backends persist: the record plus the sealed
    // envelope. Hex fields are short stand-ins here — their grammar is
    // the server's cryptography; the golden pins the *shape*.
    let stored = StoredConnection {
        record: connection(),
        credential: SealedCredential {
            format_version: SEALED_FORMAT_VERSION,
            key_id: "bmk-0123456789abcdef".into(),
            wrapped_data_key: "ab".repeat(48),
            wrap_nonce: "cd".repeat(24),
            nonce: "ef".repeat(24),
            ciphertext: "01".repeat(64),
            sealed_at: ts(1_800_000_100_000),
        },
    };
    assert_golden("broker_stored_connection.json", &stored);
}

#[test]
fn golden_handle_claims_shape() {
    assert_golden("broker_handle_claims.json", &claims());
}

#[test]
fn golden_denial_shapes() {
    // Every reason, one golden: the typed refusals an audit reads, each
    // naming the connection and grant — and structurally incapable of
    // carrying bytes.
    let denials = vec![
        BrokerDenial::unknown_handle("not a broker handle token".into()),
        BrokerDenial::handle_expired(&claims()),
        BrokerDenial::scope_not_granted(
            Some((&connection_id(), Some(&handle_id()), "acme")),
            vec!["drive.admin".to_owned()],
            "requested beyond the handle's narrowed set".into(),
        ),
        BrokerDenial::connection_revoked(&connection(), "acme", &handle_id()),
        BrokerDenial::connection_needs_reauth(&connection(), "acme", &handle_id()),
        BrokerDenial::unknown_connection(&claims()),
        BrokerDenial::unavailable("the store read failed".into()),
    ];
    assert_golden("broker_denial.json", &denials);
}

#[test]
fn golden_journaled_payload_shapes() {
    assert_golden(
        "broker_consent.json",
        &ConnectionConsent {
            connection_id: connection_id(),
            subject: Some("user-7".into()),
            scopes: BTreeSet::from(["drive.readonly".to_owned(), "drive.write".to_owned()]),
            recorded_at: ts(1_800_000_100_000),
        },
    );
    assert_golden(
        "broker_refresh.json",
        &ConnectionRefresh {
            connection_id: connection_id(),
            refreshed_at: ts(1_800_000_300_000),
            expires_at: Some(ts(1_800_003_600_000)),
        },
    );
    assert_golden(
        "broker_revocation.json",
        &ConnectionRevocation {
            connection_id: connection_id(),
            grant: BTreeSet::from(["drive.readonly".to_owned(), "drive.write".to_owned()]),
            revoked_at: ts(1_800_000_400_000),
            reason: Some("employee offboarded".into()),
        },
    );
    assert_golden("broker_issuance.json", &HandleIssuance { claims: claims() });
    assert_golden(
        "broker_reauth.json",
        // Wave 4's journaled transition: a refresh the provider classified
        // permanent flipped the connection to `needs_reauth`. The failure
        // and the grant travel typed; credential bytes never do.
        &ConnectionReauthRequired {
            connection_id: connection_id(),
            failure: ClassifiedFailure {
                class: ErrorClass::InvalidInput,
                detail: "provider refused the refresh token: invalid_grant".into(),
                at: ts(1_800_000_500_000),
            },
            grant: BTreeSet::from(["drive.readonly".to_owned(), "drive.write".to_owned()]),
            recorded_at: ts(1_800_000_500_000),
        },
    );
    assert_golden(
        "broker_use.json",
        &CredentialUse {
            handle_id: handle_id(),
            connection_id: connection_id(),
            tenant: "acme".into(),
            run_id: Some("run-9".into()),
            scopes_checked: BTreeSet::from(["drive.readonly".to_owned()]),
            used_at: ts(1_800_000_250_000),
        },
    );
}

#[test]
fn golden_broker_event_kinds_shape() {
    // The wave's seven additive RunEventKind wire names (the
    // `registry_event_kinds.json` discipline), plus wave 4's
    // `connection_needs_reauth` — the terminal OAuth refusal a refresh
    // classified permanent journals — appended last per the additive
    // evolution rule every variant since R0.6 followed.
    assert_golden(
        "broker_event_kinds.json",
        &vec![
            RunEventKind::ConnectionRegistered,
            RunEventKind::ConnectionConsented,
            RunEventKind::ConnectionRefreshed,
            RunEventKind::ConnectionRevoked,
            RunEventKind::CredentialHandleIssued,
            RunEventKind::CredentialUse,
            RunEventKind::CredentialDenied,
            RunEventKind::ConnectionNeedsReauth,
        ],
    );
}

// ---------- redaction ----------

#[test]
fn resolved_credential_debug_never_shows_bytes() {
    let resolved = ResolvedCredential {
        connection_id: connection_id(),
        handle_id: handle_id(),
        scopes: BTreeSet::from(["drive.readonly".to_owned()]),
        material: TokenMaterial {
            access_token: "sk-live-MARKER".into(),
            refresh_token: Some("rt-MARKER".into()),
            client_secret: None,
            client_id: None,
            username: None,
            password: None,
            token_url: None,
            expires_at: None,
        },
    };
    let rendered = format!("{resolved:?}");
    assert!(rendered.contains(&connection_id()));
    assert!(!rendered.contains("sk-live-MARKER"), "got: {rendered}");
    assert!(!rendered.contains("rt-MARKER"), "got: {rendered}");
    assert!(rendered.contains("[redacted]"));
}

// ---------- the scripted broker and the connector double ----------

/// The marker secret the whole suite hunts for: it must appear at the
/// connector double (the host-side boundary) and nowhere else.
const MARKER: &str = "sk-live-MARKER-9f2e";

/// A scripted in-memory broker: the core-side stand-in for the server's
/// broker. It enforces the same contract — issuance checks live state
/// and the consent ceiling, resolution verifies its own signatures,
/// checks expiry and scope coverage from the claims, then reads *live*
/// connection state — so the mediation tests exercise the real
/// fail-closed semantics, not a stub that always answers yes. Journaling
/// is the server broker's half (proven in `rusty-server/tests/broker.rs`).
#[derive(Debug)]
struct ScriptedBroker {
    /// Live connection state by id — tests mutate it to simulate
    /// revocation between calls.
    connections: Mutex<HashMap<String, ConnectionRecord>>,
    /// Every issuance, in order (the reuse assertions read it).
    issued: Mutex<Vec<HandleClaims>>,
    /// The credential resolution returns.
    material: TokenMaterial,
    /// Back-dating for minted claims (0 = now): negative values age
    /// the issued handle, so expiry and cache-turnover tests need no
    /// sleeping. The TTL stays 300 s, keeping the claims grammatical.
    issued_offset_ms: std::sync::atomic::AtomicI64,
}

impl ScriptedBroker {
    fn new(connections: Vec<ConnectionRecord>) -> Self {
        Self {
            connections: Mutex::new(
                connections
                    .into_iter()
                    .map(|record| (record.connection_id.clone(), record))
                    .collect(),
            ),
            issued: Mutex::new(Vec::new()),
            material: TokenMaterial {
                access_token: MARKER.into(),
                refresh_token: None,
                client_secret: None,
                client_id: None,
                username: None,
                password: None,
                token_url: None,
                expires_at: None,
            },
            issued_offset_ms: std::sync::atomic::AtomicI64::new(0),
        }
    }

    fn signature(claims: &HandleClaims) -> String {
        format!("scripted-signature-{}", claims.handle_id)
    }
}

#[async_trait::async_trait]
impl CredentialBroker for ScriptedBroker {
    async fn issue(&self, request: &IssueRequest) -> Result<CredentialHandle, BrokerDenial> {
        let mut connections = self.connections.lock().unwrap();
        let record = connections
            .get_mut(&request.requirement.connection_id)
            .ok_or_else(|| BrokerDenial {
                connection_id: Some(request.requirement.connection_id.clone()),
                handle_id: None,
                tenant: Some(request.tenant.clone()),
                reason: BrokerDenialReason::UnknownConnection,
                detail: "scripted broker: unknown connection".into(),
            })?;
        if record.status == ConnectionStatus::Revoked {
            return Err(BrokerDenial::connection_revoked(
                record,
                &request.tenant,
                "unissued",
            ));
        }
        let missing = scopes_missing(&record.scopes, &request.requirement.scopes);
        if !missing.is_empty() {
            return Err(BrokerDenial::scope_not_granted(
                Some((&record.connection_id, None, &request.tenant)),
                missing,
                "requested beyond the consented set".into(),
            ));
        }
        let now = Utc::now()
            + chrono::Duration::milliseconds(
                self.issued_offset_ms
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
        let claims = HandleClaims {
            handle_id: rusty_agent_runtime::broker::new_handle_id(),
            connection_id: record.connection_id.clone(),
            tenant: request.tenant.clone(),
            run_id: request.run_id.clone(),
            // An empty request narrows to nothing: the handle binds the
            // whole consent set (the contract's documented semantic).
            scopes: if request.requirement.scopes.is_empty() {
                record.scopes.clone()
            } else {
                request.requirement.scopes.clone()
            },
            issued_at: now,
            expires_at: now + chrono::Duration::milliseconds(300_000),
        };
        let handle = CredentialHandle::from_parts(claims.clone(), Self::signature(&claims));
        self.issued.lock().unwrap().push(claims);
        Ok(handle)
    }

    async fn resolve(
        &self,
        token: &str,
        scopes: &BTreeSet<String>,
    ) -> Result<ResolvedCredential, BrokerDenial> {
        let (claims, signature) = CredentialHandle::parse_token(token)?;
        if signature != Self::signature(&claims) {
            return Err(BrokerDenial::unknown_handle(
                "bad scripted signature".into(),
            ));
        }
        if claims.is_expired(Utc::now()) {
            return Err(BrokerDenial::handle_expired(&claims));
        }
        let missing = scopes_missing(&claims.scopes, scopes);
        if !missing.is_empty() {
            return Err(BrokerDenial::scope_not_granted(
                Some((
                    &claims.connection_id,
                    Some(&claims.handle_id),
                    &claims.tenant,
                )),
                missing,
                "requested beyond the handle's narrowed set".into(),
            ));
        }
        let connections = self.connections.lock().unwrap();
        let record = connections
            .get(&claims.connection_id)
            .ok_or_else(|| BrokerDenial::unknown_connection(&claims))?;
        match record.status {
            ConnectionStatus::Active => Ok(ResolvedCredential {
                connection_id: record.connection_id.clone(),
                handle_id: claims.handle_id.clone(),
                scopes: claims.scopes.clone(),
                material: self.material.clone(),
            }),
            ConnectionStatus::Revoked => Err(BrokerDenial::connection_revoked(
                record,
                &claims.tenant,
                &claims.handle_id,
            )),
            ConnectionStatus::NeedsReauth => Err(BrokerDenial::connection_needs_reauth(
                record,
                &claims.tenant,
                &claims.handle_id,
            )),
        }
    }
}

/// The host-side connector double: the HTTP/tool-call boundary. It is
/// the *only* component that ever holds the resolved credential, and it
/// records every injection so the tests can assert where the bytes went.
#[derive(Debug)]
struct ConnectorDouble {
    broker: Arc<ScriptedBroker>,
    /// Every credential injected into an outbound call, in order.
    injected: Mutex<Vec<TokenMaterial>>,
}

impl ConnectorDouble {
    /// Perform one authenticated provider call: resolve the handle and
    /// inject the credential. The answer carries no credential — tool
    /// code receives the provider's response, nothing else.
    async fn authenticated_fetch(
        &self,
        handle: &CredentialHandle,
        scopes: &BTreeSet<String>,
    ) -> RuntimeResult<Value> {
        let resolved = self
            .broker
            .resolve(&handle.token(), scopes)
            .await
            .map_err(|denial| {
                rusty_agent_runtime::error::RustyError::Tool(format!(
                    "connector resolution denied: {denial}"
                ))
            })?;
        self.injected
            .lock()
            .unwrap()
            .push(resolved.material.clone());
        Ok(json!({"status": 200, "body": "provider-response"}))
    }
}

/// The credential-requiring tool: declares its need per call and, given
/// the handle, calls *through the connector* — the only shape in which
/// the bytes stay out of tool code.
struct SearchTool {
    connector: Arc<ConnectorDouble>,
    connection_id: String,
    /// The scope this scripted tool asks for (tests escalate it).
    scope: String,
}

#[async_trait::async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "provider_search"
    }
    fn description(&self) -> &str {
        "Searches the provider."
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {"q": {"type": "string"}}})
    }
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }
    async fn call(&self, _args: Value) -> RuntimeResult<Value> {
        unreachable!("dispatch reaches call_authenticated through MediatedTool")
    }
}

#[async_trait::async_trait]
impl CredentialTool for SearchTool {
    fn credential_requirement(&self, _args: &Value) -> CredentialRequirement {
        CredentialRequirement {
            connection_id: self.connection_id.clone(),
            scopes: BTreeSet::from([self.scope.clone()]),
        }
    }
    async fn call_authenticated(
        &self,
        _args: Value,
        handle: &CredentialHandle,
    ) -> RuntimeResult<Value> {
        // The tool's whole credential contact: presenting the opaque
        // handle to the connector. No `TokenMaterial` exists anywhere in
        // this code path — the type system is the proof.
        self.connector
            .authenticated_fetch(handle, &BTreeSet::from([self.scope.clone()]))
            .await
    }
}

fn rig(
    scope: &str,
) -> (
    Arc<ScriptedBroker>,
    Arc<ConnectorDouble>,
    ToolExecutor,
    CredentialMediator,
) {
    let broker = Arc::new(ScriptedBroker::new(vec![connection()]));
    let connector = Arc::new(ConnectorDouble {
        broker: Arc::clone(&broker),
        injected: Mutex::new(Vec::new()),
    });
    let mediator = CredentialMediator::new(broker.clone(), "acme").for_run(Some("run-1".into()));
    let tool = Arc::new(SearchTool {
        connector: Arc::clone(&connector),
        connection_id: connection_id(),
        scope: scope.to_owned(),
    });
    let mut registry = ToolRegistry::new();
    registry.register_shared(Arc::new(mediator.mediate(tool)));
    let executor = ToolExecutor::new(registry);
    (broker, connector, executor, mediator)
}

#[tokio::test]
async fn tool_authenticates_through_a_handle_bytes_only_at_the_connector() {
    let (broker, connector, executor, _) = rig("drive.readonly");
    let results = executor
        .execute_batch(&[ToolCall::new(
            "c1",
            "provider_search",
            json!({"q": "rusty"}),
        )])
        .await;
    assert_eq!(results.len(), 1);
    let content = results[0].content.as_deref().unwrap();
    assert!(content.contains("provider-response"), "got: {content}");

    // The exit criterion, as assertions: one issuance (run-bound, scope-
    // narrowed), one injection at the connector, and the marker bytes in
    // the tool's answer nowhere — the tool saw the provider's response,
    // not the credential.
    let issued = broker.issued.lock().unwrap();
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].run_id.as_deref(), Some("run-1"));
    assert_eq!(
        issued[0].scopes,
        BTreeSet::from(["drive.readonly".to_owned()])
    );
    drop(issued);
    let injected = connector.injected.lock().unwrap();
    assert_eq!(injected.len(), 1);
    assert_eq!(injected[0].access_token, MARKER);
    assert!(
        !content.contains(MARKER),
        "bytes reached tool code: {content}"
    );
}

#[tokio::test]
async fn handle_reuse_within_ttl_and_reissue_after_expiry() {
    let calls = || vec![ToolCall::new("c", "provider_search", json!({}))];

    // Fresh handles: two calls, one issuance — the mediator serves the
    // live handle from its cache ("receives a handle at first use").
    let (broker, _connector, executor, _) = rig("drive.readonly");
    executor.execute_batch(&calls()).await;
    executor.execute_batch(&calls()).await;
    assert_eq!(broker.issued.lock().unwrap().len(), 1);

    // Handles minted 250 s old: still live (50 s left) but past the
    // cache's renew-after, so *every* call re-issues — the cache defers
    // issuance only while the cached handle is comfortably live.
    let (broker, _connector, executor, _) = rig("drive.readonly");
    broker
        .issued_offset_ms
        .store(-250_000, std::sync::atomic::Ordering::Relaxed);
    executor.execute_batch(&calls()).await;
    executor.execute_batch(&calls()).await;
    executor.execute_batch(&calls()).await;
    assert_eq!(broker.issued.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn expired_handle_fails_closed_at_resolution() {
    let (broker, _connector, executor, _) = rig("drive.readonly");
    // Mint handles that expired 100 s ago (issued 400 s ago — the
    // claims stay grammatical, the TTL is simply spent).
    broker
        .issued_offset_ms
        .store(-400_000, std::sync::atomic::Ordering::Relaxed);
    let results = executor
        .execute_batch(&[ToolCall::new("c1", "provider_search", json!({}))])
        .await;
    let content = results[0].content.as_deref().unwrap();
    assert!(content.starts_with("ERROR:"), "got: {content}");
    assert!(content.contains("handle_expired"), "got: {content}");
}

#[tokio::test]
async fn scope_escalation_is_refused_at_issuance() {
    // The tool asks for `drive.admin`; the consent ceiling is
    // {drive.readonly, drive.write}. Denied at issuance, not at the
    // provider — and the refusal names the missing scope.
    let (broker, connector, executor, _) = rig("drive.admin");
    let results = executor
        .execute_batch(&[ToolCall::new("c1", "provider_search", json!({}))])
        .await;
    let content = results[0].content.as_deref().unwrap();
    assert!(content.starts_with("ERROR:"), "got: {content}");
    assert!(content.contains("scope_not_granted"), "got: {content}");
    assert!(content.contains("drive.admin"), "got: {content}");
    assert!(broker.issued.lock().unwrap().is_empty());
    assert!(connector.injected.lock().unwrap().is_empty());
}

#[tokio::test]
async fn revoked_connection_fails_closed_at_the_next_call() {
    let (broker, connector, executor, _) = rig("drive.readonly");
    let calls = || vec![ToolCall::new("c", "provider_search", json!({}))];
    executor.execute_batch(&calls()).await;

    // Revoke beneath the outstanding handle. The mediator's cache still
    // holds the handle — and it does not matter: resolution reads live
    // state, so the very next call is denied.
    {
        let mut connections = broker.connections.lock().unwrap();
        let record = connections.get_mut(&connection_id()).unwrap();
        record.status = ConnectionStatus::Revoked;
        record.updated_at = Utc::now();
    }
    let results = executor.execute_batch(&calls()).await;
    let content = results[0].content.as_deref().unwrap();
    assert!(content.starts_with("ERROR:"), "got: {content}");
    assert!(content.contains("connection_revoked"), "got: {content}");
    // One injection only — the revoked call never reached the provider.
    assert_eq!(connector.injected.lock().unwrap().len(), 1);
    // And the cached handle was served (no second issuance): the
    // revocation check happened at resolution, as designed.
    assert_eq!(broker.issued.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn mediation_is_transparent_to_the_effect_boundary() {
    use rusty_agent_runtime::effects::EffectRequest;
    let broker = Arc::new(ScriptedBroker::new(vec![connection()]));
    let connector = Arc::new(ConnectorDouble {
        broker: Arc::clone(&broker),
        injected: Mutex::new(Vec::new()),
    });
    let tool: Arc<dyn CredentialTool> = Arc::new(SearchTool {
        connector,
        connection_id: connection_id(),
        scope: "drive.readonly".into(),
    });
    let mediator = CredentialMediator::new(broker, "acme");
    let mediated = mediator.mediate(Arc::clone(&tool));
    let call = ToolCall::new("c1", "provider_search", json!({"q": "x"}));
    // The wrapper delegates: the mediated tool's effect request is the
    // inner tool's own — admission cannot be bypassed by wrapping.
    let direct: EffectRequest = tool.effect_request(&call);
    let wrapped: EffectRequest = mediated.effect_request(&call);
    assert_eq!(format!("{direct:?}"), format!("{wrapped:?}"));
    assert_eq!(mediated.effect(), Effect::ReadOnly);
}

// ---------- SecretRef grammar and round-trip ----------

#[test]
fn secret_ref_parses_valid_placeholder() {
    let r = SecretRef::parse("rusty:secret:vault:api-key").unwrap();
    assert_eq!(r.to_string(), "rusty:secret:vault:api-key");
    assert_eq!(
        format!("{r:?}"),
        "SecretRef(\"rusty:secret:vault:api-key\")"
    );
}

#[test]
fn secret_ref_rejects_missing_prefix() {
    let err = SecretRef::parse("not-a-secret").unwrap_err();
    assert_eq!(err, SecretRefParseError::MissingPrefix);
}

#[test]
fn secret_ref_rejects_empty_store() {
    let err = SecretRef::parse("rusty:secret::key").unwrap_err();
    assert!(
        matches!(
            err,
            SecretRefParseError::InvalidSegment {
                segment: "store",
                ..
            }
        ),
        "got: {err}"
    );
}

#[test]
fn secret_ref_rejects_invalid_first_char() {
    let err = SecretRef::parse("rusty:secret:_vault:key").unwrap_err();
    assert!(
        matches!(
            err,
            SecretRefParseError::InvalidSegment {
                segment: "store",
                ..
            }
        ),
        "got: {err}"
    );
}

#[test]
fn secret_ref_rejects_uppercase() {
    let err = SecretRef::parse("rusty:secret:Vault:key").unwrap_err();
    assert!(
        matches!(
            err,
            SecretRefParseError::InvalidSegment {
                segment: "store",
                ..
            }
        ),
        "got: {err}"
    );
}

#[test]
fn secret_ref_rejects_too_long() {
    let long = "a".repeat(65);
    let err = SecretRef::parse(&format!("rusty:secret:{long}:key")).unwrap_err();
    assert!(
        matches!(
            err,
            SecretRefParseError::InvalidSegment {
                segment: "store",
                ..
            }
        ),
        "got: {err}"
    );
}

#[test]
fn secret_ref_rejects_invalid_char() {
    let err = SecretRef::parse("rusty:secret:vault:key@1").unwrap_err();
    assert!(
        matches!(
            err,
            SecretRefParseError::InvalidSegment { segment: "key", .. }
        ),
        "got: {err}"
    );
}

#[test]
fn secret_ref_allows_underscore_and_hyphen() {
    let r = SecretRef::parse("rusty:secret:vault_1:api-key").unwrap();
    assert_eq!(r.to_string(), "rusty:secret:vault_1:api-key");
}

#[test]
fn secret_ref_serializes_to_placeholder() {
    let r = SecretRef::parse("rusty:secret:vault:key").unwrap();
    let json = serde_json::to_string(&r).unwrap();
    assert_eq!(json, "\"rusty:secret:vault:key\"");
}

#[test]
fn secret_ref_deserializes_from_placeholder() {
    let r: SecretRef = serde_json::from_str("\"rusty:secret:vault:key\"").unwrap();
    assert_eq!(r.to_string(), "rusty:secret:vault:key");
}

#[test]
fn secret_ref_deserialize_rejects_invalid() {
    let err: Result<SecretRef, _> = serde_json::from_str("\"not-a-secret\"");
    assert!(err.is_err());
}

#[test]
fn secret_ref_try_from_string_round_trips() {
    let s = "rusty:secret:env:token".to_owned();
    let r = SecretRef::try_from(s.clone()).unwrap();
    let back: String = r.into();
    assert_eq!(back, s);
}

#[test]
fn secret_ref_display_never_shows_value() {
    let r = SecretRef::parse("rusty:secret:vault:crm-api-key").unwrap();
    let d = format!("{r}");
    assert_eq!(d, "rusty:secret:vault:crm-api-key");
    // The placeholder itself contains no credential value.
    assert!(!d.contains("sk-"));
    assert!(!d.contains("live"));
}

#[test]
fn secret_ref_debug_never_shows_value() {
    let r = SecretRef::parse("rusty:secret:vault:crm-api-key").unwrap();
    let d = format!("{r:?}");
    assert_eq!(d, "SecretRef(\"rusty:secret:vault:crm-api-key\")");
    assert!(!d.contains("sk-"));
    assert!(!d.contains("live"));
}

// ---------- SecretResolver — egress-only, never in tool code ----------

#[tokio::test]
async fn secret_resolver_known_ref_yields_material() {
    let ref_ = SecretRef::parse("rusty:secret:vault:api-key").unwrap();
    let material = TokenMaterial {
        access_token: "sk-resolver-MARKER".into(),
        refresh_token: None,
        client_secret: None,
        client_id: None,
        username: None,
        password: None,
        token_url: None,
        expires_at: None,
    };
    let resolver = ScriptedSecretResolver::new().with_secret(&ref_, material.clone());
    let resolved = resolver.resolve_secret(&ref_).await.unwrap();
    assert_eq!(resolved.access_token, material.access_token);
}

#[tokio::test]
async fn secret_resolver_unknown_ref_fails_closed() {
    let ref_ = SecretRef::parse("rusty:secret:vault:api-key").unwrap();
    let resolver = ScriptedSecretResolver::new();
    let err = resolver.resolve_secret(&ref_).await.unwrap_err();
    assert!(
        matches!(err.reason, BrokerDenialReason::BrokerUnavailable),
        "got: {err:?}"
    );
    assert!(err.to_string().contains("unknown"), "got: {err}");
}

#[tokio::test]
async fn secret_resolver_bytes_never_appear_in_tool_code() {
    // Structural proof: SecretRef has no .resolve() method and no
    // public fields — the compile-fail doc tests on the type prove
    // that. This test proves the resolver seam itself: the only way
    // to reach the bytes is through the trait, and the trait is not
    // implemented on SecretRef.
    let ref_ = SecretRef::parse("rusty:secret:vault:api-key").unwrap();
    let material = TokenMaterial {
        access_token: "sk-resolver-MARKER".into(),
        refresh_token: None,
        client_secret: None,
        client_id: None,
        username: None,
        password: None,
        token_url: None,
        expires_at: None,
    };
    let resolver = ScriptedSecretResolver::new().with_secret(&ref_, material.clone());
    let resolved = resolver.resolve_secret(&ref_).await.unwrap();
    // The placeholder never contains the bytes.
    let placeholder = ref_.to_string();
    assert!(!placeholder.contains("sk-"));
    assert!(!placeholder.contains("resolver"));
    assert!(!placeholder.contains("MARKER"));
    // The bytes exist only at the resolver boundary.
    assert_eq!(resolved.access_token, "sk-resolver-MARKER");
}

// ---------- capsule mediation (feature `wasm`) ----------

#[cfg(feature = "wasm")]
mod capsule_mediation {
    //! The reference guest is the echo component: copies its input to
    //! its output, so the test can read back exactly what the guest
    //! received. Hand-written component text (WAT) compiled by
    //! wasmtime's `wat` support — the `tests/capsule.rs` discipline.

    use super::*;
    use rusty_agent_runtime::broker::BrokeredCapsuleHost;
    use rusty_agent_runtime::capsule::{
        CapabilityGrant, CapsuleIdentity, CapsuleInterface, CapsuleManifest, ResourceBudget,
        WORLD_V1,
    };
    use rusty_agent_runtime::capsule_host::{CapsuleHost, CapsuleInvocation};
    use rusty_agent_runtime::journal::{Clock, Journal};
    use rusty_agent_runtime::record::{sha256_hex, RunEventKind};

    const REALLOC: &str = r#"
    (global $heap (mut i32) (i32.const 1024))
    (func (export "realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
      (local $ptr i32)
      (global.set $heap
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get $align))))
      (local.set $ptr (global.get $heap))
      (global.set $heap (i32.add (global.get $heap) (local.get $new_size)))
      (local.get $ptr))"#;

    const WRITE_RESULT: &str = r#"
      (i32.store8 (i32.const 512) (local.get $disc))
      (i32.store (i32.const 516) (local.get $ptr))
      (i32.store (i32.const 520) (local.get $len))
      (i32.const 512)"#;

    /// The echo component: copies the input bytes to its result region
    /// and answers them — the guest that shows its work. The output
    /// string lives at 4096 (the result header at 512): injected handle
    /// tokens make the input far longer than the 512-byte gap the pure
    /// guest's static answer fits in.
    fn echo_guest_wat() -> String {
        format!(
            r#"(component
  (core module $m
    (memory (export "memory") 1)
    {REALLOC}
    (func (export "run") (param $in_ptr i32) (param $in_len i32) (result i32)
      (local $disc i32) (local $ptr i32) (local $len i32)
      (local.set $disc (i32.const 0))
      (local.set $ptr (i32.const 4096))
      (local.set $len (local.get $in_len))
      (memory.copy (i32.const 4096) (local.get $in_ptr) (local.get $in_len))
      {WRITE_RESULT}))
  (core instance $i (instantiate $m))
  (func $run (param "input" string) (result (result string (error string)))
    (canon lift (core func $i "run")
      (memory (core memory $i "memory"))
      (realloc (core func $i "realloc"))))
  (export "run" (func $run)))"#
        )
    }

    fn echo_manifest(connection: &str) -> CapsuleManifest {
        let wat = echo_guest_wat();
        CapsuleManifest {
            identity: CapsuleIdentity {
                name: "echo".into(),
                description: None,
            },
            version: "0.1.0".into(),
            build_digest: sha256_hex(wat.as_bytes()),
            interface: CapsuleInterface {
                world: WORLD_V1.into(),
                input_schema: None,
                output_schema: None,
            },
            effects: BTreeSet::from([Effect::ReadOnly]),
            capabilities: BTreeSet::from([CapabilityGrant::Secret {
                handles: vec![connection.to_owned()],
            }]),
            budget: ResourceBudget::default(),
        }
    }

    #[tokio::test]
    async fn secret_grants_arrive_as_broker_issued_tokens() {
        let broker = Arc::new(ScriptedBroker::new(vec![connection()]));
        let host =
            CapsuleHost::from_bytes(echo_manifest(&connection_id()), echo_guest_wat()).unwrap();
        let mediator = CredentialMediator::new(broker.clone(), "acme");
        let host = BrokeredCapsuleHost::new(host, mediator);
        let journal = Journal::new("run-capsule", "thread-capsule", Clock::System);
        let outcome = host
            .invoke(
                CapsuleInvocation::new(json!({"task": "ping"})).with_journal(journal.clone(), None),
            )
            .await
            .unwrap();

        // The guest received exactly one secret: the issued handle token
        // under its connection id, bound to the invocation's run.
        let token = outcome.output["secrets"][&connection_id()].as_str()(
            "the guest echoed the injected tokens",
        );
        let issued = broker.issued.lock().unwrap();
        assert_eq!(issued.len(), 1);
        assert_eq!(issued[0].run_id.as_deref(), Some("run-capsule"));
        // The token parses back to the issued claims — opaque to the
        // guest, verifiable at the broker.
        let (parsed, _) = CredentialHandle::parse_token(token).unwrap();
        assert_eq!(parsed, issued[0]);
        // The guest's journaled input is itself the evidence that it
        // received tokens — and the marker bytes appear nowhere in it.
        let events = journal.events();
        let call = events
            .iter()
            .find(|event| event.kind == RunEventKind::WasmCall)(
            "the invocation journaled its call",
        );
        let evidence = serde_json::to_string(&call).unwrap();
        assert!(evidence.contains("secrets"), "got: {evidence}");
        assert!(
            !evidence.contains(MARKER),
            "bytes in the journal: {evidence}"
        );
    }

    #[tokio::test]
    async fn issuance_denial_refuses_the_invocation_before_guest_code() {
        // The connection is revoked at issuance time: the invocation
        // fails closed with the broker's typed denial, and nothing runs.
        let mut revoked = connection();
        revoked.status = ConnectionStatus::Revoked;
        let broker = Arc::new(ScriptedBroker::new(vec![revoked]));
        let host =
            CapsuleHost::from_bytes(echo_manifest(&connection_id()), echo_guest_wat()).unwrap();
        let host = BrokeredCapsuleHost::new(host, CredentialMediator::new(broker, "acme"));
        let err = host
            .invoke(CapsuleInvocation::new(json!({"task": "ping"})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("connection_revoked"), "got: {err}");
    }

    #[tokio::test]
    async fn smuggled_secrets_key_is_refused_at_admission() {
        let broker = Arc::new(ScriptedBroker::new(vec![connection()]));
        let host =
            CapsuleHost::from_bytes(echo_manifest(&connection_id()), echo_guest_wat()).unwrap();
        let host = BrokeredCapsuleHost::new(host, CredentialMediator::new(broker.clone(), "acme"));
        let err = host
            .invoke(CapsuleInvocation::new(
                json!({"secrets": {connection_id(): "guest-forged-token"}}),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("secrets"), "got: {err}");
        // Nothing was issued for the forged input.
        assert!(broker.issued.lock().unwrap().is_empty());
    }
}
// ---------- Wire probe — prove the rewrite before the attachment goes live ----------

fn probe_record(outcome: WireProbeOutcome, at: DateTime<Utc>) -> WireProbeRecord {
    WireProbeRecord {
        evidence_hash: "abcd1234".into(),
        secret_ref: SecretRef::parse("rusty:secret:vault:api-key").unwrap(),
        endpoint: "https://api.example.com/v1".into(),
        probed_at: at,
        outcome,
    }
}

#[test]
fn golden_wire_probe_record_shape() {
    assert_golden(
        "broker_wire_probe.json",
        &probe_record(WireProbeOutcome::Rewritten, ts(1_800_000_000_000)),
    );
}

#[tokio::test]
async fn probe_ledger_newest_wins_by_secret_ref_and_endpoint() {
    let ledger = ScriptedProbeLedger::new();
    let ref_a = SecretRef::parse("rusty:secret:vault:key-a").unwrap();
    let ref_b = SecretRef::parse("rusty:secret:vault:key-b").unwrap();

    // Two probes for the same (ref, endpoint) — the later one wins.
    let r1 = WireProbeRecord {
        evidence_hash: "h1".into(),
        secret_ref: ref_a.clone(),
        endpoint: "https://api.example.com".into(),
        probed_at: ts(1_800_000_000_000),
        outcome: WireProbeOutcome::Rewritten,
    };
    let r2 = WireProbeRecord {
        evidence_hash: "h2".into(),
        secret_ref: ref_a.clone(),
        endpoint: "https://api.example.com".into(),
        probed_at: ts(1_800_000_001_000),
        outcome: WireProbeOutcome::NotRewritten,
    };
    ledger.record_probe(r1.clone()).await.unwrap();
    ledger.record_probe(r2.clone()).await.unwrap();

    let newest = ledger
        .newest_probe(&ref_a, "https://api.example.com")
        .await
        .unwrap();
    assert_eq!(newest.evidence_hash, "h2");
    assert_eq!(newest.outcome, WireProbeOutcome::NotRewritten);

    // A different endpoint for the same ref has no record.
    assert!(ledger
        .newest_probe(&ref_a, "https://other.example.com")
        .await
        .is_none());

    // A different ref for the same endpoint has no record.
    assert!(ledger
        .newest_probe(&ref_b, "https://api.example.com")
        .await
        .is_none());
}

#[tokio::test]
async fn probe_ledger_is_append_only() {
    let ledger = ScriptedProbeLedger::new();
    let ref_ = SecretRef::parse("rusty:secret:vault:key").unwrap();

    let r1 = WireProbeRecord {
        evidence_hash: "h1".into(),
        secret_ref: ref_.clone(),
        endpoint: "https://api.example.com".into(),
        probed_at: ts(1_800_000_000_000),
        outcome: WireProbeOutcome::Rewritten,
    };
    let r2 = WireProbeRecord {
        evidence_hash: "h2".into(),
        secret_ref: ref_.clone(),
        endpoint: "https://api.example.com".into(),
        probed_at: ts(1_800_000_001_000),
        outcome: WireProbeOutcome::NotRewritten,
    };

    ledger.record_probe(r1).await.unwrap();
    ledger.record_probe(r2).await.unwrap();

    // Both records exist; newest_wins resolves the conflict, nothing is
    // overwritten.
    assert_eq!(ledger.record_count(), 2);
}

#[tokio::test]
async fn rewritten_probe_makes_attachment_live() {
    let ledger = ScriptedProbeLedger::new().with_probe(probe_record(
        WireProbeOutcome::Rewritten,
        ts(1_800_000_000_000),
    ));
    let ref_ = SecretRef::parse("rusty:secret:vault:api-key").unwrap();
    let newest = ledger
        .newest_probe(&ref_, "https://api.example.com/v1")
        .await;
    assert!(newest.is_some());
    assert_eq!(newest.unwrap().outcome, WireProbeOutcome::Rewritten);
}

#[tokio::test]
async fn not_rewritten_probe_makes_attachment_not_live() {
    let ledger = ScriptedProbeLedger::new().with_probe(probe_record(
        WireProbeOutcome::NotRewritten,
        ts(1_800_000_000_000),
    ));
    let ref_ = SecretRef::parse("rusty:secret:vault:api-key").unwrap();
    let newest = ledger
        .newest_probe(&ref_, "https://api.example.com/v1")
        .await
        .unwrap();
    assert_eq!(newest.outcome, WireProbeOutcome::NotRewritten);
}

#[tokio::test]
async fn unreachable_probe_makes_attachment_not_live() {
    let ledger = ScriptedProbeLedger::new().with_probe(probe_record(
        WireProbeOutcome::Unreachable,
        ts(1_800_000_000_000),
    ));
    let ref_ = SecretRef::parse("rusty:secret:vault:api-key").unwrap();
    let newest = ledger
        .newest_probe(&ref_, "https://api.example.com/v1")
        .await
        .unwrap();
    assert_eq!(newest.outcome, WireProbeOutcome::Unreachable);
}

#[tokio::test]
async fn missing_probe_means_no_attachment() {
    let ledger = ScriptedProbeLedger::new();
    let ref_ = SecretRef::parse("rusty:secret:vault:api-key").unwrap();
    let newest = ledger
        .newest_probe(&ref_, "https://api.example.com/v1")
        .await;
    assert!(newest.is_none(), "no probe means no liveness evidence");
}

#[tokio::test]
async fn probe_re_runs_on_newer_timestamp_take_precedence() {
    let ledger = ScriptedProbeLedger::new();
    let ref_ = SecretRef::parse("rusty:secret:vault:api-key").unwrap();
    let endpoint = "https://api.example.com/v1";

    // Initial passing probe.
    ledger
        .record_probe(WireProbeRecord {
            evidence_hash: "h-pass".into(),
            secret_ref: ref_.clone(),
            endpoint: endpoint.into(),
            probed_at: ts(1_800_000_000_000),
            outcome: WireProbeOutcome::Rewritten,
        })
        .await
        .unwrap();

    // Later failing probe (simulates credential rotation with a bad key).
    ledger
        .record_probe(WireProbeRecord {
            evidence_hash: "h-fail".into(),
            secret_ref: ref_.clone(),
            endpoint: endpoint.into(),
            probed_at: ts(1_800_000_001_000),
            outcome: WireProbeOutcome::NotRewritten,
        })
        .await
        .unwrap();

    let newest = ledger.newest_probe(&ref_, endpoint).await.unwrap();
    assert_eq!(newest.outcome, WireProbeOutcome::NotRewritten);
    assert_eq!(newest.evidence_hash, "h-fail");
}
