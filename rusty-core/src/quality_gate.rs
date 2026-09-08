//! The catalog quality bar: no item ships without evals, docs, and
//! conformance (EP-15-S10).
//!
//! The quality gate runs when a package version is submitted for indexing.
//! It verifies structurally — not by review culture — that the submission
//! carries:
//!
//! - a valid, signed manifest (EP-15-S01);
//! - documentation: setup, a configuration reference generated from the
//!   config schema, and a grant summary;
//! - a bundled eval suite that passes against the package's own recorded
//!   or mock fixtures — the gate never depends on live third-party
//!   services;
//! - kind-appropriate conformance evidence: tool-pipeline conformance for
//!   every registered tool, channel-seam conformance for adapters, storage
//!   conformance for backends, skill-payload conformance for skill packs,
//!   and blueprint validity for templates.
//!
//! During the eval run the declaration-versus-behavior harness records what
//! the package actually does; observed effect classes, egress destinations,
//! and registrations must be a subset of the manifest's declarations.
//!
//! A submission failing any check is not indexed; the report names every
//! failure. Passing evidence is attached to the registry entry so the
//! quality bar is visible at the point of install decision. When the
//! platform contracts version moves, evidence that predates the bump is
//! marked re-certification required and drops from the default index view
//! after the skew window — the catalog cannot silently rot.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::package::{DeclaredEffect, PackageId, PackageKind, PackageManifest, Version};

// ---------------------------------------------------------------------------
// Submission inputs
// ---------------------------------------------------------------------------

/// The documentation a package must ship.
///
/// All three documents are required: a setup guide, a configuration
/// reference generated from the package's config schema, and a grant
/// summary of the capabilities the package will request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DocsBundle {
    /// Setup documentation (markdown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<String>,
    /// Configuration reference generated from the config schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_reference: Option<String>,
    /// Grant summary: capabilities, egress, secret references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_summary: Option<String>,
}

/// The required documents, used to name doc failures precisely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequiredDoc {
    Setup,
    ConfigReference,
    GrantSummary,
}

impl std::fmt::Display for RequiredDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RequiredDoc::Setup => f.write_str("setup"),
            RequiredDoc::ConfigReference => f.write_str("configuration reference"),
            RequiredDoc::GrantSummary => f.write_str("grant summary"),
        }
    }
}

impl DocsBundle {
    /// The documents that are absent or empty.
    pub fn missing(&self) -> Vec<RequiredDoc> {
        let mut missing = Vec::new();
        if self.setup.as_deref().is_none_or(str::is_empty) {
            missing.push(RequiredDoc::Setup);
        }
        if self
            .config_reference
            .as_deref()
            .is_none_or(str::is_empty)
        {
            missing.push(RequiredDoc::ConfigReference);
        }
        if self.grant_summary.as_deref().is_none_or(str::is_empty) {
            missing.push(RequiredDoc::GrantSummary);
        }
        missing
    }
}

/// A recorded or mock fixture shipped in the package. Eval and conformance
/// suites run against fixtures only — never against live services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalFixture {
    /// Fixture name.
    pub name: String,
    /// When the fixture was recorded (RFC 3339).
    pub recorded_at: DateTime<Utc>,
}

/// The eval suite bundled in the package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteDecl {
    /// Suite name.
    pub name: String,
    /// Number of cases the suite declares.
    pub case_count: usize,
    /// The fixtures the suite runs against.
    pub fixtures: Vec<EvalFixture>,
}

/// A package version submitted for indexing.
#[derive(Debug, Clone)]
pub struct PackageSubmission {
    /// The package manifest (must be signed).
    pub manifest: PackageManifest,
    /// The shipped documentation.
    pub docs: DocsBundle,
    /// The bundled eval suite, if any.
    pub eval_suite: Option<EvalSuiteDecl>,
}

// ---------------------------------------------------------------------------
// Behavior observation (declaration-versus-behavior harness)
// ---------------------------------------------------------------------------

