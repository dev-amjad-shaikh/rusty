//! Print the built-in connector service pack manifests as JSON, one per
//! line, ready to publish into a running server's connector plane:
//!
//! ```sh
//! cargo run --example pack_manifests | while read -r manifest; do
//!   curl -s -X POST localhost:8100/connectors/manifests \
//!     -H 'content-type: application/json' -d "$manifest" | jq .manifest_hash
//! done
//! ```
//!
//! The ServiceNow pack is instance-scoped: pass the instance label as the
//! first argument (default `example`, which yields
//! `https://example.service-now.com`). Manifests are content-addressed, so
//! re-seeding the same server is idempotent — the plane answers
//! `already_registered: true` for anything it already holds.

use rusty_agent_runtime::connector::{manifest::ConnectorManifest, packs};

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let instance = std::env::args().nth(1).unwrap_or_else(|| "example".to_owned());
    let manifests: Vec<ConnectorManifest> = vec![
        packs::servicenow(&instance)?,
        packs::gmail()?,
        packs::slack()?,
        packs::linear()?,
        packs::notion()?,
        packs::google_calendar()?,
    ];
    for manifest in &manifests {
        println!("{}", serde_json::to_string(manifest)?);
    }
    Ok(())
}
