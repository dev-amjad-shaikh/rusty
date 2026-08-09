//! ActivityWorker lifecycle tests against a mock rusty-agent-server task queue.
//!
//! The mock implements the R0.6 lease contract exactly (the shapes the
//! server implements in `rusty-server/src/routes.rs` + `tasks.rs`):
//!
//! - `POST /tasks/claim` `{worker_id, pools?, lease_ms}` →
//!   `200 {"task": {task record}}` | `204`
//! - `POST /tasks/{id}/heartbeat` `{worker_id, lease_ms}` →
//!   `200 {lease_expires_at, cancel_requested}` | `409`
//! - `POST /tasks/{id}/complete` `{worker_id, result}` → `200 {task}` | `409`
//! - `POST /tasks/{id}/fail` `{worker_id, error_class, message, retryable}`
//!   → `200 {requeued, next_attempt_at, dead}` | `409`
//!
//! Every call is recorded so tests can assert on the exact wire bodies.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, State as AxumState};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use rusty_agent_runtime::prelude::*;
use rusty_worker::{ActivityContext, ActivityWorker};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// One recorded call to the mock server: path and decoded JSON body.
#[derive(Debug, Clone)]
struct RecordedCall {
    path: String,
    body: Value,
}

/// All recorded calls whose path starts with `path_prefix`, in order.
fn calls_to<'a>(calls: &'a [RecordedCall], path_prefix: &str) -> Vec<&'a RecordedCall> {
    calls
        .iter()
        .filter(|c| c.path.starts_with(path_prefix))
        .collect()
}

#[derive(Default)]
struct MockState {
    /// Tasks handed out on claim, FIFO; empty → `204`.
    tasks: VecDeque<Value>,
    /// Every call the worker made, in order.
    calls: Vec<RecordedCall>,
    /// Heartbeats answered per task id.
    heartbeat_counts: HashMap<String, usize>,
    /// If set, heartbeats beyond this count answer `409` (lease lost).
    heartbeat_conflict_after: Option<usize>,
    /// If set, heartbeats beyond this count answer `200` carrying
    /// `cancel_requested: true` (control-plane cancellation).
    heartbeat_cancel_requested_after: Option<usize>,
}

/// A snapshot of the calls recorded so far (cloned, so no lock is held
/// across `.await` points in tests).
fn recorded(state: &Arc<Mutex<MockState>>) -> Vec<RecordedCall> {
    state.lock().unwrap().calls.clone()
}

fn decode_body(bytes: &Bytes) -> Value {
    serde_json::from_slice(bytes).unwrap_or(Value::Null)
}

/// The full task-record wire shape the server returns on claim, leased to
/// `owner` — every field of `TaskRecord::wire()` minus `tenant`, so a drift
/// in what the worker must tolerate shows up here first.
fn task_record(id: &str, kind: &str, payload: Value, owner: &str) -> Value {
    json!({
        "task_id": id,
        "kind": kind,
        "payload": payload,
        "pool": "default",
        "status": "leased",
        "attempt": 1,
        "max_attempts": 3,
        "error_class": null,
        "last_error": null,
        "idempotency_key": format!("test:{id}"),
        "result": null,
        "run_id": "run-1",
        "thread_id": "thread-1",
        "cancel_requested": false,
        "deadline": null,
        "lease": {"owner": owner, "expires_at": "2026-08-07T12:00:30Z"},
        "next_attempt_at": null,
        "created_at": "2026-08-07T12:00:00Z",
        "updated_at": "2026-08-07T12:00:00Z",
    })
}

