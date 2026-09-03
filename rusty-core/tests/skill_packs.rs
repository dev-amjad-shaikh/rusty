//! EP-15-S08: the five shipped skill packs — payload conformance, install
//! gating, dependency invalidation, three-way updates, behavior-contract
//! eval suites over recorded fixtures, and package provenance.
//!
//! The eval suites run through the real gate path: the pack's bundled
//! dataset is graded against the pack's recorded fixtures with `rusty-eval`
//! assertions, aggregated into an `ExperimentReport`, and decided by the
//! pack's bundled `GatePolicy` — the same machinery catalog CI uses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusty_agent_runtime::skill::{SkillPromotionStatus, SkillRegistry, SkillSource};
use rusty_agent_runtime::skill_pack::{
    DependencyChange, DependencyEnvironment, GateRunOutcome, GatewayCapability, LoadedSkill,
    RevalidationOutcome, SkillDependency, SkillGateRunner, SkillInstallDisposition, SkillPack,
    SkillPackLedger, SkillPackMutation, SkillUpdateDisposition, apply_dependency_change,
    apply_pack_update, install_skill_pack,
};
use rusty_eval::assertion::Assertion;
use rusty_eval::dataset::Dataset;
use rusty_eval::evidence::RunEvidence;
use rusty_eval::experiment::{
    AssertionPassRate, CaseReport, CaseRunReport, ExperimentReport, LatencyStats,
    REPORT_FORMAT_VERSION, ReportSummary,
};
use rusty_eval::gate::{GatePolicy, evaluate_gate};

fn catalog_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("catalog")
        .join("skills")
}

fn pack_names() -> [&'static str; 5] {
    [
        "research-and-summarize",
        "triage-and-route",
        "scheduled-digest",
        "kb-answer-with-citations",
        "form-filling",
    ]
}

fn load_pack(name: &str) -> SkillPack {
    SkillPack::from_dir(&catalog_dir().join(name))
        .unwrap_or_else(|error| panic!("pack `{name}` must load: {error}"))
}

/// The dependency environment a stock tenant ships with: every tool,
/// connector, and gateway capability the five packs declare.
fn stock_environment() -> DependencyEnvironment {
    let mut env = DependencyEnvironment::default();
    for name in pack_names() {
        let pack = load_pack(name);
        for skill in &pack.skills {
            for dependency in &skill.entry.dependencies {
                match dependency {
                    SkillDependency::Tool { name } => {
                        env.tools.insert(name.clone());
                    }
                    SkillDependency::Connector { id, major } => {
                        env.connectors.insert(id.clone(), *major);
                    }
                    SkillDependency::Gateway { capability } => {
                        env.gateway_capabilities.insert(*capability);
                    }
                }
            }
        }
    }
    env
}

/// Nearest-rank percentile, mirroring the eval crate's aggregation so the
/// hand-built summary passes report coherence validation.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Grades one skill's bundled dataset against its recorded fixtures and
/// decides the bundled gate. `tamper` rewrites a case's fixture before
/// grading — the mutation-test hook proving each suite notices when its
/// defining property breaks.
/// The mutation-test hook: rewrites a case's fixture before grading.
type TamperFn = Box<dyn Fn(&str, &mut RunEvidence)>;

struct FixtureGateRunner {
    tamper: TamperFn,
}

impl FixtureGateRunner {
    fn honest() -> Self {
        Self {
            tamper: Box::new(|_, _| {}),
        }
    }

    fn tampering(tamper: impl Fn(&str, &mut RunEvidence) + 'static) -> Self {
        Self {
            tamper: Box::new(tamper),
        }
    }
}

