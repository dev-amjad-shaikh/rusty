//! Durable Work contracts (R0.6): the shared retry taxonomy and task envelope.
//!
//! This module freezes the wire shapes that the R0.6 durable-activity system
//! builds on — the Postgres task queue in `rusty-server`, the worker SDK in
//! `rusty-worker`, and the scheduler between them. Nothing here performs I/O
//! or scheduling; these are pure data contracts plus the one policy function
//! both sides must agree on ([`classify_retry`]).
//!
//! The two pillars:
//!
//! - [`ErrorClass`] + [`RetryDecision`] — the retry taxonomy. Every failed
//!   task attempt is classified into a closed set of error classes; the
//!   class, the declared [`Effect`] of the work, and the attempt ordinal map
//!   to exactly one decision: retry after a delay, dead-letter, or fail.
//! - [`TaskEnvelope`] — the unit of durable work: who sent it, who executes
//!   it, the input reference, the artifact contract, the deadline, the
//!   budget, and the idempotency key that makes retry *effective-once*
//!   rather than duplicated.
//!
//! The composition rule that makes retry safe comes from the Flight
//! Recorder: [`classify_retry`] refuses to silently retry any effect that is
//! not [`Effect::is_freely_repeatable`], so an `Idempotent` declaration —
//! with a stable idempotency key — is what unlocks automatic retry.
//!
//! Golden-file tests under `tests/golden/` pin the serialized shapes; any
//! accidental contract drift fails CI. To bless an intentional change,
//! re-run with `UPDATE_GOLDEN=1` and review the diff.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::record::{
    DecisionAction, DecisionEvent, DecisionFamily, DecisionOutcome, Effect, PayloadRef,
    PolicyVersion,
};

/// The current wire-format version of [`TaskEnvelope`].
///
/// Bump only on a breaking change to the envelope; additive evolution uses
/// serde defaults instead so previously queued tasks keep deserializing.
pub const TASK_ENVELOPE_FORMAT_VERSION: u32 = 1;

/// The base delay of the retry backoff schedule, in milliseconds.
///
/// Retry `n` (1-based) draws a delay uniformly from
/// `[0, BASE_RETRY_DELAY_MS * 2^(n-1)]`, capped at [`MAX_RETRY_DELAY_MS`].
pub const BASE_RETRY_DELAY_MS: u64 = 1_000;

/// The cap of the retry backoff schedule, in milliseconds (5 minutes).
///
/// The cap bounds worst-case queue latency for a stuck dependency while
/// keeping retry pressure negligible: at the cap, one task retries at most
/// 12 times per hour.
pub const MAX_RETRY_DELAY_MS: u64 = 300_000;

/// Why a task attempt failed. Closed set; the scheduler matches exhaustively
/// on it, and new classes are additive (old senders still deserialize).
///
/// The class is declared by the executor of the work — the worker that ran
/// the handler or the transport that carried it — not inferred from logs.
/// Each variant documents its retry semantics; the mechanical mapping lives
/// in [`classify_retry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// A transient fault with no lasting cause: connection reset, broken
    /// pipe, a dependency's internal hiccup. Retry with backoff; the same
    /// request is expected to succeed later.
    Transient,

    /// The callee asked to be slowed down (HTTP 429, `Retry-After`).
    /// Retry with backoff; a callee-supplied `Retry-After` value floors the
    /// delay (applied by the scheduler, outside this contract).
    RateLimited,

    /// The attempt did not finish within its deadline. Retry with backoff,
    /// but note the ambiguity: the work may have partially executed, which
    /// is exactly why the [`Effect`] gate in [`classify_retry`] exists — a
    /// timed-out non-idempotent effect must not be re-attempted silently.
    Timeout,

    /// The input is malformed or violates the callee's contract (HTTP 400,
    /// schema validation). Never retried: the same bytes will fail the same
    /// way on every attempt. Fails the task immediately.
    InvalidInput,

    /// An upstream dependency the task needs is down or degraded (database
    /// unreachable, model endpoint 5xx). Retry with backoff; distinct from
    /// [`ErrorClass::Transient`] so operators can distinguish "our wiring"
    /// from "their outage" in telemetry and alerting.
    DependencyFailure,

    /// The worker or the callee is out of capacity: memory pressure,
    /// connection-pool exhaustion, quota exhaustion. Retry with backoff —
    /// ideally on a different worker (placement is the scheduler's concern;
    /// this contract only carries the classification).
    ResourceExhausted,

    /// The attempt ended because the task was cancelled. Control flow, not
    /// a failure: never retried, never dead-lettered. Cancellation
    /// propagation (wave 2) classifies interrupted attempts this way so the
    /// retry machinery stays out of the cancellation path.
    Cancelled,

    /// The failure could not be classified — the handler returned an
    /// unclassified error, or the worker died mid-attempt (lease expiry
    /// classifies here). Retried with backoff up to the attempt limit, then
    /// dead-lettered: unknown failures are the DLQ's primary input, since
    /// they are the ones that need human eyes.
    Unknown,
}

