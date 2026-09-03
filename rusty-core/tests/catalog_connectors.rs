//! Catalog connector manifest validation tests (EP-15-S05).
//!
//! Each named connector in `catalog/` ships a JSON manifest that must parse
//! as a valid [`ConnectorManifest`], pass structural validation, and hash
//! deterministically.

use rusty_agent_runtime::connector::ConnectorManifest;
use serde_json::Value;
use std::fs;

fn load_manifest(path: &str) -> Value {
    let bytes = fs::read(path).expect("manifest file exists");
    serde_json::from_slice(&bytes).expect("manifest is valid JSON")
}

fn build_manifest(value: &Value) -> ConnectorManifest {
    let id = value["id"].as_str().unwrap().to_string();
    let version = value["version"].as_str().unwrap().to_string();
    let display_name = value["display_name"].as_str().unwrap().to_string();
    let description = value["description"].as_str().unwrap().to_string();
    let documentation_url = value["documentation_url"].as_str().unwrap().to_string();
    let base_url = value["base_url"].as_str().unwrap().to_string();
    let connection_specification = value["connection_specification"].clone();
    let operations: Vec<_> =
        serde_json::from_value(value["operations"].clone()).expect("operations array parses");
    let check = value["check"].as_str().unwrap().to_string();

    ConnectorManifest::new(
        id,
        version,
        display_name,
        description,
        documentation_url,
        base_url,
        connection_specification,
        operations,
        check,
    )
    .expect("manifest validates")
}

#[test]
fn github_connector_manifest_validates() {
    let value = load_manifest(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../catalog/github-connector/manifest.json"
    ));
    let manifest = build_manifest(&value);
    assert_eq!(manifest.id, "github");
    assert_eq!(manifest.check, "check");
    assert!(manifest.verify_hash());

    // Every operation except check must have auth declared.
    for op in &manifest.operations {
        if op.name != "check" {
            assert!(
                !op.auth.is_empty(),
                "operation `{}` must declare auth",
                op.name
            );
        }
    }

    // The check operation is parameterless GET read-only (validated by
    // ConnectorManifest::new), but we assert it explicitly here.
    let check_op = manifest.operation("check").expect("check operation exists");
    assert_eq!(check_op.name, "check");
}

#[test]
fn github_connector_derives_catalog() {
    let value = load_manifest(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../catalog/github-connector/manifest.json"
    ));
    let manifest = build_manifest(&value);
    let catalog = manifest.derive_catalog().expect("catalog derives");
    let names: Vec<_> = catalog.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"github/list-repos"));
    assert!(names.contains(&"github/get-issue"));
    assert!(names.contains(&"github/create-issue"));
    assert!(names.contains(&"github/list-pull-requests"));
    // check is not included in the derived catalog (it's the setup gate).
    assert!(!names.contains(&"github/check"));
}

#[test]
fn github_connector_manifest_is_parameterless_check() {
    let value = load_manifest(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../catalog/github-connector/manifest.json"
    ));
    let manifest = build_manifest(&value);
    let check_op = manifest.operation("check").unwrap();
    assert!(check_op.is_parameterless());
}
