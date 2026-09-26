//! Deterministic clustering of failed experiment runs.
//!
//! [`cluster_failures`] groups failures by normalized, machine-verifiable
//! causes. Raw executor errors, assertion observations, and judge rationales
//! remain in the source report; cluster artifacts carry only source
//! coordinates, categories, and content fingerprints. This keeps grouping
//! stable across volatile request ids without copying sensitive evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{EvalError, Result};
use crate::evidence::RunStatus;
use crate::experiment::{CaseRunReport, ExperimentReport};

/// Wire version for [`FailureClusterReport`].
pub const FAILURE_CLUSTER_REPORT_FORMAT_VERSION: u64 = 1;

/// Coarse execution outcome used in a failure signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureTermination {
    /// Execution completed, but an assertion or judge rejected the result.
    Done,
    /// Execution suspended before producing a terminal result.
    Interrupted,
    /// Execution returned an error.
    Failed,
}

/// Normalized executor-error category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionFailureCategory {
    /// Missing, rejected, or expired authentication material.
    Authentication,
    /// Authenticated principal lacks permission.
    Authorization,
    /// Deadline or timeout exhaustion.
    Timeout,
    /// Provider or tenant rate/quota limit.
    RateLimit,
    /// Connection, DNS, or other transport failure.
    Transport,
    /// Upstream service is unavailable or returned a server error.
    Unavailable,
    /// Invalid schema, JSON, parsing, or validation input.
    InvalidData,
    /// Explicit cancellation.
    Cancelled,
    /// No stable category matched.
    Unknown,
}

/// Normalized primary cause of a failed run.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum FailureCause {
    /// A completed run failed evaluation.
    Evaluation {
        /// Fingerprint of assertion configurations and judge rationale.
        fingerprint: String,
    },
    /// A run suspended before completion.
    Interrupted,
    /// A normalized executor error.
    Execution {
        /// Actionable error family.
        category: ExecutionFailureCategory,
        /// Fingerprint of normalized error text with volatile ids removed.
        fingerprint: String,
    },
}

/// One failed assertion configuration within a cluster signature.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct AssertionFailureKey {
    /// Stable assertion report key.
    pub assertion: String,
    /// Fingerprint of the assertion's expected/configuration value.
    pub expected_fingerprint: String,
}

/// The stable, explainable dimensions that define one failure cluster.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct FailureSignature {
    /// How runs in this cluster terminated.
    pub termination: FailureTermination,
    /// Primary normalized failure cause.
    pub cause: FailureCause,
    /// Sorted, deduplicated failed assertion configurations.
    pub failed_assertions: Vec<AssertionFailureKey>,
    /// Fingerprint of a rejecting judge rationale, when present.
    pub judge_fingerprint: Option<String>,
}

/// Safe structured evidence retained in a cluster artifact.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "evidence", rename_all = "snake_case")]
pub enum FailureEvidenceRef {
    /// Normalized executor-error evidence.
    Execution {
        /// Actionable error family.
        category: ExecutionFailureCategory,
        /// Fingerprint of normalized error text.
        fingerprint: String,
    },
    /// The source run suspended.
    Interrupted,
    /// Failed deterministic assertion configuration.
    Assertion {
        /// Stable assertion report key.
        assertion: String,
        /// Fingerprint of expected/configuration value; observed data is not copied.
        expected_fingerprint: String,
    },
    /// Rejecting judge rationale fingerprint; rationale text is not copied.
    Judge {
        /// Fingerprint of normalized rationale text.
        rationale_fingerprint: String,
    },
}

/// One failed run linked back to its experiment evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FailureOccurrence {
    /// Dataset case id.
    pub case_id: String,
    /// Zero-based run repetition.
    pub repetition: usize,
    /// Sorted case tags for filtering and ownership routing.
    pub tags: Vec<String>,
    /// Safe evidence references; raw values remain in the source report.
    pub evidence: Vec<FailureEvidenceRef>,
}

/// A group of failed runs with the same [`FailureSignature`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FailureCluster {
    /// Stable SHA-256-derived identity of the signature.
    pub id: String,
    /// Machine-verifiable grouping dimensions.
    pub signature: FailureSignature,
    /// Number of members in this cluster.
    pub occurrences: usize,
    /// `occurrences / total_failures`.
    pub share_of_failures: f64,
    /// Members sorted by case id and repetition.
    pub members: Vec<FailureOccurrence>,
}

