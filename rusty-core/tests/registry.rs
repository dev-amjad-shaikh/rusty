//! Configuration-registry integration tests (R0.11 Extension Plane,
//! wave 1).
//!
//! Four test groups:
//!
//! - **Golden files** — the serialized shapes of the four new
//!   `CandidateContent` variants (tool contract, model settings, memory
//!   configuration, middleware composition), the `ArtifactRecord`, the two
//!   diff views, and the R0.11-grown `PromotionEnvelope` are pinned
//!   against checked-in JSON under `tests/golden/`. Any accidental
//!   contract drift fails here; `UPDATE_GOLDEN=1` blesses an intentional
//!   change, the `tests/learn.rs` discipline.
//! - **R0.8 byte-stability** — the pre-existing golden fixtures
//!   (candidate, evaluation, envelope, receipts, pointer) deserialize
//!   through the grown contract and re-serialize byte-identically: the
//!   R0.8 wire shapes did not move, and R0.8-era envelopes (which carry
//!   no registry-kind fields) gain their approval defaults on read.
//! - **Surfaces and tags** — the new kinds' surface keys,
//!   `surface_for_kind` agreeing with `Candidate::surface` for every
//!   kind, and environment-tag validation plus the `tagged` /
//!   `split_tag` round trip.
//! - **Artifacts and diffs** — the naming rules, the commit guards
//!   (family, surface, duplicate), the prompt line diff, the structural
//!   canonical-JSON diff (a reordered object is not a change; a
//!   reordered middleware layer list is), and the cross-kind refusal.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

use rusty_agent_runtime::learn::{
    surface_for_kind, AutoPromotion, CanaryBinding, Candidate, CandidateContent,
    CandidateEvaluation, CandidateId, CandidateKind, EnvelopeRule, EnvironmentTag, EvidenceSpan,
    MiddlewareLayerConfig, PromotionEnvelope, PromotionReceipt, RollbackReceipt, VersionPointer,
};
use rusty_agent_runtime::memory::{
    ContextBudget, MemoryKind, MemoryQuery, ProvenanceAuthor, MEMORY_SCHEMA_VERSION,
};
use rusty_agent_runtime::record::{sha256_hex, DecisionFamily, RunEventKind, RunManifest};
use rusty_agent_runtime::registry::{
    diff_candidates, pointer_admission, resolution_pin, ArtifactCommit, ArtifactRecord,
    ConfigResolution, LeafChange, LeafModification, PointerBinding, RegistryDiff, RegistryError,
    TextDiffLine,
};

// ---------- golden-file machinery (the tests/learn.rs discipline) ----------

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(name)
}

/// Assert the pretty-printed serialization of `value` equals the golden
/// file's content exactly. `UPDATE_GOLDEN=1` rewrites the file instead —
/// the diff is then the contract change under review.
fn assert_golden(name: &str, value: &impl Serialize) {
    let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    assert_eq!(
        rendered,
        expected,
        "contract drift in `{}` — if intentional, re-run with UPDATE_GOLDEN=1 \
         and review the diff",
        path.display()
    );
}

// ---------- shared fixtures ----------

fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

/// Registry commits are operator-authored: `human:{id}` attribution, the
/// correction loop's discipline applied to configuration.
fn operator() -> ProvenanceAuthor {
    ProvenanceAuthor::Human {
        human_id: "amjad".into(),
    }
}

fn tool_contract_candidate() -> Candidate {
    Candidate::new(
        CandidateContent::ToolContract {
            tool: "web_search".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "default": 5},
                },
                "required": ["query"],
            }),
        },
        operator(),
        EvidenceSpan::default(),
        ts(1_760_000_002_000),
    )
    .unwrap()
}

fn model_settings_candidate() -> Candidate {
    Candidate::new(
        CandidateContent::ModelSettings {
            name: "primary".into(),
            model: "gpt-5.2-2026-06-01".into(),
            parameters: json!({"temperature": 0.2, "seed": 42, "max_tokens": 2048}),
        },
        operator(),
        EvidenceSpan::default(),
        ts(1_760_000_002_000),
    )
    .unwrap()
}

