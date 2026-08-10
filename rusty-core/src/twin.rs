//! The runtime digital twin (R0.10 wave 2): a deterministic re-execution
//! environment for recorded runs.
//!
//! Exact replay ([`crate::replay`]) answers one question — "does the same
//! graph reproduce the same evidence?" — and stops there. The twin answers
//! the four questions plain replay cannot, per `docs/adaptation-design.md`:
//!
//! 1. **What happens under faults the recording never saw** — a
//!    [`FaultSchedule`]: a deterministic, seeded list of faults attached to
//!    a twin run. Injection lands at decision points and the effect
//!    boundary, never by patching code: the twin's scheduler reads the
//!    schedule the way the production scheduler reads the world.
//! 2. **What happens under different concurrent interleavings** — schedule
//!    randomization: the parallel task set of each super-step is driven in
//!    a seeded order, so a concurrency policy is evaluated against the
//!    interleaving distribution, not the one schedule the recording
//!    happened to get. The journaled total order of evidence stays
//!    canonical; per-node latencies vary with the schedule.
//! 3. **What happens if one decision changes** — a [`CounterfactualFork`]:
//!    apply one different legal action at one decision, continue with
//!    effects served from the recording where the decision leaves their
//!    inputs untouched, and compare with [`BranchDiff`]. With
//!    [`CounterfactualFork::then_act_with`] this is also R0.5's deferred
//!    *hybrid* replay: the recorded behavior up to the fork, a new policy
//!    afterward.
//! 4. **What a candidate policy *would have* decided on the same evidence**
//!    — a shadow policy: the candidate decides, the floor acts, both
//!    journal as `PolicyDecision` events ([`DecisionRole::Shadow`] /
//!    [`DecisionRole::Acting`]) with their true propensities. Exploration
//!    by seeded draw makes the shadow a stochastic policy with known
//!    propensities: well-posed off-policy evidence by construction.
//!
//! # What the twin reuses (and does not re-implement)
//!
//! - **Determinism seams** ([`Clock`], [`RngSource`]): every timestamp,
//!   backoff jitter draw, interleaving permutation, and stochastic policy
//!   draw comes from the run's configured seams, so a twin run reproduces
//!   exactly from its seed and fixture.
//! - **The effect journal's servable-kind vocabulary**
//!   ([`crate::replay::SERVABLE_KINDS`]) and integrity verification
//!   ([`ExactReplay`]'s boundary checks): the recorded world is loaded and
//!   verified by the replay machinery, then decomposed into work items.
//! - **The retry policy gates** ([`crate::durable::classify_retry`]'s
//!   inputs via [`crate::durable::retry_legal_actions`]): the twin never
//!   re-implements the gates; the legal set a policy sees is computed by
//!   the same function the production scheduler uses.
//! - **[`BranchDiff`]**: counterfactual comparison is journaled evidence,
//!   not a log line.
//!
//! # The honest edge
//!
//! The twin is a model of the runtime, not of the world. A counterfactual
//! decision whose downstream effects are replay-servable — retry counts,
//! backoff delays, timeout bounds, placement among equivalent workers,
//! concurrency caps, checkpoint cadence — is exactly evaluable, because
//! those decisions change *when and whether* effects execute, not what
//! they return. A decision that would change an effect's *input* is
//! unevaluable: the journal has no answer to a call the recorded world
//! never received. The twin enforces the boundary structurally — policies
//! choose among the closed [`DecisionAction`] set of one decision point,
//! and no member of that set rewrites an effect's input — and refuses
//! counterfactual forks outside the recomputed legal set. Every
//! [`TwinReport`] states the bound in its required [`TwinReport::bound`]
//! field (design open question 5: the constraint lives in the record, not
//! in prose framing).
//!
//! Two modeling decisions, stated plainly:
//!
//! - **The world's baseline answer is the recording.** Each recorded
//!   effect is a work item whose truth is the journaled outcome. Re-served
//!   attempts (a retry re-issues the same bytes) receive the recorded
//!   answer again — the world answers the same bytes the same way. A
//!   recorded error therefore keeps failing across attempts: the recording
//!   is all the twin knows.
//! - **The twin's scheduler is synchronous and simulated.** Work items
//!   execute on declared worker lanes; admission waits, timeout
//!   truncations, backoff delays, and lease-expiry discoveries are
//!   computed, not slept. What a policy *observes* (a timeout bound
//!   truncates the observed latency; the world's truth is untouched) is
//!   the Wave 1 rule carried into the twin. The graph executor and
//!   checkpointing are not driven: the twin's unit of evidence is the
//!   journal it writes, and checkpoint cadence evaluation stays with the
//!   R0.5 checkpoint machinery.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::durable::{backoff_delay_ms, retry_legal_actions, ErrorClass, MAX_RETRY_DELAY_MS};
use crate::error::{Result, RustyError};
use crate::journal::{Clock, EventDraft, Journal, JournalSnapshot, RngSource};
use crate::llm::Usage;
use crate::record::{
    DecisionAction, DecisionEvent, DecisionFamily, DecisionRole, Effect, EventStatus, PayloadRef,
    PolicyVersion, RunEventKind,
};
use crate::replay::{BranchDiff, ExactReplay, LogicalClockParams, ReplayFixture, SERVABLE_KINDS};

fn twin_error(message: impl Into<String>) -> RustyError {
    // The twin's failures are replay-family failures: a run that cannot
    // answer from its evidence must stop, not improvise.
    RustyError::Replay(format!("twin: {}", message.into()))
}

/// The validity-boundary statement every [`TwinReport`] carries. Required,
/// not optional: an auditor reads the twin's constraint from the record
/// itself (design open question 5).
pub const TWIN_REPORT_BOUND: &str = "the twin evaluates only decisions that change when/whether \
     effects execute — retry counts, backoff delays, timeout bounds, placement among equivalent \
     workers, concurrency caps, checkpoint cadence — never what effects return; a decision that \
     would change an effect's input is unevaluable and was excluded or refused";

/// The policy version a counterfactual fork journals under. A fork is not a
/// policy — it is the experimenter's directed what-if, deterministic by
/// construction — so its decisions are named as such rather than
/// attributed to the floor or the candidate.
pub const TWIN_FORK_POLICY_VERSION: &str = "twin-fork";

/// The timeout ladder's minimum rung, in milliseconds. Below this, ordinary
/// work aborts early — a correctness hazard no policy may cross (the same
/// floor Wave 1 pre-registered).
pub const MIN_TIMEOUT_RUNG_MS: u64 = 100;

/// The default timeout ladder: discrete rungs between the minimum and the
/// lease boundary ([`MAX_RETRY_DELAY_MS`], the queue's worst-case discovery
/// latency for hung work — the floor's "no bound in force", modeled as the
/// top rung exactly as Wave 1 modeled it).
pub const DEFAULT_TIMEOUT_LADDER: [u64; 7] =
    [100, 500, 1_000, 5_000, 30_000, 120_000, MAX_RETRY_DELAY_MS];

/// The default concurrency ladder. The top rung is uncapped — the floor's
/// stance when no concurrency policy is in force.
pub const DEFAULT_CONCURRENCY_LADDER: [u32; 6] = [1, 2, 4, 8, 16, u32::MAX];

// ---------------------------------------------------------------------------
// Mechanism 1: fault injection.
// ---------------------------------------------------------------------------

/// One injectable fault. The taxonomy mirrors the world's failure modes the
/// scheduler already classifies: each variant names the [`ErrorClass`] the
/// injected failure is observed with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "fault", rename_all = "snake_case")]
pub enum InjectedFault {
    /// The worker dies mid-attempt. The attempt is lost and surfaces at the
    /// lease boundary ([`MAX_RETRY_DELAY_MS`]) classified
    /// [`ErrorClass::Unknown`] — the lease-expiry path, the dead-letter
    /// queue's primary input.
    WorkerCrash,

    /// The attempt never completes on its own. With no timeout bound in
    /// force it surfaces at the lease boundary; with a bound, at the bound,
    /// classified [`ErrorClass::Timeout`] either way.
    CalleeTimeout,

    /// The callee asks to be slowed down: [`ErrorClass::RateLimited`] with
    /// a `Retry-After` floor any policy's delay must respect.
    RateLimited {
        /// The callee-supplied `Retry-After` floor, in milliseconds.
        retry_after_ms: u64,
    },

    /// The worker or callee is out of capacity:
    /// [`ErrorClass::ResourceExhausted`]. Retryable — ideally on a
    /// different worker, which is what makes this the placement family's
    /// fault.
    ResourceExhausted,
}

