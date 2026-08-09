use std::collections::BTreeMap;

use rusty_eval::{
    AnnotationQueue, AnnotationStatus, AnnotationTask, EvalError, ResolutionAuthority,
    ReviewCandidate, ReviewDecision, ReviewLease, ReviewRubric, ReviewSubmission, RubricCriterion,
    TraceRef, FEEDBACK_FORMAT_VERSION,
};
use serde_json::{json, Value};

fn rubric() -> ReviewRubric {
    ReviewRubric {
        name: "answer-quality".to_owned(),
        version: "1.0.0".to_owned(),
        criteria: vec![
            RubricCriterion {
                id: "correct".to_owned(),
                description: "The answer is factually correct".to_owned(),
            },
            RubricCriterion {
                id: "clear".to_owned(),
                description: "The answer is concise and clear".to_owned(),
            },
        ],
    }
}

fn task(id: &str, priority: i32, created_at_ms: u64) -> AnnotationTask {
    AnnotationTask::new(
        id,
        TraceRef::run(format!("run-{id}")),
        json!({"messages": [{"role": "user", "content": "What is 2+2?"}]}),
        vec![
            ReviewCandidate {
                id: "a".to_owned(),
                output: json!("4"),
            },
            ReviewCandidate {
                id: "b".to_owned(),
                output: json!("four"),
            },
        ],
        rubric(),
        priority,
        created_at_ms,
        2,
    )
    .unwrap()
}

fn submission(decision: ReviewDecision) -> ReviewSubmission {
    ReviewSubmission {
        decision,
        scores: BTreeMap::from([("clear".to_owned(), 1.0), ("correct".to_owned(), 1.0)]),
        comment: None,
    }
}

fn select(candidate_id: &str) -> ReviewSubmission {
    submission(ReviewDecision::Select {
        candidate_id: candidate_id.to_owned(),
    })
}

fn queue_with(task: AnnotationTask) -> AnnotationQueue {
    let mut queue = AnnotationQueue::new("production-review").unwrap();
    queue.enqueue(task).unwrap();
    queue
}

#[test]
fn claims_by_priority_then_age_and_is_idempotent() {
    let mut queue = AnnotationQueue::new("q").unwrap();
    queue.enqueue(task("low", 1, 1)).unwrap();
    queue.enqueue(task("new-high", 9, 20)).unwrap();
    queue.enqueue(task("old-high", 9, 10)).unwrap();

    let first = queue.claim("reviewer-1", 100, 50).unwrap().unwrap();
    assert_eq!(first.task_id, "old-high");
    assert_eq!(first.expires_at_ms, 150);

    let repeated = queue.claim("reviewer-1", 110, 999).unwrap().unwrap();
    assert_eq!(repeated, first);

    let second = queue.claim("reviewer-2", 100, 50).unwrap().unwrap();
    assert_eq!(second.task_id, "old-high");
    let third = queue.claim("reviewer-3", 100, 50).unwrap().unwrap();
    assert_eq!(third.task_id, "new-high");
}

#[test]
fn expired_leases_are_reclaimed_and_cannot_submit() {
    let mut queue = queue_with(task("t", 1, 1));
    let stale = queue.claim("slow", 100, 10).unwrap().unwrap();

    let error = queue.submit(&stale, 110, select("a")).unwrap_err();
    assert!(error.to_string().contains("expired"), "{error}");

    let replacement = queue.claim("fast", 110, 10).unwrap().unwrap();
    assert_eq!(replacement.task_id, "t");
}

#[test]
fn matching_reviews_resolve_by_consensus_and_promote() {
    let mut queue = queue_with(task("t", 1, 1));
    let r1 = queue.claim("r1", 10, 100).unwrap().unwrap();
    let r2 = queue.claim("r2", 10, 100).unwrap().unwrap();
    assert_eq!(
        queue.submit(&r1, 20, select("b")).unwrap(),
        AnnotationStatus::InReview
    );
    assert_eq!(
        queue.submit(&r2, 21, select("b")).unwrap(),
        AnnotationStatus::Resolved
    );

    let task = queue.task("t").unwrap();
    let resolution = task.resolution().unwrap();
    assert_eq!(resolution.authority, ResolutionAuthority::Consensus);
    let case = task.promote("reviewed-2-plus-2", "/answer").unwrap();
    assert_eq!(case.id, "reviewed-2-plus-2");
    assert_eq!(case.expect.state[0].pointer, "/answer");
    assert_eq!(case.expect.state[0].expected, json!("four"));
    assert!(case.tags.contains(&"feedback-task:t".to_owned()));
    assert!(case.tags.contains(&"source-run:run-t".to_owned()));
    assert!(case.tags.contains(&"consensus".to_owned()));
}

