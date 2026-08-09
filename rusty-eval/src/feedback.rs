//! Deterministic human-feedback operations for evaluation and learning loops.
//!
//! The queue deliberately owns no clock, database, or worker runtime. Callers
//! supply timestamps, persist the serializable queue, and decide who may act as
//! a reviewer or adjudicator. That keeps replay and tests deterministic while
//! still providing the state-machine guarantees needed by a distributed review
//! service: leases expire, reviewers cannot vote twice, disagreements require
//! adjudication, and only resolved feedback can become an evaluation case.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::dataset::{EvalCase, Expectation, StatePredicate};
use crate::error::{EvalError, Result};

/// The feedback queue format version this build loads and writes.
pub const FEEDBACK_FORMAT_VERSION: u64 = 1;

/// A stable reference to the recorded run that produced an annotation task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRef {
    /// Stable run identifier in the Flight Recorder.
    pub run_id: String,
    /// Optional conversation or thread identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Optional event sequence at which the candidate output was captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_seq: Option<u64>,
}

impl TraceRef {
    /// Reference a run without a narrower thread or event location.
    pub fn run(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            thread_id: None,
            event_seq: None,
        }
    }
}

/// One candidate output presented to a reviewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewCandidate {
    /// Stable identifier within the task.
    pub id: String,
    /// Arbitrary structured output shown to the reviewer.
    pub output: Value,
}

/// One scored dimension in a review rubric.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RubricCriterion {
    /// Stable criterion identifier used as the score-map key.
    pub id: String,
    /// Human-readable guidance for the reviewer.
    pub description: String,
}

/// A versioned rubric attached to an annotation task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRubric {
    /// Stable rubric name.
    pub name: String,
    /// Caller-managed rubric version.
    pub version: String,
    /// Required criteria. Every submitted review must score every criterion.
    pub criteria: Vec<RubricCriterion>,
}

/// The actionable result of a review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewDecision {
    /// Prefer one of the task's candidates.
    Select { candidate_id: String },
    /// Supply a corrected output when neither candidate is acceptable.
    Correct { output: Value },
    /// Reject the example so it cannot be promoted into a dataset.
    Reject {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// A review submitted while holding a live task lease.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewSubmission {
    /// Selection, correction, or rejection.
    pub decision: ReviewDecision,
    /// Criterion id to normalized score in the inclusive range `[0, 1]`.
    pub scores: BTreeMap<String, f64>,
    /// Optional reviewer rationale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// An immutable accepted review with provenance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredReview {
    /// Reviewer identity supplied by the caller.
    pub reviewer: String,
    /// Caller-supplied submission timestamp.
    pub submitted_at_ms: u64,
    /// The validated submission.
    pub submission: ReviewSubmission,
}

/// How a final decision was reached.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolutionAuthority {
    /// All required reviewers independently made the same decision.
    Consensus,
    /// A named adjudicator resolved disagreement.
    Adjudicator { id: String },
}

/// The final, promotion-eligible result of an annotation task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewResolution {
    /// Final decision.
    pub decision: ReviewDecision,
    /// Consensus or explicit adjudication.
    pub authority: ResolutionAuthority,
    /// Caller-supplied resolution timestamp.
    pub resolved_at_ms: u64,
}

/// Current task state at a caller-supplied point in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationStatus {
    /// No accepted reviews or live leases.
    Open,
    /// At least one review or live lease exists; more reviews are required.
    InReview,
    /// The required reviews disagree and an adjudicator must decide.
    NeedsAdjudication,
    /// A consensus or adjudicated resolution exists.
    Resolved,
}

/// Proof that a reviewer currently owns a task reservation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewLease {
    /// Claimed annotation task.
    pub task_id: String,
    /// Reviewer holding the lease.
    pub reviewer: String,
    /// Monotonic occurrence id that prevents a stale worker from using a
    /// later lease issued to the same reviewer.
    pub lease_id: u64,
    /// Caller-supplied time at which this occurrence was issued.
    pub issued_at_ms: u64,
    /// Exclusive lease expiry; a lease is expired when `now_ms >=` this value.
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LeaseRecord {
    lease_id: u64,
    issued_at_ms: u64,
    expires_at_ms: u64,
}