impl InjectedFault {
    /// The class the scheduler observes for this fault.
    pub fn error_class(&self) -> ErrorClass {
        match self {
            InjectedFault::WorkerCrash => ErrorClass::Unknown,
            InjectedFault::CalleeTimeout => ErrorClass::Timeout,
            InjectedFault::RateLimited { .. } => ErrorClass::RateLimited,
            InjectedFault::ResourceExhausted => ErrorClass::ResourceExhausted,
        }
    }
}

/// Where a fault lands. Injection happens at decision points and the effect
/// boundary — the two places the production scheduler reads the world.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "anchor", rename_all = "snake_case")]
pub enum FaultAnchor {
    /// At the effect boundary: the `attempt`-th attempt (1-based) of the
    /// effect recorded at `effect_seq` fails with the injected fault.
    OnAttempt {
        /// The recorded journal sequence number of the target effect.
        effect_seq: u64,
        /// The 1-based attempt ordinal the fault fires on.
        attempt: u32,
    },

    /// At a decision point: when the scheduler makes the retry decision
    /// with sequence `decision_seq`, the in-flight attempt is observed
    /// through the injected fault instead of its real outcome (a crash
    /// here is the lease-expiry path: the worker died, the attempt is
    /// lost, the failure classifies [`ErrorClass::Unknown`]). Fires only
    /// at retry decision points — the scheduler's fail-task path, where
    /// the production scheduler would observe a lost attempt; an anchor
    /// that never coincides with one never fires, and the report's
    /// fired/declared counts say so.
    AtDecision {
        /// The twin run's decision sequence number (`d{n}`) to fire at.
        decision_seq: u64,
    },

    /// A provider rate-limit window: every attempt of every effect recorded
    /// in `from_seq..=to_seq` is answered with the injected fault — the
    /// provider-side degradation a recording from a healthy window never
    /// contains.
    Window {
        /// First recorded effect sequence covered by the window.
        from_seq: u64,
        /// Last recorded effect sequence covered by the window (inclusive).
        to_seq: u64,
    },

    /// Resource exhaustion on one worker: every attempt placed on `worker`
    /// fails. The placement family's evaluation fault: work steered onto
    /// the degraded worker pays for it.
    OnWorker {
        /// The degraded worker's identity (a member of the run's pool).
        worker: String,
    },
}

/// One entry of a [`FaultSchedule`]: an anchor plus the fault that fires
/// there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultInjection {
    /// Where the fault lands.
    #[serde(flatten)]
    pub anchor: FaultAnchor,

    /// What the world does there.
    #[serde(flatten)]
    pub fault: InjectedFault,
}

/// A deterministic, seeded fault schedule attached to a twin run.
///
/// The schedule is *data*, declared before the run: the twin's scheduler
/// reads it the way the production scheduler reads the world, and two runs
/// over the same fixture with the same schedule and seed fire exactly the
/// same faults at exactly the same points. Serializable so a schedule can
/// be committed alongside the fixture it probes — the evaluation's
/// reproducibility artifact.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultSchedule {
    /// The seed the schedule was authored under (carried for provenance;
    /// the run's own seed governs draws).
    pub seed: u64,

    /// The declared injections, consulted in declaration order: the first
    /// anchor matching an attempt or decision point wins.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub injections: Vec<FaultInjection>,
}

impl FaultSchedule {
    /// An empty schedule under `seed`: the recorded world, unfaulted.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            injections: Vec::new(),
        }
    }

    /// Builder-style: declare one more injection.
    pub fn with_injection(mut self, anchor: FaultAnchor, fault: InjectedFault) -> Self {
        self.injections.push(FaultInjection { anchor, fault });
        self
    }

    /// The fault firing on attempt `attempt` of the effect recorded at
    /// `effect_seq` when placed on `worker`, if any. Consulted at the
    /// effect boundary, once per attempt.
    pub fn fault_for_attempt(
        &self,
        effect_seq: u64,
        attempt: u32,
        worker: &str,
    ) -> Option<&InjectedFault> {
        self.injections
            .iter()
            .find(|injection| match &injection.anchor {
                FaultAnchor::OnAttempt {
                    effect_seq: seq,
                    attempt: ord,
                } => *seq == effect_seq && *ord == attempt,
                FaultAnchor::Window { from_seq, to_seq } => {
                    *from_seq <= effect_seq && effect_seq <= *to_seq
                }
                FaultAnchor::OnWorker { worker: w } => w == worker,
                FaultAnchor::AtDecision { .. } => false,
            })
            .map(|injection| &injection.fault)
    }

    /// The fault firing at the retry decision with sequence `decision_seq`,
    /// if any. Consulted at the scheduler's fail-task decision point only.
    pub fn fault_for_decision(&self, decision_seq: u64) -> Option<&InjectedFault> {
        self.injections
            .iter()
            .find(|injection| match &injection.anchor {
                FaultAnchor::AtDecision { decision_seq: seq } => *seq == decision_seq,
                _ => false,
            })
            .map(|injection| &injection.fault)
    }
}

// ---------------------------------------------------------------------------
// Mechanism 4's other half: the policies. (The shadow pair itself is the
// run machinery below.)
// ---------------------------------------------------------------------------

/// What a policy sees at one decision point: the family, the journaled
/// feature map, and the closed legal-action set computed by the runtime's
/// own gates. A policy chooses among `legal_actions` members and declares
/// its propensity — never a free-form output.
#[derive(Debug)]
pub struct DecisionContext<'a> {
    /// Which executor decision this is.
    pub family: DecisionFamily,

    /// The observation assembled at the decision point.
    pub features: &'a Map<String, Value>,

    /// Every action legal at decision time.
    pub legal_actions: &'a [DecisionAction],
}

/// A policy the twin can evaluate: the floor, or a candidate scoring the
/// same features.
///
/// `decide` receives one seeded `draw` per invocation so stochastic
/// candidates explore by seeded draw — a stochastic policy with known
/// propensities, which is what makes its journaled shadow decisions
/// well-posed off-policy evidence. Implementations must return a member of
/// `legal_actions` and a propensity in `(0, 1]`; the twin rejects either
/// violation at the boundary rather than journaling dishonest evidence.
pub trait TwinPolicy: fmt::Debug + Send + Sync {
    /// The version the policy's decisions journal under.
    fn version(&self) -> PolicyVersion;

    /// Choose an action from `ctx.legal_actions` and declare its
    /// propensity. `draw` is a seeded uniform from `[0, 1)`.
    fn decide(&self, ctx: &DecisionContext<'_>, draw: f64) -> (DecisionAction, f64);

    /// The re-queue delay after a `Retry` decision, in milliseconds. The
    /// closed action set carries no delay member — the delay is
    /// policy-parameterized within declared bounds, per the family's
    /// contract — so it is a method, not an action. Defaults to the
    /// floor's full-jitter exponential backoff.
    fn retry_delay_ms(&self, attempt: u32, draw: f64) -> u64 {
        backoff_delay_ms(attempt, draw)
    }
}

/// The `static-v0` floor: the behavior every pre-learning run had, and the
/// baseline every candidate is evaluated against.
///
/// Per family: **retry** retries whenever the gates leave a `Retry` member
/// in the legal set (the legal set is computed by
/// [`retry_legal_actions`], so the floor's stance is exactly
/// `classify_retry`'s); **timeout** imposes no bound — modeled as the
/// ladder's top rung, the lease boundary, Wave 1's `LEASE_BOUND_MS`;
/// **placement** takes the first eligible worker (the static-pool stance:
/// no re-placement intelligence); **concurrency** is uncapped. Every
/// selection has propensity 1.0: the floor is deterministic.
#[derive(Debug, Default, Clone, Copy)]
pub struct StaticFloor;

impl TwinPolicy for StaticFloor {
    fn version(&self) -> PolicyVersion {
        PolicyVersion::new(PolicyVersion::STATIC_V0)
    }

    fn decide(&self, ctx: &DecisionContext<'_>, _draw: f64) -> (DecisionAction, f64) {
        let selected = match ctx.family {
            DecisionFamily::Retry => ctx
                .legal_actions
                .iter()
                .find(|action| matches!(action, DecisionAction::Retry { .. }))
                .cloned()
                .unwrap_or(DecisionAction::Abort),
            // The floor's "no bound in force" and "unlimited" stances are
            // the ladder tops; placement's static pool is the first
            // eligible worker; the checkpoint floor writes.
            DecisionFamily::Timeout
            | DecisionFamily::Concurrency
            | DecisionFamily::CheckpointPlacement => ctx
                .legal_actions
                .last()
                .cloned()
                .unwrap_or(DecisionAction::Abort),
            DecisionFamily::WorkerPlacement => ctx
                .legal_actions
                .first()
                .cloned()
                .unwrap_or(DecisionAction::Abort),
        };
        (selected, 1.0)
    }
}

// ---------------------------------------------------------------------------
// The recorded world: a journaled run decomposed into work items.
// ---------------------------------------------------------------------------

/// What the recorded world did for one effect: the journaled outcome,
/// resolved out of the snapshot's payload references.
#[derive(Debug, Clone)]
pub struct RecordedAnswer {
    /// How the recorded call ended (`Interrupted` is rejected at load:
    /// servable calls never produce it).
    pub status: EventStatus,

