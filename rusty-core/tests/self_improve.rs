//! The self-improvement plane suite: probe honesty against crafted
//! snapshots (present, partial, and absent all reachable from evidence,
//! never from fiat), the gap report's determinism, backlog persistence and
//! its status machine, provenance discipline, and the self-build path's
//! double gate — an approved entry before drafting, an operator approval
//! before publishing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use rusty_agent_runtime::composer::{publish_effect_id, ComposerSession};
use rusty_agent_runtime::effects::ApprovalToken;
use rusty_agent_runtime::record::Effect;
use rusty_agent_runtime::self_improve::{
    assess, capability_catalog, catalog_entry, draft_skill_for_entry, publish_staged_skill,
    BacklogEntry, BacklogProvenance, BacklogStatus, BacklogStore, BuildGapSkillTool, BuildShape,
    CapabilityInspection, CapabilityStatus, InspectCapabilitiesTool, Plane, ProposeBacklogTool,
    SkillProposal, FEATURE_HOOKS_COMPATIBILITY, FEATURE_SURFACE_COMPACTION, HARNESS_PROVENANCE,
    RUNBOOK_SKILL_PREFIX,
};
use rusty_agent_runtime::skill::SkillRegistry;
use rusty_agent_runtime::tool::Tool;
use serde_json::{json, Value};

/// A fixed instant for every timestamp this suite injects.
fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 2, 9, 8, 0, 0).unwrap()
}

fn t1() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 2, 9, 9, 0, 0).unwrap()
}

fn temp_backlog_path(tag: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!("rusty-self-improve-{tag}-{}", uuid::Uuid::new_v4()))
        .join("backlog.json")
}

fn proposal() -> SkillProposal {
    SkillProposal {
        name: "runbook-incident-review".to_owned(),
        description: "Review open high-priority incidents and file a theme summary.".to_owned(),
        body: "# Incident Review\n\nList, group by category, summarize the top theme.\n".to_owned(),
        references: BTreeMap::new(),
        author: HARNESS_PROVENANCE.to_owned(),
    }
}

// --------------------------------------------------------------------- //
// The catalog and probe honesty
// --------------------------------------------------------------------- //

/// A snapshot carrying the evidence of a fully-wired demo host: the real
/// planes, the composer tools, `run_cli`, and the capability-sets feature.
fn demo_snapshot() -> CapabilityInspection {
    CapabilityInspection {
        skill_names: vec![],
        connector_manifest_ids: vec!["google-calendar".to_owned(), "servicenow".to_owned()],
        tool_names: vec![
            "compose_skill".to_owned(),
            "publish_composed_skill".to_owned(),
            "run_cli".to_owned(),
        ],
        planes: vec![
            Plane::Skills,
            Plane::Connectors,
            Plane::Knowledge,
            Plane::Memory,
            Plane::Evidence,
        ],
        features: vec!["capability_sets".to_owned()],
    }
}

fn status_of(report: &rusty_agent_runtime::self_improve::GapReport, id: &str) -> CapabilityStatus {
    report
        .assessments
        .iter()
        .find(|assessment| assessment.id == id)
        .unwrap_or_else(|| panic!("catalog knows `{id}`"))
        .status
        .clone()
}

#[test]
fn catalog_covers_the_real_planes_and_the_known_gaps() {
    let catalog = capability_catalog();
    let ids: Vec<&str> = catalog.iter().map(|capability| capability.id).collect();
    // Ids are unique — a backlog entry references them, so a duplicate
    // would make an entry's claim ambiguous.
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(ids.len(), sorted.len(), "catalog ids must be unique");
    for expected in [
        "skill-plane",
        "connector-plane",
        "knowledge-plane",
        "memory-plane",
        "flight-recorder",
        "composer-drafting",
        "approval-gated-publish",
        "surface-compaction",
        "streaming-chunk-capture",
        "telemetry-ledger",
        "token-meter",
        "agent-session-query",
        "plugin-kernel",
        "os-sandbox-confinement",
        "render-intents",
        "permission-presets",
        "durable-steer-inbox",
        "code-mode",
        "goals-subsystem",
        "hooks-compatibility",
        "operator-runbooks",
    ] {
        assert!(ids.contains(&expected), "catalog is missing `{expected}`");
    }
    // Declaration order is stable: two calls build the same catalog.
    let again: Vec<&str> = capability_catalog()
        .iter()
        .map(|capability| capability.id)
        .collect();
    assert_eq!(ids, again);
}