/// What the package actually did while its eval suite ran.
///
/// Every observed registration, effect class, and egress destination must
/// be a subset of the manifest's declarations; any excess fails the gate
/// naming the undeclared behavior.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ObservedBehavior {
    /// Tool registrations observed: `(tool name, observed effect class)`.
    #[serde(default)]
    pub registrations: Vec<(String, DeclaredEffect)>,
    /// Egress hosts contacted during the run.
    #[serde(default)]
    pub egress_hosts: Vec<String>,
}

/// One excess of observed behavior over the manifest's declarations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UndeclaredBehavior {
    /// A tool was registered that the manifest does not declare.
    UndeclaredRegistration { tool: String },
    /// A tool ran with an effect class its declaration does not permit.
    EffectClassExceedsDeclaration {
        tool: String,
        declared: DeclaredEffect,
        observed: DeclaredEffect,
    },
    /// An egress destination was contacted that the manifest does not
    /// declare.
    UndeclaredEgress { host: String },
}

impl std::fmt::Display for UndeclaredBehavior {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndeclaredBehavior::UndeclaredRegistration { tool } => {
                write!(f, "tool `{tool}` was registered but is not declared")
            }
            UndeclaredBehavior::EffectClassExceedsDeclaration {
                tool,
                declared,
                observed,
            } => write!(
                f,
                "tool `{tool}` ran with effect class `{observed:?}` but declares `{declared:?}`"
            ),
            UndeclaredBehavior::UndeclaredEgress { host } => {
                write!(f, "egress to `{host}` is not declared")
            }
        }
    }
}

/// Check observed behavior against the manifest's declarations, returning
/// every excess.
pub fn undeclared_behavior(
    manifest: &PackageManifest,
    observed: &ObservedBehavior,
) -> Vec<UndeclaredBehavior> {
    let mut excess = Vec::new();
    for (tool, effect) in &observed.registrations {
        match manifest.capabilities.tools.iter().find(|t| &t.name == tool) {
            None => excess.push(UndeclaredBehavior::UndeclaredRegistration {
                tool: tool.clone(),
            }),
            Some(decl) if decl.effect != *effect => {
                excess.push(UndeclaredBehavior::EffectClassExceedsDeclaration {
                    tool: tool.clone(),
                    declared: decl.effect,
                    observed: *effect,
                });
            }
            Some(_) => {}
        }
    }
    for host in &observed.egress_hosts {
        let declared = manifest.capabilities.egress.iter().any(|e| {
            e.host == *host
                || e
                    .host
                    .strip_prefix("*.")
                    .is_some_and(|suffix| host.ends_with(suffix) && host.len() > suffix.len())
        });
        if !declared {
            excess.push(UndeclaredBehavior::UndeclaredEgress { host: host.clone() });
        }
    }
    excess
}

// ---------------------------------------------------------------------------
// Runner traits (implementations live outside the core crate)
// ---------------------------------------------------------------------------

/// The outcome of running a submission's bundled eval suite.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GateEvalRun {
    /// Cases executed.
    pub cases_total: usize,
    /// Cases passed.
    pub cases_passed: usize,
    /// Names of failing cases.
    pub failures: Vec<String>,
    /// Behavior observed during the run.
    pub observed: ObservedBehavior,
}

/// Runs a package's bundled eval suite against its shipped fixtures.
///
/// Implementations must run fixtures only; the gate never depends on live
/// third-party services.
#[async_trait::async_trait]
pub trait GateEvalRunner: Send + Sync {
    async fn run_suite(&self, submission: &PackageSubmission) -> Result<GateEvalRun>;
}

/// The conformance suites the gate can require, keyed by package kind and
/// declared capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceSuiteKind {
    /// Tool-pipeline conformance, required for every registered tool.
    ToolPipeline,
    /// Channel-seam conformance, required for channel adapters.
    ChannelSeam,
    /// Storage conformance, required for sandbox or store backends.
    Storage,
    /// Skill-payload conformance, required for skill packs.
    SkillPayload,
    /// Blueprint validity, required for blueprint templates.
    BlueprintValidity,
}

