//! Baseline-vs-candidate comparison of experiment reports.
//!
//! [`fn@compare`] takes two [`ExperimentReport`]s — typically a stored baseline
//! and a fresh candidate — and answers the release question: did anything
//! get worse beyond an acceptable threshold? It reports per-assertion pass
//! rate deltas, per-case regressions and improvements, latency and cost
//! movement, and a list of [`Regression`] flags. The verdict is deliberately
//! simple and explainable: a threshold breach is a regression, everything
//! else is noise or improvement.

use serde::{Deserialize, Serialize};

use crate::experiment::ExperimentReport;

/// How far the candidate may fall behind the baseline before it counts as a
/// regression.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompareThresholds {
    /// Maximum tolerable absolute pass-rate drop, per assertion and per case
    /// (e.g. `0.05`: a case dropping from 1.0 to 0.9 is within tolerance,
    /// 1.0 to 0.94 flags).
    pub max_pass_rate_drop: f64,

    /// Maximum tolerable p95 latency ratio `candidate / baseline` (e.g.
    /// `1.25`: p95 may grow by 25%).
    pub max_latency_p95_ratio: f64,
}

impl Default for CompareThresholds {
    /// 5-point pass-rate drop tolerance, 25% p95 latency tolerance.
    fn default() -> Self {
        Self {
            max_pass_rate_drop: 0.05,
            max_latency_p95_ratio: 1.25,
        }
    }
}

/// How a case moved between baseline and candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseChange {
    /// Candidate pass rate is higher.
    Improved,
    /// Candidate pass rate is lower (threshold decides whether it flags).
    Regressed,
    /// Same pass rate on both sides.
    Unchanged,
    /// Present only in the candidate.
    Added,
    /// Present only in the baseline.
    Removed,
}

/// Pass-rate movement for one assertion key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionDelta {
    /// The assertion's report key.
    pub assertion: String,
    /// Baseline pass rate.
    pub baseline_rate: f64,
    /// Candidate pass rate.
    pub candidate_rate: f64,
    /// `candidate_rate - baseline_rate` (negative is worse).
    pub delta: f64,
}

/// Pass-rate movement for one case. Cases present on only one side carry
/// `None` for the missing side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseDelta {
    /// The case id.
    pub case_id: String,
    /// Baseline pass rate, when the case exists there.
    pub baseline_pass_rate: Option<f64>,
    /// Candidate pass rate, when the case exists there.
    pub candidate_pass_rate: Option<f64>,
    /// The classification.
    pub change: CaseChange,
}

/// Latency movement between the two summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyDelta {
    /// Baseline median (ms).
    pub baseline_p50: u64,
    /// Candidate median (ms).
    pub candidate_p50: u64,
    /// `candidate_p50 / baseline_p50` (`null` when the baseline is zero).
    pub p50_ratio: Option<f64>,
    /// Baseline p95 (ms).
    pub baseline_p95: u64,
    /// Candidate p95 (ms).
    pub candidate_p95: u64,
    /// `candidate_p95 / baseline_p95` (`null` when the baseline is zero).
    pub p95_ratio: Option<f64>,
}

/// A threshold breach: one concrete, explainable regression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "regression", rename_all = "snake_case")]
pub enum Regression {
    /// An assertion's pass rate dropped beyond the threshold.
    AssertionPassRate {
        /// The assertion's report key.
        assertion: String,
        /// Baseline pass rate.
        baseline: f64,
        /// Candidate pass rate.
        candidate: f64,
    },
    /// A case's pass rate dropped beyond the threshold.
    CasePassRate {
        /// The case id.
        case_id: String,
        /// Baseline pass rate.
        baseline: f64,
        /// Candidate pass rate.
        candidate: f64,
    },
    /// p95 latency grew beyond the threshold ratio.
    LatencyP95 {
        /// Baseline p95 (ms).
        baseline_ms: u64,
        /// Candidate p95 (ms).
        candidate_ms: u64,
        /// `candidate / baseline` (`inf` is serialized as `1e999`-style JSON
        /// only by extension; in practice this is a finite ratio or the
        /// zero-baseline case, recorded as `-1.0` — see [`fn@compare`]).
        ratio: f64,
    },
}

/// The full comparison of a candidate against a baseline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonReport {
    /// Baseline experiment identity.
    pub baseline: String,
    /// Candidate experiment identity.
    pub candidate: String,
    /// The thresholds applied.
    pub thresholds: CompareThresholds,
    /// Per-assertion pass-rate deltas (keys present in both reports).
    pub assertion_deltas: Vec<AssertionDelta>,
    /// Per-case movement (union of case ids, sorted).
    pub case_deltas: Vec<CaseDelta>,
    /// Latency movement.
    pub latency: LatencyDelta,
    /// Baseline total cost (USD).
    pub baseline_cost_usd: f64,
    /// Candidate total cost (USD).
    pub candidate_cost_usd: f64,
    /// Every threshold breach.
    pub regressions: Vec<Regression>,
    /// `true` when any regression flagged — the release-gate bit.
    pub regressed: bool,
}

