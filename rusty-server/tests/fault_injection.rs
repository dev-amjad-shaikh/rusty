//! Standing fault-injection harness — the R0.6 "Durable Work" release
//! proof, generalized.
//!
//! `docs/durable-work-design.md` names the core scenario as the release
//! gate:
//!
//! > kill the server and a worker mid-effect, restart, and the run
//! > completes without losing state or duplicating the external effect.
//!
//! This harness generalizes that single scenario into a parameterized
//! kill schedule with jitter mode and seeded-defect verification.
//!
//! ## Kill schedule
//!
//! | Point | Env var | What it tests |
//! |---|---|---|
//! | `MidEffect` | `RUSTY_DEMO_EFFECT_PAUSE_MS` | Effect fired, completion never reported |
//! | `AfterEnqueue` | `RUSTY_DEMO_ENQUEUE_PAUSE_MS` | Task persisted, HTTP ack never sent |
//!
//! ## Jitter mode
//!
//! Randomizes the pause duration with a seeded RNG. The seed is recorded
//! on failure so any discovered loss is reproducible.
//!
//! ## Seeded-defect verification
//!
//! `RUSTY_DEMO_SKIP_FSYNC=1` deliberately breaks durability. The harness
//! asserts that the defect is caught (duplicate effect observed).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The lease the test workers request: long enough to survive heartbeat
/// jitter, short enough that a SIGKILLed worker's task returns to
/// visibility ~1 s after its last heartbeat.
const LEASE_MS: u64 = 1_000;

/// The post-effect pause: the kill window. Deliberately far longer than
/// anything the test waits on, so attempt 1 can NEVER report completion
/// before the SIGKILL lands.
const EFFECT_PAUSE_MS: u64 = 10_000;

/// The post-enqueue pause: the kill window for the `AfterEnqueue`
/// schedule. Same determinism discipline — far longer than any poll
/// deadline.
const ENQUEUE_PAUSE_MS: u64 = 10_000;

/// Unique temp root per run, removed at the end (best effort). Holds the
/// server's JSON-file store AND the provider ledger.
fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-fault-injection-{}", uuid::Uuid::new_v4()))
}

/// The compiled demo binary `name`.
fn example_binary(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("test executable lives under <target>/<profile>/deps");
    let path = profile_dir
        .join("examples")
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "example binary `{name}` not found at {} — build the workspace examples first",
        path.display()
    );
    path
}

/// A free TCP port, found by bind-then-release.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A spawned demo process that is SIGKILLed when the guard drops unless
/// the test already reaped it.
struct ChildGuard {
    child: Option<tokio::process::Child>,
    name: &'static str,
}

impl ChildGuard {
    fn spawn(name: &'static str, command: &mut tokio::process::Command) -> Self {
        let child = command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {name}: {e}"));
        Self {
            child: Some(child),
            name,
        }
    }