#[test]
fn an_empty_snapshot_reports_every_capability_absent() {
    let report = assess(&capability_catalog(), &CapabilityInspection::default());
    assert_eq!(report.present, 0);
    assert_eq!(report.partial, 0);
    assert_eq!(report.absent, report.assessments.len());
    assert_eq!(report.gaps().count(), report.assessments.len());
}

#[test]
fn the_demo_snapshot_proves_present_partial_and_absent() {
    let report = assess(&capability_catalog(), &demo_snapshot());
    assert!(report.present > 0 && report.partial > 0 && report.absent > 0);
    assert_eq!(
        report.present + report.partial + report.absent,
        report.assessments.len(),
        "counts are derived from the assessments"
    );
    assert_eq!(status_of(&report, "skill-plane"), CapabilityStatus::Present);
    assert_eq!(
        status_of(&report, "approval-gated-publish"),
        CapabilityStatus::Present
    );
    // `run_cli` without an OS-sandbox feature is exactly Partial.
    match status_of(&report, "os-sandbox-confinement") {
        CapabilityStatus::Partial { note } => assert!(note.contains("allowlist")),
        other => panic!("os-sandbox-confinement must be Partial: {other:?}"),
    }
    assert_eq!(
        status_of(&report, "telemetry-ledger"),
        CapabilityStatus::Absent
    );
    // The evidence plane alone does not buy agent-visible session query.
    assert_eq!(
        status_of(&report, "agent-session-query"),
        CapabilityStatus::Absent
    );
}

#[test]
fn probes_flip_only_on_real_evidence() {
    let catalog = capability_catalog();

    // Drafting without the gated publish path is Partial; withdrawing both
    // is Absent; the pair together is Present.
    let mut snapshot = demo_snapshot();
    snapshot.tool_names.retain(|name| name != "publish_composed_skill");
    match assess(&catalog, &snapshot)
        .assessments
        .iter()
        .find(|a| a.id == "approval-gated-publish")
        .map(|a| a.status.clone())
        .unwrap()
    {
        CapabilityStatus::Partial { note } => assert!(note.contains("compose_skill")),
        other => panic!("drafting without publish must be Partial: {other:?}"),
    }

    // A registered runbook skill flips operator-runbooks; nothing else does.
    let mut snapshot = demo_snapshot();
    snapshot
        .skill_names
        .push(format!("{RUNBOOK_SKILL_PREFIX}incident-review"));
    assert_eq!(
        status_of(&assess(&catalog, &snapshot), "operator-runbooks"),
        CapabilityStatus::Present
    );

    // A session query tool flips agent-session-query.
    let mut snapshot = demo_snapshot();
    snapshot.tool_names.push("session_search".to_owned());
    assert_eq!(
        status_of(&assess(&catalog, &snapshot), "agent-session-query"),
        CapabilityStatus::Present
    );

    // The middleware plane without the hooks wire protocol is Partial; the
    // feature flag is the Present.
    let mut snapshot = demo_snapshot();
    snapshot.planes.push(Plane::Middleware);
    match status_of(&assess(&catalog, &snapshot), "hooks-compatibility") {
        CapabilityStatus::Partial { note } => assert!(note.contains("hooks.json")),
        other => panic!("middleware without the wire protocol must be Partial: {other:?}"),
    }
    snapshot.features.push(FEATURE_HOOKS_COMPATIBILITY.to_owned());
    assert_eq!(
        status_of(&assess(&catalog, &snapshot), "hooks-compatibility"),
        CapabilityStatus::Present
    );

    // Assembly order never changes an outcome.
    let mut shuffled = demo_snapshot();
    shuffled.tool_names.reverse();
    shuffled.planes.reverse();
    assert_eq!(
        assess(&catalog, &demo_snapshot()),
        assess(&catalog, &shuffled),
        "assess normalizes before probing"
    );
}

