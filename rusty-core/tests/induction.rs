//! Induction tests (demand-side learning, wave 3): intent mining over a
//! synthetic corpus with known cluster structure, projection
//! determinism, reassignment diffs, coverage reverse-engineering with
//! confidence and freshness grades, the gap matrix's cell assignment
//! and failing-supply subdivision, ledger seeding, and declared blocks.
//! Wire shapes are pinned against checked-in JSON under `tests/golden/`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use rusty_agent_runtime::gaps::{
    ActorRef, CitationKind, EventSource, GapLedger, GapOrigin, GapStatus, InteractionChannel,
    InteractionEvent, InteractionOutcome, ResolutionPath,
};
use rusty_agent_runtime::induction::{
    ArtifactKind, ConfidenceGrade, CoverageConfig, DEFAULT_FAILING_THRESHOLD_MILLIS, IntentMap,
    MatrixCell, MiningConfig, SupplyArtifact, crawl_coverage, declared_blocks, derive_intent_id,
    diff_assignments, join_maps, mine_intents, seed_ledger, token_signature,
};
use serde::Serialize;

// ---------- golden-file machinery ----------

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

const DAY: i64 = 86_400_000;
const BASE: i64 = 1_700_000_000_000;

fn event(
    record_id: &str,
    channel: InteractionChannel,
    utterance: &str,
    resolution_path: ResolutionPath,
    outcome: InteractionOutcome,
    occurred: i64,
    resolved: Option<i64>,
) -> InteractionEvent {
    InteractionEvent::new(
        EventSource {
            system: "servicenow".into(),
            stream: "incident".into(),
            record_id: record_id.into(),
        },
        ActorRef {
            role: "employee".into(),
            id: "u-1".into(),
        },
        channel,
        utterance,
        resolution_path,
        outcome,
        ts(occurred),
        resolved.map(ts),
        vec![],
    )
    .unwrap()
}

/// The synthetic corpus: three known clusters — "vpn" (3 events,
/// human-resolved, one escalation, the costly one), "password" (2
/// events, self-served, free), and "laptop" (1 abandoned event).
/// Utterances are engineered so intra-cluster Jaccard clears the 400‰
/// default and cross-cluster stays at zero.
fn corpus() -> Vec<InteractionEvent> {
    vec![
        event(
            "INC001",
            InteractionChannel::Incident,
            "vpn connect home office certificate error",
            ResolutionPath::HumanResolved,
            InteractionOutcome::Escalated,
            BASE,
            Some(BASE + 60_000 * 90),
        ),
        event(
            "INC002",
            InteractionChannel::Incident,
            "vpn connect home office drops hourly",
            ResolutionPath::HumanResolved,
            InteractionOutcome::Resolved,
            BASE + DAY,
            Some(BASE + DAY + 60_000 * 120),
        ),
        event(
            "INC003",
            InteractionChannel::Escalation,
            "vpn connect home office still failing",
            ResolutionPath::HumanResolved,
            InteractionOutcome::Escalated,
            BASE + 2 * DAY,
            Some(BASE + 2 * DAY + 60_000 * 150),
        ),
        event(
            "SRL001",
            InteractionChannel::PortalSearch,
            "password reset portal",
            ResolutionPath::SelfService,
            InteractionOutcome::Resolved,
            BASE + 3 * DAY,
            Some(BASE + 3 * DAY + 60_000 * 5),
        ),
        event(
            "SRL002",
            InteractionChannel::PortalSearch,
            "password reset account access",
            ResolutionPath::SelfService,
            InteractionOutcome::Resolved,
            BASE + 4 * DAY,
            Some(BASE + 4 * DAY + 60_000 * 4),
        ),
        event(
            "INC004",
            InteractionChannel::Incident,
            "new-hire laptop not arrived",
            ResolutionPath::Abandoned,
            InteractionOutcome::NoResult,
            BASE + 5 * DAY,
            None,
        ),
    ]
}

fn vpn_intent_id() -> String {
    derive_intent_id(&token_signature(
        "vpn connect home office certificate error",
    ))
}

fn mine() -> IntentMap {
    mine_intents(&corpus(), &MiningConfig::default(), ts(BASE + 6 * DAY)).unwrap()
}