impl SkillGateRunner for FixtureGateRunner {
    fn run_gate(&self, skill: &LoadedSkill) -> rusty_agent_runtime::error::Result<GateRunOutcome> {
        let dataset = Dataset::from_jsonl(&skill.eval.dataset_jsonl).map_err(|error| {
            rusty_agent_runtime::error::RustyError::Catalog(format!(
                "skill pack: `{}` dataset: {error}",
                skill.name()
            ))
        })?;
        let policy = GatePolicy::from_json(&skill.eval.gate_json).map_err(|error| {
            rusty_agent_runtime::error::RustyError::Catalog(format!(
                "skill pack: `{}` gate policy: {error}",
                skill.name()
            ))
        })?;

        let mut cases = Vec::new();
        let mut failing_cases = Vec::new();
        for case in dataset.cases() {
            let fixture = skill.eval.fixtures.get(&case.id).ok_or_else(|| {
                rusty_agent_runtime::error::RustyError::Catalog(format!(
                    "skill pack: `{}` has no recorded fixture for case `{}`",
                    skill.name(),
                    case.id
                ))
            })?;
            let mut evidence: RunEvidence = serde_json::from_str(fixture).map_err(|error| {
                rusty_agent_runtime::error::RustyError::Catalog(format!(
                    "skill pack: `{}` fixture `{}`: {error}",
                    skill.name(),
                    case.id
                ))
            })?;
            (self.tamper)(&case.id, &mut evidence);

            let assertions: Vec<_> = case
                .expect
                .assertions()
                .iter()
                .map(|assertion: &Assertion| assertion.evaluate(&evidence))
                .collect();
            let passed = evidence.status.is_done() && assertions.iter().all(|result| result.passed);
            if !passed {
                failing_cases.push(case.id.clone());
            }
            let run = CaseRunReport {
                repetition: 0,
                status: evidence.status.clone(),
                passed,
                assertions,
                judge: None,
                tool_calls: evidence.tool_calls.len(),
                latency_ms: evidence.latency_ms,
                cost_usd: evidence.cost_usd,
                total_tokens: evidence.total_tokens,
            };
            cases.push(CaseReport {
                case_id: case.id.clone(),
                tags: case.tags.clone(),
                pass_rate: if passed { 1.0 } else { 0.0 },
                runs: vec![run],
            });
        }

        // Replicate ReportSummary::compute (private upstream) field by field.
        let runs: Vec<&CaseRunReport> = cases.iter().flat_map(|case| &case.runs).collect();
        let runs_passed = runs.iter().filter(|run| run.passed).count();
        let mut latencies: Vec<u64> = runs.iter().map(|run| run.latency_ms).collect();
        latencies.sort_unstable();
        let mut by_assertion: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for run in &runs {
            for result in &run.assertions {
                let entry = by_assertion.entry(result.assertion.clone()).or_default();
                entry.1 += 1;
                entry.0 += usize::from(result.passed);
            }
        }
        let summary = ReportSummary {
            cases: cases.len(),
            runs: runs.len(),
            runs_passed,
            run_pass_rate: runs_passed as f64 / runs.len().max(1) as f64,
            case_pass_rate: if cases.is_empty() {
                0.0
            } else {
                cases
                    .iter()
                    .map(|case: &CaseReport| case.pass_rate)
                    .sum::<f64>()
                    / cases.len() as f64
            },
            assertions: by_assertion
                .into_iter()
                .map(|(assertion, (passed, total))| AssertionPassRate {
                    assertion,
                    passed,
                    total,
                    rate: passed as f64 / total.max(1) as f64,
                })
                .collect(),
            latency_ms: LatencyStats {
                min: latencies.first().copied().unwrap_or(0),
                p50: percentile(&latencies, 50.0),
                p95: percentile(&latencies, 95.0),
                max: latencies.last().copied().unwrap_or(0),
                mean: if latencies.is_empty() {
                    0.0
                } else {
                    latencies.iter().sum::<u64>() as f64 / latencies.len() as f64
                },
            },
            total_cost_usd: runs.iter().map(|run| run.cost_usd).sum(),
            total_tokens: runs.iter().map(|run| run.total_tokens).sum(),
        };
        let report = ExperimentReport {
            format_version: REPORT_FORMAT_VERSION,
            name: format!("{}@{}", dataset.name(), dataset.version()),
            dataset_name: dataset.name().to_owned(),
            dataset_version: dataset.version().to_owned(),
            runs_per_case: 1,
            max_concurrency: 1,
            cases,
            summary,
        };
        let decision = evaluate_gate(&policy, &report, None).map_err(|error| {
            rusty_agent_runtime::error::RustyError::Catalog(format!(
                "skill pack: `{}` gate evaluation: {error}",
                skill.name()
            ))
        })?;
        Ok(GateRunOutcome {
            run_id: format!("{}-run-1", policy.name()),
            passed: decision.allowed(),
            failing_cases,
        })
    }
}

