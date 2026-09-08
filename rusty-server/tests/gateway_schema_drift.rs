//! The gateway-protocol drift gate (EP-04-S01 AC5): the committed
//! artifacts must equal what the Rust types generate, so a protocol
//! change without a regeneration fails here, not in production.

use std::path::{Path, PathBuf};

use rusty_agent_server::gateway_schema::{
    protocol_schema_bundle, render_ts_types, schema_artifact_bytes, SCHEMA_ARTIFACT, TS_ARTIFACT,
};

/// The workspace root: the parent of this crate's manifest dir.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("rusty-server lives one level below the workspace root")
        .to_owned()
}

#[test]
fn schema_artifacts_are_current() {
    let root = workspace_root();
    let regenerated = [
        (SCHEMA_ARTIFACT, schema_artifact_bytes()),
        (
            TS_ARTIFACT,
            render_ts_types(&protocol_schema_bundle()).expect("the protocol schemas render"),
        ),
    ];
    for (artifact, expected) in regenerated {
        let committed = std::fs::read_to_string(root.join(artifact)).unwrap_or_else(|error| {
            panic!("{artifact}: cannot read the committed artifact: {error}")
        });
        assert_eq!(
            committed, expected,
            "{artifact}: the committed artifact has drifted from the Rust protocol types — \
             regenerate with `cargo run -p rusty-agent-server --bin gateway_protocol_schema` \
             and commit the result"
        );
    }
}

#[test]
fn the_committed_client_carries_the_contract_vocabulary() {
    let committed = std::fs::read_to_string(workspace_root().join(TS_ARTIFACT))
        .expect("the committed TypeScript artifact reads");
    // The wire vocabulary the contract pins: frame tags, the idempotency
    // option, the flattened outcome intersection, the pairing steps, the
    // lease interface. A hand-written stub that matched a gutted
    // generator would pass the byte gate — this content gate does not.
    for fragment in [
        "{ frame: \"request\";",
        "{ frame: \"response\";",
        "{ frame: \"event\"; name: string; payload: unknown; seq: number; }",
        "idempotency_key?: string | null;",
        "} & ({ ok: { result: unknown; }; }",
        "step: \"hello\";",
        "export interface TurnLease {",
    ] {
        assert!(
            committed.contains(fragment),
            "{TS_ARTIFACT}: missing `{fragment}`"
        );
    }
}