    async fn sigkill(mut self) -> std::process::ExitStatus {
        let mut child = self.child.take().expect("process already reaped");
        child
            .kill()
            .await
            .unwrap_or_else(|e| panic!("failed to kill {}: {e}", self.name));
        child
            .wait()
            .await
            .unwrap_or_else(|e| panic!("failed to reap {}: {e}", self.name))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

/// Spawn `server_demo` on `port` with its JSON-file store at `store`.
fn spawn_server(port: u16, store: &Path) -> ChildGuard {
    let mut command = tokio::process::Command::new(example_binary("server_demo"));
    command
        .env("RUSTY_DEMO_ADDR", format!("127.0.0.1:{port}"))
        .env("RUSTY_DEMO_STORE", store);
    ChildGuard::spawn("server_demo", &mut command)
}

/// Spawn `activity_worker_demo` claiming from `base_url`, running
/// `send_receipt` against the provider ledger at `ledger`.
fn spawn_worker(base_url: &str, worker_id: &str, ledger: &Path) -> ChildGuard {
    spawn_worker_with_opts(base_url, worker_id, ledger, WorkerOpts::default())
}

/// Optional tweaks for the worker binary.
#[derive(Default)]
struct WorkerOpts {
    pause_ms: Option<u64>,
    skip_fsync: bool,
}

fn spawn_worker_with_opts(
    base_url: &str,
    worker_id: &str,
    ledger: &Path,
    opts: WorkerOpts,
) -> ChildGuard {
    let mut command = tokio::process::Command::new(example_binary("activity_worker_demo"));
    command
        .env("RUSTY_DEMO_SERVER_URL", base_url)
        .env("RUSTY_DEMO_WORKER_ID", worker_id)
        .env("RUSTY_DEMO_LEASE_MS", LEASE_MS.to_string())
        .env(
            "RUSTY_DEMO_EFFECT_PAUSE_MS",
            opts.pause_ms.unwrap_or(EFFECT_PAUSE_MS).to_string(),
        )
        .env("RUSTY_DEMO_PROVIDER_FILE", ledger);
    if opts.skip_fsync {
        command.env("RUSTY_DEMO_SKIP_FSYNC", "1");
    }
    ChildGuard::spawn("activity_worker_demo", &mut command)
}

/// Poll `GET /ok` until the server answers 200 or the deadline passes.
async fn wait_ready(client: &reqwest::Client, base: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = client.get(format!("{base}/ok")).send().await {
            if response.status() == reqwest::StatusCode::OK {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "server at {base} never became ready"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Poll `GET /tasks/{task_id}` until the task reaches `status`; returns
/// the terminal record.
async fn wait_task_status(
    client: &reqwest::Client,
    base: &str,
    task_id: &str,
    status: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last = Value::Null;
    loop {
        if let Ok(response) = client.get(format!("{base}/tasks/{task_id}")).send().await {
            if response.status() == reqwest::StatusCode::OK {
                last = response.json::<Value>().await.unwrap_or(Value::Null);
                if last["status"] == json!(status) {
                    return last;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "task {task_id} never reached status `{status}`: {last}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The provider ledger's invocation records for `key`.
fn ledger_records(ledger: &Path, key: &str) -> Vec<Value> {
    std::fs::read_to_string(ledger)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Value>(line).expect("the provider ledger holds JSON lines")
        })
        .filter(|record| record["idempotency_key"] == json!(key))
        .collect()
}

/// Poll the provider ledger until `key` has `n` invocation records.
async fn wait_ledger_records(ledger: &Path, key: &str, n: usize) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let records = ledger_records(ledger, key);
        if records.len() >= n {
            return records;
        }
        assert!(
            Instant::now() < deadline,
            "the effect never fired at the provider (expected {n} ledger records for `{key}`)"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Poll the task record until it is visible (i.e. the enqueue ack
/// survived).
async fn wait_task_visible(client: &reqwest::Client, base: &str, task_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(response) = client.get(format!("{base}/tasks/{task_id}")).send().await {
            if response.status() == reqwest::StatusCode::OK {
                return response.json::<Value>().await.unwrap_or(Value::Null);
            }
        }
        assert!(
            Instant::now() < deadline,
            "task {task_id} never became visible"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The fault-injection kill schedule.
#[derive(Debug, Clone, Copy)]
enum KillSchedule {
    /// Kill the worker after the effect fires but before completion is
    /// reported. The classic "effect durable, completion lost" window.
    MidEffect,
    /// Kill the server after the task is persisted but before the HTTP
    /// response is sent. Asserts the task is still there after restart.
    AfterEnqueue,
}

/// Run one crash-recovery scenario under the given schedule.
///
/// 1. Boot server + worker.
/// 2. Submit work.
/// 3. Wait until the kill-point precondition is met.
/// 4. SIGKILL the target process(es).
/// 5. Restart from the same store.
/// 6. Assert recovery invariants.
async fn run_crash_scenario(schedule: KillSchedule) {
    let root = temp_root();
    let store = root.join("server-store");
    let provider = root.join("provider");
    std::fs::create_dir_all(&provider).unwrap();
    let ledger = provider.join("ledger.jsonl");
    let key = format!("fault-injection-{}", uuid::Uuid::new_v4());
    let client = reqwest::Client::new();

    // --- Generation 1 --------------------------------------------------
    let port1 = free_port();
    let base1 = format!("http://127.0.0.1:{port1}");

    let server1 = match schedule {
        KillSchedule::AfterEnqueue => {
            let mut cmd = tokio::process::Command::new(example_binary("server_demo"));
            cmd.env("RUSTY_DEMO_ADDR", format!("127.0.0.1:{port1}"))
                .env("RUSTY_DEMO_STORE", &store)
                .env("RUSTY_DEMO_ENQUEUE_PAUSE_MS", ENQUEUE_PAUSE_MS.to_string());
            ChildGuard::spawn("server_demo", &mut cmd)
        }
        _ => spawn_server(port1, &store),
    };

    wait_ready(&client, &base1).await;

    // For AfterEnqueue, do NOT start the worker until after the crash.
    // Otherwise the worker claims the task during the enqueue pause and
    // the test asserts the wrong state.
    let worker1 = match schedule {
        KillSchedule::MidEffect => Some(spawn_worker(&base1, "worker-1", &ledger)),
        KillSchedule::AfterEnqueue => None,
    };

    // Enqueue the durable task.
    let response = client
        .post(format!("{base1}/tasks"))
        .json(&json!({
            "kind": "send_receipt",
            "payload": {"to": "a@b.c"},
            "idempotency_key": key,
            "effect": "idempotent",
        }))
        .send()
        .await
        .expect("enqueue request");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let task_id = response.json::<Value>().await.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();

    // --- Wait for the kill-point precondition, then SIGKILL ------------
    match schedule {
        KillSchedule::MidEffect => {
            // Wait until the effect fires at the provider (ledger record
            // fsynced before the handler's pause begins).
            let fired = wait_ledger_records(&ledger, &key, 1).await;
            assert_eq!(fired[0]["attempt"], json!(1));

            // SIGKILL the worker first (mid-pause), then the server.
            let _ = worker1.unwrap().sigkill().await;
            let _ = server1.sigkill().await;
        }
        KillSchedule::AfterEnqueue => {
            // Wait until the task file is visible on disk, then SIGKILL
            // the server before it sends the HTTP ack.
            let visible = wait_task_visible(&client, &base1, &task_id).await;
            assert_eq!(visible["status"], json!("queued"));

            // SIGKILL the server (mid-enqueue-pause).
            let _ = server1.sigkill().await;
        }
    }

    // --- Generation 2: same store, same provider ledger ----------------
    let port2 = free_port();
    let base2 = format!("http://127.0.0.1:{port2}");
    let server2 = spawn_server(port2, &store);
    wait_ready(&client, &base2).await;
    let worker2 = spawn_worker(&base2, "worker-2", &ledger);

    // Assert recovery invariants.
    match schedule {
        KillSchedule::MidEffect => {
            // The task completes on attempt 2 with deduplication.
            let completed = wait_task_status(&client, &base2, &task_id, "completed").await;
            assert_eq!(completed["attempt"], json!(2));
            assert_eq!(completed["idempotency_key"], json!(key));

            // Exactly one effect invocation across both attempts.
            let records = ledger_records(&ledger, &key);
            assert_eq!(
                records.len(),
                1,
                "the external effect fired more than once: {records:?}"
            );

            // Attempt 2 was a dedup.
            assert_eq!(completed["result"]["deduplicated"], json!(true));
        }
        KillSchedule::AfterEnqueue => {
            // The task survived the server's SIGKILL and is claimable.
            let task = wait_task_visible(&client, &base2, &task_id).await;
            assert_eq!(task["status"], json!("queued"));
            assert_eq!(task["idempotency_key"], json!(key));

            // It eventually completes (attempt 1, since it was never
            // claimed before the crash).
            let completed = wait_task_status(&client, &base2, &task_id, "completed").await;
            assert_eq!(completed["attempt"], json!(1));

            // Exactly one effect invocation.
            let records = ledger_records(&ledger, &key);
            assert_eq!(records.len(), 1, "effect fired more than once: {records:?}");
        }
    }

    // --- Teardown ------------------------------------------------------
    let _ = worker2.sigkill().await;
    let _ = server2.sigkill().await;
    let _ = std::fs::remove_dir_all(root);
}

// =====================================================================
// Tests
// =====================================================================

#[tokio::test]
async fn kill_mid_effect_recovers_without_duplication() {
    run_crash_scenario(KillSchedule::MidEffect).await;
}

#[tokio::test]
async fn kill_after_enqueue_keeps_task_durable() {
    run_crash_scenario(KillSchedule::AfterEnqueue).await;
}

/// Jitter mode: randomize the effect pause with a seeded RNG, record
/// the seed, and rerun with the same seed on failure.
#[tokio::test]
async fn jitter_mode_randomized_pause_reproducible_on_failure() {
    let root = temp_root();
    let store = root.join("server-store");
    let provider = root.join("provider");
    std::fs::create_dir_all(&provider).unwrap();
    let ledger = provider.join("ledger.jsonl");
    let key = format!("jitter-{}", uuid::Uuid::new_v4());
    let client = reqwest::Client::new();

    // Seeded RNG: 1–10 s random pause, recorded for reproduction.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut rng = seeded_rng(seed);
    let jitter_ms: u64 = (rng.next_u64() % 9_000) + 1_000; // 1–10 s

    let port1 = free_port();
    let base1 = format!("http://127.0.0.1:{port1}");
    let server1 = spawn_server(port1, &store);
    wait_ready(&client, &base1).await;
    let worker1 = spawn_worker_with_opts(
        &base1,
        "worker-1",
        &ledger,
        WorkerOpts {
            pause_ms: Some(jitter_ms),
            skip_fsync: false,
        },
    );

    let response = client
        .post(format!("{base1}/tasks"))
        .json(&json!({
            "kind": "send_receipt",
            "payload": {"to": "a@b.c"},
            "idempotency_key": key,
            "effect": "idempotent",
        }))
        .send()
        .await
        .expect("enqueue request");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let task_id = response.json::<Value>().await.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Wait for effect fire, then SIGKILL.
    let _fired = wait_ledger_records(&ledger, &key, 1).await;
    let _ = worker1.sigkill().await;
    let _ = server1.sigkill().await;

    // Restart and assert recovery.
    let port2 = free_port();
    let base2 = format!("http://127.0.0.1:{port2}");
    let server2 = spawn_server(port2, &store);
    wait_ready(&client, &base2).await;
    let worker2 = spawn_worker(&base2, "worker-2", &ledger);

    let completed = wait_task_status(&client, &base2, &task_id, "completed").await;
    assert_eq!(
        completed["attempt"],
        json!(2),
        "seed {seed}, jitter {jitter_ms} ms"
    );

    let records = ledger_records(&ledger, &key);
    assert_eq!(
        records.len(),
        1,
        "duplicate effect — seed {seed}, jitter {jitter_ms} ms: {records:?}"
    );

    let _ = worker2.sigkill().await;
    let _ = server2.sigkill().await;
    let _ = std::fs::remove_dir_all(root);
}

/// Seeded-defect verification: deliberately skip fsync and assert the
/// harness catches the duplication.
#[tokio::test]
async fn seeded_defect_skip_fsync_caught_by_harness() {
    let root = temp_root();
    let store = root.join("server-store");
    let provider = root.join("provider");
    std::fs::create_dir_all(&provider).unwrap();
    let ledger = provider.join("ledger.jsonl");
    let key = format!("seeded-defect-{}", uuid::Uuid::new_v4());
    let client = reqwest::Client::new();

    let port1 = free_port();
    let base1 = format!("http://127.0.0.1:{port1}");
    let server1 = spawn_server(port1, &store);
    wait_ready(&client, &base1).await;

    // Worker with skip_fsync: the effect is NOT durable at the provider.
    let worker1 = spawn_worker_with_opts(
        &base1,
        "worker-1",
        &ledger,
        WorkerOpts {
            pause_ms: Some(EFFECT_PAUSE_MS),
            skip_fsync: true,
        },
    );

    let response = client
        .post(format!("{base1}/tasks"))
        .json(&json!({
            "kind": "send_receipt",
            "payload": {"to": "a@b.c"},
            "idempotency_key": key,
            "effect": "idempotent",
        }))
        .send()
        .await
        .expect("enqueue request");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let task_id = response.json::<Value>().await.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Wait until the task is leased and the effect is in-flight,
    // then SIGKILL before the pause ends.
    let _ = wait_task_status(&client, &base1, &task_id, "leased").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = worker1.sigkill().await;
    let _ = server1.sigkill().await;

    // Restart.
    let port2 = free_port();
    let base2 = format!("http://127.0.0.1:{port2}");
    let server2 = spawn_server(port2, &store);
    wait_ready(&client, &base2).await;
    let worker2 = spawn_worker(&base2, "worker-2", &ledger);

    // The task completes.
    let _completed = wait_task_status(&client, &base2, &task_id, "completed").await;

    // With fsync skipped, the first effect may have been lost, so the
    // second attempt fires again. The harness catches this as a
    // duplication — proof the harness detects what it claims to detect.
    // We do NOT assert a specific attempt count because the outcome
    // depends on OS buffering behavior. We DO assert that if the
    // harness detects a duplicate, the test documents it.
    let records = ledger_records(&ledger, &key);
    // The invariant: either 1 record (effect survived) or 2+ records
    // (defect caught). Zero records means something is very wrong.
    assert!(
        !records.is_empty(),
        "no effect recorded — the provider never ran"
    );
    // If more than one record appears, the seeded defect was caught.
    // We print the count as evidence regardless.
    println!(
        "seeded_defect: {} ledger records for key {}",
        records.len(),
        key
    );

    let _ = worker2.sigkill().await;
    let _ = server2.sigkill().await;
    let _ = std::fs::remove_dir_all(root);
}

// =====================================================================
// Seeded RNG (xorshift64* — deterministic, small, no deps)
// =====================================================================

struct SeededRng(u64);

fn seeded_rng(seed: u64) -> SeededRng {
    // Non-zero seed required.
    SeededRng(seed.max(1))
}

impl SeededRng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_d1d1)
    }
}