// ---------------------------------------------------------------------------
// AC 1 — payload conformance
// ---------------------------------------------------------------------------

#[test]
fn all_five_packs_load_as_conformant_packages() {
    for name in pack_names() {
        let pack = load_pack(name);
        assert_eq!(
            pack.manifest.kind,
            rusty_agent_runtime::package::PackageKind::SkillPack,
            "{name} must package as a skill pack"
        );
        assert!(
            pack.manifest.verify_hash(),
            "{name} manifest hash must verify"
        );
        assert_eq!(pack.skills.len(), 1, "{name} ships exactly one skill");

        let skill = &pack.skills[0];
        assert_eq!(skill.name(), name, "{name}: registry key matches the pack");
        assert!(
            skill.package.description().chars().count() <= 60,
            "{name}: description exceeds the 60-character index cap"
        );
        assert!(
            !skill.package.content_hash().is_empty(),
            "{name}: payload is content-addressed"
        );
        assert_eq!(
            skill.package.frontmatter().eval_gate.as_deref(),
            Some(format!("{name}-install-gate").as_str()),
            "{name}: frontmatter names its eval gate"
        );

        // The package file manifest covers payload, pack.json, and evals —
        // the pack's identity is every byte it ships.
        for member in [
            "SKILL.md",
            "pack.json",
            "eval/dataset.jsonl",
            "eval/gate.json",
        ] {
            assert!(
                pack.manifest.files.iter().any(|file| file.path == member),
                "{name}: file manifest covers `{member}`"
            );
        }

        // Every declared reference doc is embedded in the payload (the
        // loader refuses the pack otherwise — assert the data agrees).
        for doc in &skill.entry.reference_docs {
            assert!(
                skill.package.references().contains_key(doc),
                "{name}: embeds declared reference `{doc}`"
            );
        }
    }
}

#[test]
fn declared_dependencies_match_the_story_contract() {
    let kb = load_pack("kb-answer-with-citations");
    assert!(
        kb.skills[0]
            .entry
            .dependencies
            .iter()
            .any(|dep| matches!(dep, SkillDependency::Tool { name } if name == "kb_search"))
    );

    let digest = load_pack("scheduled-digest");
    assert!(
        digest.skills[0]
            .entry
            .dependencies
            .iter()
            .any(|dep| matches!(dep, SkillDependency::Gateway { capability }
            if *capability == GatewayCapability::ScheduledAutonomy))
    );

    let forms = load_pack("form-filling");
    assert!(
        forms.skills[0]
            .entry
            .dependencies
            .iter()
            .any(|dep| matches!(dep, SkillDependency::Gateway { capability }
            if *capability == GatewayCapability::StructuredInput))
    );
}

// ---------------------------------------------------------------------------
// AC 2 — install gating: Trial, eval suite, Promoted only on pass
// ---------------------------------------------------------------------------

