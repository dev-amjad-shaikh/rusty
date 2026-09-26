//! Agent Fabric crash survival — the R0.7 wave-1 release proof, automated.
//!
//! The agent-fabric analog of `crash_recovery.rs` (the R0.6 gate): an
//! agent host is SIGKILLed mid-effect inside its turn, the server is
//! SIGKILLed with the activation lease and the turn's task lease live, and
//! a replacement host must pick the agent up without losing the message or
//! duplicating the external effect:
//!
//! 1. Real processes, real SIGKILLs (the `crash_recovery.rs` discipline):
//!    `server_demo` with a JSON-file store in a temp dir, and
//!    `activity_worker_demo` in agent-host mode (`RUSTY_DEMO_AGENT_ID`)
//!    running `send_receipt` against its file-backed idempotent provider.
//! 2. The agent is registered (manifest accepting `send_receipt`) and a
//!    message is sent into its mailbox with an idempotency key and
//!    `effect: idempotent`. The host activates (fencing 1), claims the
//!    turn, and FIRES the effect: the ledger record is fsynced, then the
//!    pause begins — the classic window: **effect durable at the provider,
//!    completion never reported**.
//! 3. Inside that window the test SIGKILLs the worker, then the server.
//!    The store holds: a live activation lease (fencing 1, owner gone), a
//!    live-leased mailbox message (attempt 1, owner gone).
//! 4. Both restart from the same store dir / ledger file. The replacement
//!    host's first activate answers 409 — the dead host's lease is still
//!    live — and its retry loop waits out the expiry and STEALS the
//!    activation (fencing 2). The turn gate then opens at the task lease's
//!    expiry, attempt 2 runs, and the idempotency key makes the re-attempt
//!    a no-op AT THE EFFECT.
//! 5. The wave-1 promise, asserted end to end: the message ends
//!    `completed` with `attempt == 2`; the provider ledger holds exactly
//!    ONE invocation across both hosts; the stored result and receipt
//!    carry the first attempt's provider confirmation.
//!
//! Timing discipline mirrors `crash_recovery.rs`: 1 s leases (fast
//! steal/reclaim), every wait a poll against a deadline, and a 30 s
//! post-effect pause so the completion can never race the SIGKILL.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The lease the test hosts request (activation AND task): long enough to
/// survive heartbeat jitter, short enough that a SIGKILLed host's
/// activation is stealable and its turn re-claimable ~1 s after its last
/// heartbeat.
const LEASE_MS: u64 = 1_000;

/// The post-effect pause: the kill window (see `crash_recovery.rs`).
const EFFECT_PAUSE_MS: u64 = 30_000;

/// Unique temp root per run, removed at the end (best effort).
fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rusty-agent-recovery-proof-{}",
        uuid::Uuid::new_v4()
    ))
}

/// The compiled demo binary `name` (see `crash_recovery.rs` for the path
/// discipline and the staleness caveat).
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

/// A free TCP port (bind, read, release — the shutdown suite's way).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A spawned demo process that is SIGKILLed when the guard drops unless
/// the test already reaped it (the `crash_recovery.rs` convention).
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

    /// SIGKILL the process and reap it (uncatchable, no drain — exactly
    /// the ungraceful death this proof is about).
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
        .env("RUSTY_DEMO_STORE", store)
        // These test recovery, not identity: run the product entry open.
        .env("RUSTY_OPEN", "1");
    ChildGuard::spawn("server_demo", &mut command)
}