impl std::fmt::Display for ConformanceSuiteKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConformanceSuiteKind::ToolPipeline => f.write_str("tool-pipeline"),
            ConformanceSuiteKind::ChannelSeam => f.write_str("channel-seam"),
            ConformanceSuiteKind::Storage => f.write_str("storage"),
            ConformanceSuiteKind::SkillPayload => f.write_str("skill-payload"),
            ConformanceSuiteKind::BlueprintValidity => f.write_str("blueprint-validity"),
        }
    }
}

/// The result of running one conformance suite.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceSuiteResult {
    /// Which suite ran.
    pub suite: ConformanceSuiteKind,
    /// The suite's own version, recorded for re-certification.
    pub suite_version: String,
    /// Cases executed.
    pub cases_total: usize,
    /// Cases passed.
    pub cases_passed: usize,
    /// Names of failing cases.
    pub failures: Vec<String>,
}

impl ConformanceSuiteResult {
    /// `true` when the suite ran at least one case and every case passed.
    /// A zero-case result is not evidence of conformance.
    pub fn passed(&self) -> bool {
        self.cases_total > 0 && self.failures.is_empty() && self.cases_passed == self.cases_total
    }
}

/// Runs kind-appropriate conformance suites against a submission.
#[async_trait::async_trait]
pub trait GateConformanceRunner: Send + Sync {
    async fn run_suite(
        &self,
        suite: ConformanceSuiteKind,
        submission: &PackageSubmission,
    ) -> Result<ConformanceSuiteResult>;
}

/// The conformance suites a manifest requires, derived from its kind and
/// capability declarations.
pub fn required_conformance_suites(manifest: &PackageManifest) -> Vec<ConformanceSuiteKind> {
    let mut suites = Vec::new();
    if !manifest.capabilities.tools.is_empty() {
        suites.push(ConformanceSuiteKind::ToolPipeline);
    }
    if !manifest.capabilities.channels.is_empty() {
        suites.push(ConformanceSuiteKind::ChannelSeam);
    }
    if !manifest.capabilities.backends.is_empty() {
        suites.push(ConformanceSuiteKind::Storage);
    }
    match manifest.kind {
        PackageKind::SkillPack => suites.push(ConformanceSuiteKind::SkillPayload),
        PackageKind::BlueprintTemplate => suites.push(ConformanceSuiteKind::BlueprintValidity),
        PackageKind::Connector | PackageKind::ToolPack => {}
    }
    suites
}

// ---------------------------------------------------------------------------
// Gate evidence (what an indexed item carries)
// ---------------------------------------------------------------------------

/// Summary of the passing eval run, displayed on the registry entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalPassSummary {
    /// Suite name.
    pub suite_name: String,
    /// Cases executed.
    pub cases_total: usize,
    /// Cases passed.
    pub cases_passed: usize,
    /// When the suite ran (RFC 3339).
    pub ran_at: DateTime<Utc>,
}

/// Whether the package's fixtures are fresh enough to trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FixtureFreshness {
    /// All fixtures are within the configured maximum age.
    Fresh,
    /// At least one fixture predates the maximum age — a warning surfaced
    /// on the registry entry, honest rather than blocking.
    Stale {
        /// The stale fixtures and their ages in days at gate time.
        stale_fixtures: Vec<(String, i64)>,
    },
}

/// The evidence a gate-passing version carries on its registry entry.
///
/// Rendered by the console at the point of install decision: the quality
/// bar is visible, not buried in CI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateEvidence {
    /// Eval pass summary with case counts.
    pub eval: EvalPassSummary,
    /// Conformance suites passed, with suite versions.
    pub conformance: Vec<ConformanceSuiteResult>,
    /// Link to the package's documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
    /// Fixture freshness at gate time.
    pub fixture_freshness: FixtureFreshness,
    /// The platform contracts version the evidence was certified against.
    pub contracts_version: String,
    /// When the gate ran (RFC 3339).
    pub certified_at: DateTime<Utc>,
}

