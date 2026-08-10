//! Benchmark: the R0.10 headroom experiment — Wave 1, the gate on the
//! Adaptation release (`docs/adaptation-design.md`, "The headroom
//! experiment").
//!
//! Question, pre-registered: **per decision family, can any policy beat the
//! `static-v0` floor net of the telemetry overhead learning imposes?** The
//! experiment measures headroom, not a learner: if even a clairvoyant oracle
//! — deciding with full knowledge of the recorded outcome — cannot beat the
//! floor by more than the instrumentation costs, the family is closed
//! regardless of learner quality, and the design's negative branch applies.
//!
//! **Workload classes.** The engine-bound class is the existing
//! `checkpoint_placement` bench family (the R0.5 row, re-run alongside this
//! bench for the same-day table). This bench adds the other two:
//!
//! - **Durable-work** — a queue of tasks with declared [`Effect`] classes
//!   drained by worker(s) whose attempts fail on a scripted fault schedule:
//!   transient errors, rate limits (with `Retry-After` floors), timeouts,
//!   dependency failures, resource exhaustion, and invalid input in the
//!   declared proportions below. The retry decisions are made by the real
//!   [`classify_retry`] / [`backoff_delay_ms`] / [`retry_legal_actions`] —
//!   the same functions the server scheduler and the worker SDK share — so
//!   the floor arm *is* `static-v0`, not a restatement of it.
//! - **LLM-bound scripted** — recorded-run fixtures (fixed seed, committed
//!   generator — the artifact is the generator plus its seeds, the same
//!   discipline as `checkpoint_placement`'s analytic schedules) replayed
//!   exactly with decisions varied. Model calls carry realistic latencies
//!   and per-attempt USD costs; the control class for the R0.5 predictions.
//!
//! **Arms, per family** (`docs/adaptation-design.md`, "Measurement
//! protocol"): the floor (the exact constants of
//! [`ExecutorPolicy::static_v0`]: 1 s base / 300 s cap full-jitter backoff, 3
//! attempts, uncapped timeout and concurrency), a **clairvoyant oracle**
//! that decides knowing the world tape (the family's achievable ceiling over
//! its feature space), and one **cheap feature-based heuristic** (a
//! per-class backoff table; a per-callee rolling p99-plus-margin timeout;
//! quarantine-after-`ResourceExhausted` placement; AIMD concurrency).
//!
//! **Metrics.** Cost (USD where priced, attempt-count where not), p50/p95
//! simulated latency, completion and dead-letter rates, and **telemetry
//! overhead**: the wall-time and journal-bytes cost of emitting
//! `DecisionEvent`s with features and propensities, measured with emission
//! on versus off and charged per run, because that is how a user pays it.
//!
//! **The pre-registered bar.** Headroom exists for a family when, on at
//! least one workload class, the clairvoyant arm beats `static-v0` on cost
//! or latency by a margin exceeding the family's measured per-run telemetry
//! overhead, at non-inferior completion. Kill condition, per family: the
//! oracle's margin over the floor does not exceed the overhead on any class
//! — the family closes, and the design's negative branch (twin machinery
//! plus published evidence, no promoted learner) is the outcome. The
//! heuristic arm is informative, not gated: a family is opened by the
//! oracle, and the heuristic row shows how much of the gap a cheap policy
//! already harvests.
//!
//! **What is analytic and what is timed.** Simulated world latencies,
//! attempt counts, costs, and completion are deterministic functions of the
//! seeded tapes, so they come from an untimed, asserted accounting pass
//! (`HEADROOM-ACCOUNT` / `HEADROOM-VERDICT` lines on stdout) — the R0.5
//! discipline of an asserted accounting pass applied per family. The
//! telemetry overhead is real wall time: Criterion times the emission path
//! (decision-event construction, feature assembly, journaling into a real
//! [`Journal`] with its hash chain), and the accounting pass re-measures it
//! untimed so each verdict row is computed and printed self-contained.
//!
//! World constraints that are NOT policy choices (they bound every arm
//! equally): a hung attempt with no timeout in force surfaces at the queue's
//! lease/visibility boundary ([`LEASE_BOUND_MS`], deliberately equal to the
//! floor's own backoff cap — the stuck worker's lease expires and the
//! failure classifies `Unknown`); a callee-supplied `Retry-After` floors any
//! arm's delay; the timeout ladder has a minimum rung below which everything
//! aborts early, a correctness hazard no arm may cross.

use std::collections::{BinaryHeap, VecDeque};
use std::time::Instant;

use chrono::Utc;
use criterion::measurement::WallTime;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkGroup, Criterion};
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rusty_agent_runtime::durable::{
    backoff_delay_ms, classify_retry, retry_decision_event, retry_legal_actions, ErrorClass,
    RetryDecision,
};
use rusty_agent_runtime::journal::{Clock, EventDraft, Journal};
use rusty_agent_runtime::record::{
    DecisionAction, DecisionEvent, DecisionFamily, Effect, ExecutorPolicy, PolicyVersion,
    RunEventKind,
};
use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Pre-registered world constants.
//
// Everything in this section is declared before the bench runs: workload
// sizes, fault proportions, and the world constraints. The fault proportions
// are the experimental design — the verdicts fall out of the simulation and
// are published as measured, per the design's negative-branch commitment.
// ---------------------------------------------------------------------------

/// A hung attempt with no timeout in force surfaces here: the lease /
/// visibility boundary at which the queue reclaims the stuck worker's task
/// and classifies the failure `Unknown`. Equal to the floor's backoff cap
/// (`durable::MAX_RETRY_DELAY_MS`, 300 s) — the queue's own worst-case
/// discovery latency for a worker that never answers.
const LEASE_BOUND_MS: u64 = 300_000;

/// The timeout ladder's minimum rung. Below this, ordinary work aborts
/// early — a correctness hazard the design explicitly forbids crossing
/// ("a floor below which everything aborts early").
const MIN_TIMEOUT_RUNG_MS: u64 = 100;

/// The heuristic timeout's bound before it has observed enough completions
/// to estimate a tail. Generous on purpose: a cheap heuristic earns trust by
/// not aborting work it does not yet understand.
const HEURISTIC_WARMUP_RUNG_MS: u64 = 30_000;

/// Completions per callee the heuristic timeout keeps for its percentile
/// estimate, and the minimum sample count before it trusts its own p99.
const HEURISTIC_WINDOW: usize = 256;
const HEURISTIC_MIN_SAMPLES: usize = 8;

/// The heuristic timeout's safety margin over the observed p99.
const HEURISTIC_TIMEOUT_MARGIN_NUM: u64 = 5;
const HEURISTIC_TIMEOUT_MARGIN_DEN: u64 = 4;

/// Tasks per durable-work configuration, and the share declared
/// `NonIdempotent` — work the effect gate never retries, for any arm.
const DURABLE_TASKS: usize = 400;
const DURABLE_NON_IDEMPOTENT_P: f64 = 0.10;

/// Attempts the world tape carries per work item. Every arm's attempt budget
/// is the floor's own (3); the tape is longer so no arm can outrun the
/// world's answers.
const TAPE_LEN: usize = 6;

/// Fixed seeds — the committed artifacts. Re-running the bench reproduces
/// every tape byte-for-byte.
const DURABLE_SEED: u64 = 0x0010_d0a1_0001;
const PLACEMENT_SEED: u64 = 0x0010_d0a1_0002;
const CONCURRENCY_SEED: u64 = 0x0010_d0a1_0003;
const LLM_SEED: u64 = 0x0010_d0a1_0004;

// ---------------------------------------------------------------------------
// Deterministic randomness.
//
// One ChaCha8 stream per tape, the same primitive the runtime's
// `RngSource::Seeded` determinism seam uses. World tapes and policy-side
// draws (the floor's backoff jitter) come from separate streams so a
// policy's draws can never perturb the world it faces.
// ---------------------------------------------------------------------------

struct SimRng(ChaCha8Rng);

impl SimRng {
    fn seeded(seed: u64) -> Self {
        SimRng(ChaCha8Rng::seed_from_u64(seed))
    }

    /// A uniform draw from [0, 1) with 53 bits of mantissa.
    fn uniform(&mut self) -> f64 {
        (self.0.next_u64() >> 11) as f64 / 9_007_199_254_740_992.0
    }

    /// Approximately standard-normal draw: Irwin–Hall with four uniforms,
    /// rescaled to unit variance. Cheap and good enough for log-normal-ish
    /// latency tails; the benches price decisions, not distributional
    /// fidelity.
    fn normal(&mut self) -> f64 {
        let sum: f64 = (0..4).map(|_| self.uniform()).sum();
        (sum - 2.0) * 3.0_f64.sqrt()
    }
}