impl ErrorClass {
    /// Whether this class is retryable at all. `InvalidInput` and
    /// `Cancelled` are the only non-retryable classes; everything else —
    /// including `Unknown` — may be re-attempted up to the attempt limit.
    ///
    /// Retryability of the class is necessary but not sufficient for a
    /// retry: [`classify_retry`] additionally gates on the work's declared
    /// [`Effect`].
    pub fn is_retryable(self) -> bool {
        !matches!(self, ErrorClass::InvalidInput | ErrorClass::Cancelled)
    }
}

/// What the scheduler does with a failed task attempt.
///
/// Serialized with internal tagging (`{"decision": "retry", "after_ms": …}`)
/// so a decision recorded in the journal or the queue's attempt log is
/// self-describing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum RetryDecision {
    /// Re-queue the task to become visible again after `after_ms`
    /// milliseconds. The idempotency key and input are unchanged; the
    /// attempt counter increments.
    Retry {
        /// Delay before the task becomes visible to workers again, in
        /// milliseconds. Computed by [`backoff_delay_ms`].
        after_ms: u64,
    },

    /// Move the task to the dead-letter queue: a retryable failure class
    /// whose attempts are exhausted, or an `Unknown` failure that kept
    /// recurring. DLQ entries are operator-visible evidence — they carry the
    /// full attempt history and can be re-driven by hand after the cause is
    /// fixed.
    Dead,

    /// Fail the task immediately: a non-retryable class, or any failure of
    /// work whose declared [`Effect`] is not freely repeatable. The error
    /// surfaces to the sender; nothing is retried and nothing is
    /// dead-lettered (there is nothing a human can fix by re-driving the
    /// same input).
    Fail,
}

/// Full-jitter exponential backoff, in milliseconds.
///
/// Retry `attempt` (1-based — `1` is the first retry after the initial
/// failure) draws uniformly from `[0, base * 2^(attempt-1)]` where `base` is
/// [`BASE_RETRY_DELAY_MS`], capped at [`MAX_RETRY_DELAY_MS`]. Full jitter —
/// rather than a fixed fraction of the exponential — is what decorrelates a
/// fleet of tasks that failed together when a shared dependency recovers
/// (the thundering-herd problem); see the design doc for the citation.
///
/// `uniform` is a sample from `[0, 1)`; values outside are clamped. Source
/// it from the run's `RngSource` so seeded runs reproduce their retry
/// schedules exactly.
pub fn backoff_delay_ms(attempt: u32, uniform: f64) -> u64 {
    let exponent = attempt.saturating_sub(1).min(20);
    let exponential = BASE_RETRY_DELAY_MS
        .saturating_mul(1u64 << exponent)
        .min(MAX_RETRY_DELAY_MS);
    (uniform.clamp(0.0, 1.0) * exponential as f64) as u64
}