fn password_intent(map: &IntentMap) -> &rusty_agent_runtime::induction::Intent {
    map.intents
        .iter()
        .find(|intent| intent.label.contains("password"))
        .unwrap()
}

fn laptop_intent(map: &IntentMap) -> &rusty_agent_runtime::induction::Intent {
    map.intents
        .iter()
        .find(|intent| intent.label.contains("laptop"))
        .unwrap()
}

/// The fixture knowledge base: one strong match (names the vpn intent's
/// full signature), one weak keyword match (password), one stale
/// article referencing a retired system (laptop), and one article
/// nothing matches (latent).
fn artifacts() -> Vec<SupplyArtifact> {
    vec![
        SupplyArtifact::new(
            "KB001",
            ArtifactKind::KbArticle,
            "vpn connect home office certificate error troubleshooting",
            "When the vpn still drops hourly after failing to connect from home office, \
             check the ZTNA certificate error logs.",
            Some(ts(BASE - 30 * DAY)),
            vec!["ztna".into()],
        )
        .unwrap(),
        SupplyArtifact::new(
            "KB002",
            ArtifactKind::KbArticle,
            "Account help",
            "If you cannot sign in, the password portal can reset credentials.",
            Some(ts(BASE - 10 * DAY)),
            vec![],
        )
        .unwrap(),
        SupplyArtifact::new(
            "KB003",
            ArtifactKind::Runbook,
            "Laptop delivery tracking",
            "Track new-hire laptop shipments in the legacy freight system.",
            Some(ts(BASE - 400 * DAY)),
            vec!["legacy-freight".into()],
        )
        .unwrap(),
        SupplyArtifact::new(
            "KB004",
            ArtifactKind::KbArticle,
            "Printer driver archive",
            "Legacy campus printer drivers for the old print fleet.",
            Some(ts(BASE - 20 * DAY)),
            vec!["old-print-fleet".into()],
        )
        .unwrap(),
    ]
}

fn coverage_config() -> CoverageConfig {
    CoverageConfig {
        retired_systems: vec!["legacy-freight".into()],
        ..CoverageConfig::default()
    }
}

// ---------- tokenization ----------

#[test]
fn signatures_collapse_phrasings_to_content_tokens() {
    let a = token_signature("vpn connect home office certificate error");
    assert_eq!(
        a,
        vec!["certificate", "connect", "error", "home", "office", "vpn"]
    );
    // Stopwords and short tokens are out; what remains is the need.
    let b = token_signature("Please help me reset my password");
    assert_eq!(b, vec!["password", "reset"]);
}

// ---------- intent mining ----------

#[test]
fn mining_recovers_the_known_cluster_structure() {
    let map = mine();
    map.check().unwrap();
    assert_eq!(map.event_count, 6);
    assert_eq!(map.intents.len(), 3, "three known clusters");

    let vpn = map.get(&vpn_intent_id()).expect("the vpn cluster");
    assert_eq!(vpn.frequency, 3);
    assert!(vpn.label.contains("vpn"));
    assert_eq!(vpn.resolution.human_resolved, 3);
    assert_eq!(
        vpn.failure.reassignment_count, 1,
        "the escalation-channel event is the structural reassignment signal"
    );

    // Citation completeness: an intent without citations is not emitted.
    for intent in &map.intents {
        assert!(!intent.event_ids.is_empty());
        assert!(intent.event_ids.iter().all(|id| id.starts_with("ie-")));
    }
    assert_eq!(
        map.intents.iter().map(|i| i.event_ids.len()).sum::<usize>(),
        6,
        "every event belongs to exactly one intent"
    );
}

#[test]
fn utterances_without_content_tokens_cluster_by_channel() {
    let events = vec![
        event(
            "INC010",
            InteractionChannel::Incident,
            "???",
            ResolutionPath::Unresolved,
            InteractionOutcome::NoResult,
            BASE,
            None,
        ),
        event(
            "INC011",
            InteractionChannel::Incident,
            "—",
            ResolutionPath::Unresolved,
            InteractionOutcome::NoResult,
            BASE + DAY,
            None,
        ),
        event(
            "CHAT01",
            InteractionChannel::Chat,
            "…",
            ResolutionPath::Abandoned,
            InteractionOutcome::NoResult,
            BASE + 2 * DAY,
            None,
        ),
    ];
    let map = mine_intents(&events, &MiningConfig::default(), ts(BASE + 3 * DAY)).unwrap();
    assert_eq!(
        map.intents.len(),
        2,
        "silence clusters with silence on the same channel, not across channels"
    );
}

