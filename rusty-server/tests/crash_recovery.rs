//! Crash recovery — the R0.6 "Durable Work" release proof, automated.
//!
//! `docs/durable-work-design.md` names this exact scenario as the release
//! gate:
//!
//! > kill the server and a worker mid-effect, restart, and the run
//! > completes without losing state or duplicating the external effect —
//! > the checkpointed run state resumes, the leased task returns to
//! > visibility, and the idempotency key makes the re-attempt a no-op at
//! > the effect.
//!
//! Unlike the graceful-shutdown suite (`shutdown.rs`), this test exercises
//! the REAL crash path with real processes and real SIGKILLs — no drain
//! token, no signal handler, no cleanup:
//!
//! 1. It spawns the actual demo binaries as child processes: `server_demo`
//!    (JSON-file store in a temp dir) and `activity_worker_demo` running
//!    `send_receipt` against its file-backed idempotent "provider" — a
//!    ledger file outside the server's store, standing in for the external
//!    system the effect hits (see the example's `RUSTY_DEMO_*` test hooks).
//! 2. A task is enqueued with an idempotency key and `effect: idempotent`.
//!    Attempt 1 fires the effect: the worker appends the invocation to the
//!    provider ledger, fsyncs it, and then pauses — the classic window:
//!    **effect durable at the provider, completion never reported**.
//! 3. Inside that window the test SIGKILLs the worker, then the server.
//!    Both sides are gone mid-effect.
//! 4. Server and worker restart from the same store dir / ledger file, and
//!    the test asserts the release promise end to end:
//!    - the leased task returns to visibility at lease expiry and a second
//!      attempt runs (the record ends `completed` with `attempt == 2`);
//!    - the idempotency key makes the re-attempt a no-op AT THE EFFECT —
//!      the provider ledger holds exactly ONE invocation across both worker
//!      processes, and the stored result/receipt carry the first attempt's
//!      provider confirmation;
//!    - no state was lost: the task record, its attempt counter, and its
//!      idempotency key all survived the server's SIGKILL.
//!
//! Timing discipline (no flakiness by construction): leases are short
//! (1 s, via `RUSTY_DEMO_LEASE_MS`) so a dead worker's task returns to
//! visibility fast; every wait is a poll against a deadline, never a fixed
//! sleep; and the post-effect pause (30 s) is far longer than the kill
//! window, so the completion report can never race the SIGKILL. Total
//! runtime is a few seconds, far under the 60 s budget.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The lease the test workers request: long enough to survive heartbeat
/// jitter, short enough that a SIGKILLed worker's task returns to
/// visibility ~1 s after its last heartbeat.
const LEASE_MS: u64 = 1_000;

/// The post-effect pause: the kill window. Deliberately far longer than
/// anything the test waits on, so attempt 1 can NEVER report completion
/// before the SIGKILL lands — the crash is deterministic, not racy. (The
/// provider's dedup path does not pause, so attempt 2 is unaffected.)
const EFFECT_PAUSE_MS: u64 = 30_000;

/// Unique temp root per run, removed at the end (best effort). Holds the
/// server's JSON-file store AND the provider ledger — separate subdirs,
/// because the provider is an external system, not part of the server.
fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-crash-proof-{}", uuid::Uuid::new_v4()))
}

/// The compiled demo binary `name` (no extension juggling beyond
/// `EXE_SUFFIX`). The test executable lives at
/// `<target>/<profile>/deps/crash_recovery-<hash>`; cargo builds examples
/// alongside at `<target>/<profile>/examples/<name>`. Note the binaries are
/// whatever cargo last built: `cargo test --workspace` rebuilds them, but a
/// package-scoped `cargo test -p rusty-agent-server` after editing `rusty-worker`
/// leaves its example stale — rebuild with
/// `cargo build --workspace --examples`.
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
        "example binary `{name}` not found at {} — build the workspace examples first \
         (`cargo build --workspace --examples`); `cargo test --workspace` does this for you",
        path.display()
    );
    path
}

/// A free TCP port, found the way the shutdown suite does it: bind, read,
/// release. (The released port can in principle be taken by another process
/// before the child binds it — vanishingly unlikely on a test machine, and
/// the readiness poll fails loudly if it happens.)
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A spawned demo process that is SIGKILLed when the guard drops unless the
/// test already reaped it: a panicking assertion must never leak demo
/// processes onto the machine running the tests.
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

    /// SIGKILL the process and reap it. `Child::kill` is SIGKILL on Unix /
    /// `TerminateProcess` on Windows — uncatchable, no drain, exactly the
    /// ungraceful death this proof is about. Returns the exit status so the
    /// test can assert the kill was a kill.
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
    let mut command = tokio::process::Command::new(example_binary("activity_worker_demo"));
    command
        .env("RUSTY_DEMO_SERVER_URL", base_url)
        .env("RUSTY_DEMO_WORKER_ID", worker_id)
        .env("RUSTY_DEMO_LEASE_MS", LEASE_MS.to_string())
        .env("RUSTY_DEMO_EFFECT_PAUSE_MS", EFFECT_PAUSE_MS.to_string())
        .env("RUSTY_DEMO_PROVIDER_FILE", ledger);
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

