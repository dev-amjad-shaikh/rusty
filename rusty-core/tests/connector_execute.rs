//! Connector operation execution and ingestion normalization (EP-07-S05):
//! the generic executor over the transport seam, the operation mounted
//! as an ordinary tool, and the ServiceNow-class corpus mapping with
//! failures preserved.

use std::sync::Mutex;

use async_trait::async_trait;
use rusty_agent_runtime::connector::{
    CheckRequest, CheckResponse, ConnectorManifest, ConnectorOperation, ConnectorTransport,
    HttpMethod, OperationAuth, OperationEffect,
};
use rusty_agent_runtime::connector::{
    ConnectorOperationTool, IngestError, OperationExecutor, execute_operation, normalize_corpus,
    normalize_servicenow_record,
};
use rusty_agent_runtime::error::Result as RuntimeResult;
use rusty_agent_runtime::gaps::{InteractionChannel, InteractionOutcome, ResolutionPath};
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::tool::{EffectClass, Tool};
use serde_json::{Value, json};

// --------------------------------------------------------------------- //
// Harness
// --------------------------------------------------------------------- //

/// A scripted transport: one queued response, every request recorded.
#[derive(Debug, Default)]
struct FakeTransport {
    status: u16,
    body: Mutex<Vec<u8>>,
    seen: Mutex<Option<CheckRequest>>,
}

#[async_trait]
impl ConnectorTransport for FakeTransport {
    async fn send(&self, request: CheckRequest) -> RuntimeResult<CheckResponse> {
        *self.seen.lock().unwrap() = Some(request.clone());
        let body = std::mem::take(&mut *self.body.lock().unwrap());
        Ok(CheckResponse {
            status: if self.status == 0 { 200 } else { self.status },
            body,
        })
    }
}

fn op(name: &str, path: &str, effect: OperationEffect, params: Value) -> ConnectorOperation {
    ConnectorOperation {
        name: name.to_owned(),
        description: format!("The {name} operation."),
        method: HttpMethod::Get,
        path: path.to_owned(),
        effect,
        params_schema: params,
        headers: Vec::new(),
        auth: vec![OperationAuth::Bearer {
            token: "{credentials.token}".to_owned(),
        }],
        max_response_bytes: None,
    }
}

fn manifest() -> ConnectorManifest {
    let read = op(
        "read-corpus",
        "/api/now/corpus/{window}",
        OperationEffect::ReadOnly,
        json!({
            "type": "object",
            "required": ["window"],
            "properties": {"window": {"type": "string"}}
        }),
    );
    let check = op(
        "check-connection",
        "/api/now/table/sys_user?sysparm_limit=1",
        OperationEffect::ReadOnly,
        json!({"type": "object"}),
    );
    ConnectorManifest::new(
        "servicenow",
        "1",
        "ServiceNow",
        "ServiceNow corpus operations.",
        "https://docs.servicenow.com/",
        "https://{instance}.service-now.com",
        json!({
            "type": "object",
            "required": ["instance", "credentials"],
            "properties": {
                "instance": {"type": "string"},
                "credentials": {
                    "type": "object",
                    "required": ["token"],
                    "properties": {"token": {"type": "string", "rusty_secret": true}}
                }
            }
        }),
        vec![read, check],
        "check-connection",
    )
    .expect("the fixture manifest validates")
}

fn config() -> Value {
    json!({
        "instance": "dev123",
        "credentials": {"token": "s3cret-token"}
    })
}

// --------------------------------------------------------------------- //
// execute_operation
// --------------------------------------------------------------------- //

#[tokio::test]
async fn executes_a_declared_operation_and_returns_the_parsed_body() {
    let manifest = manifest();
    let operation = manifest.operation("read-corpus").unwrap();
    let transport = FakeTransport {
        body: Mutex::new(
            json!({"result": [{"sys_id": "INC001"}]})
                .to_string()
                .into_bytes(),
        ),
        ..FakeTransport::default()
    };
    let response = execute_operation(
        &manifest,
        operation,
        &config(),
        &json!({"window": "2026-W01"}),
        &transport,
    )
    .await
    .expect("the operation executes");
    assert_eq!(response.status, 200);
    assert_eq!(response.body["result"][0]["sys_id"], json!("INC001"));

    // The request rendered with the call argument overlaid on the
    // config — `{window}` resolved from the params…
    let request = transport.seen.lock().unwrap().clone().unwrap();
    assert!(
        request.url.contains("/api/now/corpus/2026-W01"),
        "params render into the path: {}",
        request.url
    );
    // …and the secret resolved into the auth header, never the URL.
    assert!(
        request
            .headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer s3cret-token")
    );
    assert!(!request.url.contains("s3cret-token"));
}

#[tokio::test]
async fn rejects_arguments_against_the_declared_schema_before_rendering() {
    let manifest = manifest();
    let operation = manifest.operation("read-corpus").unwrap();
    let transport = FakeTransport::default();
    let error = execute_operation(&manifest, operation, &config(), &json!({}), &transport)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("window"), "{error}");
    // The rejection happens before any wire call.
    assert!(transport.seen.lock().unwrap().is_none());
}

