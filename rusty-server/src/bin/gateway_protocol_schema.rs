//! Regenerate the committed gateway-protocol artifacts (EP-04-S01 AC5):
//! the JSON Schema bundle the server validates frames against and the
//! TypeScript declarations the client compiles.
//!
//! ```sh
//! cargo run -p rusty-agent-server --bin gateway_protocol_schema
//! ```
//!
//! The drift gate (`tests/gateway_schema_drift.rs`) regenerates in memory
//! and diffs against the committed files, so a protocol type change
//! without a regeneration fails the build.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

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

/// Write `path` when the content differs; report what happened.
fn write_artifact(path: &Path, content: &str) -> std::io::Result<&'static str> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if existing == content {
            return Ok("unchanged");
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok("written")
}

fn main() -> ExitCode {
    let bundle = protocol_schema_bundle();
    let ts = match render_ts_types(&bundle) {
        Ok(ts) => ts,
        Err(error) => {
            eprintln!("gateway-protocol TS render failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let root = workspace_root();
    for (artifact, content) in [
        (SCHEMA_ARTIFACT, schema_artifact_bytes()),
        (TS_ARTIFACT, ts),
    ] {
        let path = root.join(artifact);
        match write_artifact(&path, &content) {
            Ok(outcome) => println!("{artifact}: {outcome}"),
            Err(error) => {
                eprintln!("{artifact}: {error}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
