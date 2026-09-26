//! Gateway protocol conformance (EP-04-S01): wire round-trips, closed-
//! envelope discipline, and schema-drift detection.
//!
//! The golden files under `tests/golden/protocol/` are the generated
//! source the TypeScript wire package (`sdks/wire`) builds its types
//! from; `gateway_hello_ok_sample.json` is the cross-language probe —
//! serialized here, type-checked there. Regenerate with
//! `UPDATE_GOLDEN=1`, then regenerate the wire package.

use std::path::PathBuf;

use rusty_agent_server::protocol::{
    ClientIdentity, ConnectAuth, ConnectParams, ConnectSnapshot, ErrorShape, EventFrame, Features,
    GatewayFrame, GraphSummary, GrantedAuth, HelloOk, PolicyBudgets, RequestFrame, ResponseFrame,
    ServerIdentity, PROTOCOL_VERSION,
};
use serde_json::{json, Value};

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join("protocol")
        .join(name)
}

/// Assert pretty-printed JSON equals the golden file exactly.
fn assert_golden(name: &str, value: &impl serde::Serialize) {
    let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        rendered,
        expected,
        "schema drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1, review the diff, \
         and regenerate sdks/wire",
        path.display()
    );
}

/// One of every frame kind, with every optional field exercised.
fn sample_frames() -> Vec<GatewayFrame> {
    vec![
        GatewayFrame::Req(RequestFrame {
            id: "r-1".into(),
            method: "threads.create".into(),
            params: Some(json!({"graph": "react_agent"})),
        }),
        GatewayFrame::Res(ResponseFrame {
            id: "r-1".into(),
            ok: false,
            payload: None,
            error: Some(ErrorShape {
                code: "not_found".into(),
                message: "graph `missing` is not registered".into(),
                details: Some(json!({"graph": "missing"})),
                retryable: Some(false),
                retry_after_ms: None,
            }),
        }),
        GatewayFrame::Event(EventFrame {
            event: "run.updated".into(),
            payload: Some(json!({"run_id": "run-1", "status": "running"})),
            seq: 42,
        }),
        GatewayFrame::HelloOk(sample_hello_ok()),
    ]
}

fn sample_hello_ok() -> HelloOk {
    HelloOk {
        protocol: PROTOCOL_VERSION,
        server: ServerIdentity {
            version: "0.12.0".into(),
            conn_id: "conn-7f2c".into(),
        },
        features: Features {
            methods: vec!["threads.create".into(), "runs.stream".into()],
            events: vec!["run.updated".into(), "tick".into()],
        },
        snapshot: ConnectSnapshot {
            state_version: 17,
            graphs: vec![GraphSummary {
                name: "react_agent".into(),
                channels: vec!["messages".into()],
            }],
        },
        auth: GrantedAuth {
            role: "operator".into(),
            scopes: vec!["runs:read".into(), "runs:write".into()],
        },
        policy: PolicyBudgets {
            max_payload_bytes: 1_048_576,
            max_buffered_bytes: 8_388_608,
            tick_interval_ms: 15_000,
        },
    }
}

// ---------------------------------------------------------------------------
// Wire round-trips
// ---------------------------------------------------------------------------

#[test]
fn every_frame_kind_round_trips_byte_identically() {
    for frame in sample_frames() {
        let wire = serde_json::to_string(&frame).unwrap();
        let back: GatewayFrame = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, frame);
        assert_eq!(serde_json::to_string(&back).unwrap(), wire);
    }
}

#[test]
fn connect_params_round_trip_with_and_without_auth() {
    let open = ConnectParams {
        min_protocol: 1,
        max_protocol: PROTOCOL_VERSION,
        client: ClientIdentity {
            id: "studio".into(),
            version: "0.1.0".into(),
            platform: "web".into(),
            display_name: Some("Rusty Studio".into()),
        },
        scopes: None,
        auth: None,
    };
    let keyed = ConnectParams {
        scopes: Some(vec!["runs:read".into()]),
        auth: Some(ConnectAuth {
            api_key: Some("key".into()),
        }),
        ..open.clone()
    };
    for params in [open, keyed] {
        let wire = serde_json::to_string(&params).unwrap();
        let back: ConnectParams = serde_json::from_str(&wire).unwrap();
        assert_eq!(back, params);
    }
}

#[test]
fn absent_options_stay_off_the_wire() {
    let wire = serde_json::to_value(GatewayFrame::Req(RequestFrame {
        id: "r-2".into(),
        method: "info".into(),
        params: None,
    }))
    .unwrap();
    assert_eq!(wire, json!({"type": "req", "id": "r-2", "method": "info"}));
}

// ---------------------------------------------------------------------------
// Closed-envelope discipline
// ---------------------------------------------------------------------------

#[test]
fn unknown_frame_type_is_refused() {
    let err = serde_json::from_value::<GatewayFrame>(json!({"type": "mystery", "id": "x"}));
    assert!(err.is_err(), "an undeclared frame type must not parse");
}

#[test]
fn unknown_fields_inside_a_frame_are_refused() {
    let err = serde_json::from_value::<GatewayFrame>(
        json!({"type": "req", "id": "r-1", "method": "info", "smuggled": true}),
    );
    assert!(err.is_err(), "frames are closed objects");
    let err = serde_json::from_value::<ConnectParams>(json!({
        "min_protocol": 1, "max_protocol": 1,
        "client": {"id": "c", "version": "1", "platform": "web"},
        "smuggled": true
    }));
    assert!(err.is_err(), "connect params are a closed object");
}

// ---------------------------------------------------------------------------
// Schema-drift detection — the codegen contract
// ---------------------------------------------------------------------------

#[test]
fn gateway_frame_schema_matches_golden() {
    assert_golden(
        "gateway_frame_schema.json",
        &schemars::schema_for!(GatewayFrame),
    );
}

#[test]
fn connect_params_schema_matches_golden() {
    assert_golden(
        "connect_params_schema.json",
        &schemars::schema_for!(ConnectParams),
    );
}

/// The cross-language probe: this exact serialization must satisfy the
/// TypeScript types generated from the schemas above (`sdks/wire`'s
/// round-trip test consumes this file).
#[test]
fn hello_ok_sample_matches_golden() {
    let frame = GatewayFrame::HelloOk(sample_hello_ok());
    assert_golden("gateway_hello_ok_sample.json", &frame);
    // And the sample self-describes: it parses back as the same frame.
    let text = std::fs::read_to_string(golden_path("gateway_hello_ok_sample.json"));
    if let Ok(text) = text {
        let back: GatewayFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(back, frame);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], json!("hello-ok"));
    }
}