fn memory_configuration_candidate() -> Candidate {
    Candidate::new(
        CandidateContent::MemoryConfiguration {
            name: "default".into(),
            budget: ContextBudget::new(4096),
            default_filters: MemoryQuery {
                kinds: vec![MemoryKind::Fact, MemoryKind::Preference],
                tags: vec!["support".into()],
                ..MemoryQuery::default()
            },
            schema_version: MEMORY_SCHEMA_VERSION.to_owned(),
        },
        operator(),
        EvidenceSpan::default(),
        ts(1_760_000_002_000),
    )
    .unwrap()
}

fn middleware_composition_candidate() -> Candidate {
    Candidate::new(
        CandidateContent::MiddlewareComposition {
            name: "default".into(),
            layers: vec![
                MiddlewareLayerConfig {
                    layer: "request_logger".into(),
                    config: None,
                },
                MiddlewareLayerConfig {
                    layer: "tool_call_blocklist".into(),
                    config: Some(json!({"blocked": ["shell", "fs_write"]})),
                },
            ],
        },
        operator(),
        EvidenceSpan::default(),
        ts(1_760_000_002_000),
    )
    .unwrap()
}

fn prompt_candidate(text: &str) -> Candidate {
    Candidate::new(
        CandidateContent::Prompt {
            name: "system".into(),
            prompt: text.into(),
        },
        operator(),
        EvidenceSpan::default(),
        ts(1_760_000_002_000),
    )
    .unwrap()
}

fn artifact() -> ArtifactRecord {
    let mut record = ArtifactRecord::new(
        CandidateKind::Prompt,
        "system",
        operator(),
        ts(1_760_000_000_000),
    )
    .unwrap();
    record.commits.push(ArtifactCommit {
        candidate_id: CandidateId::from("a".repeat(64)),
        committed_at: ts(1_760_000_001_000),
    });
    record.commits.push(ArtifactCommit {
        candidate_id: CandidateId::from("b".repeat(64)),
        committed_at: ts(1_760_000_003_000),
    });
    record
}

// ---------- golden files ----------

#[test]
fn golden_candidate_tool_contract_shape() {
    assert_golden("candidate_tool_contract.json", &tool_contract_candidate());
}

#[test]
fn golden_candidate_model_settings_shape() {
    assert_golden("candidate_model_settings.json", &model_settings_candidate());
}

#[test]
fn golden_candidate_memory_configuration_shape() {
    assert_golden(
        "candidate_memory_configuration.json",
        &memory_configuration_candidate(),
    );
}

#[test]
fn golden_candidate_middleware_composition_shape() {
    assert_golden(
        "candidate_middleware_composition.json",
        &middleware_composition_candidate(),
    );
}

#[test]
fn golden_registry_artifact_shape() {
    assert_golden("registry_artifact.json", &artifact());
}

#[test]
fn golden_registry_diff_text_shape() {
    let from = prompt_candidate("You are a careful support agent.\nAnswer tersely.\nBe kind.");
    let to = prompt_candidate(
        "You are a careful support agent.\nAnswer fully.\nCite sources.\nBe kind.",
    );
    let diff = diff_candidates(&from, &to).unwrap();
    assert_golden("registry_diff_text.json", &diff);
}

#[test]
fn golden_registry_diff_structural_shape() {
    let from = model_settings_candidate();
    let to = Candidate::new(
        CandidateContent::ModelSettings {
            name: "primary".into(),
            model: "gpt-5.2-2026-06-01".into(),
            // temperature changed, seed removed, top_p added.
            parameters: json!({"temperature": 0.7, "max_tokens": 2048, "top_p": 0.9}),
        },
        operator(),
        EvidenceSpan::default(),
        ts(1_760_000_004_000),
    )
    .unwrap();
    let diff = diff_candidates(&from, &to).unwrap();
    assert_golden("registry_diff_structural.json", &diff);
}

#[test]
fn golden_promotion_envelope_registry_shape() {
    // The R0.11-grown envelope with registry-kind rules declared:
    // non-approval rules appear on the wire; the approval default
    // (memory_configuration, deliberately) stays absent.
    let envelope = PromotionEnvelope {
        envelope_version: "acme-r011-1".into(),
        prompt: EnvelopeRule::Approval,
        policy: EnvelopeRule::Approval,
        memory_set: EnvelopeRule::Approval,
        tool_permission: EnvelopeRule::Approval,
        tool_contract: EnvelopeRule::Approval,
        model_settings: EnvelopeRule::Canary {
            fraction: 0.1,
            auto: AutoPromotion {
                dataset_version: Some("support-v4".into()),
                min_improvement: 0.01,
                scopes: Vec::new(),
            },
        },
        memory_configuration: EnvelopeRule::Approval,
        middleware_composition: EnvelopeRule::Auto(AutoPromotion {
            dataset_version: None,
            min_improvement: 0.0,
            scopes: Vec::new(),
        }),
    };
    assert_golden("promotion_envelope_registry.json", &envelope);
}

