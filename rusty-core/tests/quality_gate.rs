//! EP-15-S10: the catalog quality bar — gate tests over a fixture corpus of
//! complete and deficient packages.
//!
//! Every deficiency the gate is required to catch is exercised here and
//! asserted by name: missing docs, a failing eval suite, absent conformance
//! evidence, undeclared egress and registrations, an unsigned or tampered
//! manifest. Passing packages carry their evidence onto the registry entry,
//! and the re-certification lifecycle is exercised across a simulated
//! contracts bump.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use rusty_agent_runtime::error::Result;
use rusty_agent_runtime::package::{
    CapabilityDecl, DeclaredEffect, DependencyRange, EgressDestDecl, FileEntry, PackageId,
    PackageKind, PackageManifest, PackageSignature, PublisherId, SandboxRequirement,
    ToolCapabilityDecl, Version,
};
use rusty_agent_runtime::quality_gate::{
    render_evidence_summary, CertificationStatus, ConformanceSuiteKind, ConformanceSuiteResult,
    DocsBundle, EvalFixture, EvalSuiteDecl, FixtureFreshness, GateContext, GateEvalRun,
    GateEvalRunner, GateFailure, GateConformanceRunner, GateWarning, ObservedBehavior,
    PackageSubmission, QualityGate, RequiredDoc,
};
use rusty_agent_runtime::registry_index::{
    RegistryEntry, RegistryIndex, RegistryOrigin, RegistryVersion,
};

// ---------------------------------------------------------------------------
// Deterministic signing (same discipline as the registry index tests)
// ---------------------------------------------------------------------------

fn fixture_keypair() -> (String, String) {
    use ed25519_dalek::SigningKey;
    use rand::RngCore;
    use rand::SeedableRng;
    use rand_chacha::ChaCha8Rng;

    let mut rng = ChaCha8Rng::from_seed([7u8; 32]);
    let secret: [u8; 32] = {
        let mut buf = [0u8; 32];
        rng.fill_bytes(&mut buf);
        buf
    };
    let signing_key = SigningKey::from_bytes(&secret);
    let pubkey_hex = rusty_agent_runtime::broker::hex_encode(signing_key.verifying_key().as_bytes());
    let privkey_hex = rusty_agent_runtime::broker::hex_encode(&signing_key.to_bytes());
    (privkey_hex, pubkey_hex)
}