#[test]
fn the_ranking_is_volume_times_failure_cost() {
    let map = mine();
    let ranked = map.ranked();
    assert_eq!(ranked.len(), 3);
    // vpn: 3 × (3 human-resolved × 100 + 1 escalation × 500 + 30
    // minutes over the incident norm × 10) = 3 × 1100. laptop: 1 × 300
    // abandonment. password: self-served, zero cost.
    assert_eq!(ranked[0].intent_id, vpn_intent_id());
    assert_eq!(ranked[0].failure_cost_millis, 1_100);
    assert_eq!(ranked[0].rank_score(), 3_300);
    assert_eq!(ranked[1].intent_id, laptop_intent(&map).intent_id);
    assert_eq!(ranked[2].intent_id, password_intent(&map).intent_id);
    assert_eq!(ranked[2].rank_score(), 0);
}

#[test]
fn failure_indicators_read_the_events() {
    let map = mine();
    let vpn = map.get(&vpn_intent_id()).unwrap();
    // Median of 90/120/150 is 120; the incident-channel norm is the
    // corpus's lower median (90 of 90/120).
    assert_eq!(vpn.failure.ttr_median_minutes, Some(120));
    assert_eq!(vpn.failure.ttr_category_norm_minutes, Some(90));
    assert_eq!(vpn.failure.reopen_rate_millis, 0);
    assert_eq!(vpn.failure.abandonment_rate_millis, 0);

    let laptop = laptop_intent(&map);
    assert_eq!(laptop.failure.abandonment_rate_millis, 1000);
    assert_eq!(laptop.failure.ttr_median_minutes, None);
}

#[test]
fn seasonality_histograms_account_for_every_event() {
    let map = mine();
    let total: u64 = map
        .intents
        .iter()
        .flat_map(|intent| intent.seasonality_weekday)
        .sum();
    assert_eq!(total, 6, "every event lands in exactly one bucket");
    let vpn = map.get(&vpn_intent_id()).unwrap();
    assert_eq!(vpn.seasonality_weekday.iter().sum::<u64>(), 3);
}

#[test]
fn re_mining_reproduces_the_map_byte_for_byte() {
    // The projection discipline: the map is never a store of record.
    let first = mine();
    let second = mine();
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
}

#[test]
fn a_later_pass_reports_moves_as_reassignments() {
    let before = mine();
    let vpn_id = vpn_intent_id();

    // A stricter threshold splits the vpn cluster: the phrasings no
    // longer clear the bar, so each event opens its own cluster — moves
    // the diff must report against the prior assignment.
    let strict = MiningConfig {
        jaccard_threshold_millis: 950,
        ..MiningConfig::default()
    };
    let after = mine_intents(&corpus(), &strict, ts(BASE + 6 * DAY)).unwrap();
    let moves = diff_assignments(&before, &after);
    let vpn_moves: Vec<_> = moves
        .iter()
        .filter(|m| m.from_intent.as_deref() == Some(vpn_id.as_str()))
        .collect();
    assert_eq!(vpn_moves.len(), 2, "INC002 and INC003 moved out");
    for reassignment in &vpn_moves {
        assert_ne!(reassignment.to_intent, vpn_id);
    }

    // The same pass twice reports no moves.
    assert!(diff_assignments(&after, &after).is_empty());
}

// ---------- coverage reverse-engineering ----------