async fn claim_handler(
    AxumState(state): AxumState<Arc<Mutex<MockState>>>,
    body: Bytes,
) -> Response {
    let mut state = state.lock().unwrap();
    let body = decode_body(&body);
    state.calls.push(RecordedCall {
        path: "/tasks/claim".to_string(),
        body: body.clone(),
    });
    match state.tasks.pop_front() {
        Some(mut task) => {
            // The server leases the record to the claiming worker.
            task["lease"]["owner"] = body["worker_id"].clone();
            (StatusCode::OK, Json(json!({ "task": task }))).into_response()
        }
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn heartbeat_handler(
    AxumState(state): AxumState<Arc<Mutex<MockState>>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let mut state = state.lock().unwrap();
    state.calls.push(RecordedCall {
        path: format!("/tasks/{id}/heartbeat"),
        body: decode_body(&body),
    });
    let count = state.heartbeat_counts.entry(id).or_insert(0);
    *count += 1;
    let count = *count;
    let conflict = state
        .heartbeat_conflict_after
        .is_some_and(|after| count > after);
    let cancel_requested = state
        .heartbeat_cancel_requested_after
        .is_some_and(|after| count > after);
    if conflict {
        StatusCode::CONFLICT.into_response()
    } else {
        (
            StatusCode::OK,
            Json(json!({
                "lease_expires_at": "2026-08-07T12:00:30Z",
                "cancel_requested": cancel_requested,
            })),
        )
            .into_response()
    }
}

async fn complete_handler(
    AxumState(state): AxumState<Arc<Mutex<MockState>>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let mut state = state.lock().unwrap();
    state.calls.push(RecordedCall {
        path: format!("/tasks/{id}/complete"),
        body: decode_body(&body),
    });
    // The server answers with the updated task record; the worker ignores it.
    (
        StatusCode::OK,
        Json(task_record(&id, "any", Value::Null, "w-test")),
    )
        .into_response()
}

async fn fail_handler(
    AxumState(state): AxumState<Arc<Mutex<MockState>>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let mut state = state.lock().unwrap();
    state.calls.push(RecordedCall {
        path: format!("/tasks/{id}/fail"),
        body: decode_body(&body),
    });
    (
        StatusCode::OK,
        Json(json!({
            "requeued": true,
            "next_attempt_at": "2026-08-07T12:01:00Z",
            "dead": false,
        })),
    )
        .into_response()
}

/// Start the mock task queue on an ephemeral port; returns the base URL.
async fn start_mock(state: Arc<Mutex<MockState>>) -> String {
    let app = Router::new()
        .route("/tasks/claim", post(claim_handler))
        .route("/tasks/{id}/heartbeat", post(heartbeat_handler))
        .route("/tasks/{id}/complete", post(complete_handler))
        .route("/tasks/{id}/fail", post(fail_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("mock task queue");
    });
    format!("http://{addr}")
}

/// A worker with test-sized timing: 100 ms lease (the server's minimum;
/// heartbeats every ~33 ms) and near-instant claim backoff.
fn test_worker(base_url: &str) -> ActivityWorker {
    ActivityWorker::new(base_url)
        .with_worker_id("w-test")
        .with_lease(Duration::from_millis(100))
        .with_claim_backoff(Duration::from_millis(5), Duration::from_millis(20))
}

/// Poll `cond` until it holds; fail the test after `timeout`.
async fn wait_for(what: &str, timeout: Duration, cond: impl Fn() -> bool) {
    let started = std::time::Instant::now();
    loop {
        if cond() {
            return;
        }
        assert!(started.elapsed() < timeout, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Run a worker in the background; the returned handle finishes once the
/// token is cancelled and in-flight work has drained.
fn spawn_worker(
    worker: ActivityWorker,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move { worker.run(shutdown).await })
}

async fn stop_worker(shutdown: CancellationToken, handle: tokio::task::JoinHandle<()>) {
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("worker drains within 5 s")
        .expect("worker task did not panic");
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn claim_execute_complete_happy_path() {
    let state = Arc::new(Mutex::new(MockState::default()));
    state
        .lock()
        .unwrap()
        .tasks
        .push_back(task_record("task-1", "doubler", json!({"n": 21}), ""));
    let base_url = start_mock(state.clone()).await;

    let worker = test_worker(&base_url).register("doubler", |ctx: ActivityContext| async move {
        let n = ctx.payload()["n"].as_i64().unwrap();
        // The claimed task id is the side-effect correlation handle.
        let task_id = ctx.task_id().to_string();
        let idempotency_key = ctx.idempotency_key().map(str::to_owned);
        Ok(json!({
            "doubled": n * 2,
            "task_id": task_id,
            "idempotency_key": idempotency_key,
        }))
    });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("complete call", Duration::from_secs(5), || {
        !calls_to(&recorded(&state), "/tasks/task-1/complete").is_empty()
    })
    .await;

    let calls = recorded(&state);

    let claim = calls_to(&calls, "/tasks/claim")[0];
    assert_eq!(
        claim.body,
        json!({"worker_id": "w-test", "lease_ms": 100}),
        "claim body must omit pools when unconfigured"
    );

    let complete = calls_to(&calls, "/tasks/task-1/complete")[0];
    assert_eq!(complete.body["worker_id"], json!("w-test"));
    assert_eq!(complete.body["result"]["doubled"], json!(42));
    assert_eq!(complete.body["result"]["task_id"], json!("task-1"));
    assert_eq!(
        complete.body["result"]["idempotency_key"],
        json!("test:task-1")
    );
    // No fail call happened.
    assert!(calls_to(&calls, "/tasks/task-1/fail").is_empty());

    stop_worker(shutdown, handle).await;
}

#[tokio::test]
async fn claim_sends_pools_when_configured() {
    let state = Arc::new(Mutex::new(MockState::default()));
    let base_url = start_mock(state.clone()).await;

    let worker = test_worker(&base_url).with_pools(["gpu", "email"]);
    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("two claim polls", Duration::from_secs(5), || {
        calls_to(&recorded(&state), "/tasks/claim").len() >= 2
    })
    .await;

    let calls = recorded(&state);
    let claim = calls_to(&calls, "/tasks/claim")[0];
    assert_eq!(
        claim.body,
        json!({"worker_id": "w-test", "pools": ["gpu", "email"], "lease_ms": 100})
    );

    stop_worker(shutdown, handle).await;
}

#[tokio::test]
async fn heartbeats_keep_the_lease_until_completion() {
    let state = Arc::new(Mutex::new(MockState::default()));
    state
        .lock()
        .unwrap()
        .tasks
        .push_back(task_record("task-2", "slow", json!({}), ""));
    let base_url = start_mock(state.clone()).await;

    let worker = test_worker(&base_url).register("slow", |_ctx: ActivityContext| async {
        // 200 ms of work under a 100 ms lease: without heartbeats at
        // lease / 3 the lease would lapse mid-activity.
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(json!({"done": true}))
    });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("complete call", Duration::from_secs(5), || {
        !calls_to(&recorded(&state), "/tasks/task-2/complete").is_empty()
    })
    .await;

    let calls = recorded(&state);
    let heartbeats = calls_to(&calls, "/tasks/task-2/heartbeat");
    assert!(
        heartbeats.len() >= 2,
        "expected several heartbeats during 200 ms of work, got {}",
        heartbeats.len()
    );
    assert_eq!(
        heartbeats[0].body,
        json!({"worker_id": "w-test", "lease_ms": 100})
    );
    // The lease was never lost: no 409-driven abort, task completed.
    assert!(calls_to(&calls, "/tasks/task-2/fail").is_empty());

    stop_worker(shutdown, handle).await;
}

/// Drop guard proving the handler future was really dropped (aborted), not
/// just detached.
struct AbortProbe(Arc<AtomicBool>);

impl Drop for AbortProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn conflict_on_heartbeat_aborts_the_activity() {
    let state = Arc::new(Mutex::new(MockState {
        // First heartbeat renews; the second answers 409 (lease lost).
        heartbeat_conflict_after: Some(1),
        ..MockState::default()
    }));
    state
        .lock()
        .unwrap()
        .tasks
        .push_back(task_record("task-3", "stuck", json!({}), ""));
    let base_url = start_mock(state.clone()).await;

    let aborted = Arc::new(AtomicBool::new(false));
    let probe = aborted.clone();
    let worker = test_worker(&base_url).register("stuck", move |_ctx: ActivityContext| {
        let probe = probe.clone();
        async move {
            let _guard = AbortProbe(probe);
            let () = std::future::pending().await;
            #[allow(unreachable_code)]
            Ok(Value::Null)
        }
    });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("handler abort", Duration::from_secs(5), || {
        aborted.load(Ordering::SeqCst)
    })
    .await;
    wait_for("worker to resume claiming", Duration::from_secs(5), || {
        calls_to(&recorded(&state), "/tasks/claim").len() >= 2
    })
    .await;

    let calls = recorded(&state);
    // Lease lost: the worker must NOT settle — the server owns the task.
    assert!(calls_to(&calls, "/tasks/task-3/complete").is_empty());
    assert!(calls_to(&calls, "/tasks/task-3/fail").is_empty());
    // The abort was triggered by the second heartbeat's 409, i.e. the first
    // heartbeat had renewed the lease successfully before it was lost.
    let conflicted = state
        .lock()
        .unwrap()
        .heartbeat_counts
        .get("task-3")
        .is_some_and(|count| *count >= 2);
    assert!(
        conflicted,
        "expected the 409 heartbeat to have been answered"
    );

    stop_worker(shutdown, handle).await;
}

#[tokio::test]
async fn drain_finishes_in_flight_work_then_exits() {
    let state = Arc::new(Mutex::new(MockState::default()));
    state
        .lock()
        .unwrap()
        .tasks
        .push_back(task_record("task-4", "gate", json!({}), ""));
    let base_url = start_mock(state.clone()).await;

    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let (started2, release2) = (started.clone(), release.clone());
    let worker = test_worker(&base_url).register("gate", move |_ctx: ActivityContext| {
        let (started, release) = (started2.clone(), release2.clone());
        async move {
            started.store(true, Ordering::SeqCst);
            release.notified().await;
            Ok(json!({"done": true}))
        }
    });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("activity in flight", Duration::from_secs(5), || {
        started.load(Ordering::SeqCst)
    })
    .await;

    // Initiate drain mid-activity: the worker must stop taking new leases
    // but keep running (and then settle) the in-flight task.
    shutdown.cancel();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !handle.is_finished(),
        "worker exited while an activity was still in flight"
    );

    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("worker drains within 5 s of the activity finishing")
        .expect("worker task did not panic");

    let calls = recorded(&state);
    // Exactly the in-flight claim happened: no new leases after shutdown.
    assert_eq!(calls_to(&calls, "/tasks/claim").len(), 1);
    // The in-flight activity ran to its outcome and was settled.
    let complete = calls_to(&calls, "/tasks/task-4/complete")[0];
    assert_eq!(complete.body["result"]["done"], json!(true));
}

#[tokio::test]
async fn failure_classification_maps_to_the_fail_call() {
    let state = Arc::new(Mutex::new(MockState::default()));
    {
        let mut s = state.lock().unwrap();
        s.tasks
            .push_back(task_record("task-5a", "tool_failer", json!({}), ""));
        s.tasks
            .push_back(task_record("task-5b", "hard_failer", json!({}), ""));
    }
    let base_url = start_mock(state.clone()).await;

    let worker = test_worker(&base_url)
        .register("tool_failer", |_ctx: ActivityContext| async {
            Err(RustyError::Tool("backend exploded".into()))
        })
        .register("hard_failer", |_ctx: ActivityContext| async {
            Err(RustyError::Node("bad input".into()))
        });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("both fail calls", Duration::from_secs(5), || {
        let calls = recorded(&state);
        !calls_to(&calls, "/tasks/task-5a/fail").is_empty()
            && !calls_to(&calls, "/tasks/task-5b/fail").is_empty()
    })
    .await;

    let calls = recorded(&state);

    // Tool failures are the transient executor class: an upstream
    // dependency failure, retryable.
    let tool_fail = calls_to(&calls, "/tasks/task-5a/fail")[0];
    assert_eq!(tool_fail.body["worker_id"], json!("w-test"));
    assert_eq!(tool_fail.body["error_class"], json!("dependency_failure"));
    assert_eq!(tool_fail.body["retryable"], json!(true));
    assert!(
        tool_fail.body["message"]
            .as_str()
            .unwrap()
            .contains("backend exploded"),
        "unexpected message: {}",
        tool_fail.body["message"]
    );

    // Everything else is a hard failure: unclassified, non-retryable.
    let hard_fail = calls_to(&calls, "/tasks/task-5b/fail")[0];
    assert_eq!(hard_fail.body["error_class"], json!("unknown"));
    assert_eq!(hard_fail.body["retryable"], json!(false));
    assert!(
        hard_fail.body["message"]
            .as_str()
            .unwrap()
            .contains("bad input"),
        "unexpected message: {}",
        hard_fail.body["message"]
    );

    // Failures settle via /fail only: no complete calls.
    assert!(calls_to(&calls, "/tasks/task-5a/complete").is_empty());
    assert!(calls_to(&calls, "/tasks/task-5b/complete").is_empty());

    stop_worker(shutdown, handle).await;
}

#[tokio::test]
async fn interrupt_error_settles_as_non_retryable_cancellation() {
    let state = Arc::new(Mutex::new(MockState::default()));
    state
        .lock()
        .unwrap()
        .tasks
        .push_back(task_record("task-6", "gate", json!({}), ""));
    let base_url = start_mock(state.clone()).await;

    let worker = test_worker(&base_url).register("gate", |_ctx: ActivityContext| async {
        // The task-queue protocol has no suspend semantics (HITL wiring is
        // the run-outbox wave's concern): an interrupt settles as a
        // non-retryable cancellation.
        Err(RustyError::Interrupt {
            value: json!({"question": "approve?"}),
        })
    });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("fail call", Duration::from_secs(5), || {
        !calls_to(&recorded(&state), "/tasks/task-6/fail").is_empty()
    })
    .await;

    let calls = recorded(&state);
    let fail = calls_to(&calls, "/tasks/task-6/fail")[0];
    assert_eq!(fail.body["error_class"], json!("cancelled"));
    assert_eq!(fail.body["retryable"], json!(false));
    assert!(calls_to(&calls, "/tasks/task-6/complete").is_empty());

    stop_worker(shutdown, handle).await;
}

#[tokio::test]
async fn handler_panic_fails_as_a_non_retryable_unknown_error() {
    let state = Arc::new(Mutex::new(MockState::default()));
    state
        .lock()
        .unwrap()
        .tasks
        .push_back(task_record("task-7", "panicker", json!({}), ""));
    let base_url = start_mock(state.clone()).await;

    let worker = test_worker(&base_url).register("panicker", |_ctx: ActivityContext| async {
        panic!("kaboom");
    });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("fail call", Duration::from_secs(5), || {
        !calls_to(&recorded(&state), "/tasks/task-7/fail").is_empty()
    })
    .await;

    let calls = recorded(&state);
    let fail = calls_to(&calls, "/tasks/task-7/fail")[0];
    assert_eq!(fail.body["error_class"], json!("unknown"));
    assert_eq!(fail.body["retryable"], json!(false));
    let message = fail.body["message"].as_str().unwrap();
    assert!(
        message.contains("panicked") && message.contains("kaboom"),
        "unexpected message: {message}"
    );

    stop_worker(shutdown, handle).await;
}

#[tokio::test]
async fn unknown_kind_fails_as_invalid_input_without_running() {
    let state = Arc::new(Mutex::new(MockState::default()));
    state
        .lock()
        .unwrap()
        .tasks
        .push_back(task_record("task-9", "ghost", json!({}), ""));
    let base_url = start_mock(state.clone()).await;

    let worker =
        test_worker(&base_url).register("alive", |_ctx: ActivityContext| async { Ok(Value::Null) });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("fail call", Duration::from_secs(5), || {
        !calls_to(&recorded(&state), "/tasks/task-9/fail").is_empty()
    })
    .await;

    let calls = recorded(&state);
    let fail = calls_to(&calls, "/tasks/task-9/fail")[0];
    assert_eq!(fail.body["error_class"], json!("invalid_input"));
    assert_eq!(fail.body["retryable"], json!(false));
    assert!(
        fail.body["message"]
            .as_str()
            .unwrap()
            .contains("no activity registered for kind `ghost`"),
        "unexpected message: {}",
        fail.body["message"]
    );
    // The task was settled immediately: no lease renewals were needed.
    assert!(calls_to(&calls, "/tasks/task-9/heartbeat").is_empty());

    stop_worker(shutdown, handle).await;
}

#[tokio::test]
async fn undecodable_claim_body_is_not_settled_and_polling_continues() {
    let state = Arc::new(Mutex::new(MockState::default()));
    // Server contract violation: a claimed task with no task_id cannot be
    // addressed (heartbeats/settles are keyed by id) and must not decode.
    state.lock().unwrap().tasks.push_back(json!({
        "kind": "doubler",
        "payload": {"n": 21},
    }));
    let base_url = start_mock(state.clone()).await;

    let worker = test_worker(&base_url)
        .register("doubler", |_ctx: ActivityContext| async { Ok(Value::Null) });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    // The worker logs the violation, drops the task, and keeps claiming.
    wait_for("worker to resume claiming", Duration::from_secs(5), || {
        calls_to(&recorded(&state), "/tasks/claim").len() >= 2
    })
    .await;

    let calls = recorded(&state);
    assert!(calls.iter().all(|c| c.path == "/tasks/claim"));

    stop_worker(shutdown, handle).await;
}

// ---------------------------------------------------------------------------
// Cancellation propagation (R0.6 wave 2a)

#[tokio::test]
async fn cancel_requested_on_heartbeat_aborts_and_reports_cancelled() {
    let state = Arc::new(Mutex::new(MockState {
        // First heartbeat renews; subsequent ones carry the cancel hint.
        heartbeat_cancel_requested_after: Some(1),
        ..MockState::default()
    }));
    state
        .lock()
        .unwrap()
        .tasks
        .push_back(task_record("task-8", "stuck", json!({}), ""));
    let base_url = start_mock(state.clone()).await;

    let aborted = Arc::new(AtomicBool::new(false));
    let probe = aborted.clone();
    let worker = test_worker(&base_url).register("stuck", move |_ctx: ActivityContext| {
        let probe = probe.clone();
        async move {
            let _guard = AbortProbe(probe);
            let () = std::future::pending().await;
            #[allow(unreachable_code)]
            Ok(Value::Null)
        }
    });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    // The handler future is really dropped (not detached)...
    wait_for("handler abort", Duration::from_secs(5), || {
        aborted.load(Ordering::SeqCst)
    })
    .await;
    // ...and promptly: the abort follows the second heartbeat (~66 ms of
    // a 100 ms lease), not the lease's expiry.
    wait_for("cancelled fail call", Duration::from_secs(5), || {
        !calls_to(&recorded(&state), "/tasks/task-8/fail").is_empty()
    })
    .await;

    let calls = recorded(&state);
    // Unlike lease loss, a cancelled attempt is settled through the fail
    // path while the worker still holds the lease: error_class cancelled,
    // never retryable (the server maps it to terminal-cancelled, not DLQ).
    let fail = calls_to(&calls, "/tasks/task-8/fail")[0];
    assert_eq!(fail.body["worker_id"], json!("w-test"));
    assert_eq!(fail.body["error_class"], json!("cancelled"));
    assert_eq!(fail.body["retryable"], json!(false));
    assert!(calls_to(&calls, "/tasks/task-8/complete").is_empty());

    stop_worker(shutdown, handle).await;
}

#[tokio::test]
async fn deadline_already_passed_reports_cancelled_without_running() {
    let state = Arc::new(Mutex::new(MockState::default()));
    let mut task = task_record("task-10", "late", json!({}), "");
    task["deadline"] = json!("2020-01-01T00:00:00Z");
    state.lock().unwrap().tasks.push_back(task);
    let base_url = start_mock(state.clone()).await;

    let started = Arc::new(AtomicBool::new(false));
    let flag = started.clone();
    let worker = test_worker(&base_url).register("late", move |_ctx: ActivityContext| {
        let flag = flag.clone();
        async move {
            flag.store(true, Ordering::SeqCst);
            Ok(Value::Null)
        }
    });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("cancelled fail call", Duration::from_secs(5), || {
        !calls_to(&recorded(&state), "/tasks/task-10/fail").is_empty()
    })
    .await;

    let calls = recorded(&state);
    let fail = calls_to(&calls, "/tasks/task-10/fail")[0];
    assert_eq!(fail.body["error_class"], json!("cancelled"));
    assert_eq!(fail.body["retryable"], json!(false));
    assert!(
        fail.body["message"]
            .as_str()
            .unwrap()
            .contains("deadline already passed"),
        "unexpected message: {}",
        fail.body["message"]
    );
    // The handler never ran, and no heartbeats were needed.
    assert!(!started.load(Ordering::SeqCst));
    assert!(calls_to(&calls, "/tasks/task-10/heartbeat").is_empty());
    assert!(calls_to(&calls, "/tasks/task-10/complete").is_empty());

    stop_worker(shutdown, handle).await;
}

#[tokio::test]
async fn deadline_expiring_mid_attempt_aborts_and_reports_cancelled() {
    let state = Arc::new(Mutex::new(MockState::default()));
    let mut task = task_record("task-11", "slow", json!({}), "");
    // 300 ms of headroom: long enough to start, far short of the handler.
    let deadline = chrono::Utc::now() + chrono::Duration::milliseconds(300);
    task["deadline"] = json!(deadline.to_rfc3339());
    state.lock().unwrap().tasks.push_back(task);
    let base_url = start_mock(state.clone()).await;

    let aborted = Arc::new(AtomicBool::new(false));
    let probe = aborted.clone();
    let worker = test_worker(&base_url).register("slow", move |_ctx: ActivityContext| {
        let probe = probe.clone();
        async move {
            let _guard = AbortProbe(probe);
            let () = std::future::pending().await;
            #[allow(unreachable_code)]
            Ok(Value::Null)
        }
    });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for(
        "handler abort at the deadline",
        Duration::from_secs(5),
        || aborted.load(Ordering::SeqCst),
    )
    .await;
    wait_for("cancelled fail call", Duration::from_secs(5), || {
        !calls_to(&recorded(&state), "/tasks/task-11/fail").is_empty()
    })
    .await;

    let calls = recorded(&state);
    let fail = calls_to(&calls, "/tasks/task-11/fail")[0];
    assert_eq!(fail.body["error_class"], json!("cancelled"));
    assert_eq!(fail.body["retryable"], json!(false));
    assert!(
        fail.body["message"]
            .as_str()
            .unwrap()
            .contains("deadline expired mid-attempt"),
        "unexpected message: {}",
        fail.body["message"]
    );
    assert!(calls_to(&calls, "/tasks/task-11/complete").is_empty());

    stop_worker(shutdown, handle).await;
}

// ---------------------------------------------------------------------------
// Drain (R0.6 wave 2c)

#[tokio::test]
async fn drain_grace_exceeded_aborts_and_releases_the_task_for_reassignment() {
    let state = Arc::new(Mutex::new(MockState::default()));
    state
        .lock()
        .unwrap()
        .tasks
        .push_back(task_record("task-20", "stuck", json!({}), ""));
    let base_url = start_mock(state.clone()).await;

    let started = Arc::new(AtomicBool::new(false));
    let aborted = Arc::new(AtomicBool::new(false));
    let (started2, aborted2) = (started.clone(), aborted.clone());
    let worker = test_worker(&base_url)
        // A short grace: the stuck handler must outlive it.
        .with_drain_grace(Duration::from_millis(150))
        .register("stuck", move |_ctx: ActivityContext| {
            let (started, aborted) = (started2.clone(), aborted2.clone());
            async move {
                started.store(true, Ordering::SeqCst);
                let _guard = AbortProbe(aborted);
                let () = std::future::pending().await;
                #[allow(unreachable_code)]
                Ok(Value::Null)
            }
        });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("activity in flight", Duration::from_secs(5), || {
        started.load(Ordering::SeqCst)
    })
    .await;

    // Drain mid-attempt: the grace (150 ms) — not the handler — bounds how
    // long the worker lingers.
    shutdown.cancel();
    wait_for(
        "handler abort at grace expiry",
        Duration::from_secs(5),
        || aborted.load(Ordering::SeqCst),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("worker exits promptly after the grace abort")
        .expect("worker task did not panic");

    let calls = recorded(&state);
    // The attempt was left UNSETTLED: no complete, and crucially no fail —
    // reporting `cancelled` would have killed the task. The server will
    // reassign it at lease expiry.
    assert!(calls_to(&calls, "/tasks/task-20/complete").is_empty());
    assert!(calls_to(&calls, "/tasks/task-20/fail").is_empty());
    assert_eq!(calls_to(&calls, "/tasks/claim").len(), 1);

    // Reassignment: the task returns to visibility (the mock stands in for
    // the server's lease expiry) and a worker that is not draining claims
    // and completes it.
    state
        .lock()
        .unwrap()
        .tasks
        .push_back(task_record("task-20", "stuck", json!({}), ""));
    let replacement = test_worker(&base_url)
        .with_worker_id("w-other")
        .register("stuck", |_ctx: ActivityContext| async {
            Ok(json!({"done": true}))
        });
    let shutdown_b = CancellationToken::new();
    let handle_b = spawn_worker(replacement, shutdown_b.clone());

    wait_for(
        "complete by the replacement worker",
        Duration::from_secs(5),
        || !calls_to(&recorded(&state), "/tasks/task-20/complete").is_empty(),
    )
    .await;
    let calls = recorded(&state);
    let complete = calls_to(&calls, "/tasks/task-20/complete")[0];
    assert_eq!(complete.body["worker_id"], json!("w-other"));
    assert_eq!(complete.body["result"]["done"], json!(true));

    stop_worker(shutdown_b, handle_b).await;
}

#[tokio::test]
async fn drain_is_idempotent_and_a_pre_drained_worker_never_claims() {
    let state = Arc::new(Mutex::new(MockState::default()));
    state
        .lock()
        .unwrap()
        .tasks
        .push_back(task_record("task-21", "doubler", json!({"n": 21}), ""));
    let base_url = start_mock(state.clone()).await;

    let worker = test_worker(&base_url)
        .register("doubler", |_ctx: ActivityContext| async { Ok(Value::Null) });

    // Cancelling twice (and thrice) must be exactly as effective as once:
    // drain is a state, not an event.
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    shutdown.cancel();
    let handle = spawn_worker(worker, shutdown.clone());
    shutdown.cancel();

    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("a pre-drained worker exits immediately")
        .expect("worker task did not panic");

    // Work was queued and available, yet no claim ever happened.
    let calls = recorded(&state);
    assert!(calls.is_empty(), "expected no calls at all, got: {calls:?}");
}

#[tokio::test]
async fn no_new_claims_after_drain_starts_even_with_tasks_queued() {
    let state = Arc::new(Mutex::new(MockState::default()));
    {
        let mut s = state.lock().unwrap();
        s.tasks
            .push_back(task_record("task-22a", "gate", json!({}), ""));
        s.tasks
            .push_back(task_record("task-22b", "gate", json!({}), ""));
    }
    let base_url = start_mock(state.clone()).await;

    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(tokio::sync::Notify::new());
    let (started2, release2) = (started.clone(), release.clone());
    let worker = test_worker(&base_url).register("gate", move |_ctx: ActivityContext| {
        let (started, release) = (started2.clone(), release2.clone());
        async move {
            started.store(true, Ordering::SeqCst);
            release.notified().await;
            Ok(json!({"done": true}))
        }
    });

    let shutdown = CancellationToken::new();
    let handle = spawn_worker(worker, shutdown.clone());

    wait_for("first activity in flight", Duration::from_secs(5), || {
        started.load(Ordering::SeqCst)
    })
    .await;

    // Drain while task-22b sits queued behind the in-flight task-22a.
    shutdown.cancel();
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("worker drains within 5 s of the activity finishing")
        .expect("worker task did not panic");

    let calls = recorded(&state);
    // Exactly the in-flight claim: the queued task was never taken.
    assert_eq!(calls_to(&calls, "/tasks/claim").len(), 1);
    assert!(!calls_to(&calls, "/tasks/task-22a/complete").is_empty());
    assert!(calls_to(&calls, "/tasks/task-22b/complete").is_empty());
    // The queued task is still waiting in the mock's queue for the next
    // worker (real servers re-expose it immediately — it was never leased).
    assert_eq!(state.lock().unwrap().tasks.len(), 1);
}
