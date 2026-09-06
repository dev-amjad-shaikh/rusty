//! Skill promotion types: `eval-gate` frontmatter parsing and
//! `SkillPromotion` serde round-trip.

use rusty_agent_runtime::skill::{
    ScaffoldAttribution, ScaffoldComponent, SkillPackage, SkillPromotion, SkillPromotionStatus,
    attribution_diff,
};

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
        gate_name: Some("billing-regression".to_owned()),
        gate_version: Some("1.4.0".to_owned()),
        attribution: Some(ScaffoldAttribution {
            prompt_tier_hash: Some("sha256:prefix".to_owned()),
            memory_high_water: Some(41),
            skill_pack_version: Some("2.1.0".to_owned()),
            model_stamp: Some("openai/gpt-4".to_owned()),
        }),
        changed_from_baseline: Some(vec![
            ScaffoldComponent::PromptTierHash,
            ScaffoldComponent::ModelStamp,
        ]),
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
    assert_eq!(decoded.gate_name, original.gate_name);
    assert_eq!(decoded.gate_version, original.gate_version);
    assert_eq!(decoded.attribution, original.attribution);
    assert_eq!(
        decoded.changed_from_baseline,
        original.changed_from_baseline
    );
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
    assert_eq!(promotion.attribution, None);
    assert_eq!(promotion.changed_from_baseline, None);
    assert_eq!(promotion.status, SkillPromotionStatus::Draft);
}

#[test]
fn skill_promotion_deserializes_without_gate_name() {
    // History written before the regression pack (EP-17-S02) carries no
    // gate name; it must still load, with the pack treating the record as
    // predating gate tracking.
    let json = r#"{"name":"a-skill","revision":1,"content_hash":"hash","status":"promoted","gate_run_id":"run-1","author":"dev","created_at":"2024-01-01T00:00:00Z"}"#;
    let promotion: SkillPromotion = serde_json::from_str(json).expect("deserializes");
    assert_eq!(promotion.gate_name, None);
    assert_eq!(promotion.gate_version, None);
    assert_eq!(promotion.status, SkillPromotionStatus::Promoted);
}

#[test]
fn skill_promotion_deserializes_without_gate_version() {
    // History written before held-out enforcement (EP-17-S03) carries a
    // gate name but no suite version; the bump comparison treats the
    // version as unknown rather than failing to load.
    let json = r#"{"name":"a-skill","revision":2,"content_hash":"hash","status":"promoted","gate_run_id":"run-2","gate_name":"suite-a","author":"dev","created_at":"2024-01-01T00:00:00Z"}"#;
    let promotion: SkillPromotion = serde_json::from_str(json).expect("deserializes");
    assert_eq!(promotion.gate_name.as_deref(), Some("suite-a"));
    assert_eq!(promotion.gate_version, None);
    assert_eq!(promotion.attribution, None);
    assert_eq!(promotion.changed_from_baseline, None);
}

// --------------------------------------------------------------------- //
// Scaffold attribution diff (EP-17-S04)
// --------------------------------------------------------------------- //

fn attribution(
    prompt_tier_hash: Option<&str>,
    memory_high_water: Option<u64>,
    skill_pack_version: Option<&str>,
    model_stamp: Option<&str>,
) -> ScaffoldAttribution {
    ScaffoldAttribution {
        prompt_tier_hash: prompt_tier_hash.map(str::to_owned),
        memory_high_water,
        skill_pack_version: skill_pack_version.map(str::to_owned),
        model_stamp: model_stamp.map(str::to_owned),
    }
}

#[test]
fn attribution_diff_names_each_changed_component() {
    let candidate = attribution(Some("b"), Some(2), Some("2.0.0"), Some("openai/gpt-4o"));
    let baseline = attribution(Some("a"), Some(1), Some("1.0.0"), Some("openai/gpt-4"));
    assert_eq!(
        attribution_diff(&candidate, &baseline),
        vec![
            ScaffoldComponent::PromptTierHash,
            ScaffoldComponent::MemoryHighWater,
            ScaffoldComponent::SkillPackVersion,
            ScaffoldComponent::ModelStamp,
        ]
    );
}

#[test]
fn attribution_diff_reports_declaration_order() {
    // Model stamp (declared last) and prompt tier hash (declared first)
    // changed; the diff lists them in declaration order, not discovery
    // order.
    let candidate = attribution(Some("b"), Some(1), Some("1.0.0"), Some("openai/gpt-4o"));
    let baseline = attribution(Some("a"), Some(1), Some("1.0.0"), Some("openai/gpt-4"));
    assert_eq!(
        attribution_diff(&candidate, &baseline),
        vec![
            ScaffoldComponent::PromptTierHash,
            ScaffoldComponent::ModelStamp,
        ]
    );
}

#[test]
fn attribution_diff_is_empty_for_identical_scaffolds() {
    let candidate = attribution(Some("a"), Some(1), Some("1.0.0"), Some("openai/gpt-4"));
    let baseline = candidate.clone();
    assert_eq!(attribution_diff(&candidate, &baseline), Vec::new());
}

#[test]
fn attribution_diff_treats_one_sided_unknown_as_unprovable() {
    // A field absent on either side cannot be proven to have changed, so
    // it is not reported — only two known, different values count.
    let candidate = attribution(Some("a"), Some(2), None, Some("openai/gpt-4"));
    let baseline = attribution(Some("a"), None, Some("1.0.0"), Some("openai/gpt-4"));
    assert_eq!(attribution_diff(&candidate, &baseline), Vec::new());
}

#[test]
fn scaffold_component_serde_snake_case() {
    assert_eq!(
        serde_json::to_string(&ScaffoldComponent::PromptTierHash).unwrap(),
        "\"prompt_tier_hash\""
    );
    assert_eq!(
        serde_json::to_string(&ScaffoldComponent::MemoryHighWater).unwrap(),
        "\"memory_high_water\""
    );
    assert_eq!(
        serde_json::to_string(&ScaffoldComponent::SkillPackVersion).unwrap(),
        "\"skill_pack_version\""
    );
    assert_eq!(
        serde_json::to_string(&ScaffoldComponent::ModelStamp).unwrap(),
        "\"model_stamp\""
    );
}