// ---------- R0.8 byte-stability ----------

/// Read a pre-R0.11 golden fixture, deserialize it through the grown
/// contract, and re-serialize: byte-identical output is the proof that
/// the R0.8 wire shape did not move (the wave's exit criterion: R0.8
/// candidate and pointer records keep deserializing).
fn assert_r08_golden_round_trip<T>(name: &str)
where
    T: serde::de::DeserializeOwned + Serialize,
{
    let path = golden_path(name);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden file `{}`: {e}", path.display()));
    let value: T = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("R0.8 fixture `{name}` no longer deserializes: {e}"));
    let rendered = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
    assert_eq!(
        rendered, raw,
        "R0.8 fixture `{name}` does not round-trip byte-identically — the contract moved"
    );
}

#[test]
fn r08_golden_fixtures_keep_deserializing_byte_stable() {
    assert_r08_golden_round_trip::<Candidate>("candidate.json");
    assert_r08_golden_round_trip::<Candidate>("candidate_prompt.json");
    assert_r08_golden_round_trip::<CandidateEvaluation>("candidate_evaluation.json");
    assert_r08_golden_round_trip::<PromotionEnvelope>("promotion_envelope.json");
    assert_r08_golden_round_trip::<PromotionReceipt>("promotion_receipt.json");
    assert_r08_golden_round_trip::<RollbackReceipt>("rollback_receipt.json");
    assert_r08_golden_round_trip::<VersionPointer>("version_pointer.json");
}

#[test]
fn r08_envelope_gains_approval_defaults_for_registry_kinds() {
    // An R0.8-era envelope carries no registry-kind fields; on read the
    // four new rules default to approval — a schema or ordering change is
    // a contract judgment, and the honest default is a human's name.
    let raw = std::fs::read_to_string(golden_path("promotion_envelope.json")).unwrap();
    let envelope: PromotionEnvelope = serde_json::from_str(&raw).unwrap();
    for kind in [
        CandidateKind::ToolContract,
        CandidateKind::ModelSettings,
        CandidateKind::MemoryConfiguration,
        CandidateKind::MiddlewareComposition,
    ] {
        assert_eq!(envelope.rule_for(kind), &EnvelopeRule::Approval);
    }
    envelope.validate().unwrap();
    // And the serialization stayed byte-stable: approval rules are absent
    // from the wire, so the grown envelope writes the R0.8 shape.
    let rendered = format!("{}\n", serde_json::to_string_pretty(&envelope).unwrap());
    assert_eq!(rendered, raw);
}

// ---------- surfaces and tags ----------

#[test]
fn new_kinds_carry_typed_surfaces_and_verifiable_addresses() {
    let cases = [
        (tool_contract_candidate(), "tool_contract:web_search"),
        (model_settings_candidate(), "model_settings:primary"),
        (memory_configuration_candidate(), "memory_config:default"),
        (middleware_composition_candidate(), "middleware:default"),
    ];
    for (candidate, surface) in cases {
        assert_eq!(candidate.surface().as_str(), surface);
        candidate.verify_address().unwrap();
    }
    // Two submissions of the same change converge on one id — identity is
    // integrity, unchanged for the new kinds.
    assert_eq!(
        tool_contract_candidate().candidate_id,
        tool_contract_candidate().candidate_id
    );
}