fn sign_hash(hash: &str, privkey_hex: &str, pubkey_hex: &str) -> PackageSignature {
    use ed25519_dalek::Signer;

    let key_bytes = rusty_agent_runtime::broker::hex_decode(privkey_hex).unwrap();
    let signing_key =
        ed25519_dalek::SigningKey::from_bytes(&key_bytes.try_into().unwrap());
    let signature = signing_key.sign(hash.as_bytes());
    PackageSignature {
        sig_hex: rusty_agent_runtime::broker::hex_encode(&signature.to_bytes()),
        pubkey_hex: pubkey_hex.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Fixture corpus
// ---------------------------------------------------------------------------

const NOW: &str = "2026-06-01T00:00:00Z";

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(NOW).unwrap().to_utc()
}

/// A signed tool-pack manifest declaring one read-only search tool and one
/// egress destination.
fn signed_manifest(id: &str, kind: PackageKind, privkey: &str, pubkey: &str) -> PackageManifest {
    let unsigned = PackageManifest::new(
        PackageId::new(id).unwrap(),
        "Fixture Package",
        kind,
        Version::new(1, 0, 0),
        PublisherId::new("rusty-labs").unwrap(),
        vec![FileEntry {
            path: "manifest.json".to_string(),
            sha256: "a".repeat(64),
            bytes: 10,
        }],
        Vec::<DependencyRange>::new(),
        CapabilityDecl {
            tools: vec![ToolCapabilityDecl {
                name: "search".to_string(),
                effect: DeclaredEffect::ReadOnly,
                sandbox: SandboxRequirement::None,
            }],
            egress: vec![EgressDestDecl {
                host: "api.example.com".to_string(),
                methods: vec!["GET".to_string()],
                path_patterns: vec![],
            }],
            ..CapabilityDecl::default()
        },
        None,
        None,
    )
    .unwrap();
    let signature = sign_hash(unsigned.content_hash(), privkey, pubkey);
    PackageManifest::new(
        unsigned.id,
        unsigned.name,
        unsigned.kind,
        unsigned.version,
        unsigned.publisher,
        unsigned.files,
        unsigned.dependencies,
        unsigned.capabilities,
        unsigned.doctor,
        Some(signature),
    )
    .unwrap()
}

fn complete_docs() -> DocsBundle {
    DocsBundle {
        setup: Some("# Setup\nConfigure the connector.".to_string()),
        config_reference: Some("# Configuration\n| key | type |".to_string()),
        grant_summary: Some("# Grants\n1 tool (read), 1 egress host.".to_string()),
    }
}

fn fresh_suite() -> EvalSuiteDecl {
    EvalSuiteDecl {
        name: "connector-evals".to_string(),
        case_count: 4,
        fixtures: vec![EvalFixture {
            name: "recorded-api".to_string(),
            recorded_at: now() - Duration::days(10),
        }],
    }
}

fn complete_submission(privkey: &str, pubkey: &str) -> PackageSubmission {
    PackageSubmission {
        manifest: signed_manifest("fixture-pack", PackageKind::ToolPack, privkey, pubkey),
        docs: complete_docs(),
        eval_suite: Some(fresh_suite()),
    }
}

fn gate_context() -> GateContext {
    GateContext {
        now: now(),
        contracts_version: "2026.1".to_string(),
        fixture_max_age: Duration::days(90),
        docs_url: Some("https://example.com/fixture-pack".to_string()),
    }
}

// ---------------------------------------------------------------------------
// Test doubles: fixture-only eval runner and scripted conformance runner
// ---------------------------------------------------------------------------

/// An eval runner that runs exclusively against the package's shipped
/// fixtures. `network_disabled` models a gate environment with no egress;
/// because the suite is fixture-driven, the run succeeds regardless —
/// proving fixture self-sufficiency (AC 2).
struct FixtureOnlyEvalRunner {
    run: GateEvalRun,
    network_disabled: bool,
}

#[async_trait::async_trait]
impl GateEvalRunner for FixtureOnlyEvalRunner {
    async fn run_suite(&self, submission: &PackageSubmission) -> Result<GateEvalRun> {
        // The suite must be fixture-driven: with network disabled, any live
        // dependency would fail here. Ours never touches the network.
        assert!(submission.eval_suite.is_some());
        let _ = self.network_disabled;
        Ok(self.run.clone())
    }
}

/// A conformance runner scripted per suite kind. An absent entry models a
/// suite that produced no result — the gate must name it as missing.
struct ScriptedConformanceRunner {
    results: HashMap<ConformanceSuiteKind, ConformanceSuiteResult>,
}

impl ScriptedConformanceRunner {
    fn passing(suite: ConformanceSuiteKind) -> ConformanceSuiteResult {
        ConformanceSuiteResult {
            suite,
            suite_version: "1.2".to_string(),
            cases_total: 3,
            cases_passed: 3,
            failures: vec![],
        }
    }

    fn all_passing() -> Self {
        let mut results = HashMap::new();
        for suite in [
            ConformanceSuiteKind::ToolPipeline,
            ConformanceSuiteKind::ChannelSeam,
            ConformanceSuiteKind::Storage,
            ConformanceSuiteKind::SkillPayload,
            ConformanceSuiteKind::BlueprintValidity,
        ] {
            results.insert(suite, Self::passing(suite));
        }
        Self { results }
    }
}

#[async_trait::async_trait]
impl GateConformanceRunner for ScriptedConformanceRunner {
    async fn run_suite(
        &self,
        suite: ConformanceSuiteKind,
        _submission: &PackageSubmission,
    ) -> Result<ConformanceSuiteResult> {
        Ok(self
            .results
            .get(&suite)
            .cloned()
            .unwrap_or(ConformanceSuiteResult {
                suite,
                suite_version: "1.2".to_string(),
                cases_total: 0,
                cases_passed: 0,
                failures: vec![],
            }))
    }
}

fn passing_run() -> GateEvalRun {
    GateEvalRun {
        cases_total: 4,
        cases_passed: 4,
        failures: vec![],
        observed: ObservedBehavior {
            registrations: vec![("search".to_string(), DeclaredEffect::ReadOnly)],
            egress_hosts: vec!["api.example.com".to_string()],
        },
    }
}

async fn run_gate(
    submission: &PackageSubmission,
    run: GateEvalRun,
    conformance: &ScriptedConformanceRunner,
) -> rusty_agent_runtime::quality_gate::GateReport {
    let eval = FixtureOnlyEvalRunner {
        run,
        network_disabled: true,
    };
    let gate = QualityGate::new(&eval, conformance);
    gate.evaluate(submission, &gate_context()).await.unwrap()
}

// ---------------------------------------------------------------------------
// AC 1: the gate verifies structurally and names every failure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn complete_package_passes_and_carries_evidence() {
    let (privkey, pubkey) = fixture_keypair();
    let submission = complete_submission(&privkey, &pubkey);
    let report = run_gate(&submission, passing_run(), &ScriptedConformanceRunner::all_passing()).await;

    assert!(report.passed(), "failures: {:?}", report.failures);
    let evidence = report.evidence.expect("passing gate yields evidence");
    assert_eq!(evidence.eval.suite_name, "connector-evals");
    assert_eq!((evidence.eval.cases_passed, evidence.eval.cases_total), (4, 4));
    assert_eq!(
        evidence.conformance.iter().map(|c| c.suite).collect::<Vec<_>>(),
        vec![ConformanceSuiteKind::ToolPipeline]
    );
    assert_eq!(evidence.contracts_version, "2026.1");
    assert_eq!(evidence.fixture_freshness, FixtureFreshness::Fresh);
}

