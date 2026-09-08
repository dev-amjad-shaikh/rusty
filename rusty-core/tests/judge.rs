//! Judge-sampler tests (EP-07-S12 AC2/AC5): the heuristic classifier's
//! documented classes, the one-judge-one-vote rule, and `score_turn`
//! minting a majority-voted annotation joined to its intent.

use chrono::DateTime;
use chrono::Utc;
use rusty_agent_runtime::gaps::JudgeVote;
use rusty_agent_runtime::gaps::OutcomeClass;
use rusty_agent_runtime::judge::HeuristicJudgeSampler;
use rusty_agent_runtime::judge::JudgeError;
use rusty_agent_runtime::judge::JudgeSampler;
use rusty_agent_runtime::judge::OutcomeSignal;
use rusty_agent_runtime::judge::score_turn;

fn t0() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_750_000_000, 0).unwrap()
}

fn user(text: &str) -> OutcomeSignal {
    OutcomeSignal::UserMessage {
        text: text.to_owned(),
    }
}

/// A sampler whose judge never answers — failure must surface named,
/// never launder into a neutral vote.
#[derive(Debug)]
struct DownJudge;

#[async_trait::async_trait]
impl JudgeSampler for DownJudge {
    async fn sample(&self, _signal: &OutcomeSignal) -> Result<Vec<JudgeVote>, JudgeError> {
        Err(JudgeError::Unavailable("model timeout".to_owned()))
    }
}

/// A sampler that returns no votes — the annotation's own construction
/// rules refuse it.
#[derive(Debug)]
struct SilentJudge;

#[async_trait::async_trait]
impl JudgeSampler for SilentJudge {
    async fn sample(&self, _signal: &OutcomeSignal) -> Result<Vec<JudgeVote>, JudgeError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn a_polite_redo_demand_is_still_a_redo() {
    let sampler = HeuristicJudgeSampler::new("heuristic-v1");
    let votes = sampler
        .sample(&user("Could you try again, please?"))
        .await
        .unwrap();
    assert_eq!(votes[0].vote, OutcomeClass::Redone);
}

#[tokio::test]
async fn a_polite_correction_is_still_a_correction() {
    let sampler = HeuristicJudgeSampler::new("heuristic-v1");
    // "thanks" is an acceptance marker; the correction must outrank it —
    // politeness does not launder a negative signal.
    let votes = sampler
        .sample(&user("that's not quite right, thanks though"))
        .await
        .unwrap();
    assert_eq!(votes[0].vote, OutcomeClass::Corrected);
}

#[tokio::test]
async fn a_redo_demand_outranks_a_correction() {
    let sampler = HeuristicJudgeSampler::new("heuristic-v1");
    let votes = sampler
        .sample(&user("that's wrong — start over"))
        .await
        .unwrap();
    assert_eq!(votes[0].vote, OutcomeClass::Redone);
}

#[tokio::test]
async fn an_acceptance_marker_scores_positive() {
    let sampler = HeuristicJudgeSampler::new("heuristic-v1");
    let votes = sampler
        .sample(&user("thanks, exactly right"))
        .await
        .unwrap();
    assert_eq!(votes[0].vote, OutcomeClass::Accepted);
}

#[tokio::test]
async fn a_tool_result_scores_by_its_completion() {
    let sampler = HeuristicJudgeSampler::new("heuristic-v1");
    let votes = sampler
        .sample(&OutcomeSignal::ToolResult { ok: true })
        .await
        .unwrap();
    assert_eq!(votes[0].vote, OutcomeClass::Accepted);
    let votes = sampler
        .sample(&OutcomeSignal::ToolResult { ok: false })
        .await
        .unwrap();
    assert_eq!(votes[0].vote, OutcomeClass::Redone);
}

#[tokio::test]
async fn an_ambiguous_message_abstains() {
    let sampler = HeuristicJudgeSampler::new("heuristic-v1");
    let votes = sampler
        .sample(&user("what about the staging deploy?"))
        .await
        .unwrap();
    assert_eq!(votes[0].vote, OutcomeClass::Neutral);
}

#[tokio::test]
async fn one_judge_votes_once_with_its_name_on_the_ballot() {
    let sampler = HeuristicJudgeSampler::new("heuristic-v1");
    let votes = sampler.sample(&user("perfect")).await.unwrap();
    assert_eq!(votes.len(), 1, "one judge, one vote — no ballot stuffing");
    assert_eq!(votes[0].judge, "heuristic-v1");
}

#[tokio::test]
async fn score_turn_mints_the_annotation_joined_to_the_intent() {
    let sampler = HeuristicJudgeSampler::new("heuristic-v1");
    let annotation = score_turn(
        &sampler,
        "session-9:turn-4",
        "intent.deploy-web-service",
        &user("try again"),
        t0(),
    )
    .await
    .unwrap();
    assert_eq!(annotation.turn_ref, "session-9:turn-4");
    assert_eq!(annotation.intent_id, "intent.deploy-web-service");
    assert_eq!(annotation.outcome, OutcomeClass::Redone);
    assert_eq!(annotation.judge_votes.len(), 1);
    assert_eq!(annotation.judge_votes[0].judge, "heuristic-v1");
    assert!(annotation.annotation_id.starts_with("oa-"));
}

#[tokio::test]
async fn re_scoring_a_turn_converges_on_the_same_annotation_id() {
    let sampler = HeuristicJudgeSampler::new("heuristic-v1");
    let signal = user("not quite — you missed the retry policy");
    let first = score_turn(
        &sampler,
        "session-9:turn-4",
        "intent.deploy-web-service",
        &signal,
        t0(),
    )
    .await
    .unwrap();
    let second = score_turn(
        &sampler,
        "session-9:turn-4",
        "intent.deploy-web-service",
        &signal,
        t0(),
    )
    .await
    .unwrap();
    assert_eq!(
        first.annotation_id, second.annotation_id,
        "a deterministic judge re-scored converges by content address"
    );
}

#[tokio::test]
async fn a_sampler_failure_surfaces_named() {
    let err = score_turn(
        &DownJudge,
        "session-9:turn-4",
        "intent.deploy-web-service",
        &user("perfect"),
        t0(),
    )
    .await
    .expect_err("a down judge must not mint an annotation");
    assert!(
        matches!(
            err,
            rusty_agent_runtime::judge::JudgeScoreError::Sampler(JudgeError::Unavailable(_))
        ),
        "the failure names its sampler stage: {err}"
    );
}

#[tokio::test]
async fn an_empty_ballot_is_the_annotations_refusal_not_the_samplers() {
    let err = score_turn(
        &SilentJudge,
        "session-9:turn-4",
        "intent.deploy-web-service",
        &user("perfect"),
        t0(),
    )
    .await
    .expect_err("an empty vote set cannot mint an annotation");
    assert!(
        matches!(
            err,
            rusty_agent_runtime::judge::JudgeScoreError::Annotation(_)
        ),
        "the annotation's own rule refuses: {err}"
    );
}