/// Ranked failure clusters for one experiment report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FailureClusterReport {
    /// Report schema version.
    pub format_version: u64,
    /// Source experiment identity.
    pub experiment: String,
    /// Source dataset name.
    pub dataset_name: String,
    /// Source dataset version.
    pub dataset_version: String,
    /// Number of source case runs.
    pub total_runs: usize,
    /// Number of failed source case runs.
    pub total_failures: usize,
    /// Clusters ordered by descending frequency, then signature.
    pub clusters: Vec<FailureCluster>,
}

impl FailureClusterReport {
    /// Serialize a validated clustering artifact as pretty JSON.
    pub fn to_json(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Parse an artifact and recompute its canonical value from source evidence.
    ///
    /// The returned value is always the recomputed report, never untrusted
    /// caller-supplied derived fields. Raw deserialization is intentionally not
    /// implemented for the public artifact types.
    pub fn from_json(text: &str, source: &ExperimentReport) -> Result<Self> {
        let header: FormatHeader = serde_json::from_str(text)?;
        if header.format_version != FAILURE_CLUSTER_REPORT_FORMAT_VERSION {
            return clustering_error(format!(
                "unsupported failure cluster format version: found {}, this build supports {}",
                header.format_version, FAILURE_CLUSTER_REPORT_FORMAT_VERSION
            ));
        }
        let supplied = FailureClusterReportWire::deserialize_report(text)?;
        supplied.validate()?;
        let expected = cluster_failures(source)?;
        if !reports_match(&supplied, &expected) {
            return clustering_error("failure cluster report does not match its source evidence");
        }
        Ok(expected)
    }

    fn validate(&self) -> Result<()> {
        if self.format_version != FAILURE_CLUSTER_REPORT_FORMAT_VERSION {
            return clustering_error(format!(
                "unsupported failure cluster format version: found {}, this build supports {}",
                self.format_version, FAILURE_CLUSTER_REPORT_FORMAT_VERSION
            ));
        }
        require_non_empty("experiment", &self.experiment)?;
        require_non_empty("dataset name", &self.dataset_name)?;
        require_non_empty("dataset version", &self.dataset_version)?;
        if self.total_failures > self.total_runs {
            return clustering_error("total failures cannot exceed total runs");
        }
        if (self.total_failures == 0) != self.clusters.is_empty() {
            return clustering_error("clusters must be empty exactly when there are no failures");
        }

        let mut signatures = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut occurrences = BTreeSet::new();
        let mut counted_failures = 0_usize;
        for cluster in &self.clusters {
            validate_signature(&cluster.signature)?;
            let expected_id = signature_id(&cluster.signature);
            if cluster.id != expected_id {
                return clustering_error(format!(
                    "cluster id `{}` does not match its signature",
                    cluster.id
                ));
            }
            if !ids.insert(cluster.id.as_str()) || !signatures.insert(&cluster.signature) {
                return clustering_error("cluster ids and signatures must be unique");
            }
            if cluster.occurrences == 0 || cluster.occurrences != cluster.members.len() {
                return clustering_error(format!(
                    "cluster `{}` occurrence count does not match its members",
                    cluster.id
                ));
            }
            counted_failures = counted_failures
                .checked_add(cluster.occurrences)
                .ok_or_else(|| EvalError::Clustering("failure count overflows usize".to_owned()))?;
            let expected_share = cluster.occurrences as f64 / self.total_failures as f64;
            if !same_float(cluster.share_of_failures, expected_share) {
                return clustering_error(format!(
                    "cluster `{}` share does not match its occurrence count",
                    cluster.id
                ));
            }
            if !cluster
                .members
                .windows(2)
                .all(|pair| member_key(&pair[0]) < member_key(&pair[1]))
            {
                return clustering_error(format!(
                    "cluster `{}` members are not in canonical order",
                    cluster.id
                ));
            }
            for member in &cluster.members {
                require_non_empty("failure case id", &member.case_id)?;
                if member.evidence.is_empty()
                    || !is_sorted_unique(&member.evidence)
                    || !is_sorted_unique(&member.tags)
                {
                    return clustering_error(format!(
                        "cluster `{}` contains non-canonical member evidence",
                        cluster.id
                    ));
                }
                let member_signature = signature_from_evidence(&member.evidence)?;
                if member_signature != cluster.signature {
                    return clustering_error(format!(
                        "cluster `{}` member evidence does not match its signature",
                        cluster.id
                    ));
                }
                if !occurrences.insert((member.case_id.as_str(), member.repetition)) {
                    return clustering_error(format!(
                        "failure occurrence `{}#{}` appears more than once",
                        member.case_id, member.repetition
                    ));
                }
            }
        }
        if counted_failures != self.total_failures {
            return clustering_error("cluster occurrence counts do not equal total failures");
        }
        if !self
            .clusters
            .windows(2)
            .all(|pair| cluster_sort_key(&pair[0]) < cluster_sort_key(&pair[1]))
        {
            return clustering_error("clusters are not in canonical rank order");
        }
        Ok(())
    }
}

/// Group every failed run in an experiment by deterministic root-cause shape.
pub fn cluster_failures(source: &ExperimentReport) -> Result<FailureClusterReport> {
    crate::gate::validate_report(source)
        .map_err(|error| EvalError::Clustering(format!("source report is invalid: {error}")))?;

    let mut grouped: BTreeMap<FailureSignature, Vec<FailureOccurrence>> = BTreeMap::new();
    for case in &source.cases {
        for run in &case.runs {
            if run.passed {
                continue;
            }
            let mut evidence = evidence_refs(run);
            evidence.sort();
            evidence.dedup();
            let signature = signature_from_evidence(&evidence)?;
            let mut tags = case.tags.clone();
            tags.sort();
            tags.dedup();
            grouped
                .entry(signature)
                .or_default()
                .push(FailureOccurrence {
                    case_id: case.case_id.clone(),
                    repetition: run.repetition,
                    tags,
                    evidence,
                });
        }
    }

    let total_failures = grouped.values().map(Vec::len).sum();
    let mut clusters: Vec<_> = grouped
        .into_iter()
        .map(|(signature, mut members)| {
            members.sort_by(|left, right| member_key(left).cmp(&member_key(right)));
            let occurrences = members.len();
            FailureCluster {
                id: signature_id(&signature),
                signature,
                occurrences,
                share_of_failures: occurrences as f64 / total_failures as f64,
                members,
            }
        })
        .collect();
    clusters.sort_by(|left, right| cluster_sort_key(left).cmp(&cluster_sort_key(right)));

    let result = FailureClusterReport {
        format_version: FAILURE_CLUSTER_REPORT_FORMAT_VERSION,
        experiment: source.name.clone(),
        dataset_name: source.dataset_name.clone(),
        dataset_version: source.dataset_version.clone(),
        total_runs: source.summary.runs,
        total_failures,
        clusters,
    };
    result.validate()?;
    Ok(result)
}

fn evidence_refs(run: &CaseRunReport) -> Vec<FailureEvidenceRef> {
    let mut evidence = Vec::new();
    match &run.status {
        RunStatus::Done => {}
        RunStatus::Interrupted => evidence.push(FailureEvidenceRef::Interrupted),
        RunStatus::Failed { error } => evidence.push(FailureEvidenceRef::Execution {
            category: classify_execution_error(error),
            fingerprint: text_fingerprint("execution", error),
        }),
    }
    for assertion in run.assertions.iter().filter(|assertion| !assertion.passed) {
        evidence.push(FailureEvidenceRef::Assertion {
            assertion: assertion.assertion.clone(),
            expected_fingerprint: value_fingerprint(&assertion.expected),
        });
    }
    if let Some(judge) = run.judge.as_ref().filter(|judge| !judge.passed) {
        evidence.push(FailureEvidenceRef::Judge {
            rationale_fingerprint: text_fingerprint("judge", &judge.rationale),
        });
    }
    evidence
}

fn signature_from_evidence(evidence: &[FailureEvidenceRef]) -> Result<FailureSignature> {
    let mut primary: Option<FailureCause> = None;
    let mut failed_assertions = Vec::new();
    let mut judge_fingerprint = None;
    for item in evidence {
        match item {
            FailureEvidenceRef::Execution {
                category,
                fingerprint,
            } => set_primary(
                &mut primary,
                FailureCause::Execution {
                    category: *category,
                    fingerprint: fingerprint.clone(),
                },
            )?,
            FailureEvidenceRef::Interrupted => {
                set_primary(&mut primary, FailureCause::Interrupted)?;
            }
            FailureEvidenceRef::Assertion {
                assertion,
                expected_fingerprint,
            } => failed_assertions.push(AssertionFailureKey {
                assertion: assertion.clone(),
                expected_fingerprint: expected_fingerprint.clone(),
            }),
            FailureEvidenceRef::Judge {
                rationale_fingerprint,
            } => {
                if judge_fingerprint
                    .replace(rationale_fingerprint.clone())
                    .is_some()
                {
                    return clustering_error("member evidence contains multiple judge failures");
                }
            }
        }
    }
    failed_assertions.sort();
    failed_assertions.dedup();
    let (termination, cause) = match primary {
        Some(FailureCause::Execution {
            category,
            fingerprint,
        }) => (
            FailureTermination::Failed,
            FailureCause::Execution {
                category,
                fingerprint,
            },
        ),
        Some(FailureCause::Interrupted) => {
            (FailureTermination::Interrupted, FailureCause::Interrupted)
        }
        Some(FailureCause::Evaluation { .. }) => {
            return clustering_error("evaluation cannot be primary member evidence")
        }
        None if !failed_assertions.is_empty() || judge_fingerprint.is_some() => (
            FailureTermination::Done,
            FailureCause::Evaluation {
                fingerprint: evaluation_fingerprint(
                    &failed_assertions,
                    judge_fingerprint.as_deref(),
                ),
            },
        ),
        None => return clustering_error("failed member has no failure evidence"),
    };
    Ok(FailureSignature {
        termination,
        cause,
        failed_assertions,
        judge_fingerprint,
    })
}

fn set_primary(target: &mut Option<FailureCause>, cause: FailureCause) -> Result<()> {
    if target.replace(cause).is_some() {
        return clustering_error("member evidence contains multiple termination causes");
    }
    Ok(())
}

fn validate_signature(signature: &FailureSignature) -> Result<()> {
    if !is_sorted_unique(&signature.failed_assertions) {
        return clustering_error("failed assertion keys must be sorted and unique");
    }
    for failure in &signature.failed_assertions {
        require_non_empty("assertion key", &failure.assertion)?;
        validate_fingerprint("assertion expectation", &failure.expected_fingerprint)?;
    }
    if let Some(fingerprint) = &signature.judge_fingerprint {
        validate_fingerprint("judge rationale", fingerprint)?;
    }
    match (&signature.termination, &signature.cause) {
        (FailureTermination::Done, FailureCause::Evaluation { fingerprint }) => {
            validate_fingerprint("evaluation cause", fingerprint)?;
            let expected = evaluation_fingerprint(
                &signature.failed_assertions,
                signature.judge_fingerprint.as_deref(),
            );
            if fingerprint != &expected {
                return clustering_error("evaluation cause fingerprint is not canonical");
            }
            if signature.failed_assertions.is_empty() && signature.judge_fingerprint.is_none() {
                return clustering_error("completed failure needs assertion or judge evidence");
            }
        }
        (FailureTermination::Failed, FailureCause::Execution { fingerprint, .. }) => {
            validate_fingerprint("execution cause", fingerprint)?
        }
        (FailureTermination::Interrupted, FailureCause::Interrupted) => {}
        _ => return clustering_error("termination and failure cause do not agree"),
    }
    Ok(())
}

fn classify_execution_error(error: &str) -> ExecutionFailureCategory {
    let lower = error.to_ascii_lowercase();
    if contains_any(&lower, &["unauthorized", "authentication", "invalid token"])
        || contains_status_code(&lower, "401")
    {
        ExecutionFailureCategory::Authentication
    } else if contains_any(&lower, &["forbidden", "permission denied", "not permitted"])
        || contains_status_code(&lower, "403")
    {
        ExecutionFailureCategory::Authorization
    } else if contains_any(&lower, &["timeout", "timed out", "deadline exceeded"]) {
        ExecutionFailureCategory::Timeout
    } else if contains_any(&lower, &["rate limit", "too many requests", "quota"])
        || contains_status_code(&lower, "429")
    {
        ExecutionFailureCategory::RateLimit
    } else if contains_any(&lower, &["cancelled", "canceled"]) {
        ExecutionFailureCategory::Cancelled
    } else if contains_any(
        &lower,
        &["connection", "connect", "dns", "network", "transport"],
    ) {
        ExecutionFailureCategory::Transport
    } else if contains_any(&lower, &["unavailable", "server error"])
        || ["500", "502", "503", "504"]
            .iter()
            .any(|code| contains_status_code(&lower, code))
    {
        ExecutionFailureCategory::Unavailable
    } else if contains_any(
        &lower,
        &[
            "schema",
            "deserialize",
            "invalid json",
            "parse error",
            "validation",
        ],
    ) {
        ExecutionFailureCategory::InvalidData
    } else {
        ExecutionFailureCategory::Unknown
    }
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn contains_status_code(value: &str, code: &str) -> bool {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| token == code)
}

fn normalized_text(value: &str) -> String {
    scrub_volatile_ids(value)
        .split_whitespace()
        .map(|raw| {
            let token = raw.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '-'
            });
            let mut normalized = String::new();
            let mut replacing_digits = false;
            for character in token.chars().flat_map(char::to_lowercase) {
                if character.is_ascii_digit() {
                    if !replacing_digits {
                        normalized.push('#');
                        replacing_digits = true;
                    }
                } else {
                    replacing_digits = false;
                    if character.is_alphanumeric() || matches!(character, '_' | '-' | '.' | '/') {
                        normalized.push(character);
                    }
                }
            }
            normalized
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn scrub_volatile_ids(value: &str) -> String {
    let characters: Vec<_> = value.chars().collect();
    let mut scrubbed = String::with_capacity(value.len());
    let mut index = 0;

    while index < characters.len() {
        if is_uuid_at(&characters, index) {
            scrubbed.push_str(" volatileid ");
            index += 36;
            continue;
        }

        if characters[index].is_ascii_hexdigit()
            && (index == 0 || !characters[index - 1].is_ascii_alphanumeric())
        {
            let end = characters[index..]
                .iter()
                .position(|character| !character.is_ascii_hexdigit())
                .map_or(characters.len(), |offset| index + offset);
            let candidate = &characters[index..end];
            if candidate.len() >= 8
                && candidate.iter().any(|character| character.is_ascii_digit())
                && (end == characters.len() || !characters[end].is_ascii_alphanumeric())
            {
                scrubbed.push_str(" volatileid ");
                index = end;
                continue;
            }
        }

        scrubbed.push(characters[index]);
        index += 1;
    }

    scrubbed
}

fn is_uuid_at(characters: &[char], start: usize) -> bool {
    const UUID_LENGTH: usize = 36;
    const HYPHENS: [usize; 4] = [8, 13, 18, 23];

    let Some(candidate) = characters.get(start..start + UUID_LENGTH) else {
        return false;
    };
    if (start > 0 && characters[start - 1].is_ascii_alphanumeric())
        || characters
            .get(start + UUID_LENGTH)
            .is_some_and(|character| character.is_ascii_alphanumeric())
    {
        return false;
    }

    candidate.iter().enumerate().all(|(offset, character)| {
        if HYPHENS.contains(&offset) {
            *character == '-'
        } else {
            character.is_ascii_hexdigit()
        }
    })
}

fn text_fingerprint(domain: &str, value: &str) -> String {
    fingerprint(domain, normalized_text(value).as_bytes())
}

fn value_fingerprint(value: &Value) -> String {
    let mut canonical = Vec::new();
    append_canonical_json(&mut canonical, value);
    fingerprint("assertion-expected", &canonical)
}

fn append_canonical_json(bytes: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => bytes.push(0),
        Value::Bool(value) => {
            bytes.push(1);
            bytes.push(u8::from(*value));
        }
        Value::Number(value) => {
            bytes.push(2);
            append_bytes(bytes, value.to_string().as_bytes());
        }
        Value::String(value) => {
            bytes.push(3);
            append_bytes(bytes, value.as_bytes());
        }
        Value::Array(values) => {
            bytes.push(4);
            append_u64(bytes, values.len() as u64);
            for value in values {
                append_canonical_json(bytes, value);
            }
        }
        Value::Object(values) => {
            bytes.push(5);
            append_u64(bytes, values.len() as u64);
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by_key(|(key, _)| *key);
            for (key, value) in entries {
                append_bytes(bytes, key.as_bytes());
                append_canonical_json(bytes, value);
            }
        }
    }
}

fn evaluation_fingerprint(
    assertions: &[AssertionFailureKey],
    judge_fingerprint: Option<&str>,
) -> String {
    let mut bytes = Vec::new();
    append_u64(&mut bytes, assertions.len() as u64);
    for assertion in assertions {
        append_bytes(&mut bytes, assertion.assertion.as_bytes());
        append_bytes(&mut bytes, assertion.expected_fingerprint.as_bytes());
    }
    match judge_fingerprint {
        Some(fingerprint) => {
            bytes.push(1);
            append_bytes(&mut bytes, fingerprint.as_bytes());
        }
        None => bytes.push(0),
    }
    fingerprint("evaluation", &bytes)
}

fn fingerprint(domain: &str, value: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"rusty.failure-fingerprint.v1\0");
    hasher.update((domain.len() as u64).to_be_bytes());
    hasher.update(domain.as_bytes());
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
    format_digest("fp_", &hasher.finalize())
}