#[test]
fn the_report_is_a_pure_function_of_catalog_and_snapshot() {
    let catalog = capability_catalog();
    let snapshot = demo_snapshot();
    assert_eq!(assess(&catalog, &snapshot), assess(&catalog, &snapshot));
    // Gaps arrive in catalog order.
    let report = assess(&catalog, &snapshot);
    let gap_ids: Vec<&str> = report.gaps().map(|assessment| assessment.id).collect();
    let catalog_order: Vec<&str> = catalog
        .iter()
        .map(|capability| capability.id)
        .filter(|id| gap_ids.contains(id))
        .collect();
    assert_eq!(gap_ids, catalog_order);
}

// --------------------------------------------------------------------- //
// The backlog
// --------------------------------------------------------------------- //

#[test]
fn entry_ids_are_content_derived() {
    let gaps = vec!["telemetry-ledger".to_owned()];
    let a = BacklogEntry::new(
        "Close the telemetry gap",
        "Operations need a mirrored ledger.",
        &gaps,
        BacklogProvenance::HarnessSelfImprove,
        t0(),
    )
    .unwrap();
    let b = BacklogEntry::new(
        "Close the telemetry gap",
        "Operations need a mirrored ledger.",
        &gaps,
        BacklogProvenance::HarnessSelfImprove,
        t1(),
    )
    .unwrap();
    assert_eq!(a.id, b.id, "identity is content, not the clock");
    assert!(a.id.starts_with("bl-"));
    assert_eq!(a.id.len(), 3 + 64);

    let c = BacklogEntry::new("Close another gap", "different title", &gaps,
        BacklogProvenance::HarnessSelfImprove, t0())
    .unwrap();
    assert_ne!(a.id, c.id);

    // Validation: empty titles, duplicate gaps, and gap-less entries fail.
    assert!(BacklogEntry::new("", "r", &gaps, BacklogProvenance::HarnessSelfImprove, t0()).is_err());
    let dup = vec!["g".to_owned(), "g".to_owned()];
    assert!(BacklogEntry::new("t", "r", &dup, BacklogProvenance::HarnessSelfImprove, t0()).is_err());
    assert!(BacklogEntry::new("t", "r", &[], BacklogProvenance::HarnessSelfImprove, t0()).is_err());
}

#[test]
fn provenance_labels_are_closed_and_checked() {
    let operator = BacklogProvenance::operator("harness-demo").unwrap();
    assert_eq!(operator.label(), "operator:harness-demo");
    assert_eq!(BacklogProvenance::HarnessSelfImprove.label(), HARNESS_PROVENANCE);
    assert!(BacklogProvenance::operator("").is_err());
    assert!(BacklogProvenance::operator(" has-control\n").is_err());
}

#[test]
fn the_status_machine_allows_only_the_declared_edges() {
    let entry = BacklogEntry::new(
        "Close the telemetry gap",
        "Operations need a mirrored ledger.",
        &["telemetry-ledger".to_owned()],
        BacklogProvenance::HarnessSelfImprove,
        t0(),
    )
    .unwrap();
    assert_eq!(entry.status, BacklogStatus::Proposed);

    // proposed → in_progress is not an edge.
    assert!(entry
        .transition(BacklogStatus::InProgress, None, t1())
        .is_err());
    // proposed → approved → in_progress → done is the happy path, and done
    // carries its evidence.
    let approved = entry.transition(BacklogStatus::Approved, None, t1()).unwrap();
    assert_eq!(approved.status, BacklogStatus::Approved);
    let in_progress = approved
        .transition(BacklogStatus::InProgress, None, t1())
        .unwrap();
    assert!(in_progress
        .transition(BacklogStatus::Done, None, t1())
        .is_err());
    let done = in_progress
        .transition(
            BacklogStatus::Done,
            Some("telemetry-ledger landed in 0f1e2d3".to_owned()),
            t1(),
        )
        .unwrap();
    assert_eq!(done.status, BacklogStatus::Done);
    assert!(done.evidence.is_some());
    // done is terminal.
    assert!(done.transition(BacklogStatus::Rejected, None, t1()).is_err());

    // rejected is reachable from every open state and is terminal.
    let rejected = entry.transition(BacklogStatus::Rejected, None, t1()).unwrap();
    assert!(rejected
        .transition(BacklogStatus::Approved, None, t1())
        .is_err());
    let rejected_later = approved
        .transition(BacklogStatus::Rejected, None, t1())
        .unwrap();
    assert_eq!(rejected_later.status, BacklogStatus::Rejected);

    // Evidence rides only the done transition.
    assert!(entry
        .transition(BacklogStatus::Approved, Some("too early".to_owned()), t1())
        .is_err());
}