/// Map a failed attempt to exactly one [`RetryDecision`].
///
/// This is the single place the retry policy lives, shared verbatim by the
/// server scheduler and the worker SDK so both sides of the queue always
/// agree. The order of the gates is the policy:
///
/// 1. **Effect gate.** Work whose declared [`Effect`] is not
///    [`Effect::is_freely_repeatable`] is never silently retried — a timed
///    out `NonIdempotent` charge may already have happened. This is the
///    composition with the Flight Recorder: the `Idempotent`
///    classification is what makes retry safe, and it is checked here,
///    not assumed.
/// 2. **Class gate.** `InvalidInput` and `Cancelled` fail immediately; all
///    other classes are retryable ([`ErrorClass::is_retryable`]).
/// 3. **Attempt gate.** A retryable failure with `attempt >= max_attempts`
///    goes to the dead-letter queue (`attempt` counts attempts made so far,
///    starting at 1 for the first failure; `max_attempts == 0` means no
///    retries at all).
/// 4. Otherwise: retry, with the delay from [`backoff_delay_ms`].
pub fn classify_retry(
    effect: Effect,
    class: ErrorClass,
    attempt: u32,
    max_attempts: u32,
    uniform: f64,
) -> RetryDecision {
    if !effect.is_freely_repeatable() || !class.is_retryable() {
        return RetryDecision::Fail;
    }
    if attempt >= max_attempts {
        return RetryDecision::Dead;
    }
    RetryDecision::Retry {
        after_ms: backoff_delay_ms(attempt, uniform),
    }
}

/// The closed legal-action set of a retry decision (R0.8 wave 4).
///
/// Mirrors [`classify_retry`]'s gates exactly: when a retry is legal
/// (freely-repeatable effect, retryable class, attempt budget remaining) the
/// legal set is `[Retry { attempt: attempt + 1 }, Abort]`; otherwise retrying
/// is not a legal action and the set collapses to `[Abort]`. The set is
/// computed from the same inputs as the decision itself, so a
/// [`DecisionEvent`] built by [`retry_decision_event`] can never record a
/// selected action outside its legal set.
pub fn retry_legal_actions(
    effect: Effect,
    class: ErrorClass,
    attempt: u32,
    max_attempts: u32,
) -> Vec<DecisionAction> {
    if effect.is_freely_repeatable() && class.is_retryable() && attempt < max_attempts {
        vec![
            DecisionAction::Retry {
                attempt: attempt + 1,
            },
            DecisionAction::Abort,
        ]
    } else {
        vec![DecisionAction::Abort]
    }
}

/// The [`DecisionAction`] a [`RetryDecision`] corresponds to, given the
/// 1-based attempt ordinal that failed.
///
/// A `Retry` decision takes retry ordinal `attempt + 1` (the failure was
/// attempt `attempt`; the decision schedules the next one). `Dead` and
/// `Fail` both map to `Abort`: dead-lettering is giving up on the automatic
/// path, and the dead-letter queue entry is the queue's own evidence, not a
/// decision action.
pub fn retry_selected_action(decision: &RetryDecision, attempt: u32) -> DecisionAction {
    match decision {
        RetryDecision::Retry { .. } => DecisionAction::Retry {
            attempt: attempt + 1,
        },
        RetryDecision::Dead | RetryDecision::Fail => DecisionAction::Abort,
    }
}