    /// The recorded response payload.
    pub output: Option<Value>,

    /// The recorded latency in milliseconds (0 when unmeasured).
    pub latency_ms: u64,

    /// The recorded token usage, when reported.
    pub tokens: Option<Usage>,

    /// The recorded cost in USD, when known.
    pub cost_usd: Option<f64>,
}

/// One recorded effect as a unit of twin work: the request the world
/// answered, the answer it gave, the effect classification the gates read,
/// and the super-step the journal places it in.
#[derive(Debug, Clone)]
pub struct TwinWorkItem {
    /// The recorded event's journal sequence number (fault anchors name
    /// effects by it).
    pub recorded_seq: u64,

    /// The recorded effect kind (a member of [`SERVABLE_KINDS`]).
    pub kind: RunEventKind,

    /// The node that issued the call, when journaled.
    pub node_id: Option<String>,

    /// The declared effect class — the retry gate's input.
    pub effect: Effect,

    /// The request, exactly as journaled. Retried attempts re-issue these
    /// bytes; the twin never rewrites them.
    pub request: Value,

    /// What the recorded world answered.
    pub answer: RecordedAnswer,

    /// The super-step the recording placed this effect in (0 when the
    /// journal carries no super-step structure).
    pub step: u64,
}

/// The recorded world a twin run re-executes against: the fixture's
/// servable effects as work items, grouped by super-step.
#[derive(Debug, Clone)]
pub struct TwinWorld {
    items: Vec<TwinWorkItem>,
    /// Distinct super-step indexes in ascending order.
    steps: Vec<u64>,
}

impl TwinWorld {
    /// Decompose an integrity-verified snapshot into work items. Effects
    /// journaled between `SuperStepStart` boundaries inherit that step;
    /// everything before the first boundary (or in a journal with no
    /// boundaries) lands in step 0.
    pub fn from_snapshot(snapshot: &JournalSnapshot) -> Result<Self> {
        let resolve = |payload: &PayloadRef| -> Option<Value> {
            match payload {
                PayloadRef::Inline(value) => Some(value.clone()),
                PayloadRef::Artifact(reference) => {
                    snapshot.artifacts.get(&reference.sha256).cloned()
                }
            }
        };
        let mut items = Vec::new();
        let mut steps = Vec::new();
        let mut current_step = 0u64;
        for event in &snapshot.events {
            if event.kind == RunEventKind::SuperStepStart {
                current_step = event
                    .input
                    .as_ref()
                    .and_then(&resolve)
                    .and_then(|input| input.get("step").and_then(Value::as_u64))
                    .unwrap_or(current_step + 1);
                steps.push(current_step);
                continue;
            }
            if !SERVABLE_KINDS.contains(&event.kind) {
                continue;
            }
            if event.status == EventStatus::Interrupted {
                return Err(twin_error(format!(
                    "recorded {:?} at seq {} has status `interrupted`, which servable calls \
                     never produce — the journal is inconsistent",
                    event.kind, event.seq
                )));
            }
            let request = event.input.as_ref().and_then(&resolve).ok_or_else(|| {
                twin_error(format!(
                    "recorded {:?} at seq {} has no input payload; the twin needs the request \
                     the world answered",
                    event.kind, event.seq
                ))
            })?;
            let answer = RecordedAnswer {
                status: event.status,
                output: event.output.as_ref().and_then(&resolve),
                latency_ms: event.latency_ms.unwrap_or(0),
                tokens: event.tokens,
                cost_usd: event.cost_usd,
            };
            items.push(TwinWorkItem {
                recorded_seq: event.seq,
                kind: event.kind,
                node_id: event.node_id.clone(),
                effect: event.effect,
                request,
                answer,
                step: current_step,
            });
        }
        if steps.is_empty() && !items.is_empty() {
            steps.push(0);
        }
        Ok(Self { items, steps })
    }

    /// Every work item, in recorded sequence order.
    pub fn items(&self) -> &[TwinWorkItem] {
        &self.items
    }

    /// The super-step indexes the recording contains, ascending.
    pub fn steps(&self) -> &[u64] {
        &self.steps
    }

    /// The work items of one super-step, in recorded (canonical) order.
    pub fn items_in(&self, step: u64) -> Vec<&TwinWorkItem> {
        self.items.iter().filter(|item| item.step == step).collect()
    }
}

// ---------------------------------------------------------------------------
// Run configuration and outcomes.
// ---------------------------------------------------------------------------

/// How the parallel task set of each super-step is ordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interleaving {
    /// Recorded order. The baseline every randomized run compares against.
    #[default]
    Canonical,

    /// A seeded permutation drawn from the run's [`RngSource`] — mechanism
    /// 2: the same evidence re-driven under one point of the interleaving
    /// distribution. Journaled event order stays canonical; admission
    /// waits, and therefore per-node latencies, follow the drawn order.
    Seeded,
}

/// The parameters of one twin run: determinism seams, the fault schedule,
/// the acting policy, an optional shadow, the decision ladders, and the
/// worker pool.
///
/// Two runs over the same [`Twin`] with equal configs produce byte-identical
/// journals — the twin's whole claim, proven in the test suite across
/// repeated runs and across process invocations (the checked-in golden).
pub struct TwinRunConfig {
    /// The run's seed: RNG draws, interleaving permutations, and stochastic
    /// policy draws all derive from it.
    pub seed: u64,

    /// The logical clock the run's journal timestamps from.
    pub clock: LogicalClockParams,

    /// The fault schedule the scheduler reads. Empty means the recorded
    /// world, unfaulted.
    pub faults: FaultSchedule,

    /// The policy whose actions execute. The floor by default.
    pub acting: Arc<dyn TwinPolicy>,

    /// A candidate scoring the same features without acting (mechanism 4).
    pub shadow: Option<Arc<dyn TwinPolicy>>,

    /// The timeout family's legal rungs, ascending. The top rung models
    /// "no bound in force"; keep it at [`MAX_RETRY_DELAY_MS`] for
    /// comparability with Wave 1.
    pub timeout_ladder: Vec<u64>,

    /// The concurrency family's legal rungs, ascending; `u32::MAX` is
    /// uncapped.
    pub concurrency_ladder: Vec<u32>,

    /// The worker pool placement ranks. Equivalence is a precondition: the
    /// pool declares the workers interchangeable for this workload.
    pub workers: Vec<String>,

    /// The retry attempt budget (the floor's own is 3).
    pub max_attempts: u32,

    /// Canonical or seeded ordering of each super-step's task set.
    pub interleaving: Interleaving,
}

impl TwinRunConfig {
    /// A baseline configuration: the floor acting, no shadow, no faults,
    /// canonical order, default ladders, one worker, the floor's attempt
    /// budget.
    pub fn new(seed: u64) -> Self {
        Self {
            seed,
            clock: LogicalClockParams {
                start_ms: 1_700_000_000_000,
                tick_ms: 10,
            },
            faults: FaultSchedule::new(seed),
            acting: Arc::new(StaticFloor),
            shadow: None,
            timeout_ladder: DEFAULT_TIMEOUT_LADDER.to_vec(),
            concurrency_ladder: DEFAULT_CONCURRENCY_LADDER.to_vec(),
            workers: vec!["worker-0".to_owned()],
            max_attempts: 3,
            interleaving: Interleaving::Canonical,
        }
    }

    /// Builder-style: attach a fault schedule.
    pub fn with_faults(mut self, faults: FaultSchedule) -> Self {
        self.faults = faults;
        self
    }

    /// Builder-style: act with `policy` instead of the floor.
    pub fn with_acting(mut self, policy: Arc<dyn TwinPolicy>) -> Self {
        self.acting = policy;
        self
    }

    /// Builder-style: shadow the acting policy with `candidate`.
    pub fn with_shadow(mut self, candidate: Arc<dyn TwinPolicy>) -> Self {
        self.shadow = Some(candidate);
        self
    }

    /// Builder-style: declare the worker pool.
    pub fn with_workers(mut self, workers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.workers = workers.into_iter().map(Into::into).collect();
        self
    }

    /// Builder-style: override the retry attempt budget.
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    /// Builder-style: order each super-step's task set by seeded draw.
    pub fn with_seeded_interleaving(mut self) -> Self {
        self.interleaving = Interleaving::Seeded;
        self
    }
}