#[tokio::test]
async fn the_backlog_persists_and_fails_closed_on_corruption() {
    let path = temp_backlog_path("roundtrip");
    let store = BacklogStore::open(&path).await.unwrap();
    assert!(store.is_empty());

    let entry = BacklogEntry::new(
        "Close the telemetry gap",
        "Operations need a mirrored ledger.",
        &["telemetry-ledger".to_owned()],
        BacklogProvenance::HarnessSelfImprove,
        t0(),
    )
    .unwrap();
    assert!(store.insert(entry.clone()).await.unwrap());
    // A converged re-proposal is not a second entry.
    assert!(!store.insert(entry.clone()).await.unwrap());
    assert_eq!(store.len(), 1);

    let approved = store
        .transition(&entry.id, BacklogStatus::Approved, None, t1())
        .await
        .unwrap();
    assert_eq!(approved.status, BacklogStatus::Approved);
    // Unknown ids and illegal edges fail without touching the file.
    assert!(store
        .transition("bl-unknown", BacklogStatus::Approved, None, t1())
        .await
        .is_err());

    // Reopen: the file is the truth.
    drop(store);
    let reopened = BacklogStore::open(&path).await.unwrap();
    assert_eq!(reopened.len(), 1);
    assert_eq!(reopened.get(&entry.id).unwrap(), approved);

    // A tampered file fails closed: flip one character of the title and
    // the stored id no longer matches the contents.
    let bytes = std::fs::read(&path).unwrap();
    let mut file: Value = serde_json::from_slice(&bytes).unwrap();
    let title = file["entries"][0]["title"].as_str().unwrap().to_owned();
    file["entries"][0]["title"] = json!(format!("{title}!"));
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    assert!(BacklogStore::open(&path).await.is_err());

    // As does a file that is not a backlog at all.
    std::fs::write(&path, b"not json").unwrap();
    assert!(BacklogStore::open(&path).await.is_err());

    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

// --------------------------------------------------------------------- //
// The self-build path
// --------------------------------------------------------------------- //

async fn approved_runbook_store(tag: &str) -> (Arc<BacklogStore>, BacklogEntry) {
    let store = Arc::new(
        BacklogStore::open(temp_backlog_path(tag))
            .await
            .unwrap(),
    );
    let proposed = BacklogEntry::new(
        "Ship the incident-review runbook skill",
        "operator-runbooks is Absent; the incident-review workflow recurs and belongs in a \
         governed package.",
        &["operator-runbooks".to_owned()],
        BacklogProvenance::operator("harness-demo").unwrap(),
        t0(),
    )
    .unwrap();
    store.insert(proposed.clone()).await.unwrap();
    let approved = store
        .transition(&proposed.id, BacklogStatus::Approved, None, t1())
        .await
        .unwrap();
    (store, approved)
}

#[tokio::test]
async fn drafting_requires_an_approved_skill_shaped_entry() {
    let session = ComposerSession::new("self-build");
    let (store, approved) = approved_runbook_store("gates").await;

    // A proposed entry is not a disposal.
    let proposed_only = Arc::new(
        BacklogStore::open(temp_backlog_path("gates-proposed"))
            .await
            .unwrap(),
    );
    let proposed = BacklogEntry::new(
        "Ship the incident-review runbook skill",
        "operator-runbooks is Absent.",
        &["operator-runbooks".to_owned()],
        BacklogProvenance::HarnessSelfImprove,
        t0(),
    )
    .unwrap();
    proposed_only.insert(proposed.clone()).await.unwrap();
    let refused = draft_skill_for_entry(&proposed_only, &session, &proposed.id, &proposal()).await;
    assert!(refused.is_err(), "a proposed entry must not draft");

    // A gap that is not skill-shaped is not drafted as a skill.
    let core_gap = BacklogEntry::new(
        "Land the telemetry ledger",
        "Needs a core stream.",
        &["telemetry-ledger".to_owned()],
        BacklogProvenance::operator("harness-demo").unwrap(),
        t0(),
    )
    .unwrap();
    store.insert(core_gap.clone()).await.unwrap();
    store
        .transition(&core_gap.id, BacklogStatus::Approved, None, t1())
        .await
        .unwrap();
    let refused = draft_skill_for_entry(&store, &session, &core_gap.id, &proposal()).await;
    assert!(refused.is_err(), "a core-stream gap is not skill-shaped");

    // The approved, skill-shaped entry drafts and stages.
    let staged = draft_skill_for_entry(&store, &session, &approved.id, &proposal())
        .await
        .unwrap();
    assert_eq!(staged.entry_id, approved.id);
    assert_eq!(staged.content_hash.len(), 64);
    assert_eq!(
        staged.publish_effect_id,
        publish_effect_id("self-build", &staged.content_hash),
        "the staged effect id is the composer's own derivation"
    );
    assert_eq!(session.draft_count(), 1);
}

#[tokio::test]
async fn publishing_stays_behind_the_approval_gate() {
    let session = ComposerSession::new("self-build");
    let registry = Arc::new(Mutex::new(SkillRegistry::new()));
    let (store, approved) = approved_runbook_store("publish").await;
    let staged = draft_skill_for_entry(&store, &session, &approved.id, &proposal())
        .await
        .unwrap();

    // No token: a refusal, not a default.
    let refused = publish_staged_skill(&session, &registry, &staged, None).await;
    assert!(refused.is_err());
    assert!(registry.lock().unwrap().is_empty());

    // A token scoped to another draft: the composer's own fail-closed
    // admission refuses it.
    let wrong = ApprovalToken::approve(
        publish_effect_id("self-build", &"0".repeat(64)),
        "ops:ada",
    );
    assert!(publish_staged_skill(&session, &registry, &staged, Some(&wrong))
        .await
        .is_err());
    assert!(registry.lock().unwrap().is_empty());

    // The correctly scoped operator token publishes — the only way across.
    let token = ApprovalToken::approve(staged.publish_effect_id.clone(), "ops:ada");
    let receipt = publish_staged_skill(&session, &registry, &staged, Some(&token))
        .await
        .unwrap();
    assert_eq!(receipt["name"], json!("runbook-incident-review"));
    assert_eq!(receipt["approved_by"], json!("ops:ada"));
    assert!(registry.lock().unwrap().contains("runbook-incident-review"));
}

// --------------------------------------------------------------------- //
// The tools
// --------------------------------------------------------------------- //

#[tokio::test]
async fn inspect_tool_reports_the_hosts_snapshot() {
    let tool = InspectCapabilitiesTool::new(Arc::new(demo_snapshot));
    assert_eq!(tool.name(), "inspect_capabilities");
    assert_eq!(tool.effect(), Effect::ReadOnly);
    let report = tool.call(json!({})).await.unwrap();
    assert_eq!(
        report["present"].as_u64().unwrap()
            + report["partial"].as_u64().unwrap()
            + report["absent"].as_u64().unwrap(),
        report["assessments"].as_array().unwrap().len() as u64
    );
    let runbooks = report["assessments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|assessment| assessment["id"] == json!("operator-runbooks"))
        .unwrap();
    assert_eq!(runbooks["status"]["status"], json!("absent"));
}

#[tokio::test]
async fn propose_tool_records_with_harness_provenance_and_checks_the_catalog() {
    let store = Arc::new(
        BacklogStore::open(temp_backlog_path("propose"))
            .await
            .unwrap(),
    );
    let tool = ProposeBacklogTool::new(
        Arc::clone(&store),
        rusty_agent_runtime::journal::Clock::logical(1_770_000_000_000, 1_000),
    );
    assert_eq!(tool.effect(), Effect::Idempotent);

    let args = json!({"entries": [{
        "title": "Close the telemetry gap",
        "rationale": "Operations need a mirrored ledger.",
        "gap_ids": ["telemetry-ledger"]
    }]});
    let receipt = tool.call(args.clone()).await.unwrap();
    assert_eq!(receipt["recorded"].as_array().unwrap().len(), 1);
    assert_eq!(receipt["recorded"][0]["inserted"], json!(true));
    assert_eq!(
        receipt["recorded"][0]["provenance"],
        json!(HARNESS_PROVENANCE)
    );
    assert_eq!(receipt["recorded"][0]["status"], json!("proposed"));

    // Idempotent: the same call converges.
    let again = tool.call(args).await.unwrap();
    assert_eq!(again["recorded"][0]["inserted"], json!(false));
    assert_eq!(store.len(), 1);

    // An unknown gap id fails closed and records nothing.
    let refused = tool
        .call(json!({"entries": [{
            "title": "Close a gap that does not exist",
            "rationale": "Typo.",
            "gap_ids": ["not-a-gap"]
        }]}))
        .await;
    assert!(refused.is_err());
    assert_eq!(store.len(), 1);
}

#[tokio::test]
async fn build_tool_drafts_only_for_the_approved_entry() {
    let session = ComposerSession::new("self-build");
    let (store, approved) = approved_runbook_store("build-tool").await;
    let tool = BuildGapSkillTool::new(Arc::clone(&store), Arc::clone(&session));
    assert_eq!(tool.name(), "build_gap_skill");
    assert_eq!(tool.effect(), Effect::Pure);

    let args = || {
        json!({
            "gap_id": "operator-runbooks",
            "name": "runbook-incident-review",
            "description": "Review open high-priority incidents and file a theme summary.",
            "body": "# Incident Review\n\nList, group by category, summarize the top theme.\n",
            "author": HARNESS_PROVENANCE
        })
    };
    let staged = tool.call(args()).await.unwrap();
    assert_eq!(staged["entry_id"], json!(approved.id));
    assert_eq!(staged["content_hash"].as_str().unwrap().len(), 64);
    assert!(staged["publish_effect_id"].is_string());
    assert!(staged["publish_gate"].as_str().unwrap().contains("operator"));

    // A core-stream gap is refused by shape…
    assert!(tool
        .call(json!({
            "gap_id": "telemetry-ledger",
            "name": "runbook-telemetry",
            "description": "d",
            "body": "b",
            "author": HARNESS_PROVENANCE
        }))
        .await
        .is_err());
    // …and a skill-shaped gap without an approved entry is refused by the
    // backlog gate: reject the entry and the same call must fail.
    store
        .transition(&approved.id, BacklogStatus::Rejected, None, t1())
        .await
        .unwrap();
    assert!(tool.call(args()).await.is_err());

    // Nothing publishable was ever registered anywhere: drafts live in the
    // session scratch, the registry is untouched.
    assert_eq!(session.draft_count(), 1);
}

#[test]
fn catalog_build_shapes_make_the_self_build_scope_explicit() {
    let catalog = capability_catalog();
    assert_eq!(
        catalog_entry(&catalog, "operator-runbooks").unwrap().build,
        BuildShape::Skill
    );
    assert_eq!(
        catalog_entry(&catalog, "agent-session-query").unwrap().build,
        BuildShape::ToolDefinition
    );
    assert_eq!(
        catalog_entry(&catalog, "plugin-kernel").unwrap().build,
        BuildShape::CoreStream
    );
    // Exactly one seeded gap is skill-shaped today; widening that set is a
    // deliberate catalog decision, so pin it.
    let skill_shaped: Vec<&str> = catalog
        .iter()
        .filter(|capability| capability.build == BuildShape::Skill)
        .map(|capability| capability.id)
        .collect();
    assert_eq!(skill_shaped, ["operator-runbooks"]);
}

#[test]
fn feature_flags_used_by_probes_are_the_declared_constants() {
    // The compaction probe reads the declared constant, so a snapshot
    // declaring it flips the status.
    let mut snapshot = demo_snapshot();
    snapshot.features.push(FEATURE_SURFACE_COMPACTION.to_owned());
    assert_eq!(
        status_of(&assess(&capability_catalog(), &snapshot), "surface-compaction"),
        CapabilityStatus::Present
    );
}