/// Build the [`DecisionEvent`] for one retry decision (R0.8 wave 4).
///
/// This is the emission contract between the scheduler (which owns the
/// decision inputs) and the journal: given exactly the values
/// [`classify_retry`] was called with plus its output, produce the evidence
/// record. Features pin the observation vocabulary — `failure_class`,
/// `attempt`, `max_attempts`, `effect`, and `dependency_latency_ms` when the
/// caller measured one — so offline evaluation reads stable keys.
///
/// **Propensity honesty.** Every v1 policy — including the static floor —
/// is deterministic: it assigns probability 1 to the selected action and 0
/// to every other legal action. The event therefore records `propensity:
/// 1.0`. That is the truthful value, but it also means v1's evidence cannot
/// support inverse-propensity scoring (division by a zero propensity is
/// undefined): off-policy evaluation over v1 decisions is restricted to
/// policies that would have taken the same action. Learned stochastic
/// policies must record their true propensity at decision time; this
/// function's `1.0` is the deterministic-policy degenerate case, not a
/// placeholder.
///
/// `outcome` is `None` for a `Retry` decision (the re-attempt has not
/// happened yet) and `Some(DecisionOutcome::Failure)` for `Dead`/`Fail`
/// (the operation is over and did not complete).
#[allow(clippy::too_many_arguments)]
pub fn retry_decision_event(
    run_id: impl Into<String>,
    thread_id: impl Into<String>,
    seq: u64,
    effect: Effect,
    class: ErrorClass,
    attempt: u32,
    max_attempts: u32,
    dependency_latency_ms: Option<u64>,
    decision: &RetryDecision,
    policy_version: &PolicyVersion,
    decided_at: DateTime<Utc>,
) -> DecisionEvent {
    let run_id = run_id.into();
    let mut features = Map::new();
    features.insert(
        "failure_class".to_owned(),
        serde_json::to_value(class).unwrap_or(Value::Null),
    );
    features.insert("attempt".to_owned(), Value::from(attempt));
    features.insert("max_attempts".to_owned(), Value::from(max_attempts));
    features.insert(
        "effect".to_owned(),
        serde_json::to_value(effect).unwrap_or(Value::Null),
    );
    if let Some(latency) = dependency_latency_ms {
        features.insert("dependency_latency_ms".to_owned(), Value::from(latency));
    }
    let outcome = match decision {
        RetryDecision::Retry { .. } => None,
        RetryDecision::Dead | RetryDecision::Fail => Some(DecisionOutcome::Failure),
    };
    DecisionEvent {
        id: format!("{run_id}:d{seq}"),
        run_id,
        thread_id: thread_id.into(),
        seq,
        family: DecisionFamily::Retry,
        features,
        legal_actions: retry_legal_actions(effect, class, attempt, max_attempts),
        selected: retry_selected_action(decision, attempt),
        propensity: 1.0,
        policy_version: policy_version.clone(),
        outcome,
        decided_at,
    }
}

/// The artifact contract of a [`TaskEnvelope`]: what the recipient is
/// expected to produce and the sender to consume.
///
/// v1 carries a kind identifier and an optional size bound only — enough
/// for the recipient to know it is producing the right *shape* of artifact
/// and for the queue to refuse to store an out-of-contract result. R0.7
/// (Agent Fabric wave 1) adds the optional `schema` field — additive with a
/// serde default, omitted from the wire when unset, so pre-R0.7 contracts
/// and readers see no shape change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactContract {
    /// Artifact kind identifier — a media type (`application/json`), a
    /// schema URI, or an application-defined kind name. Interpreted by the
    /// sender and recipient; the queue treats it as opaque.
    pub kind: String,

    /// Upper bound on the serialized artifact size in bytes. `None` means
    /// unbounded within the queue's own storage limits.
    #[serde(default)]
    pub max_bytes: Option<u64>,

    /// Optional JSON Schema (draft 2020-12, per the design's open-questions
    /// default) the artifact payload must validate against (R0.7). Declared
    /// on the contract so an unacceptable message can fail fast as
    /// [`ErrorClass::InvalidInput`] at submission — never retried — instead
    /// of dead-lettering after the attempt budget is spent. `None` means
    /// kind-and-size checking only, exactly the pre-R0.7 semantics.
    ///
    /// Wave 1 pins the field's *shape* (stored and golden-tested); payload
    /// validation against it is wired at the mailbox submission path in a
    /// later wave — the schema dialect constraint (design open question 5)
    /// settles first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<serde_json::Value>,
}