fn signature_id(signature: &FailureSignature) -> String {
    let encoded = serde_json::to_vec(signature).expect("serializing a signature cannot fail");
    let mut hasher = Sha256::new();
    hasher.update(b"rusty.failure-cluster.v1\0");
    hasher.update((encoded.len() as u64).to_be_bytes());
    hasher.update(encoded);
    format_digest("fc_", &hasher.finalize())
}

fn format_digest(prefix: &str, digest: &[u8]) -> String {
    let mut result = String::from(prefix);
    for byte in &digest[..16] {
        write!(&mut result, "{byte:02x}").expect("writing to a String cannot fail");
    }
    result
}

fn append_u64(target: &mut Vec<u8>, value: u64) {
    target.extend_from_slice(&value.to_be_bytes());
}

fn append_bytes(target: &mut Vec<u8>, value: &[u8]) {
    append_u64(target, value.len() as u64);
    target.extend_from_slice(value);
}

fn validate_fingerprint(label: &str, fingerprint: &str) -> Result<()> {
    if fingerprint.len() != 35
        || !fingerprint.starts_with("fp_")
        || !fingerprint[3..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return clustering_error(format!("{label} fingerprint is malformed"));
    }
    Ok(())
}

fn member_key(member: &FailureOccurrence) -> (&str, usize) {
    (&member.case_id, member.repetition)
}

