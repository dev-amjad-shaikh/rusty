//! The judge sampler (EP-07-S12 AC2): the seam that turns a completed
//! main-line turn's next state into recorded judge votes.
//!
//! W5 landed the annotation machinery — [`OutcomeAnnotation`] majority-
//! votes recorded [`JudgeVote`]s and joins them to the intent vocabulary.
//! What it deliberately left open is who *produces* the votes: this module
//! is that producer's seam.
//!
//! - **[`JudgeSampler`]** — the trait. A sampler reads the next state that
//!   followed a served turn (the following user message, or the downstream
//!   tool result) and returns one or more votes, each carrying its judge's
//!   identity. Model-backed judges, human review queues, and heuristics
//!   all implement the same seam; the annotation's majority vote and
//!   provenance rules apply unchanged.
//! - **[`HeuristicJudgeSampler`]** — the deterministic reference. Its
//!   classifier is deliberately blunt and documented: redo, rephrase, and
//!   correction markers score negative *even when phrased politely* (the
//!   AC's explicit rule — "could you try again" is a correction, whatever
//!   its manners), a successful downstream tool result scores positive,
//!   and anything ambiguous abstains to `neutral`. A heuristic is one
//!   judge and votes once — sampling it N times would stuff the ballot
//!   with identical ballots, which is theater, not majority.
//!
//! The sampler never writes the ledger: it produces votes; the caller
//! scores them into an annotation ([`score_turn`]) and records through the
//! gap ledger's own path. Determinism note: the heuristic is a pure
//! function of the signal, so the same turn re-scored converges on the
//! same annotation id (the annotation's content-address rule).

use std::fmt;

use async_trait::async_trait;

use crate::gaps::{GapError, JudgeVote, OutcomeAnnotation, OutcomeClass};

/// The next state a served turn met — the signal a judge scores. The
/// behavioral signal reads *reaction*, not intent: what the user said
/// next, or what the downstream tool chain did.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutcomeSignal {
    /// The following user message. Redo, rephrase, and correction markers
    /// score negative even when polite.
    UserMessage {
        /// The message text, verbatim.
        text: String,
    },
    /// The downstream tool result the turn's answer fed.
    ToolResult {
        /// `true` when the chain completed successfully.
        ok: bool,
    },
}

/// Markers that a user message is a redo demand — the strongest negative.
/// Lowercased substring matching; the list is the classifier's whole
/// vocabulary, honest about its bluntness.
const REDO_MARKERS: &[&str] = &[
    "try again",
    "start over",
    "from scratch",
    "do it over",
    "run it back",
];

/// Markers that a user message is a correction or rephrase — negative
/// even when the phrasing is courteous.
const CORRECTION_MARKERS: &[&str] = &[
    "not quite",
    "that's wrong",
    "that is wrong",
    "incorrect",
    "you missed",
    "that's not right",
    "that is not right",
    "please fix",
    "redo",
    "actually,",
    "wrong",
];

/// Markers that a user message accepts the turn.
const ACCEPT_MARKERS: &[&str] = &[
    "thank",
    "perfect",
    "exactly right",
    "that's right",
    "that is right",
    "looks good",
    "works now",
    "well done",
];

/// Classify one signal deterministically: redo markers win over
/// correction markers (a redo demand is the stronger signal), correction
/// markers win over acceptance markers (politeness does not launder a
/// correction — the AC's rule), a successful tool chain is positive, and
/// anything else abstains neutral.
fn classify(signal: &OutcomeSignal) -> OutcomeClass {
    match signal {
        OutcomeSignal::ToolResult { ok } => {
            if *ok {
                OutcomeClass::Accepted
            } else {
                OutcomeClass::Redone
            }
        }
        OutcomeSignal::UserMessage { text } => {
            let lowered = text.to_ascii_lowercase();
            if REDO_MARKERS.iter().any(|marker| lowered.contains(marker)) {
                return OutcomeClass::Redone;
            }
            if CORRECTION_MARKERS
                .iter()
                .any(|marker| lowered.contains(marker))
            {
                return OutcomeClass::Corrected;
            }
            if ACCEPT_MARKERS.iter().any(|marker| lowered.contains(marker)) {
                return OutcomeClass::Accepted;
            }
            OutcomeClass::Neutral
        }
    }
}

/// The judge-sampling seam: given a served turn's next state, produce the
/// judge's votes. Implemented by model-backed judges (the production
/// path), human review queues, and the deterministic heuristic (the
/// reference and the test double).
///
/// One judge, one vote per sample call: a sampler that wants majority
/// weight samples *different* judges (a panel), never the same judge
/// repeated — [`OutcomeAnnotation::from_votes`] majority-rules across the
/// returned set, and repeated identical votes would turn the jury into a
/// echo of whoever samples most.
#[async_trait]
pub trait JudgeSampler: Send + Sync + fmt::Debug {
    /// Score the turn's next state. Implementations must be deterministic
    /// given the same signal (or honestly non-deterministic over a model,
    /// with the model id as the judge name) — the votes are the
    /// annotation's provenance.
    async fn sample(&self, signal: &OutcomeSignal) -> Result<Vec<JudgeVote>, JudgeError>;
}

/// Every way judge sampling can fail.
#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    /// The sampler could not reach its judge (model timeout, queue
    /// unavailable). Surfaced, never guessed — an unscoreable turn is
    /// unscoreable, not neutral.
    #[error("judge sampling failed: {0}")]
    Unavailable(String),
}

/// The deterministic reference judge: a blunt, documented marker
/// classifier over the next state. Its judge name travels on the vote,
/// so a panel mixing it with model judges attributes honestly.
#[derive(Debug, Clone)]
pub struct HeuristicJudgeSampler {
    judge_name: String,
}

impl HeuristicJudgeSampler {
    /// A heuristic judge with the given name (recorded on its votes).
    pub fn new(judge_name: impl Into<String>) -> Self {
        Self {
            judge_name: judge_name.into(),
        }
    }
}

#[async_trait]
impl JudgeSampler for HeuristicJudgeSampler {
    async fn sample(&self, signal: &OutcomeSignal) -> Result<Vec<JudgeVote>, JudgeError> {
        Ok(vec![JudgeVote {
            judge: self.judge_name.clone(),
            vote: classify(signal),
        }])
    }
}

/// Score one turn: sample the judge(s) over the next state, then mint the
/// majority-voted [`OutcomeAnnotation`] joined to `intent_id`. The sampler
/// produces evidence; the annotation derives the verdict — the two never
/// disagree by construction.
pub async fn score_turn(
    sampler: &dyn JudgeSampler,
    turn_ref: impl Into<String>,
    intent_id: impl Into<String>,
    signal: &OutcomeSignal,
    scored_at: chrono::DateTime<chrono::Utc>,
) -> Result<OutcomeAnnotation, JudgeScoreError> {
    let votes = sampler.sample(signal).await?;
    Ok(OutcomeAnnotation::from_votes(
        turn_ref, intent_id, votes, scored_at,
    )?)
}

/// Every way turn scoring can fail: the sampler's own failure, or the
/// annotation's construction rules (an empty vote set, an over-long
/// field).
#[derive(Debug, thiserror::Error)]
pub enum JudgeScoreError {
    /// The sampler failed.
    #[error(transparent)]
    Sampler(#[from] JudgeError),
    /// The annotation refused the votes.
    #[error(transparent)]
    Annotation(#[from] GapError),
}