/// Nearest-rank percentile of an unsorted sample set.
fn percentile(samples: &[u64], p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.max(1).min(sorted.len()) - 1]
}

// ---------------------------------------------------------------------------
// The world model: what one attempt of one work item *will* do.
//
// The tape is policy-independent by construction — the world answers
// attempts; the policy decides whether and when to attempt and with what
// timeout bound. A timeout decision changes what the policy *observes*
// (an attempt slower than its bound is observed as `Timeout`), never what
// the world would have done.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptKind {
    /// The attempt completes at `latency_ms`.
    Success,
    /// The attempt fails fast at `latency_ms` with the declared class; a
    /// rate limit carries the callee's `Retry-After` floor.
    Fail {
        class: ErrorClass,
        retry_after_ms: Option<u64>,
    },
    /// The attempt never completes on its own. Without a bound the policy
    /// pays [`LEASE_BOUND_MS`]; with a bound it pays the bound and observes
    /// `ErrorClass::Timeout`.
    Hang,
}

#[derive(Debug, Clone, Copy)]
struct AttemptTruth {
    kind: AttemptKind,
    /// Success or fail-fast latency. Unused for `Hang` — a hang's whole
    /// point is that there is no latency to know.
    latency_ms: u64,
    /// What one attempt costs regardless of outcome, for priced (LLM-bound)
    /// work. Zero for the durable-work class, whose cost proxy is the
    /// attempt count itself.
    cost_usd: f64,
}

#[derive(Debug, Clone)]
struct WorkItem {
    /// The declared effect — the retry gate's input. `NonIdempotent` items
    /// fail permanently on their first fault for every arm.
    effect: Effect,
    /// Index into the workload's callee profile table (the feature the
    /// heuristic arms key on).
    callee: usize,
    tape: Vec<AttemptTruth>,
}

/// A callee's declared latency profile and fault proportions. The
/// proportions are the scripted fault schedule the design requires —
/// declared before the bench runs, drawn from the fixed seeds.
#[derive(Debug, Clone, Copy)]
struct CalleeProfile {
    name: &'static str,
    p50_ms: u64,
    /// Log-sigma of the success-latency distribution: the tail the timeout
    /// family exists to bound.
    sigma: f64,
    p_transient: f64,
    p_rate_limited: f64,
    retry_after_ms: u64,
    p_hang: f64,
    p_dependency: f64,
    p_resource: f64,
    /// Permanent-invalid share: an input the callee rejects on every
    /// attempt, so the whole tape is `InvalidInput` — the same bytes fail
    /// the same way, and no policy may route around that gate.
    p_invalid: f64,
    /// What a failed attempt costs in wall time before its class is known.
    fail_latency_ms: u64,
}

impl CalleeProfile {
    fn success_latency(&self, rng: &mut SimRng) -> u64 {
        let scaled = self.p50_ms as f64 * (self.sigma * rng.normal()).exp();
        scaled.round().max(1.0) as u64
    }

    /// One attempt's truth, drawn from the declared proportions. `with_hangs`
    /// splits the two timeout-family rows from the retry-family rows: the
    /// retry rows run on tapes without hangs so the retry decision is the
    /// only thing priced, and the timeout rows do the symmetric thing.
    fn draw(&self, rng: &mut SimRng, with_hangs: bool, cost_usd: f64) -> AttemptTruth {
        let u = rng.uniform();
        let hang_p = if with_hangs { self.p_hang } else { 0.0 };
        let mut cursor = self.p_transient;
        if u < cursor {
            return AttemptTruth {
                kind: AttemptKind::Fail {
                    class: ErrorClass::Transient,
                    retry_after_ms: None,
                },
                latency_ms: self.fail_latency_ms,
                cost_usd,
            };
        }
        cursor += self.p_rate_limited;
        if u < cursor {
            return AttemptTruth {
                kind: AttemptKind::Fail {
                    class: ErrorClass::RateLimited,
                    retry_after_ms: Some(self.retry_after_ms),
                },
                latency_ms: self.fail_latency_ms,
                cost_usd,
            };
        }
        cursor += hang_p;
        if u < cursor {
            return AttemptTruth {
                kind: AttemptKind::Hang,
                latency_ms: 0,
                cost_usd,
            };
        }
        cursor += self.p_dependency;
        if u < cursor {
            return AttemptTruth {
                kind: AttemptKind::Fail {
                    class: ErrorClass::DependencyFailure,
                    retry_after_ms: None,
                },
                latency_ms: self.fail_latency_ms,
                cost_usd,
            };
        }
        cursor += self.p_resource;
        if u < cursor {
            return AttemptTruth {
                kind: AttemptKind::Fail {
                    class: ErrorClass::ResourceExhausted,
                    retry_after_ms: None,
                },
                latency_ms: self.fail_latency_ms,
                cost_usd,
            };
        }
        let latency_ms = self.success_latency(rng);
        AttemptTruth {
            kind: AttemptKind::Success,
            latency_ms,
            cost_usd,
        }
    }