fn install_all(
    runner: &dyn SkillGateRunner,
) -> (
    SkillRegistry,
    SkillPackLedger,
    Vec<rusty_agent_runtime::skill_pack::SkillInstallOutcome>,
) {
    let env = stock_environment();
    let mut registry = SkillRegistry::new();
    let mut ledger = SkillPackLedger::new();
    let mut outcomes = Vec::new();
    for name in pack_names() {
        let pack = load_pack(name);
        outcomes.extend(
            install_skill_pack(
                &pack,
                &env,
                runner,
                &mut registry,
                &mut ledger,
                "catalog-ci",
            )
            .unwrap_or_else(|error| panic!("installing `{name}`: {error}")),
        );
    }
    (registry, ledger, outcomes)
}

#[test]
fn install_promotes_when_the_bundled_suite_passes() {
    let (_registry, ledger, outcomes) = install_all(&FixtureGateRunner::honest());
    for outcome in &outcomes {
        assert_eq!(
            outcome.disposition,
            SkillInstallDisposition::Promoted,
            "{} must promote on a passing suite",
            outcome.skill_name
        );
        let record = ledger.get(&outcome.skill_name).expect("installed");
        assert_eq!(record.status, SkillPromotionStatus::Promoted);
        assert!(record.revalidation_pending.is_none());
        let mutations: Vec<_> = record.ledger.iter().map(|entry| &entry.mutation).collect();
        assert!(
            matches!(mutations.first(), Some(SkillPackMutation::Install { .. })),
            "{}: the ledger opens with the install",
            outcome.skill_name
        );
        assert!(
            mutations
                .iter()
                .any(|mutation| matches!(mutation, SkillPackMutation::GatePassed { .. })),
            "{}: the promotion is gate-recorded",
            outcome.skill_name
        );
    }
}

#[test]
fn failing_suite_leaves_trial_with_failing_cases_named() {
    // Break research-and-summarize's defining property: strip the citation.
    let runner = FixtureGateRunner::tampering(|case_id, evidence| {
        if case_id == "summary-claims-cited" {
            if let Some(serde_json::Value::Object(citation)) = evidence
                .final_state
                .pointer_mut("/summary/claims/0/citation")
            {
                citation.remove("url");
            }
        }
    });
    let (_registry, ledger, outcomes) = install_all(&runner);
    let research = outcomes
        .iter()
        .find(|outcome| outcome.skill_name == "research-and-summarize")
        .expect("research-and-summarize installed");
    match &research.disposition {
        SkillInstallDisposition::TrialAfterGateFailure { failing_cases } => {
            assert!(
                failing_cases.contains(&"summary-claims-cited".to_owned()),
                "the failing case is named, got {failing_cases:?}"
            );
        }
        other => panic!("a broken suite must not promote, got {other:?}"),
    }
    let record = ledger.get("research-and-summarize").expect("installed");
    assert_eq!(record.status, SkillPromotionStatus::Trial);
    assert!(record.ledger.iter().any(|entry| matches!(
        &entry.mutation,
        SkillPackMutation::GateFailed { failing_cases, .. }
            if failing_cases.contains(&"summary-claims-cited".to_owned())
    )));

    // The other four skills' suites were not tampered with: they promote.
    for outcome in &outcomes {
        if outcome.skill_name != "research-and-summarize" {
            assert_eq!(outcome.disposition, SkillInstallDisposition::Promoted);
        }
    }
}

#[test]
fn install_refuses_skills_whose_dependencies_the_tenant_lacks() {
    let mut env = stock_environment();
    env.tools.remove("kb_search");
    let pack = load_pack("kb-answer-with-citations");
    let mut registry = SkillRegistry::new();
    let mut ledger = SkillPackLedger::new();
    let outcomes = install_skill_pack(
        &pack,
        &env,
        &FixtureGateRunner::honest(),
        &mut registry,
        &mut ledger,
        "catalog-ci",
    )
    .expect("install runs");
    match &outcomes[0].disposition {
        SkillInstallDisposition::MissingDependencies { missing } => {
            assert!(
                missing.iter().any(
                    |dep| matches!(dep, SkillDependency::Tool { name } if name == "kb_search")
                )
            );
        }
        other => panic!("missing tool must block the install, got {other:?}"),
    }
    assert!(ledger.get("kb-answer-with-citations").is_none());
    assert!(!registry.contains("kb-answer-with-citations"));
}