/// Compare `candidate` against `baseline` under `thresholds`.
///
/// Matching is by key: assertions by report key, cases by case id. Keys
/// present on only one side are reported (`Added` / `Removed` for cases;
/// one-sided assertions are listed in neither delta nor flags — comparing
/// rates against nothing would invent a baseline).
///
/// Zero-baseline latency: when the baseline p95 is 0 ms a ratio is
/// meaningless, so any positive candidate p95 is treated as breaching and
/// recorded with ratio `-1.0` (a sentinel, since `inf` is not JSON).
pub fn compare(
    baseline: &ExperimentReport,
    candidate: &ExperimentReport,
    thresholds: &CompareThresholds,
) -> ComparisonReport {
    let mut regressions = Vec::new();

    // ---- per-assertion deltas (shared keys only) ----
    let mut assertion_deltas = Vec::new();
    for base in &baseline.summary.assertions {
        if let Some(cand) = candidate
            .summary
            .assertions
            .iter()
            .find(|cand| cand.assertion == base.assertion)
        {
            let delta = cand.rate - base.rate;
            if -delta > thresholds.max_pass_rate_drop {
                regressions.push(Regression::AssertionPassRate {
                    assertion: base.assertion.clone(),
                    baseline: base.rate,
                    candidate: cand.rate,
                });
            }
            assertion_deltas.push(AssertionDelta {
                assertion: base.assertion.clone(),
                baseline_rate: base.rate,
                candidate_rate: cand.rate,
                delta,
            });
        }
    }

    // ---- per-case deltas (union, sorted by case id) ----
    let mut case_deltas = Vec::new();
    let mut ids: Vec<&str> = baseline
        .cases
        .iter()
        .map(|case| case.case_id.as_str())
        .chain(candidate.cases.iter().map(|case| case.case_id.as_str()))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    for id in ids {
        let base = baseline.cases.iter().find(|case| case.case_id == id);
        let cand = candidate.cases.iter().find(|case| case.case_id == id);
        let (baseline_pass_rate, candidate_pass_rate, change) = match (base, cand) {
            (Some(base), Some(cand)) => {
                let change = if cand.pass_rate < base.pass_rate {
                    CaseChange::Regressed
                } else if cand.pass_rate > base.pass_rate {
                    CaseChange::Improved
                } else {
                    CaseChange::Unchanged
                };
                if base.pass_rate - cand.pass_rate > thresholds.max_pass_rate_drop {
                    regressions.push(Regression::CasePassRate {
                        case_id: id.to_owned(),
                        baseline: base.pass_rate,
                        candidate: cand.pass_rate,
                    });
                }
                (Some(base.pass_rate), Some(cand.pass_rate), change)
            }
            (Some(base), None) => (Some(base.pass_rate), None, CaseChange::Removed),
            (None, Some(cand)) => (None, Some(cand.pass_rate), CaseChange::Added),
            (None, None) => unreachable!("case id came from one of the two reports"),
        };
        case_deltas.push(CaseDelta {
            case_id: id.to_owned(),
            baseline_pass_rate,
            candidate_pass_rate,
            change,
        });
    }

    // ---- latency ----
    let ratio =
        |base: u64, cand: u64| -> Option<f64> { (base > 0).then(|| cand as f64 / base as f64) };
    let baseline_p95 = baseline.summary.latency_ms.p95;
    let candidate_p95 = candidate.summary.latency_ms.p95;
    let p95_breach = match ratio(baseline_p95, candidate_p95) {
        Some(r) => r > thresholds.max_latency_p95_ratio,
        // Baseline measured 0 ms: any positive candidate p95 breaches.
        None => candidate_p95 > 0,
    };
    if p95_breach {
        regressions.push(Regression::LatencyP95 {
            baseline_ms: baseline_p95,
            candidate_ms: candidate_p95,
            ratio: ratio(baseline_p95, candidate_p95).unwrap_or(-1.0),
        });
    }
    let latency = LatencyDelta {
        baseline_p50: baseline.summary.latency_ms.p50,
        candidate_p50: candidate.summary.latency_ms.p50,
        p50_ratio: ratio(
            baseline.summary.latency_ms.p50,
            candidate.summary.latency_ms.p50,
        ),
        baseline_p95,
        candidate_p95,
        p95_ratio: ratio(baseline_p95, candidate_p95),
    };

    let regressed = !regressions.is_empty();
    ComparisonReport {
        baseline: baseline.name.clone(),
        candidate: candidate.name.clone(),
        thresholds: *thresholds,
        assertion_deltas,
        case_deltas,
        latency,
        baseline_cost_usd: baseline.summary.total_cost_usd,
        candidate_cost_usd: candidate.summary.total_cost_usd,
        regressions,
        regressed,
    }
}
