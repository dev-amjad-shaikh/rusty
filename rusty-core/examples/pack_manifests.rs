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
//! The ServiceNow pack is instance-agnostic: it declares an `instance`
//! config param and pins `https://{instance}.service-now.com`; the
//! subdomain arrives at instantiation (`POST /connectors/instances` with
//! `config`), never in the manifest. Manifests are content-addressed, so
//! re-seeding the same server is idempotent — the plane answers
//! `already_registered: true` for anything it already holds.

use rusty_agent_runtime::connector::{manifest::ConnectorManifest, packs};

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let manifests: Vec<ConnectorManifest> = vec![
        packs::servicenow()?,
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