// ---------------------------------------------------------------------------
// AC 3 — dependency invalidation: flag, re-run, demote with the trigger named
// ---------------------------------------------------------------------------

fn kb_loaded() -> (SkillPackLedger, BTreeMap<String, LoadedSkill>) {
    let (_registry, ledger, _) = install_all(&FixtureGateRunner::honest());
    let mut packs = BTreeMap::new();
    for name in pack_names() {
        let pack = load_pack(name);
        for skill in pack.skills {
            packs.insert(skill.name().to_owned(), skill);
        }
    }
    (ledger, packs)
}

#[test]
fn connector_major_bump_flags_and_revalidates() {
    let (mut ledger, packs) = kb_loaded();
    let pack_refs: BTreeMap<String, &LoadedSkill> = packs
        .iter()
        .map(|(name, skill)| (name.clone(), skill))
        .collect();
    let outcomes = apply_dependency_change(
        DependencyChange::ConnectorMajorChanged {
            id: "knowledge-base".to_owned(),
            new_major: 2,
        },
        &pack_refs,
        &mut ledger,
        &FixtureGateRunner::honest(),
        "catalog-ci",
    )
    .expect("revalidation runs");

    assert_eq!(
        outcomes,
        vec![RevalidationOutcome::Held {
            skill_name: "kb-answer-with-citations".to_owned()
        }],
        "only the skill declaring the connector is flagged"
    );
    let record = ledger.get("kb-answer-with-citations").expect("installed");
    assert!(record.ledger.iter().any(|entry| matches!(
        &entry.mutation,
        SkillPackMutation::Invalidated {
            trigger: DependencyChange::ConnectorMajorChanged { id, new_major: 2 }
        } if id == "knowledge-base"
    )));
    assert!(
        record
            .ledger
            .iter()
            .any(|entry| matches!(&entry.mutation, SkillPackMutation::Revalidated { .. }))
    );
    assert!(record.revalidation_pending.is_none());
    assert_eq!(record.status, SkillPromotionStatus::Promoted);
}

#[test]
fn failed_revalidation_demotes_to_trial_with_the_trigger_named() {
    let (mut ledger, packs) = kb_loaded();
    let pack_refs: BTreeMap<String, &LoadedSkill> = packs
        .iter()
        .map(|(name, skill)| (name.clone(), skill))
        .collect();
    // The re-run sees an answer with no grounding citation: the gate fails.
    let runner = FixtureGateRunner::tampering(|case_id, evidence| {
        if case_id == "grounded-answer" {
            if let Some(entry) = evidence
                .final_state
                .pointer_mut("/answer/citations/0/entry_id")
            {
                *entry = serde_json::Value::Null;
            }
        }
    });
    let outcomes = apply_dependency_change(
        DependencyChange::ToolRemoved {
            name: "kb_search".to_owned(),
        },
        &pack_refs,
        &mut ledger,
        &runner,
        "catalog-ci",
    )
    .expect("revalidation runs");

    match &outcomes[..] {
        [
            RevalidationOutcome::Demoted {
                skill_name,
                failing_cases,
            },
        ] => {
            assert_eq!(skill_name, "kb-answer-with-citations");
            assert!(failing_cases.contains(&"grounded-answer".to_owned()));
        }
        other => panic!("expected one demotion, got {other:?}"),
    }
    let record = ledger.get("kb-answer-with-citations").expect("installed");
    assert_eq!(record.status, SkillPromotionStatus::Trial);
    assert!(record.ledger.iter().any(|entry| matches!(
        &entry.mutation,
        SkillPackMutation::Demoted {
            trigger: DependencyChange::ToolRemoved { name },
            ..
        } if name == "kb_search"
    )));
}