#[test]
fn disagreement_requires_adjudication_and_correction_can_promote() {
    let mut queue = queue_with(task("t", 1, 1));
    let r1 = queue.claim("r1", 10, 100).unwrap().unwrap();
    let r2 = queue.claim("r2", 10, 100).unwrap().unwrap();
    queue.submit(&r1, 20, select("a")).unwrap();
    let status = queue.submit(&r2, 21, select("b")).unwrap();
    assert_eq!(status, AnnotationStatus::NeedsAdjudication);
    assert!(queue.claim("r3", 22, 100).unwrap().is_none());

    queue
        .adjudicate(
            "t",
            "lead",
            30,
            ReviewDecision::Correct {
                output: json!({"answer": "4", "explanation": "two plus two"}),
            },
        )
        .unwrap();

    let task = queue.task("t").unwrap();
    assert_eq!(task.status_at(30), AnnotationStatus::Resolved);
    assert_eq!(
        task.resolution().unwrap().authority,
        ResolutionAuthority::Adjudicator {
            id: "lead".to_owned()
        }
    );
    assert_eq!(
        task.promote("corrected", "/output").unwrap().expect.state[0].expected,
        json!({"answer": "4", "explanation": "two plus two"})
    );
}

#[test]
fn rejected_feedback_cannot_promote() {
    let mut queue = queue_with(task("t", 1, 1));
    for reviewer in ["r1", "r2"] {
        let lease = queue.claim(reviewer, 10, 100).unwrap().unwrap();
        queue
            .submit(
                &lease,
                20,
                submission(ReviewDecision::Reject {
                    reason: Some("bad source".to_owned()),
                }),
            )
            .unwrap();
    }
    let error = queue.task("t").unwrap().promote("x", "/x").unwrap_err();
    assert!(error.to_string().contains("rejected"), "{error}");
}

#[test]
fn invalid_reviews_preserve_the_lease_for_retry() {
    let mut queue = queue_with(task("t", 1, 1));
    let lease = queue.claim("r1", 10, 100).unwrap().unwrap();
    let invalid = ReviewSubmission {
        decision: ReviewDecision::Select {
            candidate_id: "missing".to_owned(),
        },
        scores: BTreeMap::from([("correct".to_owned(), 2.0)]),
        comment: None,
    };
    let error = queue.submit(&lease, 20, invalid).unwrap_err();
    assert!(error.to_string().contains("no candidate"), "{error}");

    queue.submit(&lease, 21, select("a")).unwrap();
    assert_eq!(queue.task("t").unwrap().reviews().len(), 1);
}

#[test]
fn definitions_reject_ambiguous_candidates_and_rubrics() {
    let error = AnnotationTask::new(
        "t",
        TraceRef::run("run"),
        Value::Null,
        vec![ReviewCandidate {
            id: "only-one".to_owned(),
            output: Value::Null,
        }],
        rubric(),
        0,
        0,
        1,
    )
    .unwrap_err();
    assert!(error.to_string().contains("exactly two"), "{error}");

    let mut bad_rubric = rubric();
    bad_rubric.criteria[1].id = "correct".to_owned();
    let error = AnnotationTask::new(
        "t",
        TraceRef::run("run"),
        Value::Null,
        vec![
            ReviewCandidate {
                id: "a".to_owned(),
                output: Value::Null,
            },
            ReviewCandidate {
                id: "b".to_owned(),
                output: Value::Null,
            },
        ],
        bad_rubric,
        0,
        0,
        1,
    )
    .unwrap_err();
    assert!(error.to_string().contains("duplicate rubric"), "{error}");
}