#[test]
fn coverage_grades_confidence_and_assesses_freshness() {
    let map = mine();
    let coverage =
        crawl_coverage(&artifacts(), &map, &coverage_config(), ts(BASE + 6 * DAY)).unwrap();
    coverage.check().unwrap();

    // The strong match: exact-signature coverage of the vpn intent.
    let strong = coverage
        .claims
        .iter()
        .find(|claim| claim.artifact.id == "KB001")
        .expect("KB001 claims vpn coverage");
    assert_eq!(strong.intent_id, vpn_intent_id());
    assert_eq!(strong.confidence, ConfidenceGrade::ExactSignature);
    assert!(!strong.freshness.stale);
    assert!(!strong.freshness.references_retired_system);

    // The weak match: keyword overlap with the password intent.
    let weak = coverage
        .claims
        .iter()
        .find(|claim| claim.artifact.id == "KB002")
        .expect("KB002 claims weak coverage");
    assert_eq!(weak.intent_id, password_intent(&map).intent_id);
    assert_eq!(weak.confidence, ConfidenceGrade::KeywordOverlap);

    // The stale article: its claim carries both freshness flags.
    let stale = coverage
        .claims
        .iter()
        .find(|claim| claim.artifact.id == "KB003")
        .expect("KB003 claims weak laptop coverage");
    assert_eq!(stale.confidence, ConfidenceGrade::KeywordOverlap);
    assert!(stale.freshness.stale, "400 days old is stale");
    assert!(stale.freshness.references_retired_system);

    // Every claim cites its artifact by construction.
    for claim in &coverage.claims {
        assert!(!claim.artifact.id.is_empty(), "an uncited claim is invalid");
        assert!(claim.claim_id.starts_with("cc-"));
    }

    // The article nothing matches is latent capability.
    assert_eq!(coverage.latent_artifacts.len(), 1);
    assert_eq!(coverage.latent_artifacts[0].id, "KB004");
}