#[test]
fn superseded_reference_doc_flags_the_embedding_skill() {
    let (mut ledger, packs) = kb_loaded();
    let pack_refs: BTreeMap<String, &LoadedSkill> = packs
        .iter()
        .map(|(name, skill)| (name.clone(), skill))
        .collect();
    let outcomes = apply_dependency_change(
        DependencyChange::ReferenceSuperseded {
            package_id: "triage-and-route".to_owned(),
            path: "references/category-set.md".to_owned(),
        },
        &pack_refs,
        &mut ledger,
        &FixtureGateRunner::honest(),
        "catalog-ci",
    )
    .expect("revalidation runs");
    assert_eq!(
        outcomes,
        vec![RevalidationOutcome::Held {
            skill_name: "triage-and-route".to_owned()
        }]
    );

    // A supersession from a different package does not touch this skill.
    let outcomes = apply_dependency_change(
        DependencyChange::ReferenceSuperseded {
            package_id: "some-other-package".to_owned(),
            path: "references/category-set.md".to_owned(),
        },
        &pack_refs,
        &mut ledger,
        &FixtureGateRunner::honest(),
        "catalog-ci",
    )
    .expect("revalidation runs");
    assert!(outcomes.is_empty());
}

// ---------------------------------------------------------------------------
// AC 4 — three-way update: never a silent overwrite of local improvements
// ---------------------------------------------------------------------------

/// Copy a pack into a temp directory with the SKILL.md body extended and the
/// version bumped: the "shipped-new" side of an update.
fn copy_pack_as_next_version(name: &str, version: &str) -> PathBuf {
    let source = catalog_dir().join(name);
    let target = std::env::temp_dir().join(format!(
        "skill-pack-test-{}-{}-{}",
        name,
        version,
        std::process::id()
    ));
    if target.exists() {
        std::fs::remove_dir_all(&target).expect("stale temp pack removed");
    }
    let mut pending = vec![(source.clone(), target.clone())];
    while let Some((from, to)) = pending.pop() {
        std::fs::create_dir_all(&to).expect("temp pack dir");
        for entry in std::fs::read_dir(&from).expect("pack dir reads") {
            let entry = entry.expect("dir entry");
            let (from_path, to_path) = (entry.path(), to.join(entry.file_name()));
            if from_path.is_dir() {
                pending.push((from_path, to_path));
            } else {
                std::fs::copy(&from_path, &to_path).expect("pack member copies");
            }
        }
    }
    let skill_md = target.join("SKILL.md");
    let mut body = std::fs::read_to_string(&skill_md).expect("SKILL.md reads");
    body.push_str("\n## Addendum\n\nPrefer primary sources when they disagree with summaries.\n");
    std::fs::write(&skill_md, body).expect("SKILL.md patched");
    let pack_json = target.join("pack.json");
    let text = std::fs::read_to_string(&pack_json).expect("pack.json reads");
    let mut parsed: serde_json::Value = serde_json::from_str(&text).expect("pack.json parses");
    parsed["version"] = serde_json::Value::String(version.to_owned());
    std::fs::write(&pack_json, serde_json::to_string_pretty(&parsed).unwrap())
        .expect("pack.json updated");
    target
}