/// Poll `GET /tasks/{task_id}` until the task reaches `status`; returns the
/// terminal record.
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

/// The provider ledger's invocation records for `key` — the ground truth
/// for how many times the external effect fired.
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

/// The release proof: both sides SIGKILLed mid-effect, then restart —
/// no lost state, no duplicated effect.
#[tokio::test]
async fn crash_mid_effect_recovers_without_losing_state_or_duplicating_the_effect() {
    let root = temp_root();
    let store = root.join("server-store");
    let provider = root.join("provider");
    std::fs::create_dir_all(&provider).unwrap();
    let ledger = provider.join("ledger.jsonl");
    let key = format!("crash-proof-{}", uuid::Uuid::new_v4());
    let client = reqwest::Client::new();

    // --- Boot generation 1: server + worker, real processes. -----------
    let port1 = free_port();
    let base1 = format!("http://127.0.0.1:{port1}");
    let server1 = spawn_server(port1, &store);
    wait_ready(&client, &base1).await;
    let worker1 = spawn_worker(&base1, "worker-1", &ledger);

    // Enqueue the durable task: idempotency key + declared idempotent
    // effect — the two declarations the effectively-once contract needs.
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

    // Attempt 1 runs and the effect FIRES at the provider (the ledger
    // record is fsynced before the handler's pause begins).
    let fired = wait_ledger_records(&ledger, &key, 1).await;
    assert_eq!(fired[0]["attempt"], json!(1));
    let provider_id = fired[0]["provider_id"].as_str().unwrap().to_string();

    // Server-side, the task is leased to worker-1 on attempt 1.
    let leased: Value = client
        .get(format!("{base1}/tasks/{task_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(leased["status"], json!("leased"), "task record: {leased}");
    assert_eq!(leased["attempt"], json!(1));
    assert_eq!(leased["lease"]["owner"], json!("worker-1"));

    // --- THE CRASH WINDOW: effect fired, completion never reported. -----
    // SIGKILL the worker first (it holds the lease and is mid-pause), then
    // the server (its task record says `leased`, its outbox/journal writes
    // are behind atomic temp+rename). No drains, no signals handled.
    let worker1_status = worker1.sigkill().await;
    let server1_status = server1.sigkill().await;
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(worker1_status.signal(), Some(9), "worker-1 died by SIGKILL");
        assert_eq!(server1_status.signal(), Some(9), "server-1 died by SIGKILL");
    }
    let _ = (worker1_status, server1_status);

    // --- Boot generation 2: same store, same provider ledger. -----------
    // A fresh port for the replacement server (the killed one's port may
    // still be settling); the store dir — the state under test — is the
    // same. The 1 s lease from generation 1 has expired by the time the
    // replacement is up, so the task is immediately claimable.
    let port2 = free_port();
    let base2 = format!("http://127.0.0.1:{port2}");
    let server2 = spawn_server(port2, &store);
    wait_ready(&client, &base2).await;
    let worker2 = spawn_worker(&base2, "worker-2", &ledger);

    // The leased task returns to visibility, attempt 2 runs, and the task
    // ends completed — polled, not slept.
    let completed = wait_task_status(&client, &base2, &task_id, "completed").await;

    // --- The release promise, asserted. ---------------------------------

    // No lost state: the record survived the server's SIGKILL with its
    // attempt counter and idempotency key intact; a re-attempt really ran.
    assert_eq!(completed["attempt"], json!(2), "task record: {completed}");
    assert_eq!(completed["idempotency_key"], json!(key));

    // No duplicated effect: across BOTH worker processes the provider
    // ledger holds exactly ONE invocation of this idempotency key — the
    // re-attempt was a no-op AT THE EFFECT, not just at the queue.
    let records = ledger_records(&ledger, &key);
    assert_eq!(
        records.len(),
        1,
        "the external effect fired more than once: {records:?}"
    );

    // Attempt 2 hit the provider's dedup and reported the FIRST attempt's
    // confirmation — the stored result says so, and the effect receipt the
    // server kept on the record carries the same provider id under the
    // task's idempotency key.
    assert_eq!(completed["result"]["deduplicated"], json!(true));
    assert_eq!(completed["result"]["provider_id"], json!(provider_id));
    assert_eq!(completed["receipt"]["provider"], json!("file-provider"));
    assert_eq!(completed["receipt"]["provider_id"], json!(provider_id));
    assert_eq!(completed["receipt"]["idempotency_key"], json!(key));

    // Generation 2 is drained by SIGKILL too — the guard's Drop would do
    // it, but an explicit kill keeps the teardown symmetric and immediate.
    let _ = worker2.sigkill().await;
    let _ = server2.sigkill().await;

    let _ = std::fs::remove_dir_all(root);
}