#[test]
fn surface_for_kind_agrees_with_candidate_surface_for_every_kind() {
    let memory_scope = rusty_agent_runtime::memory::ScopeAddress::new(
        rusty_agent_runtime::memory::MemoryScope::Agent,
        "support-1",
    );
    let candidates: Vec<(Candidate, CandidateKind, &str)> = vec![
        (prompt_candidate("p"), CandidateKind::Prompt, "system"),
        (
            Candidate::new(
                CandidateContent::Policy {
                    family: DecisionFamily::Retry,
                    parameters: json!({"max_attempts": 3}),
                },
                operator(),
                EvidenceSpan::default(),
                ts(1_760_000_002_000),
            )
            .unwrap(),
            CandidateKind::Policy,
            "retry",
        ),
        (
            Candidate::new(
                CandidateContent::MemorySet {
                    scope: memory_scope.clone(),
                    adds: Vec::new(),
                    supersedes: Vec::new(),
                },
                operator(),
                EvidenceSpan::default(),
                ts(1_760_000_002_000),
            )
            .unwrap(),
            CandidateKind::MemorySet,
            "agent:support-1",
        ),
        (
            Candidate::new(
                CandidateContent::ToolPermission {
                    tool: "shell".into(),
                    direction: rusty_agent_runtime::learn::GrantDirection::Narrow,
                },
                operator(),
                EvidenceSpan::default(),
                ts(1_760_000_002_000),
            )
            .unwrap(),
            CandidateKind::ToolPermission,
            "shell",
        ),
        (
            tool_contract_candidate(),
            CandidateKind::ToolContract,
            "web_search",
        ),
        (
            model_settings_candidate(),
            CandidateKind::ModelSettings,
            "primary",
        ),
        (
            memory_configuration_candidate(),
            CandidateKind::MemoryConfiguration,
            "default",
        ),
        (
            middleware_composition_candidate(),
            CandidateKind::MiddlewareComposition,
            "default",
        ),
    ];
    for (candidate, kind, name) in candidates {
        assert_eq!(
            surface_for_kind(kind, name),
            candidate.surface(),
            "{kind} must surface identically through both paths"
        );
    }
}

#[test]
fn environment_tags_validate_and_round_trip_through_surfaces() {
    let tag = EnvironmentTag::new("prod").unwrap();
    let base = prompt_candidate("p").surface();
    let tagged = base.tagged(&tag);
    assert_eq!(tagged.as_str(), "prompt:system@prod");
    let (split_base, split_tag) = tagged.split_tag();
    assert_eq!(split_base, base);
    assert_eq!(split_tag, Some(tag));
    // The untagged surface splits to itself with no tag.
    let (untagged, none) = base.split_tag();
    assert_eq!(untagged, base);
    assert_eq!(none, None);
    // Deployment-declared, not enumerated: any clean string is a tag.
    assert!(EnvironmentTag::new("eu-west-canary").is_ok());
    // Refused: empty, whitespace, the separators, control characters.
    for bad in ["", "pro d", "prod@x", "acme/prod", "pro\td", "p\n"] {
        assert!(EnvironmentTag::new(bad).is_err(), "tag {bad:?} must refuse");
    }
    // Malformed tags fail at deserialization (the validated-at-the-
    // boundary rule), so no payload path holds an unvalidated one.
    assert!(serde_json::from_value::<EnvironmentTag>(json!("pro d")).is_err());
}

// ---------- artifacts and diffs ----------

#[test]
fn artifact_declaration_enforces_the_naming_rules() {
    for (name, why) in [
        ("", "empty"),
        (" sys", "leading whitespace"),
        ("sys ", "trailing whitespace"),
        ("prompt@prod", "the tag separator"),
        ("acme/system", "the tenant separator"),
        ("sy\nstem", "a control character"),
    ] {
        let result = ArtifactRecord::new(CandidateKind::Prompt, name, operator(), ts(1));
        assert!(
            matches!(result, Err(RegistryError::InvalidName { .. })),
            "name {name:?} ({why}) must refuse"
        );
    }
    // Legal: the ordinary case and a memory-scope address (`:` is a name
    // character — scope addresses are names).
    assert!(ArtifactRecord::new(CandidateKind::Prompt, "system", operator(), ts(1)).is_ok());
    let memory = ArtifactRecord::new(
        CandidateKind::MemorySet,
        "agent:support-1",
        operator(),
        ts(1),
    )
    .unwrap();
    assert_eq!(memory.surface.as_str(), "memory:agent:support-1");
    assert_eq!(memory.name(), "agent:support-1");
    assert_eq!(artifact().name(), "system");
}