/// The budget a task may consume across all of its attempts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBudget {
    /// Maximum number of attempts, counting the initial one. Once reached,
    /// a retryable failure dead-letters instead of retrying (see
    /// [`classify_retry`]).
    pub max_attempts: u32,

    /// Per-attempt wall-clock timeout in milliseconds. An attempt that
    /// exceeds it is classified [`ErrorClass::Timeout`] by the worker.
    /// `None` defers to the queue's default.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// The unit of durable work: one task handed from a sender to a recipient
/// through the queue.
///
/// The envelope is everything the scheduler and the recipient need and
/// nothing they don't — the input travels as a [`PayloadRef`] (inline for
/// small values, content-addressed above the inline threshold), so the
/// queue row stays cheap to scan and the envelope composes with the Flight
/// Recorder's artifact addressing.
///
/// Every field after `format_version` that was added after v1 carries a
/// serde default, so envelopes queued by an older client keep deserializing;
/// the default [`Effect`] is the conservative `NonIdempotent`, meaning an
/// undeclared task is never silently retried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEnvelope {
    /// Envelope format version; [`TASK_ENVELOPE_FORMAT_VERSION`] for
    /// anything written now.
    #[serde(default = "current_envelope_version")]
    pub format_version: u32,

    /// Unique task id, assigned by the sender. Server-side ids are
    /// tenant-namespaced like every other id (`{tenant}/…`); the envelope
    /// itself is namespace-agnostic.
    pub task_id: String,

    /// The causal parent: the run event or task that created this one
    /// (`{run_id}:{seq}` or a task id). Task lineage composes with the
    /// journal's causal chain, so a fan-out of durable tasks is one
    /// connected evidence tree. `None` for root tasks.
    #[serde(default)]
    pub parent: Option<String>,

    /// Who submitted the task — a run id, a node name, or an agent
    /// identity. Used for attribution, quotas, and cancellation scoping.
    pub sender: String,

    /// Who executes the task: a worker pool name (the queue routes by pool)
    /// or a specific worker identity for pinned placement.
    pub recipient: String,

    /// Version pin (R0.6 wave 3): the exact worker version string this task
    /// may be dispatched to. A run started against worker version `w1`
    /// stamps its tasks with `w1`, so a mid-run deploy never changes
    /// semantics under an in-flight execution — the scheduler keeps handing
    /// the task to `w1`-advertising workers until it finishes.
    ///
    /// The pin is an *exact string match*, deliberately: semver range
    /// matching (`^1.4`) makes a claim's outcome depend on the version
    /// grammar's rules rather than on what the run actually recorded, and
    /// range resolution belongs to the scheduler's policy, not to a frozen
    /// wire contract. Ranges are documented future work; `None` (the
    /// default) means unpinned — any worker may claim the task.
    #[serde(default)]
    pub worker_version: Option<String>,

    /// The task input, inline or content-addressed. Small inputs travel
    /// inside the queue row; large ones are artifacts the recipient resolves
    /// through the journal's artifact map.
    pub input: PayloadRef,

    /// What the task is expected to produce. `None` means the result is an
    /// unconstrained JSON value (the common case for node executions).
    #[serde(default)]
    pub output_contract: Option<ArtifactContract>,

    /// Wall-clock deadline for the whole task, across all attempts. The
    /// scheduler does not re-queue a task whose deadline has passed; the
    /// worker treats an expired deadline as [`ErrorClass::Cancelled`].
    #[serde(default)]
    pub deadline: Option<DateTime<Utc>>,

    /// Attempt and timeout budget. `None` means the queue's defaults.
    #[serde(default)]
    pub budget: Option<TaskBudget>,

    /// The deduplication key for effectively-once execution: the queue
    /// refuses a duplicate submission with an existing key, and the
    /// recipient passes it to the effect it performs. Required for any task
    /// whose `effect` is [`Effect::Idempotent`] — the key is what the
    /// idempotency declaration *means* at the wire. `None` is honest only
    /// for `Pure` / `ReadOnly` work.
    #[serde(default)]
    pub idempotency_key: Option<String>,

    /// The declared effect classification of the work this task performs —
    /// the same taxonomy the Flight Recorder journals. Defaults to the
    /// conservative `NonIdempotent`, so a task that does not declare its
    /// effect is never silently retried; declaring `Idempotent` (with an
    /// idempotency key) is what unlocks automatic retry.
    #[serde(default = "default_effect")]
    pub effect: Effect,
}