#[test]
fn queue_json_round_trip_is_stable_and_versioned() {
    let queue = queue_with(task("t", 1, 1));
    let json = queue.to_json().unwrap();
    let loaded = AnnotationQueue::from_json(&json).unwrap();
    assert_eq!(loaded, queue);
    assert_eq!(loaded.to_json().unwrap(), json);

    let future = json.replacen(
        &format!("\"format_version\": {FEEDBACK_FORMAT_VERSION}"),
        "\"format_version\": 99",
        1,
    );
    let error = AnnotationQueue::from_json(&future).unwrap_err();
    assert!(matches!(
        error,
        EvalError::UnsupportedFeedbackVersion {
            found: 99,
            supported: FEEDBACK_FORMAT_VERSION
        }
    ));
}

#[test]
fn duplicate_tasks_and_unleased_reviews_are_rejected() {
    let mut queue = queue_with(task("t", 1, 1));
    let error = queue.enqueue(task("t", 2, 2)).unwrap_err();
    assert!(error.to_string().contains("duplicate"), "{error}");

    let fake_lease = ReviewLease {
        task_id: "t".to_owned(),
        reviewer: "r1".to_owned(),
        lease_id: 99,
        issued_at_ms: 1,
        expires_at_ms: 100,
    };
    let error = queue.submit(&fake_lease, 10, select("a")).unwrap_err();
    assert!(
        error.to_string().contains("does not hold a lease"),
        "{error}"
    );
}

#[test]
fn claims_wait_until_task_creation_time() {
    let mut queue = queue_with(task("future", 10, 1_000));
    assert!(queue.claim("r1", 999, 10).unwrap().is_none());
    assert_eq!(
        queue.claim("r1", 1_000, 10).unwrap().unwrap().task_id,
        "future"
    );
}

#[test]
fn reviews_require_complete_normalized_rubric_scores() {
    let mut queue = queue_with(task("t", 1, 1));
    let lease = queue.claim("r1", 10, 100).unwrap().unwrap();
    let missing = ReviewSubmission {
        decision: ReviewDecision::Select {
            candidate_id: "a".to_owned(),
        },
        scores: BTreeMap::from([("correct".to_owned(), 1.0)]),
        comment: None,
    };
    let error = queue.submit(&lease, 20, missing).unwrap_err();
    assert!(error.to_string().contains("exactly match"), "{error}");

    let mut out_of_range = select("a");
    out_of_range.scores.insert("clear".to_owned(), 1.01);
    let error = queue.submit(&lease, 20, out_of_range).unwrap_err();
    assert!(error.to_string().contains("between 0 and 1"), "{error}");
}

#[test]
fn promotion_validates_dataset_identity_and_json_pointer() {
    let mut queue = queue_with(task("t", 1, 1));
    for reviewer in ["r1", "r2"] {
        let lease = queue.claim(reviewer, 10, 100).unwrap().unwrap();
        queue.submit(&lease, 20, select("a")).unwrap();
    }
    let task = queue.task("t").unwrap();
    let error = task.promote(" ", "/answer").unwrap_err();
    assert!(error.to_string().contains("case id"), "{error}");
    let error = task.promote("case", "answer").unwrap_err();
    assert!(error.to_string().contains("JSON pointer"), "{error}");
    let error = task.promote("case", "/answer/~2").unwrap_err();
    assert!(
        error.to_string().contains("invalid RFC 6901 escape"),
        "{error}"
    );
}

#[test]
fn stale_worker_cannot_submit_through_a_replacement_lease() {
    let mut queue = queue_with(task("t", 1, 1));
    let stale = queue.claim("r1", 100, 10).unwrap().unwrap();
    let replacement = queue.claim("r1", 110, 20).unwrap().unwrap();
    assert_ne!(stale.lease_id, replacement.lease_id);

    let error = queue.submit(&stale, 111, select("a")).unwrap_err();
    assert!(error.to_string().contains("stale"), "{error}");
    queue.submit(&replacement, 111, select("a")).unwrap();
}