fn cluster_sort_key(cluster: &FailureCluster) -> (std::cmp::Reverse<usize>, &FailureSignature) {
    (std::cmp::Reverse(cluster.occurrences), &cluster.signature)
}

fn is_sorted_unique<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn require_non_empty(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return clustering_error(format!("{label} must not be empty"));
    }
    Ok(())
}

fn same_float(actual: f64, expected: f64) -> bool {
    if !actual.is_finite() || !expected.is_finite() {
        return false;
    }
    if actual == expected {
        return true;
    }
    let scale = actual.abs().max(expected.abs());
    (actual - expected).abs() <= 16.0 * f64::EPSILON * scale
}

fn reports_match(actual: &FailureClusterReport, expected: &FailureClusterReport) -> bool {
    actual.format_version == expected.format_version
        && actual.experiment == expected.experiment
        && actual.dataset_name == expected.dataset_name
        && actual.dataset_version == expected.dataset_version
        && actual.total_runs == expected.total_runs
        && actual.total_failures == expected.total_failures
        && actual.clusters.len() == expected.clusters.len()
        && actual
            .clusters
            .iter()
            .zip(&expected.clusters)
            .all(|(actual, expected)| {
                actual.id == expected.id
                    && actual.signature == expected.signature
                    && actual.occurrences == expected.occurrences
                    && same_float(actual.share_of_failures, expected.share_of_failures)
                    && actual.members == expected.members
            })
}