impl fmt::Debug for TwinRunConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwinRunConfig")
            .field("seed", &self.seed)
            .field("clock", &self.clock)
            .field("faults", &self.faults)
            .field("acting", &self.acting.version())
            .field("shadow", &self.shadow.as_ref().map(|p| p.version()))
            .field("workers", &self.workers)
            .field("max_attempts", &self.max_attempts)
            .field("interleaving", &self.interleaving)
            .finish()
    }
}

/// Aggregate outcome metrics of one twin run. Simulated, deterministic, and
/// directly comparable across arms — the same accounting Wave 1 prices.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TwinMetrics {
    /// Work items the run contained.
    pub items: usize,

    /// Items that reached a successful terminal attempt.
    pub completed: usize,

    /// Items that exhausted the attempt budget on a retryable failure.
    pub dead_lettered: usize,

    /// Items that failed permanently (the effect or class gate).
    pub failed: usize,

    /// Total attempts executed across all items.
    pub attempts: u64,

    /// Summed recorded cost in USD across served attempts.
    pub cost_usd: f64,

    /// Simulated wall time of the whole run, in milliseconds.
    pub wall_time_ms: u64,

    /// Nearest-rank p50 of per-item elapsed time (first ready to terminal),
    /// in milliseconds.
    pub item_latency_p50_ms: u64,

    /// Nearest-rank p95 of per-item elapsed time, in milliseconds.
    pub item_latency_p95_ms: u64,
}

/// A case the twin excluded or flagged under the honest edge. Structural
/// prevention covers most of the boundary (no legal action rewrites an
/// effect's input); these are the cases that still need a name in the
/// report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "case", rename_all = "snake_case")]
pub enum UnevaluableCase {
    /// A fault fired on work whose declared effect is not freely
    /// repeatable: the effect gate fails the item on the first fault, so
    /// retry counterfactuals past that point have no reachable world.
    /// Flagged rather than silent: a schedule author should read that the
    /// gate, not the policy, decided the item's future.
    GatedEffect {
        /// The recorded sequence number of the gated effect.
        effect_seq: u64,
    },

    /// A counterfactual fork named an action outside the legal set
    /// recomputed at its decision point — including every input-changing
    /// action, which no legal set ever contains. Always a refusal, never a
    /// journaled run.
    IllegalForkAction {
        /// The decision the fork named.
        decision_seq: u64,
        /// The action that was refused.
        action: DecisionAction,
    },

    /// A counterfactual fork named a decision the run never reaches.
    UnknownDecision {
        /// The decision sequence that does not exist in this run.
        decision_seq: u64,
    },
}

/// The report every twin run emits: what was evaluated, what was excluded,
/// and the bound the evaluation holds to. The validity boundary is a
/// required field of the payload, not prose about it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TwinReport {
    /// The twin run's id.
    pub run_id: String,

    /// The seed the run drew from.
    pub seed: u64,

    /// Faults the schedule declared.
    pub faults_declared: usize,

    /// Faults that actually fired. Less than declared means anchors that
    /// never coincided with an attempt or retry decision — visible, never
    /// silent.
    pub faults_fired: usize,

    /// Decisions journaled by the acting policy (shadow decisions add
    /// their own events but are not separate decision points).
    pub decisions: usize,

    /// Decision points whose outcomes are exactly evaluable: replay-servable
    /// or fault-injected downstream. In this model every journaled decision
    /// is evaluable by construction; the field exists so a future mechanism
    /// that is not must subtract here.
    pub evaluable_decisions: usize,

    /// Decision sequences where the shadow's selection differed from the
    /// acting policy's — the points a counterfactual branch estimates the
    /// candidate's outcome from.
    pub shadow_divergences: Vec<u64>,

    /// Cases excluded or flagged under the honest edge.
    pub excluded: Vec<UnevaluableCase>,

    /// The validity-boundary statement. Always [`TWIN_REPORT_BOUND`];
    /// carried as a field so the constraint travels with the evidence.
    pub bound: String,
}

/// The result of one twin run: the journaled evidence, the decision stream,
/// the metrics, and the report.
#[derive(Debug)]
pub struct TwinOutcome {
    /// The twin run's journal snapshot (super-step boundaries, served and
    /// faulted effect events, and every `PolicyDecision`).
    pub journal: JournalSnapshot,

    /// Every decision the run journaled — acting and shadow interleaved by
    /// decision point, in decision order.
    pub decisions: Vec<DecisionEvent>,

    /// Aggregate metrics.
    pub metrics: TwinMetrics,

    /// The validity report.
    pub report: TwinReport,
}

/// A directed what-if: at `decision_seq`, apply `action` instead of the
/// acting policy's choice, then continue.
#[derive(Debug)]
pub struct CounterfactualFork {
    /// The twin run's decision sequence number to fork at (a report's
    /// `shadow_divergences` are the natural candidates).
    pub decision_seq: u64,

    /// The different legal action to apply. Must be a member of the legal
    /// set recomputed at that decision point — the twin refuses anything
    /// else, which is where the honest edge bites: no legal set contains an
    /// input-changing action.
    pub action: DecisionAction,

    /// Hybrid replay (R0.5's deferred mode): after the fork, act with this
    /// policy instead of the run's original acting policy. `None` keeps the
    /// original acting policy — a single-decision what-if.
    pub then_act_with: Option<Arc<dyn TwinPolicy>>,
}

/// The result of a counterfactual branch: the branch's own outcome plus its
/// [`BranchDiff`] against the baseline — fork-plus-diff, comparison as
/// journaled evidence.
#[derive(Debug)]
pub struct CounterfactualBranch {
    /// The branch run's outcome.
    pub outcome: TwinOutcome,

    /// The baseline's outcome the branch was diffed against.
    pub baseline: TwinOutcome,

    /// The logical diff: the fork's divergence point, the work each side
    /// did after it, step-level differences, and per-side totals.
    pub diff: BranchDiff,
}

// ---------------------------------------------------------------------------
// The twin.
// ---------------------------------------------------------------------------

/// A deterministic re-execution environment over one recorded run.
///
/// Construction verifies the journal's integrity through the replay
/// module's boundary ([`ExactReplay::new`]) and decomposes the servable
/// effects into the world the scheduler re-drives. The typical flow:
///
/// 1. `let twin = Twin::from_fixture(&fixture)?;`
/// 2. `let baseline = twin.run(&TwinRunConfig::new(seed))?;` — the
///    recorded world under the floor;
/// 3. probe it: `twin.run(&config.with_faults(schedule))`,
///    `twin.run_interleavings(8, &config.with_seeded_interleaving())`,
///    `twin.run(&config.with_shadow(candidate))`, or
///    `twin.counterfactual(&config, fork)`.
#[derive(Debug)]
pub struct Twin {
    world: TwinWorld,
    run_id: String,
    thread_id: String,
}

impl Twin {
    /// A twin over an integrity-verified snapshot. Resumed-run journals are
    /// rejected with the same boundary exact replay applies: their evidence
    /// begins mid-run against checkpointed state the journal does not
    /// carry.
    pub fn from_snapshot(snapshot: JournalSnapshot) -> Result<Self> {
        // Integrity verification (head hash, artifacts, references, the
        // resumed-run boundary) is the replay module's contract, consumed —
        // not re-implemented.
        ExactReplay::new(snapshot.clone())?;
        let world = TwinWorld::from_snapshot(&snapshot)?;
        Ok(Self {
            world,
            run_id: snapshot.run_id,
            thread_id: snapshot.thread_id,
        })
    }

    /// A twin over a portable fixture's journal: a recorded production run
    /// becomes a twin case by export, unmodified.
    pub fn from_fixture(fixture: &ReplayFixture) -> Result<Self> {
        Self::from_snapshot(fixture.journal.clone())
    }

    /// The recorded world this twin re-executes against.
    pub fn world(&self) -> &TwinWorld {
        &self.world
    }

    /// The twin run id for `seed` — deterministic, so equal configs share
    /// an id and their journals compare byte-for-byte.
    fn twin_run_id(&self, seed: u64) -> String {
        format!("twin:{}:{seed:016x}", self.run_id)
    }

    /// Re-execute the recorded world under `config`.
    pub fn run(&self, config: &TwinRunConfig) -> Result<TwinOutcome> {
        self.run_inner(config, None)
    }

