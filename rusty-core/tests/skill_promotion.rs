//! Skill promotion types: `eval-gate` frontmatter parsing and
//! `SkillPromotion` serde round-trip.

use rusty_agent_runtime::skill::{SkillPackage, SkillPromotion, SkillPromotionStatus};

// --------------------------------------------------------------------- //
// eval-gate frontmatter parsing
// --------------------------------------------------------------------- //

#[test]
fn eval_gate_parses_from_frontmatter() {
    let text = "---\nname: billing-handler\ndescription: Handles billing.\neval-gate: billing-regressions\n---\n\nProcess billing requests.\n";
    let package = SkillPackage::from_markdown(text).expect("valid package with eval-gate");
    assert_eq!(
        package.frontmatter().eval_gate.as_deref(),
        Some("billing-regressions")
    );
}

#[test]
fn eval_gate_is_optional() {
    let text = "---\nname: lookup-only\ndescription: Static lookup.\n---\n\nLookup data.\n";
    let package = SkillPackage::from_markdown(text).expect("valid package without eval-gate");
    assert_eq!(package.frontmatter().eval_gate, None);
}

#[test]
fn eval_gate_included_in_content_hash() {
    let without_gate = "---\nname: a-skill\ndescription: A skill.\n---\n\nBody.\n";
    let with_gate = "---\nname: a-skill\ndescription: A skill.\neval-gate: suite-a\n---\n\nBody.\n";
    let pkg_without = SkillPackage::from_markdown(without_gate).unwrap();
    let pkg_with = SkillPackage::from_markdown(with_gate).unwrap();
    assert_ne!(pkg_without.content_hash(), pkg_with.content_hash());
}

#[test]
fn eval_gate_rejects_empty_value() {
    let text = "---\nname: a-skill\ndescription: A skill.\neval-gate: \"\"\n---\n\nBody.\n";
    let result = SkillPackage::from_markdown(text);
    assert!(result.is_err(), "empty eval-gate must be rejected");
}

// --------------------------------------------------------------------- //
// SkillPromotion serde round-trip
// --------------------------------------------------------------------- //

#[test]
fn skill_promotion_serde_round_trip() {
    let original = SkillPromotion {
        name: "billing-handler".to_owned(),
        revision: 3,
        content_hash: "abc123".to_owned(),
        status: SkillPromotionStatus::Promoted,
        gate_run_id: Some("run-42".to_owned()),
        author: "operator:ada".to_owned(),
        created_at: chrono::Utc::now(),
    };
    let json = serde_json::to_string(&original).expect("serializes");
    let decoded: SkillPromotion = serde_json::from_str(&json).expect("deserializes");
    assert_eq!(decoded.name, original.name);
    assert_eq!(decoded.revision, original.revision);
    assert_eq!(decoded.content_hash, original.content_hash);
    assert_eq!(decoded.status, original.status);
    assert_eq!(decoded.gate_run_id, original.gate_run_id);
    assert_eq!(decoded.author, original.author);
}

#[test]
fn skill_promotion_status_serde_snake_case() {
    assert_eq!(
        serde_json::to_string(&SkillPromotionStatus::Draft).unwrap(),
        "\"draft\""
    );
    assert_eq!(
        serde_json::to_string(&SkillPromotionStatus::Trial).unwrap(),
        "\"trial\""
    );
    assert_eq!(
        serde_json::to_string(&SkillPromotionStatus::Promoted).unwrap(),
        "\"promoted\""
    );
}

#[test]
fn skill_promotion_deserializes_without_gate_run_id() {
    let json = r#"{"name":"a-skill","revision":1,"content_hash":"hash","status":"draft","author":"dev","created_at":"2024-01-01T00:00:00Z"}"#;
    let promotion: SkillPromotion = serde_json::from_str(json).expect("deserializes");
    assert_eq!(promotion.gate_run_id, None);
    assert_eq!(promotion.status, SkillPromotionStatus::Draft);
}