/// One pairwise annotation task and its review state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnnotationTask {
    id: String,
    source: TraceRef,
    input: Value,
    candidates: Vec<ReviewCandidate>,
    rubric: ReviewRubric,
    priority: i32,
    created_at_ms: u64,
    required_reviews: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    leases: BTreeMap<String, LeaseRecord>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    reviews: BTreeMap<String, StoredReview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolution: Option<ReviewResolution>,
}

impl AnnotationTask {
    /// Create a pairwise annotation task.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        source: TraceRef,
        input: Value,
        candidates: Vec<ReviewCandidate>,
        rubric: ReviewRubric,
        priority: i32,
        created_at_ms: u64,
        required_reviews: usize,
    ) -> Result<Self> {
        let task = Self {
            id: id.into(),
            source,
            input,
            candidates,
            rubric,
            priority,
            created_at_ms,
            required_reviews,
            leases: BTreeMap::new(),
            reviews: BTreeMap::new(),
            resolution: None,
        };
        task.validate_definition()?;
        Ok(task)
    }

    /// Stable task identifier.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Recorded-run provenance.
    pub fn source(&self) -> &TraceRef {
        &self.source
    }

    /// Input that produced the candidates.
    pub fn input(&self) -> &Value {
        &self.input
    }

    /// The two candidates under comparison.
    pub fn candidates(&self) -> &[ReviewCandidate] {
        &self.candidates
    }

    /// Versioned scoring rubric.
    pub fn rubric(&self) -> &ReviewRubric {
        &self.rubric
    }

    /// Scheduling priority; larger values are claimed first.
    pub fn priority(&self) -> i32 {
        self.priority
    }

    /// Task creation timestamp.
    pub fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }

    /// Number of independent reviews required before resolution.
    pub fn required_reviews(&self) -> usize {
        self.required_reviews
    }

    /// Accepted reviews, keyed by reviewer identity.
    pub fn reviews(&self) -> &BTreeMap<String, StoredReview> {
        &self.reviews
    }

    /// Final resolution, when one exists.
    pub fn resolution(&self) -> Option<&ReviewResolution> {
        self.resolution.as_ref()
    }

    /// Compute status without mutating or consulting a wall clock.
    pub fn status_at(&self, now_ms: u64) -> AnnotationStatus {
        if self.resolution.is_some() {
            return AnnotationStatus::Resolved;
        }
        if self.reviews.len() >= self.required_reviews {
            return AnnotationStatus::NeedsAdjudication;
        }
        if !self.reviews.is_empty()
            || self
                .leases
                .values()
                .any(|lease| lease.expires_at_ms > now_ms)
        {
            AnnotationStatus::InReview
        } else {
            AnnotationStatus::Open
        }
    }

    /// Convert a resolved selection or correction into a versioned dataset case.
    pub fn promote(
        &self,
        case_id: impl Into<String>,
        output_pointer: impl Into<String>,
    ) -> Result<EvalCase> {
        let case_id = case_id.into();
        require_non_empty("promoted case id", &case_id)?;
        let output_pointer = output_pointer.into();
        validate_json_pointer(&output_pointer)?;
        let resolution = self
            .resolution
            .as_ref()
            .ok_or_else(|| EvalError::Feedback(format!("task `{}` is not resolved", self.id)))?;
        let expected = match &resolution.decision {
            ReviewDecision::Select { candidate_id } => self
                .candidates
                .iter()
                .find(|candidate| candidate.id == *candidate_id)
                .map(|candidate| candidate.output.clone())
                .ok_or_else(|| {
                    EvalError::Feedback(format!(
                        "task `{}` resolution selects unknown candidate `{candidate_id}`",
                        self.id
                    ))
                })?,
            ReviewDecision::Correct { output } => output.clone(),
            ReviewDecision::Reject { .. } => {
                return Err(EvalError::Feedback(format!(
                    "task `{}` was rejected and cannot be promoted",
                    self.id
                )));
            }
        };
        let authority = match &resolution.authority {
            ResolutionAuthority::Consensus => "consensus".to_owned(),
            ResolutionAuthority::Adjudicator { id } => format!("adjudicated:{id}"),
        };

        Ok(EvalCase {
            id: case_id,
            input: self.input.clone(),
            expect: Expectation {
                state: vec![StatePredicate {
                    pointer: output_pointer,
                    expected,
                }],
                ..Expectation::default()
            },
            tags: vec![
                "human-feedback".to_owned(),
                format!("feedback-task:{}", self.id),
                format!("source-run:{}", self.source.run_id),
                authority,
            ],
        })
    }

    fn validate_definition(&self) -> Result<()> {
        require_non_empty("task id", &self.id)?;
        require_non_empty("source run id", &self.source.run_id)?;
        if let Some(thread_id) = &self.source.thread_id {
            require_non_empty("source thread id", thread_id)?;
        }
        if self.candidates.len() != 2 {
            return feedback_error(format!(
                "task `{}` must contain exactly two candidates",
                self.id
            ));
        }
        let mut candidate_ids = BTreeSet::new();
        for candidate in &self.candidates {
            require_non_empty("candidate id", &candidate.id)?;
            if !candidate_ids.insert(candidate.id.as_str()) {
                return feedback_error(format!(
                    "task `{}` has duplicate candidate id `{}`",
                    self.id, candidate.id
                ));
            }
        }
        require_non_empty("rubric name", &self.rubric.name)?;
        require_non_empty("rubric version", &self.rubric.version)?;
        if self.rubric.criteria.is_empty() {
            return feedback_error(format!(
                "task `{}` rubric must contain at least one criterion",
                self.id
            ));
        }
        let mut criterion_ids = BTreeSet::new();
        for criterion in &self.rubric.criteria {
            require_non_empty("rubric criterion id", &criterion.id)?;
            require_non_empty("rubric criterion description", &criterion.description)?;
            if !criterion_ids.insert(criterion.id.as_str()) {
                return feedback_error(format!(
                    "task `{}` has duplicate rubric criterion `{}`",
                    self.id, criterion.id
                ));
            }
        }
        if self.required_reviews == 0 {
            return feedback_error(format!(
                "task `{}` must require at least one review",
                self.id
            ));
        }
        Ok(())
    }

    fn validate_submission(&self, submission: &ReviewSubmission) -> Result<()> {
        self.validate_decision(&submission.decision)?;
        let expected: BTreeSet<_> = self
            .rubric
            .criteria
            .iter()
            .map(|criterion| criterion.id.as_str())
            .collect();
        let actual: BTreeSet<_> = submission.scores.keys().map(String::as_str).collect();
        if actual != expected {
            return feedback_error(format!(
                "task `{}` review scores must exactly match rubric criteria",
                self.id
            ));
        }
        for (criterion, score) in &submission.scores {
            if !score.is_finite() || !(0.0..=1.0).contains(score) {
                return feedback_error(format!(
                    "task `{}` score for `{criterion}` must be finite and between 0 and 1",
                    self.id
                ));
            }
        }
        Ok(())
    }

    fn validate_decision(&self, decision: &ReviewDecision) -> Result<()> {
        if let ReviewDecision::Select { candidate_id } = decision {
            if !self
                .candidates
                .iter()
                .any(|candidate| candidate.id == *candidate_id)
            {
                return feedback_error(format!(
                    "task `{}` has no candidate `{candidate_id}`",
                    self.id
                ));
            }
        }
        Ok(())
    }

    fn validate_state(&self) -> Result<()> {
        self.validate_definition()?;
        if self.reviews.len() > self.required_reviews {
            return feedback_error(format!(
                "task `{}` contains more reviews than required",
                self.id
            ));
        }
        for (reviewer, stored) in &self.reviews {
            require_non_empty("reviewer id", reviewer)?;
            if stored.reviewer != *reviewer {
                return feedback_error(format!(
                    "task `{}` review key does not match reviewer",
                    self.id
                ));
            }
            if self.leases.contains_key(reviewer) {
                return feedback_error(format!(
                    "task `{}` reviewer `{reviewer}` has both a lease and review",
                    self.id
                ));
            }
            if stored.submitted_at_ms < self.created_at_ms {
                return feedback_error(format!(
                    "task `{}` review by `{reviewer}` predates task creation",
                    self.id
                ));
            }
            self.validate_submission(&stored.submission)?;
        }
        for (reviewer, lease) in &self.leases {
            require_non_empty("reviewer id", reviewer)?;
            if lease.issued_at_ms < self.created_at_ms {
                return feedback_error(format!(
                    "task `{}` lease for `{reviewer}` predates task creation",
                    self.id
                ));
            }
            if lease.expires_at_ms <= lease.issued_at_ms {
                return feedback_error(format!(
                    "task `{}` lease for `{reviewer}` has an invalid time range",
                    self.id
                ));
            }
        }
        if let Some(resolution) = &self.resolution {
            if self.reviews.len() < self.required_reviews {
                return feedback_error(format!(
                    "task `{}` resolved before collecting required reviews",
                    self.id
                ));
            }
            if self
                .reviews
                .values()
                .any(|review| review.submitted_at_ms > resolution.resolved_at_ms)
            {
                return feedback_error(format!(
                    "task `{}` resolution predates a submitted review",
                    self.id
                ));
            }
            if !self.leases.is_empty() {
                return feedback_error(format!(
                    "task `{}` is resolved but still contains reviewer leases",
                    self.id
                ));
            }
            self.validate_decision(&resolution.decision)?;
            let mut review_decisions = self
                .reviews
                .values()
                .map(|review| &review.submission.decision);
            let first_review_decision = review_decisions.next().expect("required reviews exist");
            let unanimous = review_decisions.all(|decision| decision == first_review_decision);
            match &resolution.authority {
                ResolutionAuthority::Consensus => {
                    if !unanimous || *first_review_decision != resolution.decision {
                        return feedback_error(format!(
                            "task `{}` consensus resolution does not match every review",
                            self.id
                        ));
                    }
                }
                ResolutionAuthority::Adjudicator { id } => {
                    require_non_empty("adjudicator id", id)?;
                    if unanimous {
                        return feedback_error(format!(
                            "task `{}` uses adjudication despite unanimous reviews",
                            self.id
                        ));
                    }
                }
            }
        } else {
            if self.reviews.len() + self.leases.len() > self.required_reviews {
                return feedback_error(format!(
                    "task `{}` has more reviews and leases than required",
                    self.id
                ));
            }
            if self.reviews.len() == self.required_reviews {
                let mut decisions = self
                    .reviews
                    .values()
                    .map(|review| &review.submission.decision);
                if let Some(first) = decisions.next() {
                    if decisions.all(|decision| decision == first) {
                        return feedback_error(format!(
                            "task `{}` has review consensus but no resolution",
                            self.id
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// A named, versioned collection of annotation tasks.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnnotationQueue {
    format_version: u64,
    name: String,
    tasks: BTreeMap<String, AnnotationTask>,
    next_lease_id: u64,
}

impl AnnotationQueue {
    /// Create an empty queue.
    pub fn new(name: impl Into<String>) -> Result<Self> {
        let name = name.into();
        require_non_empty("queue name", &name)?;
        Ok(Self {
            format_version: FEEDBACK_FORMAT_VERSION,
            name,
            tasks: BTreeMap::new(),
            next_lease_id: 1,
        })
    }

    /// Queue name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// All tasks in stable id order.
    pub fn tasks(&self) -> &BTreeMap<String, AnnotationTask> {
        &self.tasks
    }

    /// Find a task by id.
    pub fn task(&self, task_id: &str) -> Option<&AnnotationTask> {
        self.tasks.get(task_id)
    }

    /// Add a validated task without overwriting an existing id.
    pub fn enqueue(&mut self, task: AnnotationTask) -> Result<()> {
        task.validate_state()?;
        if self.tasks.contains_key(task.id()) {
            return feedback_error(format!("duplicate annotation task `{}`", task.id()));
        }
        self.tasks.insert(task.id.clone(), task);
        Ok(())
    }

    /// Claim the highest-priority eligible task.
    ///
    /// Ordering is deterministic: priority descending, creation time
    /// ascending, then task id ascending. Calling claim again while the same
    /// reviewer owns a live lease returns that lease idempotently.
    pub fn claim(
        &mut self,
        reviewer: impl Into<String>,
        now_ms: u64,
        lease_ms: u64,
    ) -> Result<Option<ReviewLease>> {
        let reviewer = reviewer.into();
        require_non_empty("reviewer id", &reviewer)?;
        if lease_ms == 0 {
            return feedback_error("lease duration must be greater than zero");
        }
        self.prune_expired_leases(now_ms);
        if let Some((task_id, lease)) = self.tasks.iter().find_map(|(task_id, task)| {
            task.leases
                .get(&reviewer)
                .map(|lease| (task_id.clone(), lease.clone()))
        }) {
            return Ok(Some(ReviewLease {
                task_id,
                reviewer,
                lease_id: lease.lease_id,
                issued_at_ms: lease.issued_at_ms,
                expires_at_ms: lease.expires_at_ms,
            }));
        }

        let task_id = self
            .tasks
            .values()
            .filter(|task| {
                task.created_at_ms <= now_ms
                    && task.resolution.is_none()
                    && !task.reviews.contains_key(&reviewer)
                    && task.reviews.len() < task.required_reviews
                    && task.reviews.len() + task.leases.len() < task.required_reviews
            })
            .min_by(|left, right| {
                right
                    .priority
                    .cmp(&left.priority)
                    .then_with(|| left.created_at_ms.cmp(&right.created_at_ms))
                    .then_with(|| left.id.cmp(&right.id))
            })
            .map(|task| task.id.clone());

        let Some(task_id) = task_id else {
            return Ok(None);
        };
        let expires_at_ms = now_ms.checked_add(lease_ms).ok_or_else(|| {
            EvalError::Feedback("lease expiry overflows u64 milliseconds".to_owned())
        })?;
        let lease_id = self.next_lease_id;
        self.next_lease_id = self.next_lease_id.checked_add(1).ok_or_else(|| {
            EvalError::Feedback("lease occurrence id space is exhausted".to_owned())
        })?;
        self.tasks
            .get_mut(&task_id)
            .expect("selected task remains present")
            .leases
            .insert(
                reviewer.clone(),
                LeaseRecord {
                    lease_id,
                    issued_at_ms: now_ms,
                    expires_at_ms,
                },
            );
        Ok(Some(ReviewLease {
            task_id,
            reviewer,
            lease_id,
            issued_at_ms: now_ms,
            expires_at_ms,
        }))
    }

    /// Submit a review under a live lease.
    pub fn submit(
        &mut self,
        lease: &ReviewLease,
        now_ms: u64,
        submission: ReviewSubmission,
    ) -> Result<AnnotationStatus> {
        require_non_empty("reviewer id", &lease.reviewer)?;
        let task_id = &lease.task_id;
        let reviewer = &lease.reviewer;
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| EvalError::Feedback(format!("unknown annotation task `{task_id}`")))?;
        if task.resolution.is_some() {
            return feedback_error(format!("task `{task_id}` is already resolved"));
        }
        if task.reviews.contains_key(reviewer) {
            return feedback_error(format!(
                "reviewer `{reviewer}` already reviewed task `{task_id}`"
            ));
        }
        if task.reviews.len() >= task.required_reviews {
            return feedback_error(format!(
                "task `{task_id}` already collected all required reviews"
            ));
        }
        let active = task.leases.get(reviewer).cloned().ok_or_else(|| {
            EvalError::Feedback(format!(
                "reviewer `{reviewer}` does not hold a lease for task `{task_id}`"
            ))
        })?;
        if active.lease_id != lease.lease_id
            || active.issued_at_ms != lease.issued_at_ms
            || active.expires_at_ms != lease.expires_at_ms
        {
            return feedback_error(format!(
                "lease {} for task `{task_id}` is stale",
                lease.lease_id
            ));
        }
        if now_ms >= active.expires_at_ms {
            task.leases.remove(reviewer);
            return feedback_error(format!(
                "reviewer `{reviewer}` lease for task `{task_id}` expired"
            ));
        }
        if now_ms < active.issued_at_ms || now_ms < task.created_at_ms {
            return feedback_error(format!(
                "review timestamp for task `{task_id}` predates its lease or task creation"
            ));
        }
        if task
            .reviews
            .values()
            .any(|review| review.submitted_at_ms > now_ms)
        {
            return feedback_error(format!(
                "review timestamp for task `{task_id}` predates an accepted review"
            ));
        }
        task.validate_submission(&submission)?;
        task.leases.remove(reviewer);
        task.reviews.insert(
            reviewer.to_owned(),
            StoredReview {
                reviewer: reviewer.to_owned(),
                submitted_at_ms: now_ms,
                submission,
            },
        );

        if task.reviews.len() == task.required_reviews {
            let mut decisions = task
                .reviews
                .values()
                .map(|review| &review.submission.decision);
            let first = decisions.next().expect("at least one review").clone();
            if decisions.all(|decision| *decision == first) {
                task.resolution = Some(ReviewResolution {
                    decision: first,
                    authority: ResolutionAuthority::Consensus,
                    resolved_at_ms: now_ms,
                });
                task.leases.clear();
            }
        }
        Ok(task.status_at(now_ms))
    }

    /// Resolve a disagreement after all required reviews have arrived.
    pub fn adjudicate(
        &mut self,
        task_id: &str,
        adjudicator: impl Into<String>,
        now_ms: u64,
        decision: ReviewDecision,
    ) -> Result<()> {
        let adjudicator = adjudicator.into();
        require_non_empty("adjudicator id", &adjudicator)?;
        let task = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| EvalError::Feedback(format!("unknown annotation task `{task_id}`")))?;
        if task.resolution.is_some() {
            return feedback_error(format!("task `{task_id}` is already resolved"));
        }
        if task.reviews.len() < task.required_reviews {
            return feedback_error(format!(
                "task `{task_id}` has not collected all required reviews"
            ));
        }
        if task
            .reviews
            .values()
            .any(|review| review.submitted_at_ms > now_ms)
        {
            return feedback_error(format!(
                "adjudication for task `{task_id}` predates a submitted review"
            ));
        }
        task.validate_decision(&decision)?;
        task.resolution = Some(ReviewResolution {
            decision,
            authority: ResolutionAuthority::Adjudicator { id: adjudicator },
            resolved_at_ms: now_ms,
        });
        task.leases.clear();
        Ok(())
    }

    /// Serialize the queue with stable map ordering.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Load a queue, rejecting unsupported versions and invalid state.
    pub fn from_json(text: &str) -> Result<Self> {
        let header: FeedbackVersionHeader = serde_json::from_str(text)?;
        if header.format_version != FEEDBACK_FORMAT_VERSION {
            return Err(EvalError::UnsupportedFeedbackVersion {
                found: header.format_version,
                supported: FEEDBACK_FORMAT_VERSION,
            });
        }
        let wire: AnnotationQueueWire = serde_json::from_str(text)?;
        let queue = wire.into_queue()?;
        require_non_empty("queue name", &queue.name)?;
        let mut lease_ids = BTreeSet::new();
        let mut leased_reviewers = BTreeSet::new();
        for (task_id, task) in &queue.tasks {
            if task_id != task.id() {
                return feedback_error(format!(
                    "annotation task map key `{task_id}` does not match task id `{}`",
                    task.id()
                ));
            }
            task.validate_state()?;
            for lease in task.leases.values() {
                if lease.lease_id == 0 || lease.lease_id >= queue.next_lease_id {
                    return feedback_error(format!(
                        "task `{task_id}` contains invalid lease occurrence {}",
                        lease.lease_id
                    ));
                }
                if !lease_ids.insert(lease.lease_id) {
                    return feedback_error(format!(
                        "duplicate lease occurrence {}",
                        lease.lease_id
                    ));
                }
            }
            for reviewer in task.leases.keys() {
                if !leased_reviewers.insert(reviewer.as_str()) {
                    return feedback_error(format!(
                        "reviewer `{reviewer}` holds leases for multiple tasks"
                    ));
                }
            }
        }
        Ok(queue)
    }

    fn prune_expired_leases(&mut self, now_ms: u64) {
        for task in self.tasks.values_mut() {
            task.leases.retain(|_, lease| lease.expires_at_ms > now_ms);
        }
    }
}

#[derive(Deserialize)]
struct FeedbackVersionHeader {
    format_version: u64,
}

#[derive(Deserialize)]
struct AnnotationTaskWire {
    id: String,
    source: TraceRef,
    input: Value,
    candidates: Vec<ReviewCandidate>,
    rubric: ReviewRubric,
    priority: i32,
    created_at_ms: u64,
    required_reviews: usize,
    #[serde(default)]
    leases: BTreeMap<String, LeaseRecord>,
    #[serde(default)]
    reviews: BTreeMap<String, StoredReview>,
    #[serde(default)]
    resolution: Option<ReviewResolution>,
}

impl AnnotationTaskWire {
    fn into_task(self) -> AnnotationTask {
        AnnotationTask {
            id: self.id,
            source: self.source,
            input: self.input,
            candidates: self.candidates,
            rubric: self.rubric,
            priority: self.priority,
            created_at_ms: self.created_at_ms,
            required_reviews: self.required_reviews,
            leases: self.leases,
            reviews: self.reviews,
            resolution: self.resolution,
        }
    }
}

#[derive(Deserialize)]
struct AnnotationQueueWire {
    format_version: u64,
    name: String,
    tasks: BTreeMap<String, AnnotationTaskWire>,
    next_lease_id: u64,
}

impl AnnotationQueueWire {
    fn into_queue(self) -> Result<AnnotationQueue> {
        if self.next_lease_id == 0 {
            return feedback_error("next lease occurrence id must be greater than zero");
        }
        Ok(AnnotationQueue {
            format_version: self.format_version,
            name: self.name,
            tasks: self
                .tasks
                .into_iter()
                .map(|(id, task)| (id, task.into_task()))
                .collect(),
            next_lease_id: self.next_lease_id,
        })
    }
}

fn require_non_empty(label: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        feedback_error(format!("{label} must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_json_pointer(pointer: &str) -> Result<()> {
    if !pointer.is_empty() && !pointer.starts_with('/') {
        return feedback_error("output pointer must be an RFC 6901 JSON pointer");
    }
    for segment in pointer.split('/').skip(1) {
        let mut chars = segment.chars();
        while let Some(character) = chars.next() {
            if character == '~' && !matches!(chars.next(), Some('0' | '1')) {
                return feedback_error("output pointer contains an invalid RFC 6901 escape");
            }
        }
    }
    Ok(())
}

fn feedback_error<T>(message: impl Into<String>) -> Result<T> {
    Err(EvalError::Feedback(message.into()))
}