#[test]
fn crawling_twice_reproduces_the_map_byte_for_byte() {
    let map = mine();
    let first = crawl_coverage(&artifacts(), &map, &coverage_config(), ts(BASE + 6 * DAY)).unwrap();
    let second =
        crawl_coverage(&artifacts(), &map, &coverage_config(), ts(BASE + 6 * DAY)).unwrap();
    assert_eq!(
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
}

// ---------- the gap matrix ----------

#[test]
fn every_intent_lands_in_exactly_one_cell_and_failure_subdivides_supply() {
    let map = mine();
    let coverage =
        crawl_coverage(&artifacts(), &map, &coverage_config(), ts(BASE + 6 * DAY)).unwrap();
    let matrix = join_maps(
        &map,
        &coverage,
        DEFAULT_FAILING_THRESHOLD_MILLIS,
        ts(BASE + 6 * DAY),
    );
    matrix.check().unwrap();
    assert_eq!(matrix.rows.len(), map.intents.len());

    // vpn: exact coverage, zero measured failure — supply that works.
    let vpn_row = matrix.row(&vpn_intent_id()).unwrap();
    assert_eq!(vpn_row.cell, MatrixCell::WorkingSupply);
    assert_eq!(vpn_row.claim_ids.len(), 1);

    // password: weak coverage, zero measured failure — working.
    let password_row = matrix.row(&password_intent(&map).intent_id).unwrap();
    assert_eq!(password_row.cell, MatrixCell::WorkingSupply);

    // laptop: covered by the stale retired-system article with a
    // 1000-per-mille failure rate — the trap cell: the article reads as
    // an answer and fails as one.
    let laptop_row = matrix.row(&laptop_intent(&map).intent_id).unwrap();
    assert_eq!(laptop_row.cell, MatrixCell::FailingSupply);
    assert_eq!(laptop_row.claim_ids.len(), 1);
    assert_eq!(laptop_row.failure_rate_millis, 1000);

    // The latent cell rides along and seeds nothing.
    assert_eq!(matrix.latent_artifacts.len(), 1);
}

// ---------- seeding and declared blocks ----------

#[test]
fn seeding_files_the_learn_now_and_failing_cells_with_cited_evidence() {
    let map = mine();
    // No coverage at all: every intent is learn-now.
    let coverage = crawl_coverage(&[], &map, &coverage_config(), ts(BASE + 6 * DAY)).unwrap();
    let matrix = join_maps(
        &map,
        &coverage,
        DEFAULT_FAILING_THRESHOLD_MILLIS,
        ts(BASE + 6 * DAY),
    );
    assert!(
        matrix
            .rows
            .iter()
            .all(|row| row.cell == MatrixCell::LearnNow)
    );

    let mut ledger = GapLedger::new();
    let seeded = seed_ledger(
        &mut ledger,
        &matrix,
        &map,
        ts(BASE + 6 * DAY),
        DEFAULT_FAILING_THRESHOLD_MILLIS as u32,
    )
    .unwrap();
    assert_eq!(seeded.len(), 3, "every learn-now row seeds one entry");

    for gap_id in &seeded {
        let entry = ledger.entry(gap_id).unwrap();
        assert_eq!(entry.origin, GapOrigin::Induction);
        assert!(
            !entry.evidence.is_empty(),
            "seeded rows are cited by schema"
        );
        assert!(
            entry
                .evidence
                .iter()
                .any(|citation| citation.kind == CitationKind::InteractionEvent)
        );
        assert_eq!(entry.status, GapStatus::Open);
        assert!(entry.observed);
    }

    // Priority: the work order leads with the vpn intent — volume ×
    // failure-cost, not insertion order.
    let work_order = ledger.work_order();
    assert_eq!(work_order[0].subject.intent_id().unwrap(), vpn_intent_id());

    // Re-seeding converges: same ids, reinforced volume, no duplicates.
    let again = seed_ledger(
        &mut ledger,
        &matrix,
        &map,
        ts(BASE + 7 * DAY),
        DEFAULT_FAILING_THRESHOLD_MILLIS as u32,
    )
    .unwrap();
    assert_eq!(seeded, again);
    for gap_id in &seeded {
        let entry = ledger.entry(gap_id).unwrap();
        let intent = map.get(entry.subject.intent_id().unwrap()).unwrap();
        assert_eq!(entry.volume, 2 * intent.frequency);
    }
}

#[test]
fn a_failing_supply_row_seeds_with_coverage_citations_and_a_measured_closure() {
    let map = mine();
    let coverage =
        crawl_coverage(&artifacts(), &map, &coverage_config(), ts(BASE + 6 * DAY)).unwrap();
    let matrix = join_maps(
        &map,
        &coverage,
        DEFAULT_FAILING_THRESHOLD_MILLIS,
        ts(BASE + 6 * DAY),
    );

    let mut ledger = GapLedger::new();
    let seeded = seed_ledger(
        &mut ledger,
        &matrix,
        &map,
        ts(BASE + 6 * DAY),
        DEFAULT_FAILING_THRESHOLD_MILLIS as u32,
    )
    .unwrap();
    assert_eq!(seeded.len(), 1, "only the failing cell seeds");
    let entry = ledger.entry(&seeded[0]).unwrap();
    assert_eq!(
        entry.subject.intent_id().unwrap(),
        laptop_intent(&map).intent_id
    );
    assert!(
        entry
            .evidence
            .iter()
            .any(|citation| citation.kind == CitationKind::CoverageEdge),
        "the failing-supply row cites the supply that fails"
    );
}

#[test]
fn declared_blocks_mount_empty_where_the_matrix_shows_no_supply() {
    let map = mine();
    let coverage = crawl_coverage(&[], &map, &coverage_config(), ts(BASE + 6 * DAY)).unwrap();
    let matrix = join_maps(
        &map,
        &coverage,
        DEFAULT_FAILING_THRESHOLD_MILLIS,
        ts(BASE + 6 * DAY),
    );

    let blocks = declared_blocks(&matrix, &map, 3, 2_000);
    assert_eq!(blocks.len(), 3);
    // Work order first: the vpn block leads, and every block is an
    // empty commitment (no supply anywhere).
    assert_eq!(blocks[0].intent_id, vpn_intent_id());
    for block in &blocks {
        assert!(block.empty, "an empty block is a visible commitment");
        assert!(block.description.contains("escalation path"));
        assert_eq!(block.char_limit, 2_000);
    }
}

// ---------- golden files ----------

#[test]
fn golden_intent_map() {
    assert_golden("induction_intent_map.json", &mine());
}

#[test]
fn golden_coverage_map() {
    let map = mine();
    let coverage =
        crawl_coverage(&artifacts(), &map, &coverage_config(), ts(BASE + 6 * DAY)).unwrap();
    assert_golden("induction_coverage_map.json", &coverage);
}

#[test]
fn golden_gap_matrix() {
    let map = mine();
    let coverage =
        crawl_coverage(&artifacts(), &map, &coverage_config(), ts(BASE + 6 * DAY)).unwrap();
    let matrix = join_maps(
        &map,
        &coverage,
        DEFAULT_FAILING_THRESHOLD_MILLIS,
        ts(BASE + 6 * DAY),
    );
    assert_golden("induction_gap_matrix.json", &matrix);
}