#[test]
fn admit_commit_guards_family_surface_and_duplicates() {
    let artifact = artifact();
    // A model-settings candidate is not this prompt artifact's family.
    assert!(matches!(
        artifact.admit_commit(&model_settings_candidate(), ts(5)),
        Err(RegistryError::FamilyMismatch { .. })
    ));
    // A prompt candidate for a *different* prompt surfaces elsewhere.
    let other = Candidate::new(
        CandidateContent::Prompt {
            name: "other".into(),
            prompt: "You are someone else.".into(),
        },
        operator(),
        EvidenceSpan::default(),
        ts(1_760_000_002_000),
    )
    .unwrap();
    assert!(matches!(
        artifact.admit_commit(&other, ts(5)),
        Err(RegistryError::SurfaceMismatch { .. })
    ));
    // The right candidate admits; re-admitting the same one duplicates.
    let prompt = prompt_candidate("You are a careful support agent.");
    let committed = artifact.admit_commit(&prompt, ts(5)).unwrap();
    assert_eq!(committed.candidate_id, prompt.candidate_id);
    let mut history = artifact.clone();
    history.commits.push(committed);
    assert!(matches!(
        history.admit_commit(&prompt, ts(6)),
        Err(RegistryError::DuplicateCommit { .. })
    ));
}

#[test]
fn prompt_versions_diff_as_lines() {
    let from = prompt_candidate("line one\nline two\nline three");
    let to = prompt_candidate("line one\nline 2\nline three\nline four");
    let diff = diff_candidates(&from, &to).unwrap();
    let RegistryDiff::Text { lines } = &diff else {
        panic!("a prompt diff is a text view");
    };
    assert_eq!(
        lines,
        &vec![
            TextDiffLine::Context("line one".into()),
            TextDiffLine::Removed("line two".into()),
            TextDiffLine::Added("line 2".into()),
            TextDiffLine::Context("line three".into()),
            TextDiffLine::Added("line four".into()),
        ]
    );
    assert!(!diff.is_empty());
    // Identical prompts diff to pure context.
    let same = diff_candidates(&from, &prompt_candidate("line one\nline two\nline three")).unwrap();
    assert!(same.is_empty());
}

#[test]
fn json_families_diff_as_canonical_leaves() {
    let from = model_settings_candidate();
    let to = Candidate::new(
        CandidateContent::ModelSettings {
            name: "primary".into(),
            model: "gpt-5.2-2026-06-01".into(),
            parameters: json!({"temperature": 0.7, "max_tokens": 2048, "top_p": 0.9}),
        },
        operator(),
        EvidenceSpan::default(),
        ts(1_760_000_004_000),
    )
    .unwrap();
    let diff = diff_candidates(&from, &to).unwrap();
    let RegistryDiff::Structural {
        added,
        removed,
        changed,
    } = &diff
    else {
        panic!("a model-settings diff is a structural view");
    };
    assert_eq!(
        added,
        &vec![LeafChange {
            path: "/parameters/top_p".into(),
            value: json!(0.9),
        }]
    );
    assert_eq!(
        removed,
        &vec![LeafChange {
            path: "/parameters/seed".into(),
            value: json!(42),
        }]
    );
    assert_eq!(
        changed,
        &vec![LeafModification {
            path: "/parameters/temperature".into(),
            from: json!(0.2),
            to: json!(0.7),
        }]
    );
}

#[test]
fn canonical_form_decides_what_counts_as_a_change() {
    // Object-key order is not a change: the two parameter sets serialize
    // differently but canonicalize equal, so the candidates converge on
    // one content address and the diff is empty.
    let reordered = Candidate::new(
        CandidateContent::ModelSettings {
            name: "primary".into(),
            model: "gpt-5.2-2026-06-01".into(),
            parameters: json!({"max_tokens": 2048, "seed": 42, "temperature": 0.2}),
        },
        operator(),
        EvidenceSpan::default(),
        ts(1_760_000_004_000),
    )
    .unwrap();
    let base = model_settings_candidate();
    assert_eq!(base.candidate_id, reordered.candidate_id);
    assert!(diff_candidates(&base, &reordered).unwrap().is_empty());

    // Array order *is* a change: a middleware composition's layer order
    // is the artifact.
    let swapped = Candidate::new(
        CandidateContent::MiddlewareComposition {
            name: "default".into(),
            layers: vec![
                MiddlewareLayerConfig {
                    layer: "tool_call_blocklist".into(),
                    config: Some(json!({"blocked": ["shell", "fs_write"]})),
                },
                MiddlewareLayerConfig {
                    layer: "request_logger".into(),
                    config: None,
                },
            ],
        },
        operator(),
        EvidenceSpan::default(),
        ts(1_760_000_004_000),
    )
    .unwrap();
    let diff = diff_candidates(&middleware_composition_candidate(), &swapped).unwrap();
    assert!(!diff.is_empty(), "a layer reorder must show as a change");
}