/// Render the gate evidence the way the console presents it on an item's
/// registry entry (AC 4).
pub fn render_evidence_summary(evidence: &GateEvidence) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "evals: {} — {}/{} cases passed ({})\n",
        evidence.eval.suite_name,
        evidence.eval.cases_passed,
        evidence.eval.cases_total,
        evidence.eval.ran_at.to_rfc3339(),
    ));
    for suite in &evidence.conformance {
        out.push_str(&format!(
            "conformance: {} v{} — {}/{} cases passed\n",
            suite.suite, suite.suite_version, suite.cases_passed, suite.cases_total,
        ));
    }
    match &evidence.docs_url {
        Some(url) => out.push_str(&format!("docs: {url}\n")),
        None => out.push_str("docs: bundled\n"),
    }
    match &evidence.fixture_freshness {
        FixtureFreshness::Fresh => out.push_str("fixtures: fresh\n"),
        FixtureFreshness::Stale { stale_fixtures } => {
            out.push_str("fixtures: STALE —");
            for (name, age_days) in stale_fixtures {
                out.push_str(&format!(" {name} ({age_days}d old)"));
            }
            out.push('\n');
        }
    }
    out.push_str(&format!("certified against contracts {}\n", evidence.contracts_version));
    out
}

// ---------------------------------------------------------------------------
// Re-certification lifecycle (AC 5)
// ---------------------------------------------------------------------------

/// Where an indexed item's certification stands against the running
/// platform's contracts version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CertificationStatus {
    /// Evidence matches the current contracts version.
    Current,
    /// Evidence predates a contracts bump. The item remains installable
    /// for the skew window.
    ReCertificationRequired {
        /// The contracts version the evidence was certified against.
        certified_against: String,
    },
    /// The skew window elapsed without re-certification; the item drops
    /// from the default index view.
    Expired {
        /// The contracts version the stale evidence was certified against.
        certified_against: String,
    },
}

/// Evaluate an item's certification status after a contracts-version bump.
///
/// `bump_at` is when the platform moved to `current_contracts_version`;
/// `skew_window` is how long stale items remain installable and visible
/// (the skew window of EP-13-S12).
pub fn certification_status(
    evidence: &GateEvidence,
    current_contracts_version: &str,
    bump_at: DateTime<Utc>,
    skew_window: Duration,
    now: DateTime<Utc>,
) -> CertificationStatus {
    if evidence.contracts_version == current_contracts_version {
        return CertificationStatus::Current;
    }
    let certified_against = evidence.contracts_version.clone();
    if now >= bump_at + skew_window {
        CertificationStatus::Expired { certified_against }
    } else {
        CertificationStatus::ReCertificationRequired { certified_against }
    }
}

// ---------------------------------------------------------------------------
// Gate report
// ---------------------------------------------------------------------------

/// One structural deficiency found by the gate. Every failure is named
/// precisely so the submitter can act on the report directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateFailure {
    /// The manifest's stored hash does not match its content.
    ManifestHashInvalid,
    /// The manifest carries no signature.
    SignatureMissing,
    /// The signature does not verify over the manifest's content hash.
    SignatureInvalid { reason: String },
    /// A required document is absent or empty.
    DocsMissing { doc: RequiredDoc },
    /// No eval suite is bundled.
    EvalSuiteMissing,
    /// The bundled eval suite declares no cases.
    EvalSuiteEmpty,
    /// The eval suite ran fixtures but did not pass.
    EvalCasesFailed { failures: Vec<String> },
    /// A required conformance suite produced no result.
    ConformanceMissing { suite: ConformanceSuiteKind },
    /// A required conformance suite ran and failed.
    ConformanceFailed {
        suite: ConformanceSuiteKind,
        failures: Vec<String>,
    },
    /// Observed behavior exceeded the manifest's declarations.
    UndeclaredBehavior { behavior: UndeclaredBehavior },
}