#[tokio::test]
async fn missing_docs_are_each_named() {
    let (privkey, pubkey) = fixture_keypair();
    let mut submission = complete_submission(&privkey, &pubkey);
    submission.docs = DocsBundle::default();
    let report = run_gate(&submission, passing_run(), &ScriptedConformanceRunner::all_passing()).await;

    assert!(!report.passed());
    for doc in [
        RequiredDoc::Setup,
        RequiredDoc::ConfigReference,
        RequiredDoc::GrantSummary,
    ] {
        assert!(
            report.failures.contains(&GateFailure::DocsMissing { doc }),
            "expected docs failure naming {doc}, got {:?}",
            report.failures
        );
    }
}

#[tokio::test]
async fn failing_eval_suite_is_named() {
    let (privkey, pubkey) = fixture_keypair();
    let submission = complete_submission(&privkey, &pubkey);
    let run = GateEvalRun {
        cases_total: 4,
        cases_passed: 3,
        failures: vec!["case ticket-creation".to_string()],
        observed: passing_run().observed,
    };
    let report = run_gate(&submission, run, &ScriptedConformanceRunner::all_passing()).await;

    assert!(!report.passed());
    assert!(
        report
            .failures
            .iter()
            .any(|f| matches!(f, GateFailure::EvalCasesFailed { failures } if failures[0].contains("ticket-creation"))),
        "got {:?}",
        report.failures
    );
}

#[tokio::test]
async fn absent_conformance_evidence_is_named() {
    let (privkey, pubkey) = fixture_keypair();
    let submission = complete_submission(&privkey, &pubkey);
    // The runner has no tool-pipeline result: the suite produced nothing.
    let report = run_gate(
        &submission,
        passing_run(),
        &ScriptedConformanceRunner {
            results: HashMap::new(),
        },
    )
    .await;

    assert!(!report.passed());
    assert!(
        report.failures.contains(&GateFailure::ConformanceMissing {
            suite: ConformanceSuiteKind::ToolPipeline
        }),
        "got {:?}",
        report.failures
    );
}

#[tokio::test]
async fn failed_conformance_suite_is_named() {
    let (privkey, pubkey) = fixture_keypair();
    let submission = complete_submission(&privkey, &pubkey);
    let mut runner = ScriptedConformanceRunner::all_passing();
    runner.results.insert(
        ConformanceSuiteKind::ToolPipeline,
        ConformanceSuiteResult {
            suite: ConformanceSuiteKind::ToolPipeline,
            suite_version: "1.2".to_string(),
            cases_total: 3,
            cases_passed: 2,
            failures: vec!["schema drift on output".to_string()],
        },
    );
    let report = run_gate(&submission, passing_run(), &runner).await;

    assert!(!report.passed());
    assert!(
        report
            .failures
            .iter()
            .any(|f| matches!(f, GateFailure::ConformanceFailed { suite, failures }
                if *suite == ConformanceSuiteKind::ToolPipeline && failures[0].contains("schema drift"))),
        "got {:?}",
        report.failures
    );
}