#[test]
fn loading_rejects_resolutions_that_bypass_required_reviews() {
    let queue = queue_with(task("t", 1, 1));
    let mut value: Value = serde_json::from_str(&queue.to_json().unwrap()).unwrap();
    value["tasks"]["t"]["resolution"] = json!({
        "decision": {"kind": "select", "candidate_id": "a"},
        "authority": {"kind": "consensus"},
        "resolved_at_ms": 10
    });

    let error = AnnotationQueue::from_json(&serde_json::to_string(&value).unwrap()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("before collecting required reviews"),
        "{error}"
    );
}

#[test]
fn loading_rejects_complete_consensus_without_a_resolution() {
    let mut queue = queue_with(task("t", 1, 1));
    for reviewer in ["r1", "r2"] {
        let lease = queue.claim(reviewer, 10, 100).unwrap().unwrap();
        queue.submit(&lease, 20, select("a")).unwrap();
    }
    let mut value: Value = serde_json::from_str(&queue.to_json().unwrap()).unwrap();
    value["tasks"]["t"]
        .as_object_mut()
        .unwrap()
        .remove("resolution");

    let error = AnnotationQueue::from_json(&serde_json::to_string(&value).unwrap()).unwrap_err();
    assert!(
        error.to_string().contains("consensus but no resolution"),
        "{error}"
    );
}

#[test]
fn loading_rejects_adjudication_of_unanimous_reviews() {
    let mut queue = queue_with(task("t", 1, 1));
    for reviewer in ["r1", "r2"] {
        let lease = queue.claim(reviewer, 10, 100).unwrap().unwrap();
        queue.submit(&lease, 20, select("a")).unwrap();
    }
    let mut value: Value = serde_json::from_str(&queue.to_json().unwrap()).unwrap();
    value["tasks"]["t"]["resolution"] = json!({
        "decision": {"kind": "select", "candidate_id": "b"},
        "authority": {"kind": "adjudicator", "id": "lead"},
        "resolved_at_ms": 21
    });

    let error = AnnotationQueue::from_json(&serde_json::to_string(&value).unwrap()).unwrap_err();
    assert!(
        error.to_string().contains("despite unanimous reviews"),
        "{error}"
    );
}

#[test]
fn future_version_is_reported_before_future_schema_is_parsed() {
    let error =
        AnnotationQueue::from_json(r#"{"format_version":99,"future_shape":true}"#).unwrap_err();
    assert!(matches!(
        error,
        EvalError::UnsupportedFeedbackVersion {
            found: 99,
            supported: FEEDBACK_FORMAT_VERSION
        }
    ));
}

#[test]
fn audit_timestamps_cannot_move_backward() {
    let mut queue = queue_with(task("t", 1, 1));
    let r1 = queue.claim("r1", 100, 100).unwrap().unwrap();
    let error = queue.submit(&r1, 99, select("a")).unwrap_err();
    assert!(error.to_string().contains("predates"), "{error}");
    queue.submit(&r1, 110, select("a")).unwrap();

    let r2 = queue.claim("r2", 111, 100).unwrap().unwrap();
    queue.submit(&r2, 120, select("b")).unwrap();
    let error = queue
        .adjudicate(
            "t",
            "lead",
            119,
            ReviewDecision::Select {
                candidate_id: "a".to_owned(),
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("predates"), "{error}");
}

#[test]
fn loading_rejects_one_reviewer_leased_to_multiple_tasks() {
    let mut queue = AnnotationQueue::new("q").unwrap();
    queue.enqueue(task("a", 2, 1)).unwrap();
    queue.enqueue(task("b", 1, 1)).unwrap();
    queue.claim("r1", 10, 100).unwrap().unwrap();
    queue.claim("r0", 10, 100).unwrap().unwrap();
    queue.claim("r2", 10, 100).unwrap().unwrap();

    let mut value: Value = serde_json::from_str(&queue.to_json().unwrap()).unwrap();
    let second_lease = value["tasks"]["b"]["leases"]
        .as_object_mut()
        .unwrap()
        .remove("r2")
        .unwrap();
    value["tasks"]["b"]["leases"]["r1"] = second_lease;

    let error = AnnotationQueue::from_json(&serde_json::to_string(&value).unwrap()).unwrap_err();
    assert!(
        error.to_string().contains("leases for multiple tasks"),
        "{error}"
    );
}