    /// Re-run the recorded world under `runs` seeded interleavings
    /// (mechanism 2). Run `k` draws its permutation and jitter from
    /// `config.seed` mixed with `k`, so the interleaving distribution is
    /// sampled deterministically and each point of it reproduces exactly.
    /// The journaled total order of evidence is canonical in every run —
    /// what varies is admission order and therefore per-node latencies.
    pub fn run_interleavings(
        &self,
        runs: usize,
        config: &TwinRunConfig,
    ) -> Result<Vec<TwinOutcome>> {
        (0..runs)
            .map(|k| {
                let run_config = TwinRunConfig {
                    seed: config.seed ^ (k as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15),
                    clock: config.clock,
                    faults: config.faults.clone(),
                    acting: config.acting.clone(),
                    shadow: config.shadow.clone(),
                    timeout_ladder: config.timeout_ladder.clone(),
                    concurrency_ladder: config.concurrency_ladder.clone(),
                    workers: config.workers.clone(),
                    max_attempts: config.max_attempts,
                    interleaving: Interleaving::Seeded,
                };
                self.run(&run_config)
            })
            .collect()
    }

    /// Fork the recorded run at one decision: `baseline` behavior up to the
    /// fork, `fork.action` applied at it, then `fork.then_act_with` (or the
    /// original acting policy) afterward — with the same fault schedule
    /// still in force.
    ///
    /// Refuses, with [`UnevaluableCase`] naming the case, when the fork's
    /// action is outside the legal set recomputed at its decision point
    /// (the honest edge made structural) or when the run never reaches the
    /// named decision.
    pub fn counterfactual(
        &self,
        baseline: &TwinRunConfig,
        fork: CounterfactualFork,
    ) -> Result<CounterfactualBranch> {
        let base_outcome = self.run(baseline)?;
        let branch_outcome = self.run_inner(baseline, Some(&fork))?;
        let diff = BranchDiff::between(&base_outcome.journal, &branch_outcome.journal);
        Ok(CounterfactualBranch {
            outcome: branch_outcome,
            baseline: base_outcome,
            diff,
        })
    }

    /// The shared run machinery. With `fork = Some`, the decision at
    /// `fork.decision_seq` is overridden (validated against its recomputed
    /// legal set) and the acting policy swaps afterward.
    fn run_inner(
        &self,
        config: &TwinRunConfig,
        fork: Option<&CounterfactualFork>,
    ) -> Result<TwinOutcome> {
        if config.workers.is_empty() {
            return Err(twin_error(
                "the worker pool is empty; placement needs at least one equivalent worker",
            ));
        }
        if config
            .timeout_ladder
            .iter()
            .any(|rung| *rung < MIN_TIMEOUT_RUNG_MS)
        {
            return Err(twin_error(format!(
                "timeout ladder carries a rung below the {MIN_TIMEOUT_RUNG_MS} ms minimum — \
                 below it ordinary work aborts early, a correctness hazard no policy may cross"
            )));
        }
        let run_id = self.twin_run_id(config.seed);
        let mut state = RunState {
            journal: Journal::new(
                run_id.clone(),
                self.thread_id.clone(),
                Clock::logical(config.clock.start_ms, config.clock.tick_ms),
            ),
            rng: RngSource::seeded(config.seed),
            run_id,
            decisions: Vec::new(),
            pending: Vec::new(),
            next_decision_seq: 0,
            faults_fired: 0,
            divergences: Vec::new(),
            excluded: Vec::new(),
            forked: false,
        };
        // The acting policy is swappable at exactly one point: the fork.
        let mut acting = config.acting.clone();
        // Simulated worker lanes: free-at times in milliseconds since run
        // start. Steps are barriers; a step starts when every lane is free.
        let mut lane_free = vec![0u64; config.workers.len()];
        let mut item_elapsed: Vec<u64> = Vec::new();
        let mut metrics = TwinMetrics {
            items: self.world.items.len(),
            ..TwinMetrics::default()
        };

        for &step in self.world.steps() {
            let items = self.world.items_in(step);
            if items.is_empty() {
                continue;
            }
            let step_start_time = lane_free.iter().max().copied().unwrap_or(0);
            let step_event = state.journal.record(
                EventDraft::new(RunEventKind::SuperStepStart, Effect::Pure).input(
                    serde_json::json!({
                        "step": step,
                        "active_nodes": items
                            .iter()
                            .map(|item| item.node_id.clone().unwrap_or_else(|| format!("seq:{}", item.recorded_seq)))
                            .collect::<Vec<_>>(),
                    }),
                ),
            );

            // The concurrency decision for this step: how many admissions
            // the policy allows at once, bounded above by the pool (a
            // learned limit may only narrow what the pool admits).
            let mut features = Map::new();
            features.insert("step".to_owned(), Value::from(step));
            features.insert("queue_depth".to_owned(), Value::from(items.len()));
            features.insert("workers".to_owned(), Value::from(config.workers.len()));
            let legal: Vec<DecisionAction> = config
                .concurrency_ladder
                .iter()
                .map(|limit| DecisionAction::SetConcurrency { limit: *limit })
                .collect();
            let action = decide(
                &mut state,
                fork,
                &mut acting,
                config,
                DecisionFamily::Concurrency,
                features,
                legal,
                &step_event,
            )?;
            let admitted = match action {
                DecisionAction::SetConcurrency { limit } => {
                    (limit as usize).min(config.workers.len()).max(1)
                }
                _ => 1,
            };
            flush_pending(&mut state);

            // Mechanism 2: the execution order of the parallel task set.
            // What the order changes is which item waits behind which on
            // the admitted lanes; the journaled order stays canonical
            // because each item's drafts are buffered and flushed in
            // recorded order after the step executes.
            let mut order: Vec<usize> = (0..items.len()).collect();
            if config.interleaving == Interleaving::Seeded {
                for i in (1..order.len()).rev() {
                    let j = (state.rng.next_f64() * (i + 1) as f64) as usize;
                    order.swap(i, j.min(i));
                }
            }

            let mut step_outcomes: BTreeMap<String, &str> = BTreeMap::new();
            let mut buffers: Vec<Vec<EventDraft>> = vec![Vec::new(); items.len()];
            for &position in &order {
                let item = items[position];
                let key = format!(
                    "{}#{}",
                    item.node_id.as_deref().unwrap_or("seq"),
                    item.recorded_seq
                );
                let outcome = run_item(
                    &mut state,
                    fork,
                    &mut acting,
                    config,
                    item,
                    admitted,
                    &mut lane_free,
                    &step_event,
                    step_start_time,
                )?;
                buffers[position] = std::mem::take(&mut state.pending);
                metrics.attempts += outcome.attempts;
                metrics.cost_usd += outcome.cost_usd;
                item_elapsed.push(outcome.elapsed_ms);
                let disposition = match outcome.terminal() {
                    Terminal::Completed => {
                        metrics.completed += 1;
                        "completed"
                    }
                    Terminal::DeadLettered => {
                        metrics.dead_lettered += 1;
                        "dead_lettered"
                    }
                    Terminal::Failed => {
                        metrics.failed += 1;
                        "failed"
                    }
                };
                step_outcomes.insert(key, disposition);
            }

            // Canonical flush: the journal's total order of evidence is
            // stable across interleavings — execution order only moved the
            // timing attributes (admission waits, latencies, timestamps).
            for buffer in &mut buffers {
                for draft in buffer.drain(..) {
                    state.journal.record(draft);
                }
            }

            // The step's end evidence is the per-item dispositions — the
            // twin's analogue of the recorded journal's channel values, and
            // what BranchDiff's step diffs read.
            state.journal.record(
                EventDraft::new(RunEventKind::SuperStepEnd, Effect::Pure)
                    .output(serde_json::to_value(&step_outcomes)?)
                    .parent(step_event),
            );
        }

        metrics.wall_time_ms = lane_free.iter().max().copied().unwrap_or(0);
        metrics.item_latency_p50_ms = percentile(&item_elapsed, 50.0);
        metrics.item_latency_p95_ms = percentile(&item_elapsed, 95.0);

        if let Some(fork) = fork {
            if !state.forked {
                let case = UnevaluableCase::UnknownDecision {
                    decision_seq: fork.decision_seq,
                };
                return Err(twin_error(format!(
                    "counterfactual refused: the run never reaches decision d{} — {}",
                    fork.decision_seq,
                    serde_json::to_string(&case).unwrap_or_else(|_| format!("{case:?}"))
                )));
            }
        }

        let journal = state.journal.snapshot();
        let report = TwinReport {
            run_id: state.run_id.clone(),
            seed: config.seed,
            faults_declared: config.faults.injections.len(),
            faults_fired: state.faults_fired,
            decisions: state.next_decision_seq as usize,
            evaluable_decisions: state.next_decision_seq as usize,
            shadow_divergences: state.divergences,
            excluded: state.excluded,
            bound: TWIN_REPORT_BOUND.to_owned(),
        };
        Ok(TwinOutcome {
            journal,
            decisions: state.decisions,
            metrics,
            report,
        })
    }
}

/// How a work item ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Terminal {
    Completed,
    DeadLettered,
    Failed,
}