impl std::fmt::Display for GateFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateFailure::ManifestHashInvalid => {
                f.write_str("manifest content hash does not match its content")
            }
            GateFailure::SignatureMissing => f.write_str("manifest is not signed"),
            GateFailure::SignatureInvalid { reason } => {
                write!(f, "signature invalid: {reason}")
            }
            GateFailure::DocsMissing { doc } => write!(f, "documentation missing: {doc}"),
            GateFailure::EvalSuiteMissing => f.write_str("no eval suite bundled"),
            GateFailure::EvalSuiteEmpty => f.write_str("eval suite declares no cases"),
            GateFailure::EvalCasesFailed { failures } => {
                write!(f, "eval cases failed: {}", failures.join(", "))
            }
            GateFailure::ConformanceMissing { suite } => {
                write!(f, "conformance suite `{suite}` produced no result")
            }
            GateFailure::ConformanceFailed { suite, failures } => {
                write!(f, "conformance suite `{suite}` failed: {}", failures.join(", "))
            }
            GateFailure::UndeclaredBehavior { behavior } => {
                write!(f, "undeclared behavior: {behavior}")
            }
        }
    }
}

/// A non-blocking concern surfaced on the registry entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateWarning {
    /// A fixture predates the configured maximum age.
    FixtureStale {
        /// The stale fixtures and their ages in days at gate time.
        stale_fixtures: Vec<(String, i64)>,
        /// The configured maximum age in days.
        max_age_days: i64,
    },
}

impl std::fmt::Display for GateWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateWarning::FixtureStale {
                stale_fixtures,
                max_age_days,
            } => {
                let names: Vec<&str> = stale_fixtures.iter().map(|(n, _)| n.as_str()).collect();
                write!(
                    f,
                    "fixtures older than {max_age_days} days: {}",
                    names.join(", ")
                )
            }
        }
    }
}

/// The verdict of a gate run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateReport {
    /// The package under review.
    pub package_id: PackageId,
    /// The version under review.
    pub version: Version,
    /// Every failure found. Empty means the gate passed.
    pub failures: Vec<GateFailure>,
    /// Non-blocking warnings (e.g., fixture staleness).
    pub warnings: Vec<GateWarning>,
    /// The evidence to attach to the registry entry; present only when the
    /// gate passed.
    pub evidence: Option<GateEvidence>,
}

impl GateReport {
    /// `true` when the submission may be indexed.
    pub fn passed(&self) -> bool {
        self.failures.is_empty() && self.evidence.is_some()
    }
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

/// Context for a single gate run.
#[derive(Debug, Clone)]
pub struct GateContext {
    /// When the gate runs.
    pub now: DateTime<Utc>,
    /// The platform contracts version the evidence certifies against.
    pub contracts_version: String,
    /// Fixtures older than this are flagged stale (warning, not blocking).
    pub fixture_max_age: Duration,
    /// The documentation link recorded on the registry entry.
    pub docs_url: Option<String>,
}

/// The catalog quality gate. Structurally blocks indexing of any item
/// missing docs, evals, or conformance evidence.
pub struct QualityGate<'a> {
    eval_runner: &'a dyn GateEvalRunner,
    conformance_runner: &'a dyn GateConformanceRunner,
}

impl<'a> QualityGate<'a> {
    pub fn new(
        eval_runner: &'a dyn GateEvalRunner,
        conformance_runner: &'a dyn GateConformanceRunner,
    ) -> Self {
        Self {
            eval_runner,
            conformance_runner,
        }
    }