/// Spawn `activity_worker_demo` in agent-host mode for `agent_id`,
/// running `send_receipt` against the provider ledger at `ledger`.
fn spawn_host(base_url: &str, worker_id: &str, agent_id: &str, ledger: &Path) -> ChildGuard {
    let mut command = tokio::process::Command::new(example_binary("activity_worker_demo"));
    command
        .env("RUSTY_DEMO_SERVER_URL", base_url)
        .env("RUSTY_DEMO_WORKER_ID", worker_id)
        .env("RUSTY_DEMO_AGENT_ID", agent_id)
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

/// The wave-1 exit criterion: agent host and server SIGKILLed mid-turn,
/// mid-effect — the replacement host steals the activation, re-claims the
/// turn, and the effect fires exactly once across both generations.
#[tokio::test]
async fn agent_host_crash_mid_turn_recovers_without_losing_the_message_or_duplicating_the_effect() {
    let root = temp_root();
    let store = root.join("server-store");
    let provider = root.join("provider");
    std::fs::create_dir_all(&provider).unwrap();
    let ledger = provider.join("ledger.jsonl");
    let key = format!("agent-crash-proof-{}", uuid::Uuid::new_v4());
    let client = reqwest::Client::new();

    // --- Boot generation 1: server + agent host, real processes. --------
    let port1 = free_port();
    let base1 = format!("http://127.0.0.1:{port1}");
    let server1 = spawn_server(port1, &store);
    wait_ready(&client, &base1).await;

    // Register the agent: a manifest accepting exactly `send_receipt`.
    let response = client
        .post(format!("{base1}/agents"))
        .json(&json!({
            "agent_id": "receipt-agent",
            "manifest": {
                "agent_kind": "demo",
                "manifest_version": "demo/1.0.0",
                "accepts": {"send_receipt": {"kind": "application/json"}}
            }
        }))
        .send()
        .await
        .expect("register request");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);

    let worker1 = spawn_host(&base1, "worker-1", "receipt-agent", &ledger);

    // Send the message into the agent's mailbox: idempotency key +
    // declared idempotent effect — the effectively-once declarations.
    let response = client
        .post(format!("{base1}/agents/receipt-agent/mailbox"))
        .json(&json!({
            "kind": "send_receipt",
            "payload": {"to": "a@b.c"},
            "idempotency_key": key,
            "effect": "idempotent",
        }))
        .send()
        .await
        .expect("mailbox send request");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let task_id = response.json::<Value>().await.unwrap()["task_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The host activates (fencing 1), claims the turn, and the effect
    // FIRES at the provider (fsynced before the pause begins).
    let fired = wait_ledger_records(&ledger, &key, 1).await;
    assert_eq!(fired[0]["attempt"], json!(1));
    let provider_id = fired[0]["provider_id"].as_str().unwrap().to_string();

    // Server-side: the message is leased to worker-1 on attempt 1,
    // addressed to the agent's mailbox.
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
    assert_eq!(leased["recipient"], json!("agent:receipt-agent"));

    // The activation lease is live, held by the host about to die.
    let status: Value = client
        .get(format!("{base1}/agents/receipt-agent/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["activation"]["owner"], json!("worker-1"));
    assert_eq!(status["activation"]["fencing"], json!(1));
    assert_eq!(status["mailbox"]["in_flight"], json!(1));

    // --- THE CRASH WINDOW: effect fired, turn never settled. ------------
    // SIGKILL the host first (it holds both leases and is mid-pause), then
    // the server. No drains, no signals handled.
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
    // The 1 s leases from generation 1 expire while the replacement boots:
    // the replacement host's activate first answers 409 (the dead host's
    // lease is still live), its retry waits out the expiry, and it steals
    // the activation with fencing bumped — that is the retry loop doing
    // exactly what single-activation requires.
    let port2 = free_port();
    let base2 = format!("http://127.0.0.1:{port2}");
    let server2 = spawn_server(port2, &store);
    wait_ready(&client, &base2).await;
    let worker2 = spawn_host(&base2, "worker-2", "receipt-agent", &ledger);

    // The stolen activation is visible on the status read: worker-2,
    // fencing 2 — the dead host's fencing-1 pair can never pass a guard
    // again.
    let deadline = Instant::now() + Duration::from_secs(15);
    let stolen = loop {
        if let Ok(response) = client
            .get(format!("{base2}/agents/receipt-agent/status"))
            .send()
            .await
        {
            let body: Value = response.json().await.unwrap_or(Value::Null);
            if body["activation"]["owner"] == json!("worker-2") {
                break body;
            }
        }
        assert!(
            Instant::now() < deadline,
            "worker-2 never stole the activation"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(stolen["activation"]["fencing"], json!(2));

    // The expired turn returns to visibility, attempt 2 runs, and the
    // message ends completed — polled, not slept.
    let completed = wait_task_status(&client, &base2, &task_id, "completed").await;

    // --- The wave-1 promise, asserted. ----------------------------------

    // No lost state: the record survived the server's SIGKILL with its
    // attempt counter, idempotency key, and recipient intact; a second
    // attempt really ran.
    assert_eq!(completed["attempt"], json!(2), "task record: {completed}");
    assert_eq!(completed["idempotency_key"], json!(key));
    assert_eq!(completed["recipient"], json!("agent:receipt-agent"));

    // No duplicated effect: across BOTH host processes the provider ledger
    // holds exactly ONE invocation of this idempotency key — the
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
    // message's idempotency key.
    assert_eq!(completed["result"]["deduplicated"], json!(true));
    assert_eq!(completed["result"]["provider_id"], json!(provider_id));
    assert_eq!(completed["receipt"]["provider"], json!("file-provider"));
    assert_eq!(completed["receipt"]["provider_id"], json!(provider_id));
    assert_eq!(completed["receipt"]["idempotency_key"], json!(key));

    // Generation 2 is drained by SIGKILL too (the guard's Drop would do
    // it; an explicit kill keeps the teardown symmetric and immediate).
    let _ = worker2.sigkill().await;
    let _ = server2.sigkill().await;

    let _ = std::fs::remove_dir_all(root);
}