/// What one work item cost the run.
#[derive(Debug, Default)]
struct ItemOutcome {
    terminal: Option<Terminal>,
    attempts: u64,
    elapsed_ms: u64,
    cost_usd: f64,
}

impl ItemOutcome {
    fn terminal(&self) -> Terminal {
        // Every path out of `run_item` sets the terminal state; defaulting
        // to `Failed` here would hide a machinery bug as evidence.
        self.terminal.expect("run_item always settles its item")
    }
}

/// Mutable per-run evidence state: the journal being written, the decision
/// stream, the current item's buffered drafts, and the counters the report
/// is built from.
struct RunState {
    journal: Journal,
    rng: RngSource,
    run_id: String,
    decisions: Vec<DecisionEvent>,
    /// Drafts of the item currently executing, flushed in canonical order
    /// at the step boundary (see `run_inner`); keeps the journaled total
    /// order stable under seeded interleavings.
    pending: Vec<EventDraft>,
    next_decision_seq: u64,
    faults_fired: usize,
    divergences: Vec<u64>,
    excluded: Vec<UnevaluableCase>,
    /// `true` once the fork decision has been taken (a fork that never
    /// fires is the `UnknownDecision` refusal).
    forked: bool,
}

/// Write every buffered draft to the journal in order.
fn flush_pending(state: &mut RunState) {
    for draft in std::mem::take(&mut state.pending) {
        state.journal.record(draft);
    }
}

/// One decision point: assemble the evidence, let the acting policy (and
/// the shadow) score the same features, validate both against the contract,
/// journal both, and return the action that executes.
///
/// Validation is the policy plane's enforcement at the twin boundary: a
/// selection outside `legal_actions` or a propensity outside `(0, 1]` is a
/// contract violation, rejected rather than journaled — dishonest evidence
/// is worse than no evidence.
#[allow(clippy::too_many_arguments)]
fn decide(
    state: &mut RunState,
    fork: Option<&CounterfactualFork>,
    acting: &mut Arc<dyn TwinPolicy>,
    config: &TwinRunConfig,
    family: DecisionFamily,
    features: Map<String, Value>,
    legal_actions: Vec<DecisionAction>,
    parent: &str,
) -> Result<DecisionAction> {
    let seq = state.next_decision_seq;
    let ctx = DecisionContext {
        family,
        features: &features,
        legal_actions: &legal_actions,
    };

    // Mechanism 3: the fork overrides exactly one decision. The override
    // must be legal *here* — the legal set is recomputed from the same
    // gates, so an input-changing or gate-crossing action is refused
    // against evidence, not against the experimenter's say-so.
    let at_fork = fork.is_some_and(|fork| fork.decision_seq == seq);
    let (selected, propensity, version) = if at_fork {
        let fork = fork.expect("checked above");
        if !legal_actions.contains(&fork.action) {
            let case = UnevaluableCase::IllegalForkAction {
                decision_seq: seq,
                action: fork.action.clone(),
            };
            return Err(twin_error(format!(
                "counterfactual refused: {:?} is not in the legal set at decision d{seq} \
                 ({legal_actions:?}) — {}",
                fork.action,
                serde_json::to_string(&case).unwrap_or_else(|_| format!("{case:?}"))
            )));
        }
        state.forked = true;
        (
            fork.action.clone(),
            1.0,
            PolicyVersion::new(TWIN_FORK_POLICY_VERSION),
        )
    } else {
        let (selected, propensity) = acting.decide(&ctx, state.rng.next_f64());
        validate_decision(&selected, propensity, &legal_actions, &acting.version())?;
        (selected, propensity, acting.version())
    };

    let decided_at = state.journal.clock().now();
    let acting_event = DecisionEvent {
        id: format!("{}:d{seq}", state.run_id),
        run_id: state.run_id.clone(),
        thread_id: state.journal.thread_id().to_owned(),
        seq,
        family,
        features: features.clone(),
        legal_actions: legal_actions.clone(),
        selected: selected.clone(),
        propensity,
        policy_version: version,
        role: Some(DecisionRole::Acting),
        outcome: None,
        decided_at,
    };
    journal_decision(state, &acting_event, parent);

    // Mechanism 4: the shadow scores the same features and journals its
    // decision with its true propensity; it never acts. Divergence from
    // the acting decision is recorded — those are the points a
    // counterfactual branch estimates the candidate's outcome from.
    if let Some(shadow) = &config.shadow {
        let (shadow_selected, shadow_propensity) = shadow.decide(&ctx, state.rng.next_f64());
        validate_decision(
            &shadow_selected,
            shadow_propensity,
            &legal_actions,
            &shadow.version(),
        )?;
        let shadow_event = DecisionEvent {
            id: format!("{}:d{seq}:shadow", state.run_id),
            selected: shadow_selected.clone(),
            propensity: shadow_propensity,
            policy_version: shadow.version(),
            role: Some(DecisionRole::Shadow),
            decided_at: state.journal.clock().now(),
            ..acting_event.clone()
        };
        journal_decision(state, &shadow_event, parent);
        if shadow_selected != selected {
            state.divergences.push(seq);
        }
    }

    state.next_decision_seq += 1;

    // Hybrid replay: after the fork decision, the new policy acts.
    if at_fork {
        if let Some(next) = fork.and_then(|fork| fork.then_act_with.as_ref()) {
            *acting = next.clone();
        }
    }
    Ok(selected)
}

/// The policy-plane contract, enforced where decisions are journaled.
fn validate_decision(
    selected: &DecisionAction,
    propensity: f64,
    legal_actions: &[DecisionAction],
    version: &PolicyVersion,
) -> Result<()> {
    if !legal_actions.contains(selected) {
        return Err(twin_error(format!(
            "policy `{}` selected {selected:?}, which is outside the legal set \
             {legal_actions:?} — a policy chooses among declared members, never free-form \
             outputs",
            version.as_str()
        )));
    }
    if !(0.0 < propensity && propensity <= 1.0) {
        return Err(twin_error(format!(
            "policy `{}` declared propensity {propensity}, outside (0, 1] — propensity is \
             assigned at decision time and must be truthful, or off-policy evaluation over \
             this evidence is meaningless",
            version.as_str()
        )));
    }
    Ok(())
}

/// Buffer one decision as a `PolicyDecision` draft (flushed in canonical
/// order at the step boundary) and keep it in the run's decision stream.
fn journal_decision(state: &mut RunState, event: &DecisionEvent, parent: &str) {
    state.pending.push(
        EventDraft::new(RunEventKind::PolicyDecision, Effect::Pure)
            .output(
                serde_json::to_value(event)
                    .unwrap_or_else(|_| serde_json::json!({"id": event.id, "error": "decision payload failed to serialize"})),
            )
            .parent(parent),
    );
    state.decisions.push(event.clone());
}

/// What one attempt was observed to do — after faults, timeout bounds, and
/// the recorded world's answer are composed. What the policy observes is
/// not always what the world did: a bound truncates the observation, the
/// truth stays.
struct AttemptObservation {
    status: EventStatus,
    class: Option<ErrorClass>,
    retry_after_ms: Option<u64>,
    /// Observed attempt latency, excluding admission wait.
    latency_ms: u64,
    output: Option<Value>,
    tokens: Option<Usage>,
    cost_usd: Option<f64>,
}