fn current_envelope_version() -> u32 {
    TASK_ENVELOPE_FORMAT_VERSION
}

fn default_effect() -> Effect {
    Effect::NonIdempotent
}

impl TaskEnvelope {
    /// A minimal envelope: sender, recipient, and input, with every optional
    /// field unset and the effect conservatively [`Effect::NonIdempotent`].
    /// Set the optional fields directly; they are public.
    pub fn new(
        task_id: impl Into<String>,
        sender: impl Into<String>,
        recipient: impl Into<String>,
        input: PayloadRef,
    ) -> Self {
        Self {
            format_version: TASK_ENVELOPE_FORMAT_VERSION,
            task_id: task_id.into(),
            parent: None,
            sender: sender.into(),
            recipient: recipient.into(),
            worker_version: None,
            input,
            output_contract: None,
            deadline: None,
            budget: None,
            idempotency_key: None,
            effect: Effect::NonIdempotent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn error_class_retryability() {
        for class in [
            ErrorClass::Transient,
            ErrorClass::RateLimited,
            ErrorClass::Timeout,
            ErrorClass::DependencyFailure,
            ErrorClass::ResourceExhausted,
            ErrorClass::Unknown,
        ] {
            assert!(class.is_retryable(), "{class:?} must be retryable");
        }
        assert!(!ErrorClass::InvalidInput.is_retryable());
        assert!(!ErrorClass::Cancelled.is_retryable());
    }

    #[test]
    fn error_class_serde_names_are_snake_case() {
        assert_eq!(
            serde_json::to_value(ErrorClass::DependencyFailure).unwrap(),
            json!("dependency_failure")
        );
        assert_eq!(
            serde_json::to_value(ErrorClass::ResourceExhausted).unwrap(),
            json!("resource_exhausted")
        );
    }

    #[test]
    fn backoff_bounds_and_cap() {
        // Zero jitter always yields zero delay.
        assert_eq!(backoff_delay_ms(1, 0.0), 0);
        assert_eq!(backoff_delay_ms(8, 0.0), 0);
        // First retry tops out at the base delay; later retries grow
        // exponentially and clamp at the 5-minute cap.
        assert!(backoff_delay_ms(1, 0.999) < BASE_RETRY_DELAY_MS);
        assert!(backoff_delay_ms(2, 0.999) < 2 * BASE_RETRY_DELAY_MS);
        assert_eq!(backoff_delay_ms(1, 1.0), BASE_RETRY_DELAY_MS);
        assert_eq!(backoff_delay_ms(20, 1.0), MAX_RETRY_DELAY_MS);
        // A saturating attempt ordinal cannot overflow.
        assert_eq!(backoff_delay_ms(u32::MAX, 1.0), MAX_RETRY_DELAY_MS);
    }

    #[test]
    fn classify_retry_gates_in_order() {
        let repeatable = Effect::Idempotent;

        // Gate 1: a non-freely-repeatable effect is never silently retried,
        // even for an eminently retryable class.
        assert_eq!(
            classify_retry(Effect::NonIdempotent, ErrorClass::Transient, 1, 5, 0.5),
            RetryDecision::Fail
        );
        assert_eq!(
            classify_retry(Effect::Compensatable, ErrorClass::Timeout, 1, 5, 0.5),
            RetryDecision::Fail
        );

        // Gate 2: non-retryable classes fail immediately.
        assert_eq!(
            classify_retry(repeatable, ErrorClass::InvalidInput, 1, 5, 0.5),
            RetryDecision::Fail
        );
        assert_eq!(
            classify_retry(repeatable, ErrorClass::Cancelled, 1, 5, 0.5),
            RetryDecision::Fail
        );

        // Gate 3: exhausting the attempt budget dead-letters.
        assert_eq!(
            classify_retry(repeatable, ErrorClass::Unknown, 5, 5, 0.5),
            RetryDecision::Dead
        );
        assert_eq!(
            classify_retry(repeatable, ErrorClass::Transient, 1, 0, 0.5),
            RetryDecision::Dead
        );

        // Otherwise: retry with a jittered delay within the bound.
        match classify_retry(repeatable, ErrorClass::RateLimited, 1, 5, 0.5) {
            RetryDecision::Retry { after_ms } => {
                assert!(after_ms <= BASE_RETRY_DELAY_MS);
            }
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    #[test]
    fn retry_decision_serde_shape() {
        assert_eq!(
            serde_json::to_value(RetryDecision::Retry { after_ms: 500 }).unwrap(),
            json!({"decision": "retry", "after_ms": 500})
        );
        assert_eq!(
            serde_json::to_value(RetryDecision::Dead).unwrap(),
            json!({"decision": "dead"})
        );
        assert_eq!(
            serde_json::to_value(RetryDecision::Fail).unwrap(),
            json!({"decision": "fail"})
        );
    }

    #[test]
    fn retry_legal_actions_mirror_the_classifier_gates() {
        let repeatable = Effect::Idempotent;

        // A retryable failure with budget remaining: retry or abort.
        assert_eq!(
            retry_legal_actions(repeatable, ErrorClass::Timeout, 1, 3),
            vec![DecisionAction::Retry { attempt: 2 }, DecisionAction::Abort]
        );
        // Budget exhausted: retrying is not a legal action.
        assert_eq!(
            retry_legal_actions(repeatable, ErrorClass::Timeout, 3, 3),
            vec![DecisionAction::Abort]
        );
        assert_eq!(
            retry_legal_actions(repeatable, ErrorClass::Transient, 1, 0),
            vec![DecisionAction::Abort]
        );
        // Non-retryable class or non-repeatable effect: abort only.
        assert_eq!(
            retry_legal_actions(repeatable, ErrorClass::InvalidInput, 1, 3),
            vec![DecisionAction::Abort]
        );
        assert_eq!(
            retry_legal_actions(Effect::NonIdempotent, ErrorClass::Transient, 1, 3),
            vec![DecisionAction::Abort]
        );
    }

    #[test]
    fn retry_decision_event_selected_action_stays_inside_the_legal_set() {
        // Property: for every combination the classifier can see, the event's
        // selected action is a member of its legal set.
        let effects = [
            Effect::Pure,
            Effect::ReadOnly,
            Effect::Idempotent,
            Effect::NonIdempotent,
            Effect::Compensatable,
        ];
        let classes = [
            ErrorClass::Transient,
            ErrorClass::RateLimited,
            ErrorClass::Timeout,
            ErrorClass::InvalidInput,
            ErrorClass::DependencyFailure,
            ErrorClass::ResourceExhausted,
            ErrorClass::Cancelled,
            ErrorClass::Unknown,
        ];
        let decided_at = DateTime::<Utc>::from_timestamp_millis(1_760_000_000_000).unwrap();
        for effect in effects {
            for class in classes {
                for (attempt, max_attempts) in [(1, 3), (3, 3), (1, 0)] {
                    let decision = classify_retry(effect, class, attempt, max_attempts, 0.5);
                    let event = retry_decision_event(
                        "run-1",
                        "thread-1",
                        1,
                        effect,
                        class,
                        attempt,
                        max_attempts,
                        None,
                        &decision,
                        &PolicyVersion::default(),
                        decided_at,
                    );
                    assert!(
                        event.legal_actions.contains(&event.selected),
                        "selected {:?} must be inside legal set {:?} for {effect:?}/{class:?} \
                         attempt {attempt}/{max_attempts}",
                        event.selected,
                        event.legal_actions,
                    );
                }
            }
        }
    }

    #[test]
    fn retry_decision_event_shape_and_outcome_mapping() {
        let decided_at = DateTime::<Utc>::from_timestamp_millis(1_760_000_000_000).unwrap();
        let decision = classify_retry(Effect::Idempotent, ErrorClass::Timeout, 1, 3, 0.5);
        let event = retry_decision_event(
            "run-9",
            "thread-2",
            3,
            Effect::Idempotent,
            ErrorClass::Timeout,
            1,
            3,
            Some(840),
            &decision,
            &PolicyVersion::new("policy-0123456789ab"),
            decided_at,
        );
        assert_eq!(event.id, "run-9:d3");
        assert_eq!(event.seq, 3);
        assert_eq!(event.family, DecisionFamily::Retry);
        assert_eq!(
            event.selected,
            DecisionAction::Retry { attempt: 2 },
            "the failure was attempt 1; the decision schedules attempt 2"
        );
        // Deterministic v1 policies record the degenerate propensity, 1.0.
        assert_eq!(event.propensity, 1.0);
        // A retry decision has no outcome until the re-attempt completes.
        assert!(event.outcome.is_none());
        assert_eq!(event.features.get("failure_class"), Some(&json!("timeout")));
        assert_eq!(event.features.get("attempt"), Some(&json!(1)));
        assert_eq!(
            event.features.get("dependency_latency_ms"),
            Some(&json!(840))
        );

        // Terminal decisions record the failure outcome immediately.
        let dead = retry_decision_event(
            "run-9",
            "thread-2",
            4,
            Effect::Idempotent,
            ErrorClass::Timeout,
            3,
            3,
            None,
            &RetryDecision::Dead,
            &PolicyVersion::default(),
            decided_at,
        );
        assert_eq!(dead.selected, DecisionAction::Abort);
        assert_eq!(dead.outcome, Some(DecisionOutcome::Failure));
        assert!(
            !dead.features.contains_key("dependency_latency_ms"),
            "an unmeasured latency is absent from the wire, not null"
        );
    }

    #[test]
    fn envelope_minimal_json_deserializes_with_defaults() {
        // A v1 envelope carrying only the required fields — the shape an
        // older, smaller client would write — loads with honest defaults:
        // current format version, no options set, conservative effect.
        let minimal = json!({
            "task_id": "t-1",
            "sender": "run-9",
            "recipient": "pool-default",
            "input": {"kind": "inline", "value": {"n": 1}},
        });
        let envelope: TaskEnvelope = serde_json::from_value(minimal).unwrap();
        assert_eq!(envelope.format_version, TASK_ENVELOPE_FORMAT_VERSION);
        assert_eq!(envelope.effect, Effect::NonIdempotent);
        assert!(envelope.parent.is_none());
        assert!(envelope.idempotency_key.is_none());
        assert!(envelope.worker_version.is_none());
        assert!(envelope.deadline.is_none());
        assert!(envelope.budget.is_none());
    }

    #[test]
    fn envelope_serde_roundtrip() {
        let mut envelope = TaskEnvelope::new(
            "t-7",
            "run-9:node-a",
            "pool-default",
            PayloadRef::inline(json!({"n": 1})),
        );
        envelope.parent = Some("run-9:3".into());
        envelope.output_contract = Some(ArtifactContract {
            kind: "application/json".into(),
            max_bytes: Some(65_536),
            schema: None,
        });
        envelope.deadline = DateTime::<Utc>::from_timestamp_millis(1_760_000_000_000);
        envelope.budget = Some(TaskBudget {
            max_attempts: 5,
            timeout_ms: Some(30_000),
        });
        envelope.idempotency_key = Some("run-9:charge:7".into());
        envelope.worker_version = Some("activity-worker/1.4.0".into());
        envelope.effect = Effect::Idempotent;

        let back: TaskEnvelope =
            serde_json::from_str(&serde_json::to_string(&envelope).unwrap()).unwrap();
        assert_eq!(envelope, back);
    }
}
