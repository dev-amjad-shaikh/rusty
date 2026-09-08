//! Editorial-governance tests (EP-07-S03): the prompt section is a pure
//! function of the `PatchPreference` and `SessionArtifactNameClass` enums,
//! the name classifier is deterministic, `CreateNew` justifications are
//! structurally mandatory, provenance attaches to skill candidates only,
//! and the rung distribution counts the window with every rung present.

use chrono::{DateTime, Utc};
use rusty_agent_runtime::learn::{Candidate, CandidateContent, EvidenceSpan};
use rusty_agent_runtime::memory::ProvenanceAuthor;
use rusty_agent_runtime::skill_editorial::{
    DistributionWindow, EditorialError, EditorialProvenance, MAX_CREATE_NEW_JUSTIFICATION_LEN,
    PatchPreference, SessionArtifactNameClass, classify_session_artifact_name,
    render_patch_preference_section, rung_distribution,
};

fn ts(millis: i64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(millis).unwrap()
}

fn distiller() -> ProvenanceAuthor {
    ProvenanceAuthor::Distiller {
        name: "test-distiller".into(),
    }
}

fn skill_candidate(name: &str) -> Candidate {
    Candidate::new(
        CandidateContent::Skill {
            name: name.to_owned(),
            content_hash: format!("sha256:{name}"),
            binding: None,
        },
        distiller(),
        EvidenceSpan::default(),
        ts(1_750_000_000_000),
    )
    .expect("the candidate builds")
}

// --------------------------------------------------------------------- //
// The prompt is generated from the type
// --------------------------------------------------------------------- //

#[test]
fn prompt_section_tracks_the_typed_preference_order() {
    let section = render_patch_preference_section();

    // Every rung appears, numbered, in `PatchPreference::ALL` order — the
    // text walks the enum, so a reorder of the type is a reorder of the
    // prompt. Position assertions are computed from the enum, never
    // hard-coded.
    let mut last_index = 0usize;
    let mut last_rung = 0u64;
    for rung in PatchPreference::ALL {
        let numbered = format!("{}. ", rung.rung());
        let at = section
            .find(&numbered)
            .unwrap_or_else(|| panic!("rung {rung} is numbered in the prompt"));
        assert!(at > last_index || rung.rung() == 1, "rungs render in order");
        last_index = at;
        last_rung = rung.rung();
        let instruction_at = section
            .find(rung.instruction())
            .unwrap_or_else(|| panic!("rung {rung} carries its instruction"));
        assert_eq!(instruction_at, at + numbered.len());
    }
    assert_eq!(last_rung, PatchPreference::ALL.len() as u64);

    // The ban section renders every typed class.
    for class in SessionArtifactNameClass::ALL {
        assert!(
            section.contains(class.ban_text()),
            "the ban names {class:?}"
        );
    }
}

#[test]
fn prompt_section_is_deterministic() {
    assert_eq!(
        render_patch_preference_section(),
        render_patch_preference_section()
    );
}

// --------------------------------------------------------------------- //
// The session-artifact name ban
// --------------------------------------------------------------------- //

#[test]
fn classifier_names_date_stamps() {
    for name in [
        "fix-login-2026-09-01",
        "deploy-20260901",
        "report-202601",
        "migrate-2026-09-01-users",
    ] {
        assert_eq!(
            classify_session_artifact_name(name),
            Some(SessionArtifactNameClass::DateStamp),
            "{name} is a date stamp"
        );
    }
}

#[test]
fn classifier_names_ticket_numbers() {
    for name in ["proj-1234-workaround", "fix-issue-#42", "jira-999-flow"] {
        assert_eq!(
            classify_session_artifact_name(name),
            Some(SessionArtifactNameClass::TicketNumber),
            "{name} is a ticket number"
        );
    }
}

#[test]
fn classifier_names_one_off_task_titles() {
    for name in [
        "tmp-migrate-users",
        "one-off-report",
        "ad-hoc-query",
        "hotfix-prod-outage",
    ] {
        assert_eq!(
            classify_session_artifact_name(name),
            Some(SessionArtifactNameClass::OneOffTaskTitle),
            "{name} is a one-off task title"
        );
    }
}

#[test]
fn classifier_passes_durable_names() {
    for name in [
        "deploy-web-service",
        "oauth-2-flow",
        "utf-8-sanitizer",
        "monthly-billing-reconcile",
        "refund-policy-lookup",
    ] {
        assert_eq!(
            classify_session_artifact_name(name),
            None,
            "{name} is a durable name"
        );
    }
}

// --------------------------------------------------------------------- //
// Editorial provenance
// --------------------------------------------------------------------- //

#[test]
fn create_new_without_a_justification_never_validates() {
    let bare = EditorialProvenance {
        rung: PatchPreference::CreateNew,
        create_new_justification: None,
    };
    assert!(matches!(
        bare.validate(),
        Err(EditorialError::MissingCreateNewJustification)
    ));

    let empty = EditorialProvenance {
        rung: PatchPreference::CreateNew,
        create_new_justification: Some("   ".to_owned()),
    };
    assert!(matches!(
        empty.validate(),
        Err(EditorialError::EmptyJustification)
    ));

    let untrimmed = EditorialProvenance {
        rung: PatchPreference::CreateNew,
        create_new_justification: Some(" trailing whitespace ".to_owned()),
    };
    assert!(matches!(
        untrimmed.validate(),
        Err(EditorialError::EmptyJustification)
    ));

    let overlong = EditorialProvenance {
        rung: PatchPreference::CreateNew,
        create_new_justification: Some("x".repeat(MAX_CREATE_NEW_JUSTIFICATION_LEN + 1)),
    };
    assert!(matches!(
        overlong.validate(),
        Err(EditorialError::JustificationTooLong)
    ));

    let stated = EditorialProvenance::create_new(
        "no existing skill covers refund reconciliation; the umbrella candidates are read-only",
    )
    .expect("a stated reason validates");
    assert_eq!(stated.rung, PatchPreference::CreateNew);
}