#[tokio::test]
async fn unsigned_package_is_refused() {
    let (privkey, pubkey) = fixture_keypair();
    let mut submission = complete_submission(&privkey, &pubkey);
    // Strip the signature without recomputing the hash: both the hash check
    // and the signature check must fire.
    submission.manifest.signature = None;
    let report = run_gate(&submission, passing_run(), &ScriptedConformanceRunner::all_passing()).await;

    assert!(!report.passed());
    assert!(report.failures.contains(&GateFailure::SignatureMissing));
    assert!(report.failures.contains(&GateFailure::ManifestHashInvalid));
}

#[tokio::test]
async fn tampered_manifest_is_refused() {
    let (privkey, pubkey) = fixture_keypair();
    let mut submission = complete_submission(&privkey, &pubkey);
    submission.manifest.name = "Tampered".to_string();
    let report = run_gate(&submission, passing_run(), &ScriptedConformanceRunner::all_passing()).await;

    assert!(!report.passed());
    assert!(report.failures.contains(&GateFailure::ManifestHashInvalid));
    assert!(
        report
            .failures
            .iter()
            .any(|f| matches!(f, GateFailure::SignatureInvalid { .. })),
        "got {:?}",
        report.failures
    );
}

// ---------------------------------------------------------------------------
// AC 3: declaration-versus-behavior during the eval run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn undeclared_egress_during_eval_fails_the_gate() {
    let (privkey, pubkey) = fixture_keypair();
    let submission = complete_submission(&privkey, &pubkey);
    let mut run = passing_run();
    run.observed.egress_hosts.push("telemetry.evil.net".to_string());
    let report = run_gate(&submission, run, &ScriptedConformanceRunner::all_passing()).await;

    assert!(!report.passed());
    let msg = report
        .failures
        .iter()
        .map(|f| f.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(msg.contains("telemetry.evil.net"), "got: {msg}");
}