/// Drive one work item to its terminal state: attempts, faults, decision
/// points, and the retry loop, against the simulated worker lanes.
#[allow(clippy::too_many_arguments)]
fn run_item(
    state: &mut RunState,
    fork: Option<&CounterfactualFork>,
    acting: &mut Arc<dyn TwinPolicy>,
    config: &TwinRunConfig,
    item: &TwinWorkItem,
    admitted: usize,
    lane_free: &mut [u64],
    step_event: &str,
    step_start_time: u64,
) -> Result<ItemOutcome> {
    let mut outcome = ItemOutcome::default();
    let item_ready = step_start_time;
    let mut prev_attempt_end = item_ready;
    let mut attempt = 0u32;

    loop {
        attempt += 1;
        outcome.attempts += 1;

        // Placement: which equivalent worker this attempt runs on. Decided
        // per attempt — a retry may re-place, which is exactly what the
        // family would learn.
        let mut features = Map::new();
        features.insert("attempt".to_owned(), Value::from(attempt));
        features.insert("effect".to_owned(), serde_json::to_value(item.effect)?);
        features.insert(
            "lane_free_ms".to_owned(),
            serde_json::to_value(&*lane_free)?,
        );
        if let Some(node) = &item.node_id {
            features.insert("node".to_owned(), Value::from(node.clone()));
        }
        let legal: Vec<DecisionAction> = config
            .workers
            .iter()
            .map(|worker| DecisionAction::SelectWorker {
                worker: worker.clone(),
            })
            .collect();
        let action = decide(
            state,
            fork,
            acting,
            config,
            DecisionFamily::WorkerPlacement,
            features,
            legal,
            step_event,
        )?;
        let worker = match &action {
            DecisionAction::SelectWorker { worker } => worker.clone(),
            _ => config.workers[0].clone(),
        };
        let lane = config
            .workers
            .iter()
            .position(|w| w == &worker)
            .unwrap_or(0)
            // The concurrency cap admits only so many lanes at once; a
            // placement onto a lane beyond the cap queues behind it.
            .min(admitted.saturating_sub(1))
            .min(lane_free.len() - 1);

        // Timeout: the bound this attempt runs under.
        let mut features = Map::new();
        features.insert(
            "recorded_latency_ms".to_owned(),
            Value::from(item.answer.latency_ms),
        );
        features.insert("attempt".to_owned(), Value::from(attempt));
        features.insert("effect".to_owned(), serde_json::to_value(item.effect)?);
        if let Some(node) = &item.node_id {
            features.insert("node".to_owned(), Value::from(node.clone()));
        }
        let legal: Vec<DecisionAction> = config
            .timeout_ladder
            .iter()
            .map(|millis| DecisionAction::SetTimeout { millis: *millis })
            .collect();
        let action = decide(
            state,
            fork,
            acting,
            config,
            DecisionFamily::Timeout,
            features,
            legal,
            step_event,
        )?;
        let bound = match action {
            DecisionAction::SetTimeout { millis } => millis,
            _ => MAX_RETRY_DELAY_MS,
        };

        // The effect boundary: the fault schedule is consulted the way the
        // production scheduler reads the world — first matching anchor
        // wins, and a fired fault replaces the recorded answer.
        let fault = config
            .faults
            .fault_for_attempt(item.recorded_seq, attempt, &worker)
            .cloned();
        let mut observation = match &fault {
            Some(fault) => {
                state.faults_fired += 1;
                faulted_observation(fault, bound, item)
            }
            None => recorded_observation(item, bound),
        };

        // A crash at the decision point: the scheduler's fail-task path
        // observes the in-flight attempt through the injected fault (lease
        // expiry classifies `Unknown`). Consulted only where the production
        // scheduler would see a lost attempt — a retry decision about to be
        // made.
        if observation.status == EventStatus::Error {
            if let Some(at_decision) = config
                .faults
                .fault_for_decision(state.next_decision_seq)
                .cloned()
            {
                state.faults_fired += 1;
                observation = faulted_observation(&at_decision, bound, item);
            }
        }

        // Admission on the lane: the attempt starts when the lane frees and
        // any retry delay has elapsed; the journaled latency is what the
        // run experiences — the wait plus the observed attempt.
        let ready = if attempt == 1 {
            item_ready
        } else {
            prev_attempt_end
        };
        let start = ready.max(lane_free[lane]);
        let wait = start.saturating_sub(ready);
        let end = start + observation.latency_ms;
        lane_free[lane] = end;
        let journaled_latency = wait + observation.latency_ms;

        // Buffer the attempt as the recorded kind with the observed
        // outcome (flushed in canonical order at the step boundary).
        // Retried attempts re-issue the same bytes — the twin never
        // rewrites an effect's input, which is the honest edge made
        // structural.
        let mut draft = EventDraft::new(item.kind, item.effect)
            .input(item.request.clone())
            .latency_ms(journaled_latency)
            .status(observation.status)
            .parent(step_event);
        if let Some(node) = &item.node_id {
            draft = draft.node(node.clone());
        }
        match observation.status {
            EventStatus::Ok => {
                if let Some(output) = &observation.output {
                    draft = draft.output(output.clone());
                }
                if let Some(tokens) = observation.tokens {
                    draft = draft.tokens(tokens);
                }
            }
            EventStatus::Error => {
                let class = observation.class.unwrap_or(ErrorClass::Unknown);
                draft = draft.output(serde_json::json!({
                    "error": serde_json::to_value(class)?,
                }));
            }
            EventStatus::Interrupted => unreachable!("servable calls never interrupt"),
        }
        if let Some(cost) = observation.cost_usd {
            draft = draft.cost_usd(cost);
            outcome.cost_usd += cost;
        }
        state.pending.push(draft);
        prev_attempt_end = end;

        if observation.status == EventStatus::Ok {
            outcome.terminal = Some(Terminal::Completed);
            outcome.elapsed_ms = end.saturating_sub(item_ready);
            return Ok(outcome);
        }

        // The retry decision: the scheduler's fail-task path. The legal set
        // comes from the runtime's own gates — effect gate, class gate,
        // attempt budget — so no policy, learned or otherwise, can route
        // around them.
        let class = observation.class.unwrap_or(ErrorClass::Unknown);
        if fault.is_some() && !item.effect.is_freely_repeatable() {
            // The fault's future is the gate's, not the policy's: flag it
            // so the report names what bounded the evaluation.
            let flag = UnevaluableCase::GatedEffect {
                effect_seq: item.recorded_seq,
            };
            if !state.excluded.contains(&flag) {
                state.excluded.push(flag);
            }
        }
        let mut features = Map::new();
        features.insert("failure_class".to_owned(), serde_json::to_value(class)?);
        features.insert("attempt".to_owned(), Value::from(attempt));
        features.insert("max_attempts".to_owned(), Value::from(config.max_attempts));
        features.insert("effect".to_owned(), serde_json::to_value(item.effect)?);
        features.insert(
            "dependency_latency_ms".to_owned(),
            Value::from(journaled_latency),
        );
        if let Some(retry_after) = observation.retry_after_ms {
            features.insert("retry_after_ms".to_owned(), Value::from(retry_after));
        }
        let legal = retry_legal_actions(item.effect, class, attempt, config.max_attempts);
        // Dead-letter is the retryable-budget-exhausted case: the legal set
        // collapses to Abort while the gates would still have allowed a
        // retry had budget remained.
        let budget_exhausted = item.effect.is_freely_repeatable()
            && class.is_retryable()
            && attempt >= config.max_attempts;
        let action = decide(
            state,
            fork,
            acting,
            config,
            DecisionFamily::Retry,
            features,
            legal,
            step_event,
        )?;
        match action {
            DecisionAction::Retry { .. } => {
                // The delay is policy-parameterized within declared bounds,
                // floored by the callee's Retry-After — the one delay the
                // world imposes.
                let delay = acting
                    .retry_delay_ms(attempt, state.rng.next_f64())
                    .max(observation.retry_after_ms.unwrap_or(0));
                lane_free[lane] = lane_free[lane].max(prev_attempt_end + delay);
                prev_attempt_end += delay;
            }
            _ => {
                outcome.terminal = Some(if budget_exhausted {
                    Terminal::DeadLettered
                } else {
                    Terminal::Failed
                });
                outcome.elapsed_ms = end.saturating_sub(item_ready);
                return Ok(outcome);
            }
        }
    }
}

/// The recorded world's answer, observed through the timeout bound: a
/// completing attempt slower than its bound is observed as `Timeout` at the
/// bound — the policy's observation changes, the world's truth does not.
fn recorded_observation(item: &TwinWorkItem, bound: u64) -> AttemptObservation {
    match item.answer.status {
        EventStatus::Ok if item.answer.latency_ms > bound => AttemptObservation {
            status: EventStatus::Error,
            class: Some(ErrorClass::Timeout),
            retry_after_ms: None,
            latency_ms: bound,
            output: None,
            tokens: None,
            cost_usd: item.answer.cost_usd,
        },
        _ => AttemptObservation {
            status: item.answer.status,
            class: (item.answer.status == EventStatus::Error).then_some(ErrorClass::Unknown),
            retry_after_ms: None,
            latency_ms: item.answer.latency_ms,
            output: item.answer.output.clone(),
            tokens: item.answer.tokens,
            cost_usd: item.answer.cost_usd,
        },
    }
}