#[tokio::test]
async fn call_arguments_can_never_shadow_config_keys() {
    // A `params` value naming a config key loses the collision: the
    // config's own value renders, so a caller cannot swap the instance —
    // or a credential — by argument.
    let manifest = manifest();
    let operation = manifest.operation("read-corpus").unwrap();
    let transport = FakeTransport {
        body: Mutex::new(b"[]".to_vec()),
        ..FakeTransport::default()
    };
    execute_operation(
        &manifest,
        operation,
        &config(),
        &json!({"window": "w", "instance": "evil"}),
        &transport,
    )
    .await
    .unwrap();
    let request = transport.seen.lock().unwrap().clone().unwrap();
    assert!(
        request.url.starts_with("https://dev123."),
        "{}",
        request.url
    );
}

#[tokio::test]
async fn non_2xx_is_a_typed_error_and_auth_refusals_echo_no_body() {
    let manifest = manifest();
    let operation = manifest.operation("read-corpus").unwrap();
    let transport = FakeTransport {
        status: 403,
        body: Mutex::new(b"the token neighborhood is s3cret".to_vec()),
        ..FakeTransport::default()
    };
    let error = execute_operation(
        &manifest,
        operation,
        &config(),
        &json!({"window": "w"}),
        &transport,
    )
    .await
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("403"), "{message}");
    assert!(!message.contains("s3cret"), "{message}");
}

#[tokio::test]
async fn a_non_json_body_is_a_typed_error_not_an_empty_corpus() {
    let manifest = manifest();
    let operation = manifest.operation("read-corpus").unwrap();
    let transport = FakeTransport {
        body: Mutex::new(b"<html>an error page</html>".to_vec()),
        ..FakeTransport::default()
    };
    let error = execute_operation(
        &manifest,
        operation,
        &config(),
        &json!({"window": "w"}),
        &transport,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("non-JSON"), "{error}");
}

// --------------------------------------------------------------------- //
// ConnectorOperationTool — the operation as an ordinary tool
// --------------------------------------------------------------------- //

/// A scripted executor: records the arguments, answers a fixed body.
#[derive(Debug, Default)]
struct FakeExecutor {
    seen: Mutex<Option<Value>>,
}

#[async_trait]
impl OperationExecutor for FakeExecutor {
    async fn execute(&self, params: Value) -> RuntimeResult<Value> {
        *self.seen.lock().unwrap() = Some(params);
        Ok(json!({"result": []}))
    }
}

#[tokio::test]
async fn the_operation_mounts_as_an_ordinary_tool() {
    let manifest = manifest();
    let operation = manifest.operation("read-corpus").unwrap();
    let tool = ConnectorOperationTool::new(
        &manifest,
        operation,
        std::sync::Arc::new(FakeExecutor::default()),
    );

    // The catalog name, the declared schema, and the manifest's effect
    // classification — the contracts the guard pipeline reads.
    assert_eq!(tool.name(), "servicenow/read-corpus");
    assert_eq!(tool.parameters_schema()["required"], json!(["window"]));
    assert_eq!(tool.effect(), Effect::ReadOnly);
    assert_eq!(tool.effect_class(), EffectClass::Egress);

    // Arguments validate against the declared schema before the
    // executor is invoked — ingestion has no privileged path.
    let error = tool.call(json!({})).await.unwrap_err();
    assert!(error.to_string().contains("window"), "{error}");

    let body = tool.call(json!({"window": "2026-W01"})).await.unwrap();
    assert_eq!(body, json!({"result": []}));
}

// --------------------------------------------------------------------- //
// Ingestion normalization — the corpus mapping, failures preserved
// --------------------------------------------------------------------- //

fn record(stream: &str, sys_id: &str, extra: Value) -> Value {
    let mut record = json!({
        "stream": stream,
        "sys_id": sys_id,
        "caller_id": "u-1",
        "occurred_at": "2026-01-05T12:00:00Z",
    });
    record
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    record
}

#[test]
fn every_channel_maps_to_its_demand_shape() {
    for (stream, extra, channel) in [
        (
            "sp_log",
            json!({"query": "vpn", "result_count": 4, "clicked": true}),
            InteractionChannel::PortalSearch,
        ),
        (
            "sys_cs_conversation",
            json!({"question": "reset password", "state": "deflected"}),
            InteractionChannel::Chat,
        ),
        (
            "sc_request",
            json!({"short_description": "new laptop", "state": "closed_complete"}),
            InteractionChannel::Request,
        ),
        (
            "incident",
            json!({"short_description": "vpn down", "state": "resolved"}),
            InteractionChannel::Incident,
        ),
        (
            "sn_customerservice_case",
            json!({"short_description": "billing", "state": "closed"}),
            InteractionChannel::Case,
        ),
        (
            "sys_escalation",
            json!({"reason": "tier 2", "state": "resolved"}),
            InteractionChannel::Escalation,
        ),
    ] {
        let event =
            normalize_servicenow_record("servicenow", &record(stream, "ID-1", extra)).unwrap();
        assert_eq!(event.channel, channel, "stream {stream}");
        assert_eq!(event.source.stream, stream);
        assert_eq!(event.source.record_id, "ID-1");
    }
}