    /// The full tape for one work item. Invalid input is decided once, up
    /// front, and poisons the whole tape: it is the one fault class whose
    /// semantics are "permanent by definition".
    fn draw_tape(
        &self,
        rng: &mut SimRng,
        with_hangs: bool,
        cost: &dyn Fn(&mut SimRng, u64) -> f64,
    ) -> Vec<AttemptTruth> {
        if rng.uniform() < self.p_invalid {
            return (0..TAPE_LEN)
                .map(|_| AttemptTruth {
                    kind: AttemptKind::Fail {
                        class: ErrorClass::InvalidInput,
                        retry_after_ms: None,
                    },
                    latency_ms: self.fail_latency_ms,
                    cost_usd: cost(rng, self.fail_latency_ms),
                })
                .collect();
        }
        (0..TAPE_LEN)
            .map(|_| {
                let attempt = self.draw(rng, with_hangs, 0.0);
                let cost_usd = cost(rng, attempt.latency_ms);
                AttemptTruth {
                    cost_usd,
                    ..attempt
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// The arms.
//
// Retry and timeout vary independently so each family is priced in
// isolation: the retry rows hold timeout at the floor (no bound — harmless
// there because the retry tapes contain no hangs), the timeout rows hold
// retry at the floor.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryArm {
    /// `static-v0`: [`classify_retry`] with the floor's exact constants —
    /// full-jitter exponential backoff, three attempts.
    Floor,
    /// Clairvoyant: retries only when some attempt left in budget succeeds,
    /// and never pays backoff — only the world's own `Retry-After` floor.
    Oracle,
    /// The per-class backoff table.
    Heuristic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeoutArm {
    /// `static-v0`: no bound in force (`TimeoutPolicyParameters` pins both
    /// fields `None`). A hang costs [`LEASE_BOUND_MS`].
    Floor,
    /// Clairvoyant: bounds a hang at the minimum rung, bounds a completing
    /// attempt exactly at its true latency — never premature, never late.
    Oracle,
    /// Rolling per-callee p99 plus margin.
    Heuristic,
}

impl RetryArm {
    fn name(self) -> &'static str {
        match self {
            RetryArm::Floor => "floor",
            RetryArm::Oracle => "oracle",
            RetryArm::Heuristic => "heuristic",
        }
    }
}

impl TimeoutArm {
    fn name(self) -> &'static str {
        match self {
            TimeoutArm::Floor => "floor",
            TimeoutArm::Oracle => "oracle",
            TimeoutArm::Heuristic => "heuristic",
        }
    }
}

/// The heuristic retry table: a fixed delay per failure class, the kind of
/// policy a per-class lookup distills to. `RateLimited` adds its margin on
/// top of the callee's `Retry-After`, the one delay the world imposes.
fn heuristic_backoff_ms(class: ErrorClass, retry_after_ms: Option<u64>) -> u64 {
    let table = match class {
        ErrorClass::Transient => 150,
        ErrorClass::RateLimited => 1_000,
        ErrorClass::Timeout => 2_000,
        ErrorClass::DependencyFailure => 1_000,
        ErrorClass::ResourceExhausted => 5_000,
        ErrorClass::Unknown => 10_000,
        // Both are gated non-retryable classes; the table is never consulted
        // for them. Zero documents that rather than inventing a delay.
        ErrorClass::InvalidInput | ErrorClass::Cancelled => 0,
    };
    table + retry_after_ms.unwrap_or(0)
}

/// The heuristic timeout's feature state: the rolling per-callee completion
/// latencies the design's timeout family names as its features ("per-tool
/// latency distributions ... journaled as the feature snapshot at decision
/// time"). Only *completing* attempts feed the estimate — a premature abort
/// is not evidence about how long success takes.
struct HeuristicTimeout {
    samples: Vec<VecDeque<u64>>,
}

impl HeuristicTimeout {
    fn new(callees: usize) -> Self {
        Self {
            samples: (0..callees).map(|_| VecDeque::new()).collect(),
        }
    }

    fn observe(&mut self, callee: usize, latency_ms: u64) {
        let window = &mut self.samples[callee];
        window.push_back(latency_ms);
        while window.len() > HEURISTIC_WINDOW {
            window.pop_front();
        }
    }

    fn bound(&self, callee: usize) -> u64 {
        let window = &self.samples[callee];
        if window.len() < HEURISTIC_MIN_SAMPLES {
            return HEURISTIC_WARMUP_RUNG_MS;
        }
        let samples: Vec<u64> = window.iter().copied().collect();
        let p99 = percentile(&samples, 99.0);
        (p99 * HEURISTIC_TIMEOUT_MARGIN_NUM / HEURISTIC_TIMEOUT_MARGIN_DEN)
            .clamp(MIN_TIMEOUT_RUNG_MS, LEASE_BOUND_MS)
    }
}

/// What one simulated work item cost one arm.
#[derive(Debug, Clone, Copy, Default)]
struct ItemOutcome {
    completed: bool,
    /// Budget exhausted on a retryable failure — the DLQ case, the
    /// completion-side penalty the retry family's reward names.
    dead_lettered: bool,
    attempts: u64,
    /// Wall time the item's owner experiences: attempt latencies plus every
    /// delay the policy chose (backoff) or was forced into (hang waits,
    /// premature aborts).
    elapsed_ms: u64,
    /// DecisionEvent emissions the run pays for: one retry decision per
    /// failed attempt, one timeout decision per attempt when the timeout
    /// family is in force.
    decisions: u64,
    cost_usd: f64,
}

/// Run one work item to its terminal state under one (retry, timeout) arm
/// pair. Shared by the durable-work and LLM-bound classes — the classes
/// differ in tapes and cost, not in decision mechanics.
///
/// The gates never move, for any arm: the effect gate, the class gate, and
/// the attempt budget are [`retry_legal_actions`]'s, exactly as the
/// scheduler applies them. What varies between arms is only the delay and
/// the bound — the learnable numbers.
fn simulate_item(
    item: &WorkItem,
    retry_arm: RetryArm,
    timeout_arm: TimeoutArm,
    heuristic: &mut HeuristicTimeout,
    jitter: &mut SimRng,
) -> ItemOutcome {
    let floor = ExecutorPolicy::static_v0();
    let max_attempts = floor.retry.max_attempts;
    let mut outcome = ItemOutcome::default();
    let mut attempt: u32 = 1;

    loop {
        let truth = item.tape[(attempt - 1) as usize];
        outcome.attempts += 1;
        outcome.cost_usd += truth.cost_usd;

        // The timeout decision. Every arm faces the same truth; the arms
        // differ in what they let the truth cost them.
        let bound = match timeout_arm {
            TimeoutArm::Floor => None,
            TimeoutArm::Oracle => Some(match truth.kind {
                AttemptKind::Hang => MIN_TIMEOUT_RUNG_MS,
                _ => truth.latency_ms,
            }),
            TimeoutArm::Heuristic => Some(heuristic.bound(item.callee)),
        };
        if timeout_arm != TimeoutArm::Floor {
            outcome.decisions += 1;
        }

        // Observe the attempt under the chosen bound.
        let (class, retry_after_ms) = match truth.kind {
            AttemptKind::Success => match bound {
                Some(b) if truth.latency_ms > b => {
                    // Premature abort: the attempt would have completed and
                    // the policy killed it. A wasted attempt — the timeout
                    // family's stated cost of a wrong bound.
                    outcome.elapsed_ms += b;
                    (ErrorClass::Timeout, None)
                }
                _ => {
                    outcome.elapsed_ms += truth.latency_ms;
                    if timeout_arm == TimeoutArm::Heuristic {
                        heuristic.observe(item.callee, truth.latency_ms);
                    }
                    outcome.completed = true;
                    return outcome;
                }
            },
            AttemptKind::Fail {
                class,
                retry_after_ms,
            } => {
                outcome.elapsed_ms += truth.latency_ms;
                (class, retry_after_ms)
            }
            AttemptKind::Hang => match bound {
                None => {
                    outcome.elapsed_ms += LEASE_BOUND_MS;
                    // Lease expiry is how the floor discovers a hang; the
                    // durable contract classifies a worker that died
                    // mid-attempt as `Unknown`.
                    (ErrorClass::Unknown, None)
                }
                Some(b) => {
                    outcome.elapsed_ms += b;
                    (ErrorClass::Timeout, None)
                }
            },
        };

        // The retry decision — the family's wired emission point since R0.8,
        // charged to every arm (the floor journals it too).
        outcome.decisions += 1;
        let legal = retry_legal_actions(item.effect, class, attempt, max_attempts);
        let retry_is_legal = legal
            .iter()
            .any(|a| matches!(a, DecisionAction::Retry { .. }));
        if !retry_is_legal {
            // Effect gate, class gate, or budget exhausted. Budget
            // exhaustion on a retryable class is the dead-letter case; the
            // gates are an immediate fail.
            outcome.dead_lettered = item.effect.is_freely_repeatable() && class.is_retryable();
            return outcome;
        }

        let delay_ms = match retry_arm {
            RetryArm::Floor => {
                match classify_retry(item.effect, class, attempt, max_attempts, jitter.uniform()) {
                    RetryDecision::Retry { after_ms } => after_ms.max(retry_after_ms.unwrap_or(0)),
                    // The legal set was computed from the same gates, so the
                    // classifier cannot disagree with it.
                    other => unreachable!("legal set admits retry, classifier gave {other:?}"),
                }
            }
            RetryArm::Oracle => {
                // Clairvoyance: retry — immediately, honoring only the
                // world's Retry-After — exactly when a remaining in-budget
                // attempt completes. Otherwise abort now and save the
                // doomed attempts the floor would pay for.
                let worth_retrying = ((attempt + 1)..=max_attempts)
                    .any(|k| matches!(item.tape[(k - 1) as usize].kind, AttemptKind::Success));
                if !worth_retrying {
                    return outcome;
                }
                retry_after_ms.unwrap_or(0)
            }
            RetryArm::Heuristic => heuristic_backoff_ms(class, retry_after_ms),
        };

        outcome.elapsed_ms += delay_ms;
        attempt += 1;
    }
}

// ---------------------------------------------------------------------------
// Aggregate statistics for one arm over one workload.
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ArmStats {
    items: u64,
    completed: u64,
    dead_lettered: u64,
    attempts: u64,
    /// Attempts that did not complete their item — every attempt of a
    /// failed item, and all but the last of a completed one. The
    /// attempt-count cost proxy the design names for unpriced work, and the
    /// USD multiplier for priced work.
    wasted_attempts: u64,
    decisions: u64,
    cost_usd: f64,
    latencies: Vec<u64>,
}

impl ArmStats {
    fn record(&mut self, outcome: &ItemOutcome) {
        self.items += 1;
        self.completed += u64::from(outcome.completed);
        self.dead_lettered += u64::from(outcome.dead_lettered);
        self.attempts += outcome.attempts;
        self.wasted_attempts += outcome.attempts - u64::from(outcome.completed);
        self.decisions += outcome.decisions;
        self.cost_usd += outcome.cost_usd;
        self.latencies.push(outcome.elapsed_ms);
    }

    fn completion_rate(&self) -> f64 {
        self.completed as f64 / self.items.max(1) as f64
    }

    fn p50_ms(&self) -> u64 {
        percentile(&self.latencies, 50.0)
    }

    fn p95_ms(&self) -> u64 {
        percentile(&self.latencies, 95.0)
    }

    fn mean_ms(&self) -> f64 {
        self.latencies.iter().sum::<u64>() as f64 / self.latencies.len().max(1) as f64
    }
}

// ---------------------------------------------------------------------------
// Workload class: durable-work.
//
// Four callee profiles spanning the shapes durable work actually takes:
// a fast payment-style write path, a search read, a notification send, and
// a heavy report builder with a fat tail and real hang risk. Fault
// proportions are declared here — the scripted fault schedule.
// ---------------------------------------------------------------------------

const DURABLE_CALLEES: [CalleeProfile; 4] = [
    CalleeProfile {
        name: "payments",
        p50_ms: 120,
        sigma: 0.35,
        p_transient: 0.06,
        p_rate_limited: 0.03,
        retry_after_ms: 2_000,
        p_hang: 0.02,
        p_dependency: 0.02,
        p_resource: 0.01,
        p_invalid: 0.005,
        fail_latency_ms: 25,
    },
    CalleeProfile {
        name: "search",
        p50_ms: 300,
        sigma: 0.45,
        p_transient: 0.10,
        p_rate_limited: 0.02,
        retry_after_ms: 1_000,
        p_hang: 0.03,
        p_dependency: 0.03,
        p_resource: 0.01,
        p_invalid: 0.005,
        fail_latency_ms: 40,
    },
    CalleeProfile {
        name: "notify",
        p50_ms: 80,
        sigma: 0.30,
        p_transient: 0.04,
        p_rate_limited: 0.0,
        retry_after_ms: 0,
        p_hang: 0.01,
        p_dependency: 0.06,
        p_resource: 0.0,
        p_invalid: 0.005,
        fail_latency_ms: 20,
    },
    CalleeProfile {
        name: "report",
        p50_ms: 2_500,
        sigma: 0.60,
        p_transient: 0.05,
        p_rate_limited: 0.0,
        retry_after_ms: 0,
        p_hang: 0.08,
        p_dependency: 0.02,
        p_resource: 0.04,
        p_invalid: 0.005,
        fail_latency_ms: 60,
    },
];

/// The durable-work workload: `DURABLE_TASKS` items spread evenly across
/// the callee table, `DURABLE_NON_IDEMPOTENT_P` declared non-retryable.
/// `with_hangs` selects the timeout-family tape variant.
fn durable_workload(seed: u64, with_hangs: bool) -> Vec<WorkItem> {
    let mut rng = SimRng::seeded(seed);
    let no_cost = |_: &mut SimRng, _: u64| 0.0;
    (0..DURABLE_TASKS)
        .map(|i| {
            let callee = i % DURABLE_CALLEES.len();
            let effect = if rng.uniform() < DURABLE_NON_IDEMPOTENT_P {
                Effect::NonIdempotent
            } else {
                Effect::Idempotent
            };
            WorkItem {
                effect,
                callee,
                tape: DURABLE_CALLEES[callee].draw_tape(&mut rng, with_hangs, &no_cost),
            }
        })
        .collect()
}

/// The family under test. Retry and timeout are priced in isolation: the
/// family's own arm varies while the other dimension is pinned at the floor
/// for all three arms, so a row's differences belong to one family only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Retry,
    Timeout,
}

impl Family {
    fn name(self) -> &'static str {
        match self {
            Family::Retry => "retry",
            Family::Timeout => "timeout",
        }
    }

    /// The (retry, timeout) arm pairings for this family's floor / oracle /
    /// heuristic triple.
    fn arm_triple(self) -> [(RetryArm, TimeoutArm); 3] {
        match self {
            Family::Retry => [
                (RetryArm::Floor, TimeoutArm::Floor),
                (RetryArm::Oracle, TimeoutArm::Floor),
                (RetryArm::Heuristic, TimeoutArm::Floor),
            ],
            Family::Timeout => [
                (RetryArm::Floor, TimeoutArm::Floor),
                (RetryArm::Floor, TimeoutArm::Oracle),
                (RetryArm::Floor, TimeoutArm::Heuristic),
            ],
        }
    }
}

/// Run one family's arm triple over one workload. Each arm gets its own
/// jitter stream (only the floor consumes it) and its own heuristic state,
/// but every arm faces byte-identical tapes.
fn run_arms(workload: &[WorkItem], callees: usize, family: Family, seed: u64) -> Vec<ArmStats> {
    family
        .arm_triple()
        .iter()
        .enumerate()
        .map(|(i, &(r, t))| {
            let mut stats = ArmStats::default();
            let mut heuristic = HeuristicTimeout::new(callees);
            let mut jitter = SimRng::seeded(seed ^ ((i as u64 + 1) * 0x9e37_79b9));
            for item in workload {
                let outcome = simulate_item(item, r, t, &mut heuristic, &mut jitter);
                stats.record(&outcome);
            }
            stats
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Workload class: LLM-bound scripted.
//
// A recorded run is an agent loop: `LLM_STEPS` steps, one model call each,
// plus a tool call on `LLM_TOOL_P` of steps. Model calls are declared
// `ReadOnly` (they write nothing; re-execution is safe) and tool calls are
// 90 % `Idempotent` — the declarations that make retry legal at all. Every
// attempt is priced: a failed model call costs its tokens the same as a
// successful one, which is what makes wasted attempts a USD metric here
// instead of an attempt-count proxy.
// ---------------------------------------------------------------------------

const LLM_RUNS: usize = 40;
const LLM_STEPS: usize = 24;
const LLM_TOOL_P: f64 = 0.6;

const LLM_MODEL_PROFILE: CalleeProfile = CalleeProfile {
    name: "model",
    p50_ms: 2_200,
    sigma: 0.45,
    p_transient: 0.04,
    p_rate_limited: 0.05,
    retry_after_ms: 5_000,
    p_hang: 0.025,
    p_dependency: 0.01,
    p_resource: 0.0,
    p_invalid: 0.001,
    fail_latency_ms: 400,
};

const LLM_TOOL_PROFILE: CalleeProfile = CalleeProfile {
    name: "tool",
    p50_ms: 350,
    sigma: 0.30,
    p_transient: 0.03,
    p_rate_limited: 0.01,
    retry_after_ms: 1_000,
    p_hang: 0.01,
    p_dependency: 0.02,
    p_resource: 0.0,
    p_invalid: 0.002,
    fail_latency_ms: 50,
};

/// One recorded run: the ordered call sequence its journal would carry.
struct LlmRun {
    calls: Vec<WorkItem>,
}

/// The scripted fixture set. The generator plus `LLM_SEED` is the committed
/// artifact: replay reproduces these runs exactly, and the arms vary only
/// decisions — never the recorded world.
fn llm_fixtures(seed: u64, with_hangs: bool) -> Vec<LlmRun> {
    let mut rng = SimRng::seeded(seed);
    // Model-attempt pricing: a base charge scaled by the attempt's latency
    // (longer generations bill more tokens) and a per-call size factor.
    let model_cost = |r: &mut SimRng, latency_ms: u64| {
        0.003 * (latency_ms.max(1) as f64 / 2_200.0) * (0.6 + 1.2 * r.uniform())
    };
    let tool_cost = |_: &mut SimRng, _: u64| 0.0002;
    (0..LLM_RUNS)
        .map(|_| {
            let mut calls = Vec::new();
            for _ in 0..LLM_STEPS {
                calls.push(WorkItem {
                    effect: Effect::ReadOnly,
                    callee: 0,
                    tape: LLM_MODEL_PROFILE.draw_tape(&mut rng, with_hangs, &model_cost),
                });
                if rng.uniform() < LLM_TOOL_P {
                    // The non-idempotent share is deliberately small: every
                    // fault on such a call fails its run at the effect gate,
                    // for every arm — a large share would let the gate, not
                    // the policies, dominate the completion columns.
                    let effect = if rng.uniform() < 0.9 {
                        Effect::Idempotent
                    } else {
                        Effect::NonIdempotent
                    };
                    calls.push(WorkItem {
                        effect,
                        callee: 1,
                        tape: LLM_TOOL_PROFILE.draw_tape(&mut rng, with_hangs, &tool_cost),
                    });
                }
            }
            LlmRun { calls }
        })
        .collect()
}

/// All three arms over the fixture set for one family.
///
/// Accounting is per call for cost and attempts (a run's items are its
/// calls: the wasted-attempt definition — every attempt of a failed call,
/// all but the last of a completed one — is only meaningful there), and per
/// run for completion and latency (a run completes when every call does;
/// its latency is the sequential sum).
fn run_llm_arms(fixtures: &[LlmRun], family: Family, seed: u64) -> Vec<ArmStats> {
    family
        .arm_triple()
        .iter()
        .enumerate()
        .map(|(i, &(r, t))| {
            let mut stats = ArmStats::default();
            let mut jitter = SimRng::seeded(seed ^ ((i as u64 + 7) * 0x9e37_79b9));
            for run in fixtures {
                let mut heuristic = HeuristicTimeout::new(2);
                let mut run_latency_ms = 0u64;
                let mut run_completed = true;
                let mut run_dead = false;
                for call in &run.calls {
                    let outcome = simulate_item(call, r, t, &mut heuristic, &mut jitter);
                    stats.attempts += outcome.attempts;
                    stats.wasted_attempts += outcome.attempts - u64::from(outcome.completed);
                    stats.decisions += outcome.decisions;
                    stats.cost_usd += outcome.cost_usd;
                    run_latency_ms += outcome.elapsed_ms;
                    if !outcome.completed {
                        run_completed = false;
                        run_dead = outcome.dead_lettered;
                        break;
                    }
                }
                stats.items += 1;
                stats.completed += u64::from(run_completed);
                stats.dead_lettered += u64::from(run_dead);
                stats.latencies.push(run_latency_ms);
            }
            stats
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Family: equivalent-worker placement (durable-work class).
//
// A fleet of identical workers drains one queue; each worker is serial.
// Workers degrade on a scripted schedule — windows during which any attempt
// they execute fails `ResourceExhausted`. The placement decision is the
// claim: which free worker takes the next queued task. Retry is pinned at
// the floor for every arm, so the rows differ only in where work lands.
//
// The arms: the floor claims round-robin in free order (any idle worker —
// the v1 scheduler has no placement preference); the oracle knows the
// degradation windows and idles a worker through its own window rather than
// feed it work; the heuristic quarantines a worker briefly after observing
// a `ResourceExhausted` from it — the per-worker recent-failure feature the
// family's design row names.
// ---------------------------------------------------------------------------

const PLACEMENT_WORKERS: usize = 4;
const PLACEMENT_TASKS: usize = 240;
/// (worker, window start ms, window end ms) — the scripted fault schedule.
const PLACEMENT_WINDOWS: [(usize, u64, u64); 2] = [(1, 2_000, 5_000), (3, 7_000, 8_500)];
const PLACEMENT_FAIL_LATENCY_MS: u64 = 25;
/// How long the heuristic refuses a worker after one observed exhaustion.
const PLACEMENT_QUARANTINE_MS: u64 = 30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlacementArm {
    Floor,
    Oracle,
    Heuristic,
}

impl PlacementArm {
    fn name(self) -> &'static str {
        match self {
            PlacementArm::Floor => "floor",
            PlacementArm::Oracle => "oracle",
            PlacementArm::Heuristic => "heuristic",
        }
    }
}

fn placement_degraded(worker: usize, at_ms: u64) -> bool {
    PLACEMENT_WINDOWS
        .iter()
        .any(|&(w, start, end)| w == worker && at_ms >= start && at_ms < end)
}

/// The window's end when `worker` is degraded at `at_ms` — the oracle's
/// clairvoyant idle-until. `None` when the worker is healthy.
fn placement_window_end(worker: usize, at_ms: u64) -> Option<u64> {
    PLACEMENT_WINDOWS
        .iter()
        .find(|&&(w, start, end)| w == worker && at_ms >= start && at_ms < end)
        .map(|&(_, _, end)| end)
}

/// One arm of the placement experiment: drain `PLACEMENT_TASKS` tasks over
/// the fleet, retrying per the floor. Tasks are queue-ordered; each task's
/// tape is `TAPE_LEN` success latencies drawn once, so every arm pays the
/// same work for the same task.
fn run_placement_arm(arm: PlacementArm, seed: u64) -> ArmStats {
    let mut rng = SimRng::seeded(seed);
    let profile = CalleeProfile {
        name: "placement-callee",
        p50_ms: 120,
        sigma: 0.30,
        ..DURABLE_CALLEES[1]
    };
    let tapes: Vec<Vec<u64>> = (0..PLACEMENT_TASKS)
        .map(|_| {
            (0..TAPE_LEN)
                .map(|_| profile.success_latency(&mut rng))
                .collect()
        })
        .collect();

    let floor = ExecutorPolicy::static_v0();
    let max_attempts = floor.retry.max_attempts;
    let mut jitter = SimRng::seeded(seed ^ 0x051e_ca11);

    let mut busy_until = [0u64; PLACEMENT_WORKERS];
    let mut quarantine_until = [0u64; PLACEMENT_WORKERS];
    let mut ready: VecDeque<usize> = (0..PLACEMENT_TASKS).collect();
    let mut retry_heap: BinaryHeap<std::cmp::Reverse<(u64, usize)>> = BinaryHeap::new();
    let mut attempt_of = vec![0u32; PLACEMENT_TASKS];

    let mut stats = ArmStats::default();
    let mut now = 0u64;

    loop {
        // Retries whose backoff has elapsed rejoin the queue.
        while let Some(&std::cmp::Reverse((available, task))) = retry_heap.peek() {
            if available <= now {
                retry_heap.pop();
                ready.push_back(task);
            } else {
                break;
            }
        }
        let settled = stats.completed + stats.dead_lettered;
        if settled as usize == PLACEMENT_TASKS {
            break;
        }
        if ready.is_empty() {
            // Nothing claimable: the only event that can create work is a
            // retry coming due (a free worker with an empty queue creates
            // nothing). If no retry is pending the remaining tasks are all
            // in a terminal state.
            match retry_heap.peek().map(|r| (r.0).0) {
                Some(t) => now = now.max(t),
                None => break,
            }
            continue;
        }

        // The claim: the earliest-free worker is the candidate, and the
        // placement policy decides whether it may take the work.
        let worker = (0..PLACEMENT_WORKERS)
            .min_by_key(|&w| busy_until[w])
            .expect("fleet is non-empty");
        now = now.max(busy_until[worker]);

        // Re-release anything that came due while time advanced, so the
        // claim always sees the true queue.
        while let Some(&std::cmp::Reverse((available, task))) = retry_heap.peek() {
            if available <= now {
                retry_heap.pop();
                ready.push_back(task);
            } else {
                break;
            }
        }
        if ready.is_empty() {
            continue;
        }

        let veto_until = match arm {
            PlacementArm::Floor => None,
            PlacementArm::Oracle => placement_window_end(worker, now),
            PlacementArm::Heuristic => {
                (quarantine_until[worker] > now).then_some(quarantine_until[worker])
            }
        };
        if let Some(until) = veto_until {
            // A vetoed worker idles; its busy-until moves to the moment the
            // policy trusts it again.
            busy_until[worker] = until.max(busy_until[worker]);
            continue;
        }

        let task = ready.pop_front().expect("ready is non-empty");
        attempt_of[task] += 1;
        stats.attempts += 1;
        stats.decisions += 1; // the placement decision itself

        if placement_degraded(worker, now) {
            // The work lands on a sick worker: a wasted attempt, a floor
            // retry decision, and — for the heuristic — the observation
            // that teaches it to avoid this worker.
            stats.decisions += 1;
            stats.wasted_attempts += 1;
            busy_until[worker] = now + PLACEMENT_FAIL_LATENCY_MS;
            if arm == PlacementArm::Heuristic {
                quarantine_until[worker] =
                    now + PLACEMENT_FAIL_LATENCY_MS + PLACEMENT_QUARANTINE_MS;
            }
            let attempt = attempt_of[task];
            match classify_retry(
                Effect::Idempotent,
                ErrorClass::ResourceExhausted,
                attempt,
                max_attempts,
                jitter.uniform(),
            ) {
                RetryDecision::Retry { after_ms } => retry_heap.push(std::cmp::Reverse((
                    now + PLACEMENT_FAIL_LATENCY_MS + after_ms,
                    task,
                ))),
                RetryDecision::Dead | RetryDecision::Fail => {
                    stats.dead_lettered += 1;
                    stats.latencies.push(now + PLACEMENT_FAIL_LATENCY_MS);
                }
            }
        } else {
            let latency = tapes[task][(attempt_of[task] - 1) as usize];
            busy_until[worker] = now + latency;
            stats.completed += 1;
            stats.latencies.push(now + latency);
        }
    }

    stats.items = PLACEMENT_TASKS as u64;
    stats
}

// ---------------------------------------------------------------------------
// Family: concurrency/backpressure (durable-work class).
//
// One shared callee with a hard concurrency ceiling: an attempt admitted
// while `K` or more attempts are already in flight is rejected on the spot
// (`RateLimited`, ten milliseconds, one-second Retry-After). The policy's
// decision is the admission cap. The floor is uncapped — `static-v0`'s
// `max_parallel: None` — so its whole queue thunders into the ceiling and
// backs off; the oracle caps exactly at the ceiling it knows; the heuristic
// is AIMD over observed rejections, the standard cheap controller and the
// family's named heuristic.
//
// Retry is pinned at the floor for every arm.
// ---------------------------------------------------------------------------

const CONCURRENCY_TASKS: usize = 120;
const CONCURRENCY_CEILING: u32 = 8;
const CONCURRENCY_REJECT_LATENCY_MS: u64 = 10;
const CONCURRENCY_RETRY_AFTER_MS: u64 = 1_000;
const AIMD_START_CAP: u32 = 16;
const AIMD_SUCCESS_STREAK_PER_INC: u32 = 32;
const AIMD_MAX_CAP: u32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConcurrencyArm {
    Floor,
    Oracle,
    Heuristic,
}

impl ConcurrencyArm {
    fn name(self) -> &'static str {
        match self {
            ConcurrencyArm::Floor => "floor",
            ConcurrencyArm::Oracle => "oracle",
            ConcurrencyArm::Heuristic => "heuristic",
        }
    }
}

fn run_concurrency_arm(arm: ConcurrencyArm, seed: u64) -> ArmStats {
    let mut rng = SimRng::seeded(seed);
    let profile = CalleeProfile {
        name: "rate-limited-callee",
        p50_ms: 200,
        sigma: 0.25,
        ..DURABLE_CALLEES[0]
    };
    let tapes: Vec<Vec<u64>> = (0..CONCURRENCY_TASKS)
        .map(|_| {
            (0..TAPE_LEN)
                .map(|_| profile.success_latency(&mut rng))
                .collect()
        })
        .collect();

    let floor = ExecutorPolicy::static_v0();
    let max_attempts = floor.retry.max_attempts;
    let mut jitter = SimRng::seeded(seed ^ 0x0c0c_4e57);

    let mut cap = match arm {
        ConcurrencyArm::Floor => u32::MAX,
        ConcurrencyArm::Oracle => CONCURRENCY_CEILING,
        ConcurrencyArm::Heuristic => AIMD_START_CAP,
    };
    let mut aimd_streak = 0u32;

    let mut ready: VecDeque<usize> = (0..CONCURRENCY_TASKS).collect();
    let mut retry_heap: BinaryHeap<std::cmp::Reverse<(u64, usize)>> = BinaryHeap::new();
    let mut end_heap: BinaryHeap<std::cmp::Reverse<(u64, usize)>> = BinaryHeap::new();
    let mut attempt_of = vec![0u32; CONCURRENCY_TASKS];
    let mut in_flight = 0u32;
    let mut now = 0u64;

    let mut stats = ArmStats::default();

    loop {
        while let Some(&std::cmp::Reverse((available, task))) = retry_heap.peek() {
            if available <= now {
                retry_heap.pop();
                ready.push_back(task);
            } else {
                break;
            }
        }

        // Admission: the cap is the policy; the ceiling is the world.
        while in_flight < cap {
            let Some(task) = ready.pop_front() else {
                break;
            };
            attempt_of[task] += 1;
            stats.attempts += 1;
            stats.decisions += 1;
            if in_flight >= CONCURRENCY_CEILING {
                stats.wasted_attempts += 1;
                if arm == ConcurrencyArm::Heuristic {
                    cap = (cap / 2).max(1);
                    aimd_streak = 0;
                }
                match classify_retry(
                    Effect::Idempotent,
                    ErrorClass::RateLimited,
                    attempt_of[task],
                    max_attempts,
                    jitter.uniform(),
                ) {
                    RetryDecision::Retry { after_ms } => retry_heap.push(std::cmp::Reverse((
                        now + CONCURRENCY_REJECT_LATENCY_MS
                            + after_ms.max(CONCURRENCY_RETRY_AFTER_MS),
                        task,
                    ))),
                    RetryDecision::Dead | RetryDecision::Fail => {
                        stats.dead_lettered += 1;
                        stats.latencies.push(now + CONCURRENCY_REJECT_LATENCY_MS);
                    }
                }
            } else {
                in_flight += 1;
                let latency = tapes[task][(attempt_of[task] - 1) as usize];
                end_heap.push(std::cmp::Reverse((now + latency, task)));
            }
        }

        if (stats.completed + stats.dead_lettered) as usize == CONCURRENCY_TASKS {
            break;
        }

        // Advance to the next completion or retry arrival.
        let next_end = end_heap.peek().map(|r| (r.0).0);
        let next_retry = retry_heap.peek().map(|r| (r.0).0);
        now = match (next_end, next_retry) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };

        while let Some(&std::cmp::Reverse((end, _task))) = end_heap.peek() {
            if end <= now {
                end_heap.pop();
                in_flight -= 1;
                stats.completed += 1;
                stats.latencies.push(end);
                if arm == ConcurrencyArm::Heuristic {
                    aimd_streak += 1;
                    if aimd_streak >= AIMD_SUCCESS_STREAK_PER_INC {
                        cap = (cap + 1).min(AIMD_MAX_CAP);
                        aimd_streak = 0;
                    }
                }
            } else {
                break;
            }
        }
    }

    stats.items = CONCURRENCY_TASKS as u64;
    stats
}

// ---------------------------------------------------------------------------
// Telemetry: what one decision costs to record.
//
// The emitters below are the measurement of "emission on versus off": each
// builds the family's DecisionEvent exactly as the production emission
// points do — the retry emitter calls the same [`retry_decision_event`] the
// scheduler uses, the others construct the same envelope with their
// family's feature snapshot — and journals it as a `PolicyDecision` event's
// output payload, the wiring R0.8 established.
// ---------------------------------------------------------------------------

fn emit_retry_decision(journal: &Journal, seq: u64, class: ErrorClass, attempt: u32) {
    let floor = ExecutorPolicy::static_v0();
    let decision = classify_retry(
        Effect::Idempotent,
        class,
        attempt,
        floor.retry.max_attempts,
        0.5,
    );
    let event = retry_decision_event(
        journal.run_id(),
        journal.thread_id(),
        seq,
        Effect::Idempotent,
        class,
        attempt,
        floor.retry.max_attempts,
        Some(840),
        &decision,
        &PolicyVersion::default(),
        Utc::now(),
    );
    journal.record(
        EventDraft::new(RunEventKind::PolicyDecision, Effect::Pure)
            .output(serde_json::to_value(&event).expect("decision event serializes")),
    );
}

/// The timeout family's feature snapshot: per-callee completion percentiles
/// plus the elapsed time, the features the family's design row names.
fn timeout_feature_snapshot(samples: &[u64], elapsed_ms: u64) -> Map<String, Value> {
    let mut features = Map::new();
    features.insert("callee".to_owned(), Value::from("report"));
    features.insert("p50_ms".to_owned(), Value::from(percentile(samples, 50.0)));
    features.insert("p95_ms".to_owned(), Value::from(percentile(samples, 95.0)));
    features.insert("p99_ms".to_owned(), Value::from(percentile(samples, 99.0)));
    features.insert("elapsed_ms".to_owned(), Value::from(elapsed_ms));
    features.insert("sample_count".to_owned(), Value::from(samples.len() as u64));
    features
}

fn emit_timeout_decision(journal: &Journal, seq: u64, samples: &[u64], bound_ms: u64) {
    let event = DecisionEvent {
        id: format!("{}:d{seq}", journal.run_id()),
        run_id: journal.run_id().to_owned(),
        thread_id: journal.thread_id().to_owned(),
        seq,
        family: DecisionFamily::Timeout,
        features: timeout_feature_snapshot(samples, 0),
        legal_actions: [100, 500, 2_000, 10_000, 30_000]
            .iter()
            .map(|&millis| DecisionAction::SetTimeout { millis })
            .collect(),
        selected: DecisionAction::SetTimeout { millis: bound_ms },
        propensity: 1.0,
        policy_version: PolicyVersion::default(),
        role: None,
        outcome: None,
        decided_at: Utc::now(),
    };
    journal.record(
        EventDraft::new(RunEventKind::PolicyDecision, Effect::Pure)
            .output(serde_json::to_value(&event).expect("decision event serializes")),
    );
}

fn emit_placement_decision(journal: &Journal, seq: u64, queue_depth: u64, worker: &str) {
    let mut features = Map::new();
    features.insert("queue_depth".to_owned(), Value::from(queue_depth));
    features.insert(
        "worker_health".to_owned(),
        serde_json::json!({"w0": "ok", "w1": "degraded", "w2": "ok", "w3": "ok"}),
    );
    features.insert("recent_resource_exhausted".to_owned(), Value::from(1));
    let event = DecisionEvent {
        id: format!("{}:d{seq}", journal.run_id()),
        run_id: journal.run_id().to_owned(),
        thread_id: journal.thread_id().to_owned(),
        seq,
        family: DecisionFamily::WorkerPlacement,
        features,
        legal_actions: ["w0", "w1", "w2", "w3"]
            .iter()
            .map(|w| DecisionAction::SelectWorker {
                worker: (*w).to_owned(),
            })
            .collect(),
        selected: DecisionAction::SelectWorker {
            worker: worker.to_owned(),
        },
        propensity: 1.0,
        policy_version: PolicyVersion::default(),
        role: None,
        outcome: None,
        decided_at: Utc::now(),
    };
    journal.record(
        EventDraft::new(RunEventKind::PolicyDecision, Effect::Pure)
            .output(serde_json::to_value(&event).expect("decision event serializes")),
    );
}

fn emit_concurrency_decision(
    journal: &Journal,
    seq: u64,
    queue_depth: u64,
    in_flight: u32,
    cap: u32,
) {
    let mut features = Map::new();
    features.insert("queue_depth".to_owned(), Value::from(queue_depth));
    features.insert("in_flight".to_owned(), Value::from(in_flight));
    features.insert("rate_limited_recent".to_owned(), Value::from(1));
    let event = DecisionEvent {
        id: format!("{}:d{seq}", journal.run_id()),
        run_id: journal.run_id().to_owned(),
        thread_id: journal.thread_id().to_owned(),
        seq,
        family: DecisionFamily::Concurrency,
        features,
        legal_actions: [1, 2, 4, 8, 16, 32]
            .iter()
            .map(|&limit| DecisionAction::SetConcurrency { limit })
            .collect(),
        selected: DecisionAction::SetConcurrency { limit: cap },
        propensity: 1.0,
        policy_version: PolicyVersion::default(),
        role: None,
        outcome: None,
        decided_at: Utc::now(),
    };
    journal.record(
        EventDraft::new(RunEventKind::PolicyDecision, Effect::Pure)
            .output(serde_json::to_value(&event).expect("decision event serializes")),
    );
}

/// Criterion-timed telemetry: the wall-time half of the overhead, measured
/// per decision on the real emission path (event construction + feature
/// assembly + journal record with its hash chain).
fn bench_telemetry(c: &mut Criterion) {
    let mut group: BenchmarkGroup<'_, WallTime> = c.benchmark_group("headroom_telemetry");
    group
        .sample_size(50)
        .warm_up_time(std::time::Duration::from_millis(300))
        .measurement_time(std::time::Duration::from_millis(1500));

    group.bench_function("emit_retry_decision", |b| {
        b.iter_batched(
            || Journal::new("bench", "bench", Clock::System),
            |journal| emit_retry_decision(&journal, 0, ErrorClass::RateLimited, 1),
            BatchSize::PerIteration,
        );
    });

    let mut rng = SimRng::seeded(0x7e1e_1e17);
    let samples: Vec<u64> = (0..HEURISTIC_WINDOW)
        .map(|_| DURABLE_CALLEES[3].success_latency(&mut rng))
        .collect();
    group.bench_function("emit_timeout_decision", |b| {
        b.iter_batched(
            || Journal::new("bench", "bench", Clock::System),
            |journal| emit_timeout_decision(&journal, 0, &samples, 12_500),
            BatchSize::PerIteration,
        );
    });

    group.bench_function("timeout_feature_snapshot", |b| {
        b.iter(|| criterion::black_box(timeout_feature_snapshot(&samples, 3_000)));
    });

    group.bench_function("emit_placement_decision", |b| {
        b.iter_batched(
            || Journal::new("bench", "bench", Clock::System),
            |journal| emit_placement_decision(&journal, 0, 17, "w2"),
            BatchSize::PerIteration,
        );
    });

    group.bench_function("emit_concurrency_decision", |b| {
        b.iter_batched(
            || Journal::new("bench", "bench", Clock::System),
            |journal| emit_concurrency_decision(&journal, 0, 42, 7, 8),
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// The accounting pass: deterministic family metrics, the overhead
// re-measurement, and the verdict per family against the pre-registered
// bar. Untimed, asserted, printed with HEADROOM-* prefixes — the R0.5
// discipline applied per family.
// ---------------------------------------------------------------------------

/// Untimed re-measurement of one emitter's per-decision wall cost, so each
/// verdict row is computed self-contained (Criterion confirms with CIs).
fn measure_ns_per_decision(emit: impl Fn(&Journal, u64), iterations: u64) -> f64 {
    let journal = Journal::new("overhead", "overhead", Clock::System);
    // Warm the path: the first records grow the journal's storage.
    for seq in 0..1_000 {
        emit(&journal, seq);
    }
    let started = Instant::now();
    for seq in 1_000..1_000 + iterations {
        emit(&journal, seq);
    }
    started.elapsed().as_nanos() as f64 / iterations as f64
}

/// The journal-bytes half of the overhead: the snapshot-size delta of a
/// journal carrying `iterations` decision events over an empty one.
fn measure_bytes_per_decision(emit: impl Fn(&Journal, u64), iterations: u64) -> f64 {
    let empty = Journal::new("overhead", "overhead", Clock::System);
    let baseline = serde_json::to_vec(&empty.snapshot())
        .expect("snapshot serializes")
        .len();
    let journal = Journal::new("overhead", "overhead", Clock::System);
    for seq in 0..iterations {
        emit(&journal, seq);
    }
    let with_decisions = serde_json::to_vec(&journal.snapshot())
        .expect("snapshot serializes")
        .len();
    (with_decisions - baseline) as f64 / iterations as f64
}

fn print_account(family: &str, class: &str, arm: &str, stats: &ArmStats) {
    println!(
        "HEADROOM-ACCOUNT family={family} class={class} arm={arm} items={} completed={} dead={} \
         attempts={} wasted={} decisions={} cost_usd={:.4} p50_ms={} p95_ms={} mean_ms={:.1}",
        stats.items,
        stats.completed,
        stats.dead_lettered,
        stats.attempts,
        stats.wasted_attempts,
        stats.decisions,
        stats.cost_usd,
        stats.p50_ms(),
        stats.p95_ms(),
        stats.mean_ms(),
    );
}

/// The pre-registered bar, applied to one family row. `items_per_run`
/// scales the per-item latency margin to the per-run granularity the
/// overhead is charged at ("charged per run because that is how a user
/// pays it"); `decisions_per_run` is the emission load of the row with the
/// family's telemetry on — the larger of the floor's and the oracle's
/// decision rate, because an instrumented deployment pays emission on every
/// decision the family makes, not only on the ones the v1 floor happens to
/// journal today.
#[allow(clippy::too_many_arguments)]
fn print_verdict(
    family: &str,
    class: &str,
    floor: &ArmStats,
    oracle: &ArmStats,
    items_per_run: u64,
    decisions_per_run: f64,
    ns_per_decision: f64,
) {
    // The margin the clairvoyant ceiling offers, per run, in wall ms.
    let margin_ms = (floor.mean_ms() - oracle.mean_ms()) * items_per_run as f64;
    let overhead_ms = decisions_per_run * ns_per_decision / 1.0e6;
    let non_inferior = oracle.completion_rate() >= floor.completion_rate();
    let headroom = margin_ms > overhead_ms && non_inferior;
    println!(
        "HEADROOM-VERDICT family={family} class={class} floor_mean_ms={:.1} oracle_mean_ms={:.1} \
         margin_ms_per_run={:.1} overhead_ms_per_run={:.3} floor_completion={:.3} \
         oracle_completion={:.3} floor_cost_usd={:.4} oracle_cost_usd={:.4} \
         floor_wasted={} oracle_wasted={} non_inferior_completion={non_inferior} \
         headroom={}",
        floor.mean_ms(),
        oracle.mean_ms(),
        margin_ms,
        overhead_ms,
        floor.completion_rate(),
        oracle.completion_rate(),
        floor.cost_usd,
        oracle.cost_usd,
        floor.wasted_attempts,
        oracle.wasted_attempts,
        if headroom { "YES" } else { "NO" },
    );
}

/// Oracle dominance is a construction invariant, not a measurement: the
/// oracle sees the same tapes and may only choose weakly better actions.
/// Asserting it turns a bug in any arm into a loud failure instead of a
/// quietly wrong published number.
fn assert_oracle_dominates(family: &str, class: &str, floor: &ArmStats, oracle: &ArmStats) {
    assert!(
        oracle.mean_ms() <= floor.mean_ms() * 1.0001,
        "{family}/{class}: oracle latency must not exceed the floor's"
    );
    assert!(
        oracle.cost_usd <= floor.cost_usd * 1.0001,
        "{family}/{class}: oracle cost must not exceed the floor's"
    );
    assert!(
        oracle.completion_rate() >= floor.completion_rate(),
        "{family}/{class}: oracle completion must be non-inferior"
    );
}

fn accounting(_c: &mut Criterion) {
    println!("# R0.10 headroom experiment — accounting pass (untimed, asserted)");
    println!("# bar: oracle beats the floor on latency per run by more than the");
    println!("# family's per-run telemetry overhead, at non-inferior completion");

    // The bench's floor must be the shipped floor, not a restatement of it:
    // the constants the arms use come from `ExecutorPolicy::static_v0()`,
    // and the backoff function's bounds agree with them.
    let floor_policy = ExecutorPolicy::static_v0();
    assert_eq!(floor_policy.retry.max_attempts, 3);
    assert!(backoff_delay_ms(1, 0.999) < floor_policy.retry.base_delay_ms);
    assert_eq!(backoff_delay_ms(20, 1.0), floor_policy.retry.max_delay_ms);

    // Determinism: the same seeds must produce the same world twice — the
    // reproducibility the experiment's committed artifacts promise.
    let check_a = durable_workload(DURABLE_SEED, false);
    let check_b = durable_workload(DURABLE_SEED, false);
    let sum = |w: &[WorkItem]| -> u64 {
        w.iter()
            .flat_map(|i| i.tape.iter())
            .map(|t| t.latency_ms)
            .sum()
    };
    assert_eq!(
        sum(&check_a),
        sum(&check_b),
        "tapes reproduce from the seed"
    );

    println!(
        "# durable-work callee mix: {}",
        DURABLE_CALLEES
            .iter()
            .map(|c| c.name)
            .collect::<Vec<_>>()
            .join(", ")
    );

    // ---- family: retry, class: durable-work (tapes without hangs) ----
    let workload = durable_workload(DURABLE_SEED, false);
    let stats = run_arms(
        &workload,
        DURABLE_CALLEES.len(),
        Family::Retry,
        DURABLE_SEED,
    );
    let retry_arms = [RetryArm::Floor, RetryArm::Oracle, RetryArm::Heuristic];
    for (arm, s) in retry_arms.iter().zip(&stats) {
        print_account(Family::Retry.name(), "durable_work", arm.name(), s);
    }
    assert_oracle_dominates("retry", "durable_work", &stats[0], &stats[1]);

    // ---- family: timeout, class: durable-work (tapes with hangs) ----
    let workload_hangs = durable_workload(DURABLE_SEED, true);
    let stats_timeout = run_arms(
        &workload_hangs,
        DURABLE_CALLEES.len(),
        Family::Timeout,
        DURABLE_SEED,
    );
    let timeout_arms = [TimeoutArm::Floor, TimeoutArm::Oracle, TimeoutArm::Heuristic];
    for (arm, s) in timeout_arms.iter().zip(&stats_timeout) {
        print_account(Family::Timeout.name(), "durable_work", arm.name(), s);
    }
    assert_oracle_dominates(
        "timeout",
        "durable_work",
        &stats_timeout[0],
        &stats_timeout[1],
    );

    // ---- family: retry, class: llm_bound_scripted ----
    let fixtures = llm_fixtures(LLM_SEED, false);
    let llm_retry = run_llm_arms(&fixtures, Family::Retry, LLM_SEED);
    for (arm, s) in retry_arms.iter().zip(&llm_retry) {
        print_account(Family::Retry.name(), "llm_bound", arm.name(), s);
    }
    assert_oracle_dominates("retry", "llm_bound", &llm_retry[0], &llm_retry[1]);

    // ---- family: timeout, class: llm_bound_scripted ----
    let fixtures_hangs = llm_fixtures(LLM_SEED, true);
    let llm_timeout = run_llm_arms(&fixtures_hangs, Family::Timeout, LLM_SEED);
    for (arm, s) in timeout_arms.iter().zip(&llm_timeout) {
        print_account(Family::Timeout.name(), "llm_bound", arm.name(), s);
    }
    assert_oracle_dominates("timeout", "llm_bound", &llm_timeout[0], &llm_timeout[1]);

    // ---- family: worker placement, class: durable-work ----
    let placement_arms = [
        PlacementArm::Floor,
        PlacementArm::Oracle,
        PlacementArm::Heuristic,
    ];
    let placement: Vec<ArmStats> = placement_arms
        .iter()
        .map(|&arm| run_placement_arm(arm, PLACEMENT_SEED))
        .collect();
    for (arm, s) in placement_arms.iter().zip(&placement) {
        print_account("placement", "durable_work", arm.name(), s);
    }
    assert_oracle_dominates("placement", "durable_work", &placement[0], &placement[1]);

    // ---- family: concurrency, class: durable-work ----
    let concurrency_arms = [
        ConcurrencyArm::Floor,
        ConcurrencyArm::Oracle,
        ConcurrencyArm::Heuristic,
    ];
    let concurrency: Vec<ArmStats> = concurrency_arms
        .iter()
        .map(|&arm| run_concurrency_arm(arm, CONCURRENCY_SEED))
        .collect();
    for (arm, s) in concurrency_arms.iter().zip(&concurrency) {
        print_account("concurrency", "durable_work", arm.name(), s);
    }
    assert_oracle_dominates(
        "concurrency",
        "durable_work",
        &concurrency[0],
        &concurrency[1],
    );

    // ---- telemetry overhead, re-measured untimed for the verdict math ----
    let mut rng = SimRng::seeded(0x7e1e_1e17);
    let samples: Vec<u64> = (0..HEURISTIC_WINDOW)
        .map(|_| DURABLE_CALLEES[3].success_latency(&mut rng))
        .collect();
    let retry_ns = measure_ns_per_decision(
        |j, seq| emit_retry_decision(j, seq, ErrorClass::RateLimited, 1),
        20_000,
    );
    let timeout_ns = measure_ns_per_decision(
        |j, seq| emit_timeout_decision(j, seq, &samples, 12_500),
        20_000,
    );
    let placement_ns =
        measure_ns_per_decision(|j, seq| emit_placement_decision(j, seq, 17, "w2"), 20_000);
    let concurrency_ns =
        measure_ns_per_decision(|j, seq| emit_concurrency_decision(j, seq, 42, 7, 8), 20_000);
    let retry_bytes = measure_bytes_per_decision(
        |j, seq| emit_retry_decision(j, seq, ErrorClass::RateLimited, 1),
        2_000,
    );
    let timeout_bytes = measure_bytes_per_decision(
        |j, seq| emit_timeout_decision(j, seq, &samples, 12_500),
        2_000,
    );
    let placement_bytes =
        measure_bytes_per_decision(|j, seq| emit_placement_decision(j, seq, 17, "w2"), 2_000);
    let concurrency_bytes =
        measure_bytes_per_decision(|j, seq| emit_concurrency_decision(j, seq, 42, 7, 8), 2_000);
    println!(
        "HEADROOM-OVERHEAD family=retry ns_per_decision={retry_ns:.0} bytes_per_decision={retry_bytes:.0}"
    );
    println!(
        "HEADROOM-OVERHEAD family=timeout ns_per_decision={timeout_ns:.0} bytes_per_decision={timeout_bytes:.0}"
    );
    println!(
        "HEADROOM-OVERHEAD family=placement ns_per_decision={placement_ns:.0} bytes_per_decision={placement_bytes:.0}"
    );
    println!(
        "HEADROOM-OVERHEAD family=concurrency ns_per_decision={concurrency_ns:.0} bytes_per_decision={concurrency_bytes:.0}"
    );

    // ---- the verdicts ----
    // Decision rates: the durable-work and placement/concurrency rows treat
    // their whole workload as one run, so the run's decisions are the row
    // total; the LLM rows' stats span LLM_RUNS recorded runs, so the rate
    // divides by the run count.
    let max_decisions =
        |floor: &ArmStats, oracle: &ArmStats| floor.decisions.max(oracle.decisions) as f64;
    print_verdict(
        "retry",
        "durable_work",
        &stats[0],
        &stats[1],
        DURABLE_TASKS as u64,
        max_decisions(&stats[0], &stats[1]),
        retry_ns,
    );
    print_verdict(
        "timeout",
        "durable_work",
        &stats_timeout[0],
        &stats_timeout[1],
        DURABLE_TASKS as u64,
        max_decisions(&stats_timeout[0], &stats_timeout[1]),
        timeout_ns,
    );
    // LLM rows are already per run: one item in these stats is one run.
    print_verdict(
        "retry",
        "llm_bound",
        &llm_retry[0],
        &llm_retry[1],
        1,
        max_decisions(&llm_retry[0], &llm_retry[1]) / LLM_RUNS as f64,
        retry_ns,
    );
    print_verdict(
        "timeout",
        "llm_bound",
        &llm_timeout[0],
        &llm_timeout[1],
        1,
        max_decisions(&llm_timeout[0], &llm_timeout[1]) / LLM_RUNS as f64,
        timeout_ns,
    );
    print_verdict(
        "placement",
        "durable_work",
        &placement[0],
        &placement[1],
        PLACEMENT_TASKS as u64,
        max_decisions(&placement[0], &placement[1]),
        placement_ns,
    );
    print_verdict(
        "concurrency",
        "durable_work",
        &concurrency[0],
        &concurrency[1],
        CONCURRENCY_TASKS as u64,
        max_decisions(&concurrency[0], &concurrency[1]),
        concurrency_ns,
    );
}

criterion_group!(benches, accounting, bench_telemetry);
criterion_main!(benches);