/// The injected fault as an observation. Latencies follow the world's
/// rules, not convenience: a crash is discovered at the lease boundary, a
/// hang surfaces at the tighter of the bound and the boundary, and
/// fail-fast faults answer at the recorded latency (the attempt ran, then
/// the world said no).
fn faulted_observation(
    fault: &InjectedFault,
    bound: u64,
    item: &TwinWorkItem,
) -> AttemptObservation {
    let (latency_ms, retry_after_ms) = match fault {
        InjectedFault::WorkerCrash => (MAX_RETRY_DELAY_MS, None),
        InjectedFault::CalleeTimeout => (bound.min(MAX_RETRY_DELAY_MS), None),
        InjectedFault::RateLimited { retry_after_ms } => {
            (item.answer.latency_ms, Some(*retry_after_ms))
        }
        InjectedFault::ResourceExhausted => (item.answer.latency_ms, None),
    };
    AttemptObservation {
        status: EventStatus::Error,
        class: Some(fault.error_class()),
        retry_after_ms,
        latency_ms,
        output: None,
        tokens: None,
        // Unpriced by the recording, so unpriced here: the attempt count
        // is the honest cost proxy, the same accounting Wave 1's
        // durable-work class uses.
        cost_usd: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx<'a>(
        family: DecisionFamily,
        features: &'a Map<String, Value>,
        legal: &'a [DecisionAction],
    ) -> DecisionContext<'a> {
        DecisionContext {
            family,
            features,
            legal_actions: legal,
        }
    }

    #[test]
    fn fault_schedule_matches_in_declaration_order() {
        let schedule = FaultSchedule::new(7)
            .with_injection(
                FaultAnchor::Window {
                    from_seq: 0,
                    to_seq: 10,
                },
                InjectedFault::ResourceExhausted,
            )
            .with_injection(
                FaultAnchor::OnAttempt {
                    effect_seq: 3,
                    attempt: 1,
                },
                InjectedFault::RateLimited {
                    retry_after_ms: 500,
                },
            )
            .with_injection(
                FaultAnchor::OnWorker {
                    worker: "w1".to_owned(),
                },
                InjectedFault::WorkerCrash,
            );

        // The window is declared first and wins for seq 3 despite the more
        // specific OnAttempt entry — declaration order is the precedence
        // rule, and it is deterministic.
        assert_eq!(
            schedule.fault_for_attempt(3, 1, "w0"),
            Some(&InjectedFault::ResourceExhausted)
        );
        // Outside the window: the specific anchors answer.
        assert_eq!(
            schedule.fault_for_attempt(11, 1, "w1"),
            Some(&InjectedFault::WorkerCrash)
        );
        assert_eq!(schedule.fault_for_attempt(11, 1, "w0"), None);
        // AtDecision anchors answer only the decision query.
        assert_eq!(schedule.fault_for_decision(0), None);
        let schedule = schedule.with_injection(
            FaultAnchor::AtDecision { decision_seq: 2 },
            InjectedFault::WorkerCrash,
        );
        assert_eq!(
            schedule.fault_for_decision(2),
            Some(&InjectedFault::WorkerCrash)
        );
        // Outside the window and off the degraded worker: nothing fires.
        assert_eq!(schedule.fault_for_attempt(11, 2, "w0"), None);
    }

    #[test]
    fn injected_faults_classify_into_the_retry_taxonomy() {
        assert_eq!(
            InjectedFault::WorkerCrash.error_class(),
            ErrorClass::Unknown
        );
        assert_eq!(
            InjectedFault::CalleeTimeout.error_class(),
            ErrorClass::Timeout
        );
        assert_eq!(
            InjectedFault::RateLimited { retry_after_ms: 50 }.error_class(),
            ErrorClass::RateLimited
        );
        assert_eq!(
            InjectedFault::ResourceExhausted.error_class(),
            ErrorClass::ResourceExhausted
        );
    }

    #[test]
    fn floor_decides_the_static_v0_stance_per_family() {
        let features = Map::new();
        let floor = StaticFloor;

        // Retry: retries while the gates leave a Retry member legal.
        let legal = vec![DecisionAction::Retry { attempt: 2 }, DecisionAction::Abort];
        let (action, propensity) =
            floor.decide(&ctx(DecisionFamily::Retry, &features, &legal), 0.5);
        assert_eq!(action, DecisionAction::Retry { attempt: 2 });
        assert_eq!(propensity, 1.0);

        // Gates closed: only Abort remains.
        let legal = vec![DecisionAction::Abort];
        let (action, _) = floor.decide(&ctx(DecisionFamily::Retry, &features, &legal), 0.5);
        assert_eq!(action, DecisionAction::Abort);

        // Timeout: the top rung — "no bound in force" modeled as the lease
        // boundary, matching Wave 1's accounting.
        let legal: Vec<DecisionAction> = DEFAULT_TIMEOUT_LADDER
            .iter()
            .map(|millis| DecisionAction::SetTimeout { millis: *millis })
            .collect();
        let (action, _) = floor.decide(&ctx(DecisionFamily::Timeout, &features, &legal), 0.5);
        assert_eq!(
            action,
            DecisionAction::SetTimeout {
                millis: MAX_RETRY_DELAY_MS
            }
        );

        // Concurrency: uncapped. Placement: the first eligible worker (the
        // static-pool stance).
        let legal = vec![
            DecisionAction::SetConcurrency { limit: 1 },
            DecisionAction::SetConcurrency { limit: u32::MAX },
        ];
        let (action, _) = floor.decide(&ctx(DecisionFamily::Concurrency, &features, &legal), 0.5);
        assert_eq!(action, DecisionAction::SetConcurrency { limit: u32::MAX });
        let legal = vec![
            DecisionAction::SelectWorker {
                worker: "w0".to_owned(),
            },
            DecisionAction::SelectWorker {
                worker: "w1".to_owned(),
            },
        ];
        let (action, _) = floor.decide(
            &ctx(DecisionFamily::WorkerPlacement, &features, &legal),
            0.5,
        );
        assert_eq!(
            action,
            DecisionAction::SelectWorker {
                worker: "w0".to_owned()
            }
        );
    }

    #[test]
    fn validate_decision_rejects_illegal_actions_and_bad_propensities() {
        let legal = vec![DecisionAction::Abort];
        let version = PolicyVersion::new("policy-test");
        assert!(validate_decision(&DecisionAction::Abort, 1.0, &legal, &version).is_ok());
        let error = validate_decision(&DecisionAction::Retry { attempt: 2 }, 1.0, &legal, &version)
            .unwrap_err();
        assert!(error.to_string().contains("outside the legal set"));
        for bad in [0.0, -0.5, 1.5] {
            assert!(validate_decision(&DecisionAction::Abort, bad, &legal, &version).is_err());
        }
    }

    #[test]
    fn timeout_bound_truncates_the_observation_not_the_world() {
        let item = TwinWorkItem {
            recorded_seq: 0,
            kind: RunEventKind::ToolCall,
            node_id: Some("n".to_owned()),
            effect: Effect::Idempotent,
            request: json!({"tool": "t", "arguments": {}}),
            answer: RecordedAnswer {
                status: EventStatus::Ok,
                output: Some(json!({"r": 1})),
                latency_ms: 10_000,
                tokens: None,
                cost_usd: Some(0.01),
            },
            step: 0,
        };
        // Slower than the bound: observed as Timeout at the bound.
        let observed = recorded_observation(&item, 5_000);
        assert_eq!(observed.status, EventStatus::Error);
        assert_eq!(observed.class, Some(ErrorClass::Timeout));
        assert_eq!(observed.latency_ms, 5_000);
        // Within the bound: the recorded answer, untouched.
        let observed = recorded_observation(&item, 30_000);
        assert_eq!(observed.status, EventStatus::Ok);
        assert_eq!(observed.latency_ms, 10_000);
        assert_eq!(observed.output, Some(json!({"r": 1})));
    }

    #[test]
    fn faulted_observations_follow_the_worlds_latency_rules() {
        let item = TwinWorkItem {
            recorded_seq: 0,
            kind: RunEventKind::ToolCall,
            node_id: None,
            effect: Effect::Idempotent,
            request: json!({}),
            answer: RecordedAnswer {
                status: EventStatus::Ok,
                output: None,
                latency_ms: 2_000,
                tokens: None,
                cost_usd: None,
            },
            step: 0,
        };
        // A crash is discovered at the lease boundary.
        let crash = faulted_observation(&InjectedFault::WorkerCrash, 5_000, &item);
        assert_eq!(crash.latency_ms, MAX_RETRY_DELAY_MS);
        // A hang surfaces at the tighter of bound and boundary.
        let hang = faulted_observation(&InjectedFault::CalleeTimeout, 5_000, &item);
        assert_eq!(hang.latency_ms, 5_000);
        let hang = faulted_observation(&InjectedFault::CalleeTimeout, MAX_RETRY_DELAY_MS, &item);
        assert_eq!(hang.latency_ms, MAX_RETRY_DELAY_MS);
        // Rate limits carry their Retry-After floor.
        let limited = faulted_observation(
            &InjectedFault::RateLimited {
                retry_after_ms: 800,
            },
            5_000,
            &item,
        );
        assert_eq!(limited.retry_after_ms, Some(800));
        assert_eq!(limited.latency_ms, 2_000);
    }

    #[test]
    fn percentile_is_nearest_rank() {
        assert_eq!(percentile(&[], 50.0), 0);
        assert_eq!(percentile(&[10], 95.0), 10);
        assert_eq!(percentile(&[1, 2, 3, 4], 50.0), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 95.0), 4);
    }
}
