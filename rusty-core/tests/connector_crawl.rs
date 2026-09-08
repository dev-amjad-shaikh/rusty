//! Supply-crawl normalization tests (EP-07-S07): connector-pulled
//! knowledge records into `SupplyArtifact`s — the field mapping and its
//! fallbacks, the typed refusals (missing identity, missing title, bad
//! timestamp, unknown kind), and the all-or-nothing corpus rule.

use rusty_agent_runtime::connector::{
    CrawlError, normalize_supply_corpus, normalize_supply_record, parse_artifact_kind,
};
use rusty_agent_runtime::induction::ArtifactKind;
use serde_json::json;

#[test]
fn a_full_kb_record_maps_every_field() {
    let artifact = normalize_supply_record(
        ArtifactKind::KbArticle,
        &json!({
            "sys_id": "KB001",
            "short_description": "vpn connect home office certificate error",
            "text": "Check the ZTNA certificate logs.",
            "sys_updated_on": "2026-07-15T10:30:00Z",
            "u_systems": "ztna, legacy-erp"
        }),
    )
    .unwrap();

    assert_eq!(artifact.artifact_id, "KB001");
    assert_eq!(artifact.kind, ArtifactKind::KbArticle);
    assert_eq!(artifact.title, "vpn connect home office certificate error");
    assert_eq!(artifact.body, "Check the ZTNA certificate logs.");
    assert_eq!(
        artifact.last_revised.unwrap().to_rfc3339(),
        "2026-07-15T10:30:00+00:00"
    );
    assert_eq!(artifact.systems_referenced, vec!["ztna", "legacy-erp"]);
}

#[test]
fn the_generic_fallbacks_carry_a_rest_export() {
    let artifact = normalize_supply_record(
        ArtifactKind::Runbook,
        &json!({
            "id": "RB-7",
            "name": "Restart the batch fleet",
            "description": "Drain, restart, verify.",
            "updated_on": "2026-08-01T00:00:00Z",
            "systems": ["batch-fleet"]
        }),
    )
    .unwrap();

    assert_eq!(artifact.artifact_id, "RB-7");
    assert_eq!(artifact.title, "Restart the batch fleet");
    assert_eq!(artifact.body, "Drain, restart, verify.");
    assert!(artifact.last_revised.is_some());
    assert_eq!(artifact.systems_referenced, vec!["batch-fleet"]);
}

#[test]
fn a_missing_body_is_empty_text_not_an_error() {
    let artifact = normalize_supply_record(
        ArtifactKind::Sop,
        &json!({"sys_id": "SOP-1", "short_description": "Title-only procedure"}),
    )
    .unwrap();
    assert_eq!(artifact.body, "");
    assert!(artifact.last_revised.is_none());
    assert!(artifact.systems_referenced.is_empty());
}

#[test]
fn a_record_without_identity_is_refused() {
    let error = normalize_supply_record(
        ArtifactKind::KbArticle,
        &json!({"short_description": "no id here"}),
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            CrawlError::MissingField {
                field: "sys_id",
                ..
            }
        ),
        "expected MissingField(sys_id), got {error}"
    );
}

#[test]
fn a_record_without_a_title_is_refused() {
    let error = normalize_supply_record(
        ArtifactKind::KbArticle,
        &json!({"sys_id": "KB-X", "short_description": "   "}),
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            CrawlError::MissingField {
                field: "short_description",
                ..
            }
        ),
        "expected MissingField(short_description), got {error}"
    );
}

#[test]
fn an_unparseable_revision_date_is_refused() {
    let error = normalize_supply_record(
        ArtifactKind::KbArticle,
        &json!({
            "sys_id": "KB-Y",
            "short_description": "dated badly",
            "sys_updated_on": "last Tuesday"
        }),
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            CrawlError::BadTimestamp {
                field: "sys_updated_on",
                ..
            }
        ),
        "expected BadTimestamp(sys_updated_on), got {error}"
    );
}

#[test]
fn an_unknown_kind_is_a_typed_refusal() {
    assert_eq!(
        parse_artifact_kind("kb_article").unwrap(),
        ArtifactKind::KbArticle
    );
    assert_eq!(parse_artifact_kind("macro").unwrap(), ArtifactKind::Macro);
    let error = parse_artifact_kind("wiki_page").unwrap_err();
    assert!(
        matches!(error, CrawlError::UnknownKind(ref kind) if kind == "wiki_page"),
        "expected UnknownKind(wiki_page), got {error}"
    );
}

#[test]
fn one_bad_record_fails_the_whole_corpus() {
    let corpus = vec![
        json!({"sys_id": "KB-1", "short_description": "good record"}),
        json!({"short_description": "no identity"}),
    ];
    let error = normalize_supply_corpus(ArtifactKind::KbArticle, &corpus).unwrap_err();
    assert!(
        matches!(
            error,
            CrawlError::MissingField {
                field: "sys_id",
                ..
            }
        ),
        "expected the batch to fail on the bad record, got {error}"
    );

    // And a clean corpus preserves record order.
    let artifacts = normalize_supply_corpus(
        ArtifactKind::KbArticle,
        &[
            json!({"sys_id": "KB-2", "short_description": "second"}),
            json!({"sys_id": "KB-1", "short_description": "first"}),
        ],
    )
    .unwrap();
    let ids: Vec<&str> = artifacts.iter().map(|a| a.artifact_id.as_str()).collect();
    assert_eq!(ids, vec!["KB-2", "KB-1"]);
}