#[test]
fn the_failure_classes_are_preserved_never_coerced() {
    // A zero-result search.
    let event = normalize_servicenow_record(
        "servicenow",
        &record(
            "sp_log",
            "SP-1",
            json!({"query": "expense policy", "result_count": 0, "clicked": false}),
        ),
    )
    .unwrap();
    assert_eq!(event.resolution_path, ResolutionPath::Unresolved);
    assert_eq!(event.outcome, InteractionOutcome::NoResult);

    // Results shown, none clicked.
    let event = normalize_servicenow_record(
        "servicenow",
        &record(
            "sp_log",
            "SP-2",
            json!({"query": "expense policy", "result_count": 7, "clicked": false}),
        ),
    )
    .unwrap();
    assert_eq!(event.outcome, InteractionOutcome::NoClick);

    // An abandoned chat.
    let event = normalize_servicenow_record(
        "servicenow",
        &record(
            "sys_cs_conversation",
            "CH-1",
            json!({"question": "where is my invoice", "state": "abandoned"}),
        ),
    )
    .unwrap();
    assert_eq!(event.resolution_path, ResolutionPath::Abandoned);

    // A reopened incident: the fix did not hold.
    let event = normalize_servicenow_record(
        "servicenow",
        &record(
            "incident",
            "INC-1",
            json!({"short_description": "vpn down again", "state": "resolved", "reopen_count": 2}),
        ),
    )
    .unwrap();
    assert_eq!(event.outcome, InteractionOutcome::Reopened);

    // A multi-reassignment ticket is an escalation in fact.
    let event = normalize_servicenow_record(
        "servicenow",
        &record(
            "incident",
            "INC-2",
            json!({"short_description": "bounced between queues", "state": "closed", "reassignment_count": 3}),
        ),
    )
    .unwrap();
    assert_eq!(event.outcome, InteractionOutcome::Escalated);
}

#[test]
fn unrecognized_states_and_shapes_are_typed_errors() {
    // An unknown stream is never guessed.
    assert!(matches!(
        normalize_servicenow_record(
            "servicenow",
            &record(
                "cmdb_ci",
                "CI-1",
                json!({"short_description": "x", "state": "closed"})
            ),
        ),
        Err(IngestError::UnknownStream(_))
    ));

    // A state the mapping does not recognize is a typed error, not a
    // coercion to success.
    assert!(matches!(
        normalize_servicenow_record(
            "servicenow",
            &record(
                "incident",
                "INC-9",
                json!({"short_description": "x", "state": "awaiting_vendor"}),
            ),
        ),
        Err(IngestError::UnrecognizedState { .. })
    ));

    // A missing required field names the field.
    assert!(matches!(
        normalize_servicenow_record(
            "servicenow",
            &record("incident", "INC-10", json!({"state": "resolved"})),
        ),
        Err(IngestError::MissingField {
            field: "short_description",
            ..
        })
    ));

    // A bad timestamp is a typed error — demand evidence needs a real
    // `occurred_at`.
    assert!(matches!(
        normalize_servicenow_record(
            "servicenow",
            &record(
                "incident",
                "INC-11",
                json!({"short_description": "x", "state": "resolved", "occurred_at": "last tuesday"}),
            ),
        ),
        Err(IngestError::BadTimestamp { .. })
    ));
}

#[test]
fn corpus_normalization_resolves_journeys_to_event_links() {
    let search = record(
        "sp_log",
        "SP-9",
        json!({"query": "vpn", "result_count": 0, "clicked": false}),
    );
    let mut incident = record(
        "incident",
        "INC-20",
        json!({"short_description": "vpn is down", "state": "resolved"}),
    );
    // The incident cites the search that preceded it.
    incident["links"] = json!(["sp_log/SP-9", "sp_log/SP-UNKNOWN"]);
    let events = normalize_corpus("servicenow", &[search, incident]).unwrap();
    assert_eq!(events.len(), 2);
    // The in-corpus reference resolved to the search's event id; the
    // out-of-corpus one dropped — a link must cite an immutable row.
    assert_eq!(events[1].links, vec![events[0].event_id.clone()]);
}

#[test]
fn reingestion_converges_on_the_same_event_ids() {
    let corpus = vec![
        record(
            "sp_log",
            "SP-10",
            json!({"query": "vpn", "result_count": 0, "clicked": false}),
        ),
        record(
            "incident",
            "INC-30",
            json!({"short_description": "vpn down", "state": "resolved"}),
        ),
    ];
    let first = normalize_corpus("servicenow", &corpus).unwrap();
    let second = normalize_corpus("servicenow", &corpus).unwrap();
    let first_ids: Vec<&str> = first.iter().map(|e| e.event_id.as_str()).collect();
    let second_ids: Vec<&str> = second.iter().map(|e| e.event_id.as_str()).collect();
    assert_eq!(first_ids, second_ids, "content addresses are stable");
}