#[test]
fn patch_rungs_carry_no_justification() {
    for rung in [
        PatchPreference::PatchLoadedSkill,
        PatchPreference::PatchExistingUmbrella,
        PatchPreference::AddSupportFile,
    ] {
        EditorialProvenance::patch(rung).expect("a patch landing validates");
        let contradictory = EditorialProvenance {
            rung,
            create_new_justification: Some("a reason that contradicts the patch".to_owned()),
        };
        assert!(matches!(
            contradictory.validate(),
            Err(EditorialError::UnexpectedJustification { .. })
        ));
    }
}

#[test]
fn provenance_attaches_to_skill_candidates_only() {
    let provenance =
        EditorialProvenance::create_new("the library has no coverage of this intent").unwrap();
    let candidate = skill_candidate("refund-reconciliation")
        .with_editorial_provenance(provenance.clone())
        .expect("a skill candidate accepts provenance");
    assert_eq!(candidate.editorial, Some(provenance));

    let memory_candidate = Candidate::new(
        CandidateContent::Prompt {
            name: "system".to_owned(),
            prompt: "Answer briefly.".to_owned(),
        },
        distiller(),
        EvidenceSpan::default(),
        ts(1_750_000_000_000),
    )
    .unwrap();
    assert!(matches!(
        memory_candidate.with_editorial_provenance(
            EditorialProvenance::patch(PatchPreference::PatchLoadedSkill).unwrap()
        ),
        Err(EditorialError::NotSkillCandidate)
    ));
}

#[test]
fn provenance_is_attribution_not_identity() {
    // The content address must not move when provenance attaches — two
    // distillations of the same change stay one candidate, and the
    // provenance travels beside the address.
    let bare = skill_candidate("refund-reconciliation");
    let decorated = skill_candidate("refund-reconciliation")
        .with_editorial_provenance(
            EditorialProvenance::patch(PatchPreference::PatchExistingUmbrella).unwrap(),
        )
        .unwrap();
    assert_eq!(bare.candidate_id, decorated.candidate_id);

    // And the wire stays backward-compatible: a candidate serialized
    // before provenance existed deserializes with it absent.
    let wire = serde_json::to_value(&bare).unwrap();
    assert!(wire.get("editorial").is_none());
    let restored: Candidate = serde_json::from_value(wire).unwrap();
    assert_eq!(restored.editorial, None);

    let wire = serde_json::to_value(&decorated).unwrap();
    assert_eq!(
        wire["editorial"]["rung"],
        serde_json::json!("patch_existing_umbrella")
    );
    let restored: Candidate = serde_json::from_value(wire).unwrap();
    assert_eq!(restored, decorated);
}

// --------------------------------------------------------------------- //
// The rung distribution
// --------------------------------------------------------------------- //

#[test]
fn distribution_counts_every_rung_in_the_window() {
    let mutations = vec![
        (ts(1_000), PatchPreference::PatchLoadedSkill),
        (ts(2_000), PatchPreference::PatchLoadedSkill),
        (ts(3_000), PatchPreference::CreateNew),
        (ts(4_000), PatchPreference::AddSupportFile),
    ];
    let distribution = rung_distribution(mutations, &DistributionWindow::default()).unwrap();
    assert_eq!(distribution.total, 4);
    assert_eq!(distribution.rungs.len(), PatchPreference::ALL.len());
    let count = |rung: PatchPreference| {
        distribution
            .rungs
            .iter()
            .find(|row| row.rung == rung)
            .unwrap()
            .mutations
    };
    assert_eq!(count(PatchPreference::PatchLoadedSkill), 2);
    assert_eq!(count(PatchPreference::PatchExistingUmbrella), 0);
    assert_eq!(count(PatchPreference::AddSupportFile), 1);
    assert_eq!(count(PatchPreference::CreateNew), 1);
}

#[test]
fn distribution_window_is_since_inclusive_until_exclusive() {
    let mutations = vec![
        (ts(1_000), PatchPreference::PatchLoadedSkill),
        (ts(2_000), PatchPreference::CreateNew),
        (ts(3_000), PatchPreference::CreateNew),
    ];
    let window = DistributionWindow {
        since: Some(ts(2_000)),
        until: Some(ts(3_000)),
    };
    let distribution = rung_distribution(mutations.clone(), &window).unwrap();
    assert_eq!(distribution.total, 1, "the boundary mutation is excluded");

    // Adjacent windows never double-count the boundary.
    let before = rung_distribution(
        mutations.clone(),
        &DistributionWindow {
            since: None,
            until: Some(ts(2_000)),
        },
    )
    .unwrap();
    let after = rung_distribution(
        mutations,
        &DistributionWindow {
            since: Some(ts(2_000)),
            until: None,
        },
    )
    .unwrap();
    assert_eq!(before.total + after.total, 3);
}

#[test]
fn inverted_windows_are_callers_bugs_named_as_errors() {
    let window = DistributionWindow {
        since: Some(ts(3_000)),
        until: Some(ts(2_000)),
    };
    assert!(matches!(
        rung_distribution(Vec::new(), &window),
        Err(EditorialError::InvalidWindow)
    ));
    assert!(matches!(
        rung_distribution(
            Vec::new(),
            &DistributionWindow {
                since: Some(ts(2_000)),
                until: Some(ts(2_000)),
            }
        ),
        Err(EditorialError::InvalidWindow)
    ));
}