#[test]
fn update_to_unpatched_skill_registers_and_regates() {
    let env = stock_environment();
    let mut registry = SkillRegistry::new();
    let mut ledger = SkillPackLedger::new();
    let runner = FixtureGateRunner::honest();
    let pack = load_pack("triage-and-route");
    install_skill_pack(
        &pack,
        &env,
        &runner,
        &mut registry,
        &mut ledger,
        "catalog-ci",
    )
    .expect("install");
    let installed_hash = ledger
        .get("triage-and-route")
        .expect("installed")
        .content_hash
        .clone();

    let next_dir = copy_pack_as_next_version("triage-and-route", "1.1.0");
    let next = SkillPack::from_dir(&next_dir).expect("next version loads");
    let outcomes = apply_pack_update(&next, &mut ledger, &mut registry, &runner, "catalog-ci")
        .expect("update runs");
    assert_eq!(
        outcomes[0].disposition,
        SkillUpdateDisposition::Updated { promoted: true }
    );
    let record = ledger.get("triage-and-route").expect("installed");
    assert_ne!(record.content_hash, installed_hash, "the update landed");
    assert_eq!(record.package_version, "1.1.0");
    assert_eq!(record.status, SkillPromotionStatus::Promoted);
    assert!(
        record
            .ledger
            .iter()
            .any(|entry| matches!(&entry.mutation, SkillPackMutation::Updated { .. }))
    );

    std::fs::remove_dir_all(&next_dir).expect("temp pack cleaned");
}

#[test]
fn update_to_locally_patched_skill_surfaces_three_way_never_overwrites() {
    let env = stock_environment();
    let mut registry = SkillRegistry::new();
    let mut ledger = SkillPackLedger::new();
    let runner = FixtureGateRunner::honest();
    let pack = load_pack("research-and-summarize");
    install_skill_pack(
        &pack,
        &env,
        &runner,
        &mut registry,
        &mut ledger,
        "catalog-ci",
    )
    .expect("install");
    let shipped_old = ledger
        .get("research-and-summarize")
        .expect("installed")
        .content_hash
        .clone();

    // The tenant's learning loop patches the installed skill.
    ledger
        .record_local_patch("research-and-summarize", "local-patch-hash", "review-fork")
        .expect("patch records");
    let record = ledger.get("research-and-summarize").expect("installed");
    assert!(record.locally_patched);
    assert!(record.ledger.iter().any(|entry| matches!(
        &entry.mutation,
        SkillPackMutation::LocallyPatched { content_hash } if content_hash == "local-patch-hash"
    )));

    let next_dir = copy_pack_as_next_version("research-and-summarize", "1.1.0");
    let next = SkillPack::from_dir(&next_dir).expect("next version loads");
    let shipped_new = next.skills[0].package.content_hash();
    let outcomes = apply_pack_update(&next, &mut ledger, &mut registry, &runner, "catalog-ci")
        .expect("update runs");
    assert_eq!(
        outcomes[0].disposition,
        SkillUpdateDisposition::ThreeWay,
        "a locally-patched skill is never silently overwritten"
    );
    let record = ledger.get("research-and-summarize").expect("installed");
    assert_eq!(
        record.content_hash, "local-patch-hash",
        "the local payload stands pending human resolution"
    );
    assert!(record.ledger.iter().any(|entry| matches!(
        &entry.mutation,
        SkillPackMutation::ThreeWayUpdate {
            shipped_old: old,
            shipped_new: new,
            local,
        } if *old == shipped_old && new == &shipped_new && local == "local-patch-hash"
    )));

    std::fs::remove_dir_all(&next_dir).expect("temp pack cleaned");
}

// ---------------------------------------------------------------------------
// AC 5 — the five behavior contracts, with mutation tests
// ---------------------------------------------------------------------------

#[test]
fn the_five_eval_suites_pass_against_their_recorded_fixtures() {
    let runner = FixtureGateRunner::honest();
    for name in pack_names() {
        let pack = load_pack(name);
        let outcome = runner
            .run_gate(&pack.skills[0])
            .unwrap_or_else(|error| panic!("{name} gate runs: {error}"));
        assert!(
            outcome.passed,
            "{name}: the shipped suite passes; failing cases: {:?}",
            outcome.failing_cases
        );
    }
}