#[test]
fn diff_across_kinds_is_a_category_error() {
    assert!(matches!(
        diff_candidates(&prompt_candidate("p"), &model_settings_candidate()),
        Err(RegistryError::DiffAcrossKinds { .. })
    ));
}

#[test]
fn the_gate_covers_registry_kinds_through_their_envelope_rules() {
    // The default envelope rules every registry kind to approval: an
    // out-of-envelope promotion names the candidate's own effect id —
    // the scope check that makes approvals non-transferable, extended to
    // the new kinds by `rule_for`, not by a parallel gate.
    let envelope = PromotionEnvelope::r08_default();
    let outcome = rusty_agent_runtime::learn::admit_promotion(
        &envelope,
        &model_settings_candidate(),
        None,
        None,
    );
    // No evaluation on record: the gate refuses on evidence before the
    // envelope rule even reads — promotion is gated on evidence for the
    // registry kinds exactly as for the R0.8 ones.
    assert!(matches!(
        outcome,
        Err(rusty_agent_runtime::learn::LearnError::Refused(
            rusty_agent_runtime::learn::PromotionRefusal::NotEvaluated { .. }
        ))
    ));
}

// ---------- admission resolution (R0.11 wave 2) ----------

/// A tagged, active-slot resolution of the shared prompt artifact.
fn prompt_resolution() -> ConfigResolution {
    let candidate = prompt_candidate("You are a careful support agent.");
    let (digest, model) = resolution_pin(&candidate).unwrap();
    ConfigResolution {
        surface: candidate.surface(),
        tag: Some(EnvironmentTag::new("prod").unwrap()),
        candidate_id: candidate.candidate_id,
        pointer: PointerBinding::Active,
        digest,
        model,
    }
}

#[test]
fn golden_config_resolution_shape() {
    assert_golden("config_resolution.json", &prompt_resolution());
}

#[test]
fn golden_config_resolution_model_settings_shape() {
    // The second shape the wave ships: the canary slot admitted, and the
    // model identifier carried alongside the parameters digest (the
    // manifest's `model` slot is an identifier, and the walk is
    // incomplete without it).
    let candidate = model_settings_candidate();
    let (digest, model) = resolution_pin(&candidate).unwrap();
    let resolution = ConfigResolution {
        surface: candidate.surface(),
        tag: Some(EnvironmentTag::new("staging").unwrap()),
        candidate_id: candidate.candidate_id,
        pointer: PointerBinding::Canary,
        digest,
        model,
    };
    assert_golden("config_resolution_model_settings.json", &resolution);
}

#[test]
fn golden_registry_event_kinds_shape() {
    // The wave's additive RunEventKind wire name (the
    // `learn_event_kinds.json` discipline): pinned so no wire shape
    // lands unpinned, appended after `signing_key_rotated` per the
    // additive evolution rule every variant since R0.6 followed.
    assert_golden(
        "registry_event_kinds.json",
        &vec![RunEventKind::ConfigResolved],
    );
}

#[test]
fn resolution_pin_matches_the_manifest_pin_functions() {
    // One derivation, two homes: the journaled digest and the digest the
    // R0.7 pin functions record must agree by construction, for every
    // resolvable family.
    let prompt = prompt_candidate("You are a careful support agent.");
    let (digest, model) = resolution_pin(&prompt).unwrap();
    let manifest = RunManifest::new().pin_prompt("system", "You are a careful support agent.");
    assert_eq!(digest, manifest.prompts["system"]);
    assert_eq!(digest, sha256_hex(b"You are a careful support agent."));
    assert_eq!(model, None);

    let contract = tool_contract_candidate();
    let (digest, model) = resolution_pin(&contract).unwrap();
    let schema = json!({
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "limit": {"type": "integer", "default": 5},
        },
        "required": ["query"],
    });
    let manifest = RunManifest::new().pin_tool_schema("web_search", &schema);
    assert_eq!(digest, manifest.tool_schemas["web_search"]);
    assert_eq!(model, None);

    let settings = model_settings_candidate();
    let (digest, model) = resolution_pin(&settings).unwrap();
    let parameters = json!({"temperature": 0.2, "seed": 42, "max_tokens": 2048});
    let manifest = RunManifest::new().pin_model("gpt-5.2-2026-06-01", &parameters);
    assert_eq!(digest, manifest.model_params.as_deref().unwrap());
    assert_eq!(model.as_deref(), Some("gpt-5.2-2026-06-01"));
}