    /// Run every structural check against a submission, collecting all
    /// failures. A passing report carries the evidence the registry entry
    /// must record.
    pub async fn evaluate(
        &self,
        submission: &PackageSubmission,
        ctx: &GateContext,
    ) -> Result<GateReport> {
        let manifest = &submission.manifest;
        let mut failures: Vec<GateFailure> = Vec::new();
        let mut warnings: Vec<GateWarning> = Vec::new();

        // --- Manifest validity and signature ---
        if !manifest.verify_hash() {
            failures.push(GateFailure::ManifestHashInvalid);
        }
        match &manifest.signature {
            None => failures.push(GateFailure::SignatureMissing),
            Some(sig) => {
                if let Err(e) = sig.verify(&manifest.unsigned_content_hash()) {
                    failures.push(GateFailure::SignatureInvalid {
                        reason: e.to_string(),
                    });
                }
            }
        }

        // --- Documentation ---
        for doc in submission.docs.missing() {
            failures.push(GateFailure::DocsMissing { doc });
        }

        // --- Eval suite: present, then passing against fixtures ---
        let mut eval_summary: Option<EvalPassSummary> = None;
        let mut fixture_freshness = FixtureFreshness::Fresh;
        match &submission.eval_suite {
            None => failures.push(GateFailure::EvalSuiteMissing),
            Some(suite) => {
                if suite.case_count == 0 {
                    failures.push(GateFailure::EvalSuiteEmpty);
                } else {
                    let run = self.eval_runner.run_suite(submission).await?;
                    if !run.failures.is_empty() || run.cases_passed < run.cases_total {
                        failures.push(GateFailure::EvalCasesFailed {
                            failures: run.failures.clone(),
                        });
                    } else {
                        eval_summary = Some(EvalPassSummary {
                            suite_name: suite.name.clone(),
                            cases_total: run.cases_total,
                            cases_passed: run.cases_passed,
                            ran_at: ctx.now,
                        });
                    }

                    // Declaration-versus-behavior harness (AC 3).
                    for behavior in undeclared_behavior(manifest, &run.observed) {
                        failures.push(GateFailure::UndeclaredBehavior { behavior });
                    }

                    // Fixture staleness: a warning, honest rather than
                    // blocking (AC 2).
                    let stale: Vec<(String, i64)> = suite
                        .fixtures
                        .iter()
                        .filter_map(|f| {
                            let age = ctx.now - f.recorded_at;
                            (age > ctx.fixture_max_age)
                                .then(|| (f.name.clone(), age.num_days()))
                        })
                        .collect();
                    if !stale.is_empty() {
                        warnings.push(GateWarning::FixtureStale {
                            stale_fixtures: stale.clone(),
                            max_age_days: ctx.fixture_max_age.num_days(),
                        });
                        fixture_freshness = FixtureFreshness::Stale {
                            stale_fixtures: stale,
                        };
                    }
                }
            }
        }

        // --- Kind-appropriate conformance evidence ---
        let mut conformance_passed: Vec<ConformanceSuiteResult> = Vec::new();
        for suite_kind in required_conformance_suites(manifest) {
            let result = self
                .conformance_runner
                .run_suite(suite_kind, submission)
                .await?;
            if result.passed() {
                conformance_passed.push(result);
            } else if result.cases_total == 0 && result.failures.is_empty() {
                failures.push(GateFailure::ConformanceMissing { suite: suite_kind });
            } else {
                failures.push(GateFailure::ConformanceFailed {
                    suite: suite_kind,
                    failures: result.failures,
                });
            }
        }

        let evidence = if failures.is_empty() {
            let eval = eval_summary
                .expect("a gate with no eval summary cannot reach zero failures");
            Some(GateEvidence {
                eval,
                conformance: conformance_passed,
                docs_url: ctx.docs_url.clone(),
                fixture_freshness,
                contracts_version: ctx.contracts_version.clone(),
                certified_at: ctx.now,
            })
        } else {
            None
        };

        Ok(GateReport {
            package_id: manifest.id.clone(),
            version: manifest.version.clone(),
            failures,
            warnings,
            evidence,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{
        CapabilityDecl, EgressDestDecl, FileEntry, PackageId, PublisherId, ToolCapabilityDecl,
    };

    fn manifest_with_caps(tools: Vec<(&str, DeclaredEffect)>, egress: Vec<&str>) -> PackageManifest {
        PackageManifest::new(
            PackageId::new("gate-test").unwrap(),
            "Gate Test",
            PackageKind::ToolPack,
            Version::new(1, 0, 0),
            PublisherId::new("rusty-labs").unwrap(),
            vec![FileEntry {
                path: "tools/x.rs".to_string(),
                sha256: "a".repeat(64),
                bytes: 1,
            }],
            vec![],
            CapabilityDecl {
                tools: tools
                    .into_iter()
                    .map(|(name, effect)| ToolCapabilityDecl {
                        name: name.to_string(),
                        effect,
                        sandbox: crate::package::SandboxRequirement::None,
                    })
                    .collect(),
                egress: egress
                    .into_iter()
                    .map(|host| EgressDestDecl {
                        host: host.to_string(),
                        methods: vec!["GET".to_string()],
                        path_patterns: vec![],
                    })
                    .collect(),
                ..CapabilityDecl::default()
            },
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn undeclared_registration_is_named() {
        let m = manifest_with_caps(vec![("search", DeclaredEffect::ReadOnly)], vec![]);
        let observed = ObservedBehavior {
            registrations: vec![("shell".to_string(), DeclaredEffect::Write)],
            egress_hosts: vec![],
        };
        let excess = undeclared_behavior(&m, &observed);
        assert_eq!(
            excess,
            vec![UndeclaredBehavior::UndeclaredRegistration {
                tool: "shell".to_string()
            }]
        );
        assert!(excess[0].to_string().contains("shell"));
    }

    #[test]
    fn effect_class_excess_is_named() {
        let m = manifest_with_caps(vec![("search", DeclaredEffect::ReadOnly)], vec![]);
        let observed = ObservedBehavior {
            registrations: vec![("search".to_string(), DeclaredEffect::Write)],
            egress_hosts: vec![],
        };
        let excess = undeclared_behavior(&m, &observed);
        assert_eq!(excess.len(), 1);
        assert!(matches!(
            excess[0],
            UndeclaredBehavior::EffectClassExceedsDeclaration { .. }
        ));
        let msg = excess[0].to_string();
        assert!(msg.contains("search"));
        assert!(msg.contains("ReadOnly"));
        assert!(msg.contains("Write"));
    }

    #[test]
    fn undeclared_egress_is_named() {
        let m = manifest_with_caps(vec![], vec!["api.example.com"]);
        let observed = ObservedBehavior {
            registrations: vec![],
            egress_hosts: vec!["evil.example.net".to_string()],
        };
        let excess = undeclared_behavior(&m, &observed);
        assert_eq!(
            excess,
            vec![UndeclaredBehavior::UndeclaredEgress {
                host: "evil.example.net".to_string()
            }]
        );
    }

    #[test]
    fn wildcard_egress_covers_subdomains() {
        let m = manifest_with_caps(vec![], vec!["*.example.com"]);
        let observed = ObservedBehavior {
            registrations: vec![],
            egress_hosts: vec!["api.example.com".to_string()],
        };
        assert!(undeclared_behavior(&m, &observed).is_empty());
        // The bare suffix itself does not match a wildcard declaration.
        let observed = ObservedBehavior {
            registrations: vec![],
            egress_hosts: vec!["example.com".to_string()],
        };
        assert_eq!(undeclared_behavior(&m, &observed).len(), 1);
    }

    #[test]
    fn declared_subset_passes_cleanly() {
        let m = manifest_with_caps(
            vec![("search", DeclaredEffect::ReadOnly)],
            vec!["api.example.com"],
        );
        let observed = ObservedBehavior {
            registrations: vec![("search".to_string(), DeclaredEffect::ReadOnly)],
            egress_hosts: vec!["api.example.com".to_string()],
        };
        assert!(undeclared_behavior(&m, &observed).is_empty());
    }

    #[test]
    fn docs_missing_names_each_document() {
        let docs = DocsBundle {
            setup: Some("set it up".to_string()),
            config_reference: None,
            grant_summary: Some(String::new()),
        };
        assert_eq!(
            docs.missing(),
            vec![RequiredDoc::ConfigReference, RequiredDoc::GrantSummary]
        );
    }

    #[test]
    fn required_suites_follow_kind_and_capabilities() {
        let tool_pack = manifest_with_caps(vec![("t", DeclaredEffect::Pure)], vec![]);
        assert_eq!(
            required_conformance_suites(&tool_pack),
            vec![ConformanceSuiteKind::ToolPipeline]
        );

        let mut skill_pack = manifest_with_caps(vec![], vec![]);
        skill_pack.kind = PackageKind::SkillPack;
        assert_eq!(
            required_conformance_suites(&skill_pack),
            vec![ConformanceSuiteKind::SkillPayload]
        );

        let mut template = manifest_with_caps(vec![], vec![]);
        template.kind = PackageKind::BlueprintTemplate;
        assert_eq!(
            required_conformance_suites(&template),
            vec![ConformanceSuiteKind::BlueprintValidity]
        );

        let mut adapter = manifest_with_caps(vec![], vec![]);
        adapter.capabilities.channels.push("slack".to_string());
        adapter.capabilities.backends.push("postgres".to_string());
        assert_eq!(
            required_conformance_suites(&adapter),
            vec![ConformanceSuiteKind::ChannelSeam, ConformanceSuiteKind::Storage]
        );
    }

    fn evidence_at(contracts: &str) -> GateEvidence {
        GateEvidence {
            eval: EvalPassSummary {
                suite_name: "suite".to_string(),
                cases_total: 4,
                cases_passed: 4,
                ran_at: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                    .unwrap()
                    .to_utc(),
            },
            conformance: vec![ConformanceSuiteResult {
                suite: ConformanceSuiteKind::ToolPipeline,
                suite_version: "1.0".to_string(),
                cases_total: 3,
                cases_passed: 3,
                failures: vec![],
            }],
            docs_url: Some("https://example.com/docs".to_string()),
            fixture_freshness: FixtureFreshness::Fresh,
            contracts_version: contracts.to_string(),
            certified_at: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .to_utc(),
        }
    }

    #[test]
    fn certification_lifecycle_across_contracts_bump() {
        let evidence = evidence_at("2026.1");
        let bump_at = DateTime::parse_from_rfc3339("2026-03-01T00:00:00Z")
            .unwrap()
            .to_utc();
        let window = Duration::days(30);

        // No bump: current.
        assert_eq!(
            certification_status(&evidence, "2026.1", bump_at, window, bump_at),
            CertificationStatus::Current
        );

        // Inside the skew window: re-certification required, still visible.
        let inside = bump_at + Duration::days(10);
        assert_eq!(
            certification_status(&evidence, "2026.2", bump_at, window, inside),
            CertificationStatus::ReCertificationRequired {
                certified_against: "2026.1".to_string()
            }
        );

        // Past the window: expired, drops from the default view.
        let after = bump_at + Duration::days(31);
        assert_eq!(
            certification_status(&evidence, "2026.2", bump_at, window, after),
            CertificationStatus::Expired {
                certified_against: "2026.1".to_string()
            }
        );
    }

    #[test]
    fn evidence_summary_renders_the_bar() {
        let evidence = GateEvidence {
            fixture_freshness: FixtureFreshness::Stale {
                stale_fixtures: vec![("servicenow-tickets".to_string(), 45)],
            },
            ..evidence_at("2026.1")
        };
        let rendered = render_evidence_summary(&evidence);
        assert!(rendered.contains("evals: suite — 4/4 cases passed"));
        assert!(rendered.contains("conformance: tool-pipeline v1.0 — 3/3 cases passed"));
        assert!(rendered.contains("docs: https://example.com/docs"));
        assert!(rendered.contains("STALE"));
        assert!(rendered.contains("servicenow-tickets (45d old)"));
        assert!(rendered.contains("certified against contracts 2026.1"));
    }
}