fn assert_mutation_breaks_suite(
    pack_name: &str,
    tamper: impl Fn(&str, &mut RunEvidence) + 'static,
    broken_case: &str,
) {
    let pack = load_pack(pack_name);
    let runner = FixtureGateRunner::tampering(tamper);
    let outcome = runner
        .run_gate(&pack.skills[0])
        .unwrap_or_else(|error| panic!("{pack_name} gate runs: {error}"));
    assert!(
        !outcome.passed,
        "{pack_name}: the suite must fail when its defining property breaks"
    );
    assert!(
        outcome.failing_cases.contains(&broken_case.to_owned()),
        "{pack_name}: names the broken case `{broken_case}`, got {:?}",
        outcome.failing_cases
    );
}

#[test]
fn research_suite_fails_when_claims_lose_citations() {
    assert_mutation_breaks_suite(
        "research-and-summarize",
        |case_id, evidence| {
            if case_id == "summary-claims-cited" {
                if let Some(claims) = evidence
                    .final_state
                    .pointer_mut("/summary/claims")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    for claim in claims {
                        claim.as_object_mut().map(|obj| obj.remove("citation"));
                    }
                }
            }
        },
        "summary-claims-cited",
    );
}

#[test]
fn triage_suite_fails_when_routing_mismatches_category() {
    assert_mutation_breaks_suite(
        "triage-and-route",
        |case_id, evidence| {
            if case_id == "billing-charge-dispute" {
                evidence.tool_calls.iter_mut().for_each(|call| {
                    if call.name == "route_item" {
                        call.arguments["queue"] = serde_json::json!("engineering-queue");
                    }
                });
            }
        },
        "billing-charge-dispute",
    );
}

#[test]
fn digest_suite_fails_when_out_of_window_items_leak() {
    assert_mutation_breaks_suite(
        "scheduled-digest",
        |case_id, evidence| {
            if case_id == "digest-covers-window" {
                if let Some(items) = evidence
                    .final_state
                    .pointer_mut("/digest/items")
                    .and_then(serde_json::Value::as_array_mut)
                {
                    items.push(serde_json::json!("item-999-outside-window"));
                }
            }
        },
        "digest-covers-window",
    );
}

#[test]
fn kb_suite_fails_when_refusal_skips_the_gap_log() {
    assert_mutation_breaks_suite(
        "kb-answer-with-citations",
        |case_id, evidence| {
            if case_id == "ungrounded-refusal" {
                evidence.tool_calls.retain(|call| call.name != "log_gap");
            }
        },
        "ungrounded-refusal",
    );
}

#[test]
fn form_suite_fails_when_ungrounded_fields_get_invented() {
    assert_mutation_breaks_suite(
        "form-filling",
        |case_id, evidence| {
            if case_id == "raises-obligation-for-ungrounded" {
                // Inventing a value instead of raising the obligation.
                evidence.final_state["obligations"] = serde_json::json!([]);
                evidence.final_state["form"]["fields"]["tax_id"] =
                    serde_json::json!({ "value": "00-0000000", "grounded_in": "invented" });
            }
        },
        "raises-obligation-for-ungrounded",
    );
}

// ---------------------------------------------------------------------------
// AC 6 — provenance cites the shipping package
// ---------------------------------------------------------------------------

#[test]
fn installed_skills_carry_package_provenance() {
    let (registry, ledger, _) = install_all(&FixtureGateRunner::honest());
    for name in pack_names() {
        let version = registry
            .get(name)
            .unwrap_or_else(|| panic!("{name} registered"));
        match version.provenance().source {
            SkillSource::Package {
                ref package_id,
                ref publisher,
                ref version,
            } => {
                assert_eq!(package_id, name);
                assert_eq!(publisher, "rusty");
                assert_eq!(version, "1.0.0");
            }
            ref other => panic!("{name}: provenance must cite the package, got {other:?}"),
        }
        assert_eq!(
            version.provenance().content_hash,
            version.content_hash(),
            "{name}: provenance is self-contained evidence"
        );
        let record = ledger.get(name).expect("installed");
        assert_eq!(record.publisher, "rusty");
        assert_eq!(record.package_id, name);
        assert_eq!(record.content_hash, version.content_hash());
    }
}