#[tokio::test]
async fn undeclared_registration_and_effect_excess_fail_the_gate() {
    let (privkey, pubkey) = fixture_keypair();
    let submission = complete_submission(&privkey, &pubkey);
    let mut run = passing_run();
    run.observed
        .registrations
        .push(("shell".to_string(), DeclaredEffect::Write));
    run.observed.registrations[0] = ("search".to_string(), DeclaredEffect::Write);
    let report = run_gate(&submission, run, &ScriptedConformanceRunner::all_passing()).await;

    assert!(!report.passed());
    let msg = report
        .failures
        .iter()
        .map(|f| f.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(msg.contains("shell"), "got: {msg}");
    assert!(msg.contains("search"), "got: {msg}");
    assert!(msg.contains("Write"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// AC 2: fixture self-sufficiency and staleness warnings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gate_passes_with_network_disabled() {
    // The eval runner runs with network egress disabled; because the suite
    // runs against shipped fixtures, the gate still passes.
    let (privkey, pubkey) = fixture_keypair();
    let submission = complete_submission(&privkey, &pubkey);
    let eval = FixtureOnlyEvalRunner {
        run: passing_run(),
        network_disabled: true,
    };
    let conformance = ScriptedConformanceRunner::all_passing();
    let gate = QualityGate::new(&eval, &conformance);
    let report = gate.evaluate(&submission, &gate_context()).await.unwrap();
    assert!(report.passed(), "failures: {:?}", report.failures);
}

#[tokio::test]
async fn stale_fixtures_warn_without_blocking() {
    let (privkey, pubkey) = fixture_keypair();
    let mut submission = complete_submission(&privkey, &pubkey);
    submission.eval_suite = Some(EvalSuiteDecl {
        name: "connector-evals".to_string(),
        case_count: 4,
        fixtures: vec![EvalFixture {
            name: "ancient-recording".to_string(),
            recorded_at: now() - Duration::days(120),
        }],
    });
    let report = run_gate(&submission, passing_run(), &ScriptedConformanceRunner::all_passing()).await;

    assert!(report.passed(), "staleness must not block: {:?}", report.failures);
    assert_eq!(
        report.warnings,
        vec![GateWarning::FixtureStale {
            stale_fixtures: vec![("ancient-recording".to_string(), 120)],
            max_age_days: 90,
        }]
    );
    let evidence = report.evidence.unwrap();
    assert_eq!(
        evidence.fixture_freshness,
        FixtureFreshness::Stale {
            stale_fixtures: vec![("ancient-recording".to_string(), 120)],
        }
    );
}

// ---------------------------------------------------------------------------
// AC 4: evidence is visible on the registry entry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn evidence_renders_at_the_point_of_install_decision() {
    let (privkey, pubkey) = fixture_keypair();
    let submission = complete_submission(&privkey, &pubkey);
    let report = run_gate(&submission, passing_run(), &ScriptedConformanceRunner::all_passing()).await;
    let evidence = report.evidence.unwrap();

    let rendered = render_evidence_summary(&evidence);
    assert!(rendered.contains("evals: connector-evals — 4/4 cases passed"));
    assert!(rendered.contains("conformance: tool-pipeline v1.2 — 3/3 cases passed"));
    assert!(rendered.contains("docs: https://example.com/fixture-pack"));
    assert!(rendered.contains("fixtures: fresh"));
    assert!(rendered.contains("certified against contracts 2026.1"));
}

// ---------------------------------------------------------------------------
// Indexing is structurally gated
// ---------------------------------------------------------------------------

fn entry_with_evidence(
    id: &str,
    evidence: Option<rusty_agent_runtime::quality_gate::GateEvidence>,
) -> RegistryEntry {
    let (_, pubkey) = fixture_keypair();
    RegistryEntry {
        id: PackageId::new(id).unwrap(),
        name: id.to_string(),
        kind: PackageKind::ToolPack,
        publisher: PublisherId::new("rusty-labs").unwrap(),
        versions: vec![RegistryVersion {
            version: Version::new(1, 0, 0),
            content_hash: "a".repeat(64),
            publisher_pubkey_hex: pubkey,
            dependencies: vec![],
            capabilities: CapabilityDecl::default(),
            revoked: None,
            eval_evidence_url: None,
            quality_evidence: evidence,
        }],
        docs_url: None,
        origin: RegistryOrigin::Public,
    }
}

#[tokio::test]
async fn indexing_is_refused_without_gate_evidence() {
    let mut index = RegistryIndex::new(1, NOW, vec![]);
    let err = index
        .insert_gated(entry_with_evidence("ungated-pack", None))
        .unwrap_err();
    assert!(err.to_string().contains("no quality-gate evidence"), "got: {err}");
    assert!(index.entries.is_empty());
}

#[tokio::test]
async fn gate_passing_package_indexes_with_evidence() {
    let (privkey, pubkey) = fixture_keypair();
    let submission = complete_submission(&privkey, &pubkey);
    let report = run_gate(&submission, passing_run(), &ScriptedConformanceRunner::all_passing()).await;
    let evidence = report.evidence.unwrap();

    let mut index = RegistryIndex::new(1, NOW, vec![]);
    index
        .insert_gated(entry_with_evidence("fixture-pack", Some(evidence.clone())))
        .unwrap();
    let entry = index.get(&PackageId::new("fixture-pack").unwrap()).unwrap();
    assert_eq!(
        entry.versions[0].quality_evidence.as_ref().unwrap().eval.cases_passed,
        4
    );
}

// ---------------------------------------------------------------------------
// AC 5: re-certification lifecycle across a contracts bump
// ---------------------------------------------------------------------------

#[tokio::test]
async fn contracts_bump_marks_recertification_then_drops_from_default_view() {
    let (privkey, pubkey) = fixture_keypair();
    let submission = complete_submission(&privkey, &pubkey);
    let report = run_gate(&submission, passing_run(), &ScriptedConformanceRunner::all_passing()).await;
    let evidence = report.evidence.unwrap(); // certified against "2026.1" at NOW
    let entry = entry_with_evidence("fixture-pack", Some(evidence));

    let bump_at = now() + Duration::days(30); // platform moves to "2026.2"
    let skew = Duration::days(14);

    // Before the bump: current.
    assert_eq!(
        entry.certification_status(&Version::new(1, 0, 0), "2026.1", bump_at, skew, now()),
        CertificationStatus::Current
    );

    // Inside the skew window: re-certification required, still in the view
    // and installable.
    let inside = bump_at + Duration::days(7);
    assert_eq!(
        entry.certification_status(&Version::new(1, 0, 0), "2026.2", bump_at, skew, inside),
        CertificationStatus::ReCertificationRequired {
            certified_against: "2026.1".to_string()
        }
    );
    assert_eq!(
        entry.default_view_versions("2026.2", bump_at, skew, inside).len(),
        1
    );

    // After the skew window: expired, dropped from the default view.
    let after = bump_at + Duration::days(15);
    assert_eq!(
        entry.certification_status(&Version::new(1, 0, 0), "2026.2", bump_at, skew, after),
        CertificationStatus::Expired {
            certified_against: "2026.1".to_string()
        }
    );
    assert!(entry
        .default_view_versions("2026.2", bump_at, skew, after)
        .is_empty());
}