#[test]
fn resolution_pin_refuses_kinds_without_a_manifest_digest_slot() {
    // Policies bind through the checkpoint header, tool permissions
    // through the capsule machinery, memory configuration and middleware
    // compositions in their own waves — a resolution request for any of
    // them is refused, not faked.
    let memory_scope = rusty_agent_runtime::memory::ScopeAddress::new(
        rusty_agent_runtime::memory::MemoryScope::Agent,
        "support-1",
    );
    for content in [
        CandidateContent::Policy {
            family: DecisionFamily::Retry,
            parameters: json!({"max_attempts": 3}),
        },
        CandidateContent::MemorySet {
            scope: memory_scope,
            adds: Vec::new(),
            supersedes: Vec::new(),
        },
        CandidateContent::ToolPermission {
            tool: "shell".into(),
            direction: rusty_agent_runtime::learn::GrantDirection::Narrow,
        },
        CandidateContent::MemoryConfiguration {
            name: "default".into(),
            budget: ContextBudget::new(4096),
            default_filters: MemoryQuery::default(),
            schema_version: MEMORY_SCHEMA_VERSION.to_owned(),
        },
        CandidateContent::MiddlewareComposition {
            name: "default".into(),
            layers: Vec::new(),
        },
    ] {
        let candidate =
            Candidate::new(content, operator(), EvidenceSpan::default(), ts(1)).unwrap();
        assert!(
            matches!(
                resolution_pin(&candidate),
                Err(RegistryError::UnresolvableKind { .. })
            ),
            "{} must refuse",
            candidate.kind().as_str()
        );
    }
}

#[test]
fn pointer_admission_binds_canary_only_when_the_draw_admits() {
    let active = CandidateId::from("a".repeat(64));
    let canary = CandidateId::from("b".repeat(64));
    let pointer = |fraction: f64| VersionPointer {
        surface: prompt_candidate("p")
            .surface()
            .tagged(&EnvironmentTag::new("prod").unwrap()),
        active: Some(active.clone()),
        canary: Some(CanaryBinding {
            candidate_id: canary.clone(),
            fraction,
        }),
    };

    // A full-fraction canary admits every run; the slot is journaled as
    // the canary's, not the active version's.
    assert_eq!(
        pointer_admission(&pointer(1.0), "run-1"),
        Some((canary.clone(), PointerBinding::Canary))
    );
    // A negligible fraction practically never admits: the active version
    // serves. (The seeded draw's uniformity is pinned in
    // `learn::canary_admits`' own tests; here the composition.)
    assert_eq!(
        pointer_admission(&pointer(f64::MIN_POSITIVE / u64::MAX as f64), "run-1"),
        Some((active.clone(), PointerBinding::Active))
    );
    // The draw is deterministic: a recorded run re-derives its
    // assignment exactly.
    assert_eq!(
        pointer_admission(&pointer(0.5), "run-7"),
        pointer_admission(&pointer(0.5), "run-7")
    );

    // Without a canary the active version serves; with nothing promoted
    // the pointer resolves nothing — admission refuses rather than
    // guessing (registry artifacts have no static fallback).
    let no_canary = VersionPointer {
        surface: prompt_candidate("p").surface(),
        active: Some(active.clone()),
        canary: None,
    };
    assert_eq!(
        pointer_admission(&no_canary, "run-1"),
        Some((active, PointerBinding::Active))
    );
    assert_eq!(
        pointer_admission(
            &VersionPointer::new(prompt_candidate("p").surface()),
            "run-1"
        ),
        None
    );
}

#[test]
fn config_resolution_additive_wire_evolution() {
    // The untagged surface and a non-model family: `tag` and `model` are
    // absent from the wire (the sparse-wire rule), and a payload without
    // them — the shape a resolver for the untagged surface writes —
    // deserializes with both unset.
    let mut untagged = prompt_resolution();
    untagged.tag = None;
    let wire = serde_json::to_value(&untagged).unwrap();
    assert!(wire.get("tag").is_none());
    assert!(wire.get("model").is_none());
    let back: ConfigResolution = serde_json::from_value(wire).unwrap();
    assert_eq!(back, untagged);

    // And the tagged, model-carrying resolution round-trips whole.
    let tagged = prompt_resolution();
    let back: ConfigResolution =
        serde_json::from_str(&serde_json::to_string(&tagged).unwrap()).unwrap();
    assert_eq!(back, tagged);
}