fn clustering_error<T>(message: impl Into<String>) -> Result<T> {
    Err(EvalError::Clustering(message.into()))
}

#[derive(Deserialize)]
struct FormatHeader {
    format_version: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssertionFailureKeyWire {
    assertion: String,
    expected_fingerprint: String,
}

impl From<AssertionFailureKeyWire> for AssertionFailureKey {
    fn from(value: AssertionFailureKeyWire) -> Self {
        Self {
            assertion: value.assertion,
            expected_fingerprint: value.expected_fingerprint,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case", deny_unknown_fields)]
enum FailureCauseWire {
    Evaluation {
        fingerprint: String,
    },
    Interrupted,
    Execution {
        category: ExecutionFailureCategory,
        fingerprint: String,
    },
}

impl From<FailureCauseWire> for FailureCause {
    fn from(value: FailureCauseWire) -> Self {
        match value {
            FailureCauseWire::Evaluation { fingerprint } => Self::Evaluation { fingerprint },
            FailureCauseWire::Interrupted => Self::Interrupted,
            FailureCauseWire::Execution {
                category,
                fingerprint,
            } => Self::Execution {
                category,
                fingerprint,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FailureSignatureWire {
    termination: FailureTermination,
    cause: FailureCauseWire,
    failed_assertions: Vec<AssertionFailureKeyWire>,
    judge_fingerprint: Option<String>,
}

impl From<FailureSignatureWire> for FailureSignature {
    fn from(value: FailureSignatureWire) -> Self {
        Self {
            termination: value.termination,
            cause: value.cause.into(),
            failed_assertions: value
                .failed_assertions
                .into_iter()
                .map(Into::into)
                .collect(),
            judge_fingerprint: value.judge_fingerprint,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "evidence", rename_all = "snake_case", deny_unknown_fields)]
enum FailureEvidenceRefWire {
    Execution {
        category: ExecutionFailureCategory,
        fingerprint: String,
    },
    Interrupted,
    Assertion {
        assertion: String,
        expected_fingerprint: String,
    },
    Judge {
        rationale_fingerprint: String,
    },
}

impl From<FailureEvidenceRefWire> for FailureEvidenceRef {
    fn from(value: FailureEvidenceRefWire) -> Self {
        match value {
            FailureEvidenceRefWire::Execution {
                category,
                fingerprint,
            } => Self::Execution {
                category,
                fingerprint,
            },
            FailureEvidenceRefWire::Interrupted => Self::Interrupted,
            FailureEvidenceRefWire::Assertion {
                assertion,
                expected_fingerprint,
            } => Self::Assertion {
                assertion,
                expected_fingerprint,
            },
            FailureEvidenceRefWire::Judge {
                rationale_fingerprint,
            } => Self::Judge {
                rationale_fingerprint,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FailureOccurrenceWire {
    case_id: String,
    repetition: usize,
    tags: Vec<String>,
    evidence: Vec<FailureEvidenceRefWire>,
}

impl From<FailureOccurrenceWire> for FailureOccurrence {
    fn from(value: FailureOccurrenceWire) -> Self {
        Self {
            case_id: value.case_id,
            repetition: value.repetition,
            tags: value.tags,
            evidence: value.evidence.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FailureClusterWire {
    id: String,
    signature: FailureSignatureWire,
    occurrences: usize,
    share_of_failures: f64,
    members: Vec<FailureOccurrenceWire>,
}

impl From<FailureClusterWire> for FailureCluster {
    fn from(value: FailureClusterWire) -> Self {
        Self {
            id: value.id,
            signature: value.signature.into(),
            occurrences: value.occurrences,
            share_of_failures: value.share_of_failures,
            members: value.members.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FailureClusterReportWire {
    format_version: u64,
    experiment: String,
    dataset_name: String,
    dataset_version: String,
    total_runs: usize,
    total_failures: usize,
    clusters: Vec<FailureClusterWire>,
}

impl FailureClusterReportWire {
    fn deserialize_report(text: &str) -> Result<FailureClusterReport> {
        let wire: Self = serde_json::from_str(text)?;
        Ok(FailureClusterReport {
            format_version: wire.format_version,
            experiment: wire.experiment,
            dataset_name: wire.dataset_name,
            dataset_version: wire.dataset_version,
            total_runs: wire.total_runs,
            total_failures: wire.total_failures,
            clusters: wire.clusters.into_iter().map(Into::into).collect(),
        })
    }
}
