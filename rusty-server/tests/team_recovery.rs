//! Team crash recovery — the R0.7 "Agent Fabric" release proof, automated.
//!
//! `docs/agent-fabric-design.md` names this exact scenario as the release
//! gate for the whole release (the R0.6 precedent is `crash_recovery.rs`;
//! the wave-1 single-agent analog is `agent_recovery.rs`):
//!
//! > a three-agent team — supervisor, two workers — executes a fan-out with
//! > a delegated follow-up; the server and one agent host are SIGKILLed
//! > after the fan-out has partially settled (one child complete, one in
//! > flight); everything restarts from the same store; the test asserts
//! > (1) the team completes without duplicating any idempotent effect,
//! > (2) the in-flight child's message was re-delivered under its
//! > idempotency key, and (3) `TeamTrace` assembly from the persisted
//! > journals yields ONE connected causal tree.
//!
//! The scenario, end to end (real processes, real SIGKILLs):
//!
//! 1. `server_demo` (JSON-file store in a temp dir) and three
//!    `activity_worker_demo` agent hosts: `worker-a`, `worker-b`
//!    (provider-backed, running the `work` turns against the file-backed
//!    idempotent provider) and, after the restart, `supervisor`
//!    (provider-less, draining the `coordination_result` outcomes).
//! 2. The supervisor's fan-out `fo-1` has two members: `alpha` →
//!    `worker-a`, `beta` → `worker-b`, both `effect: idempotent`.
//!    `worker-a` runs with a short provider pause so its turn completes;
//!    `worker-b` runs with the 30 s pause — the classic window: **effect
//!    durable at the provider, completion never reported**.
//! 3. When `alpha`'s settlement is journaled, the delegated follow-up
//!    `d-1` (member `follow` → `worker-a`) is submitted through the public
//!    API on the supervisor's behalf, carrying the causal parent the
//!    supervisor's turn would have stamped: `alpha`'s `MailboxReceive`
//!    event in `fo-1`'s journal. `d-1` settles before the crash; its
//!    journal is the second journal the team trace stitches in. (The demo
//!    host deliberately has no nested-submission behavior — the pattern
//!    under test is the runtime's, and the submission path is the one an
//!    agent's turn would call.)
//! 4. The fan-out is now partially settled: `alpha` complete with its
//!    effect fsynced to the provider ledger, `beta` in flight (attempt 1
//!    leased to `worker-b`'s host, effect fired, mid-pause). Inside that
//!    window the test SIGKILLs `worker-b`'s host, then the server.
//! 5. Everything restarts from the same store dir / ledger file. The
//!    replacement `worker-b` host steals the expired activation, re-claims
//!    the turn at attempt 2, and the idempotency key makes the re-attempt
//!    a no-op AT THE EFFECT — the fan-out settles, the outcome reaches the
//!    supervisor's mailbox (as does `d-1`'s, queued since generation 1),
//!    and the supervisor host drains both.
//! 6. The release promise, asserted: every idempotency key holds exactly
//!    ONE provider invocation across all host generations; `beta`'s
//!    message was re-delivered under its derived key
//!    (`coordination:fo-1:beta`, attempt 2, `deduplicated: true`); and the
//!    team's persisted journals — `fo-1`'s and `d-1`'s — assemble into ONE
//!    connected causal tree rooted at `fo-1`'s `CoordinationStart`,
//!    matching the golden expectation exactly (ids, kinds, seqs, parents).
//!
//! The trace is assembled two ways: server-side per pattern
//! (`GET /coordination/{id}/trace` — both connected), and client-side as
//! the cross-journal UNION the release gate asks for, from the persisted
//! journal events the server exposes, with `TeamTrace`'s exact semantics
//! (a parent outside the assembled set makes its event a root; one root
//! plus full reachability is one connected tree). Member *run* journals do
//! not exist at this layer — the demo hosts settle turns without
//! journaling runs (the wave-2 host integration boundary) — so the
//! pattern journals are the team's journals here; supervision journals
//! carry no parent links into the tree and are excluded by the server
//! side as well.
//!
//! Timing discipline mirrors `crash_recovery.rs`: 1 s leases (fast
//! steal/reclaim), every wait a poll against a deadline (never a fixed
//! sleep), and a 30 s post-effect pause for the killed host so the
//! completion can never race the SIGKILL. `KillOnDrop`-style guards reap
//! every spawned process. One coordination-specific rule: a
//! `GET /coordination/{id}` read is also a DRIVE (reconcile-on-read), and
//! the file backend's correctness rests on its documented one-writer
//! boundary — so while a pattern's settlement hook could still be in
//! flight the test gates only on pure reads (task visibility, the
//! provider ledger), and issues journal reads exclusively after the
//! outcome-message chain has proven every hook committed.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The lease the test hosts request (activation AND task): long enough to
/// survive heartbeat jitter, short enough that a SIGKILLed host's
/// activation is stealable and its turn re-claimable ~1 s after its last
/// heartbeat.
const LEASE_MS: u64 = 1_000;

/// The post-effect pause for the host the proof kills: the crash window.
/// Deliberately far longer than anything the test waits on, so attempt 1
/// can NEVER report completion before the SIGKILL lands (the provider's
/// dedup path does not pause, so the replacement host is unaffected).
const EFFECT_PAUSE_MS: u64 = 30_000;

/// The post-effect pause for hosts whose turns must COMPLETE in generation
/// 1 (`worker-a`, and the provider-less `supervisor` stand-in): long
/// enough to be a real provider call, short enough to settle promptly.
const FAST_PAUSE_MS: u64 = 250;

/// Unique temp root per run, removed at the end (best effort). Holds the
/// server's JSON-file store AND the provider ledger — separate subdirs,
/// because the provider is an external system, not part of the server.
fn temp_root() -> PathBuf {
    std::env::temp_dir().join(format!("rusty-team-proof-{}", uuid::Uuid::new_v4()))
}

/// The compiled demo binary `name` (see `crash_recovery.rs` for the path
/// discipline and the staleness caveat: build the workspace examples
/// first — `cargo build --workspace --examples`).
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
/// the test already reaped it (the `crash_recovery.rs` convention): a
/// panicking assertion must never leak demo processes.
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
    /// the ungraceful death this proof is about). Returns the exit status
    /// so the test can assert the kill was a kill.
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

/// Spawn `activity_worker_demo` in agent-host mode for `agent_id`. When
/// `ledger` is set the host runs turns against the file-backed idempotent
/// provider; when `None` it runs the plain stand-in (the supervisor host —
/// its `coordination_result` turns are not external effects).
fn spawn_host(
    base_url: &str,
    worker_id: &str,
    agent_id: &str,
    ledger: Option<&Path>,
    pause_ms: u64,
) -> ChildGuard {
    let mut command = tokio::process::Command::new(example_binary("activity_worker_demo"));
    command
        .env("RUSTY_DEMO_SERVER_URL", base_url)
        .env("RUSTY_DEMO_WORKER_ID", worker_id)
        .env("RUSTY_DEMO_AGENT_ID", agent_id)
        .env("RUSTY_DEMO_LEASE_MS", LEASE_MS.to_string())
        .env("RUSTY_DEMO_EFFECT_PAUSE_MS", pause_ms.to_string());
    if let Some(ledger) = ledger {
        command.env("RUSTY_DEMO_PROVIDER_FILE", ledger);
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
/// the terminal record. Tolerates 404s: coordination member and outcome
/// tasks arrive through the outbox relay, so they are invisible until the
/// first relay poll publishes them.
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

/// `GET /coordination/{id}` (reconciling on read); asserts 200.
async fn get_coordination(client: &reqwest::Client, base: &str, coordination_id: &str) -> Value {
    let response = client
        .get(format!("{base}/coordination/{coordination_id}"))
        .send()
        .await
        .expect("get coordination request");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert_eq!(status, reqwest::StatusCode::OK, "body: {body}");
    serde_json::from_str::<Value>(&body).unwrap_or(Value::Null)
}

/// The journal events of a coordination record, asserted present.
fn journal_events(record: &Value) -> &Vec<Value> {
    record["journal"]["events"]
        .as_array()
        .expect("a driven coordination always has a journal")
}

/// Poll `GET /tasks/{task_id}` until the task is visible at all (200, any
/// status); returns the record. This is the pure-read gate the test uses
/// to learn that a pattern's settle has committed: the outcome message
/// task is pushed to the outbox in the same `journal_and_enqueue` commit
/// as the pattern's terminal journal events, so its appearance proves the
/// settle drive ran — without ever issuing a reconcile-driving
/// `GET /coordination/{id}` while a settlement hook for that pattern
/// could still be in flight.
async fn wait_task_visible(client: &reqwest::Client, base: &str, task_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
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

/// The cross-journal TeamTrace assertion, assembled client-side from the
/// persisted journal events with `TeamTrace`'s exact semantics
/// (`rusty-core/src/team_trace.rs`): an event whose parent is absent from
/// the assembled set is a root; the tree is connected when there is
/// exactly one root and every node is reachable from it. The golden
/// expectation pins the full shape: the exact event set, each event's
/// kind, and each event's parent link.
fn assert_one_connected_tree(events: &[Value], golden: &[(&str, &str, Option<&str>)], root: &str) {
    // The event set, kinds, and parent links are exactly the golden
    // expectation — no missing evidence, no extra evidence.
    let by_id: HashMap<String, &Value> = events
        .iter()
        .map(|event| {
            (
                event["id"].as_str().expect("events carry ids").to_string(),
                event,
            )
        })
        .collect();
    assert_eq!(
        by_id.len(),
        golden.len(),
        "the team's journals hold a different event set than the golden: {events:?}"
    );
    for (id, kind, parent) in golden {
        let event = by_id
            .get(*id)
            .unwrap_or_else(|| panic!("golden event `{id}` missing from the journals"));
        assert_eq!(event["kind"], json!(kind), "event `{id}` kind");
        match parent {
            Some(parent) => assert_eq!(event["parent"], json!(parent), "event `{id}` parent link"),
            None => assert!(
                event.get("parent").is_none() || event["parent"].is_null(),
                "event `{id}` must be parentless (the team's root spawn event)"
            ),
        }
    }

    // Roots: parentless events and events whose parent lives outside the
    // assembled set (the cross-journal stitch rule).
    let ids: HashSet<&str> = by_id.keys().map(String::as_str).collect();
    let mut roots: Vec<&str> = by_id
        .values()
        .filter(|event| match event["parent"].as_str() {
            Some(parent) => !ids.contains(parent),
            None => true,
        })
        .map(|event| event["id"].as_str().unwrap())
        .collect();
    roots.sort_unstable();
    assert_eq!(
        roots,
        vec![root],
        "the team's trace must have exactly one root — its spawn event"
    );

    // Reachability: breadth-first from the root over children adjacency —
    // every event in every journal reaches the team's root spawn event by
    // parent links.
    let mut children_of: HashMap<&str, Vec<&str>> = HashMap::new();
    for event in by_id.values() {
        if let Some(parent) = event["parent"].as_str() {
            children_of
                .entry(parent)
                .or_default()
                .push(event["id"].as_str().unwrap());
        }
    }
    let mut visited: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::from([root]);
    while let Some(id) = queue.pop_front() {
        if !visited.insert(id) {
            continue;
        }
        if let Some(children) = children_of.get(id) {
            queue.extend(children.iter().copied());
        }
    }
    assert_eq!(
        visited.len(),
        golden.len(),
        "the team's trace is not one connected tree: unreachable events remain"
    );
}

/// The release proof: the server and one agent host SIGKILLed with the
/// fan-out partially settled, then everything restarted from the same
/// store — the team completes, no idempotent effect duplicates, the
/// in-flight child is re-delivered under its idempotency key, and the
/// team's journals assemble into one connected causal tree.
#[tokio::test]
async fn team_crash_mid_fan_out_recovers_without_duplicating_effects_and_the_trace_is_one_tree() {
    let root = temp_root();
    let store = root.join("server-store");
    let provider = root.join("provider");
    std::fs::create_dir_all(&provider).unwrap();
    let ledger = provider.join("ledger.jsonl");
    let client = reqwest::Client::new();

    // The runtime-derived ids and keys this proof asserts (open mode, so
    // the tenant is `default`).
    let alpha_task = "default--fo-1--alpha";
    let beta_task = "default--fo-1--beta";
    let follow_task = "default--d-1--follow";
    let alpha_key = "coordination:fo-1:alpha";
    let beta_key = "coordination:fo-1:beta";
    let follow_key = "coordination:d-1:follow";

    // --- Boot generation 1: server + the two worker hosts. --------------
    let port1 = free_port();
    let base1 = format!("http://127.0.0.1:{port1}");
    let server1 = spawn_server(port1, &store);
    wait_ready(&client, &base1).await;

    // Register the team: the supervisor (its manifest must declare the
    // reserved `coordination_result` kind, or its patterns would strand)
    // and the two workers (accepting `work` at the pinned version).
    for (agent_id, manifest) in [
        (
            "supervisor",
            json!({
                "agent_kind": "supervisor",
                "manifest_version": "supervisor/1.0.0",
                "accepts": {"coordination_result": {"kind": "application/json"}}
            }),
        ),
        (
            "worker-a",
            json!({
                "agent_kind": "worker",
                "manifest_version": "worker/1.0.0",
                "accepts": {"work": {"kind": "application/json"}}
            }),
        ),
        (
            "worker-b",
            json!({
                "agent_kind": "worker",
                "manifest_version": "worker/1.0.0",
                "accepts": {"work": {"kind": "application/json"}}
            }),
        ),
    ] {
        let response = client
            .post(format!("{base1}/agents"))
            .json(&json!({"agent_id": agent_id, "manifest": manifest}))
            .send()
            .await
            .expect("register request");
        assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    }

    // worker-a runs with the fast pause: its turns complete in generation
    // 1. worker-b runs with the 30 s pause: the deterministic kill window.
    let worker_a1 = spawn_host(
        &base1,
        "worker-a-host-1",
        "worker-a",
        Some(&ledger),
        FAST_PAUSE_MS,
    );
    let worker_b1 = spawn_host(
        &base1,
        "worker-b-host-1",
        "worker-b",
        Some(&ledger),
        EFFECT_PAUSE_MS,
    );

    // The supervisor's fan-out: alpha → worker-a, beta → worker-b, both
    // idempotent effects, the window wide enough to hold both at once.
    let response = client
        .post(format!("{base1}/coordination/fan_out"))
        .json(&json!({
            "coordination_id": "fo-1",
            "delegator": "supervisor",
            "fan_out": {
                "members": [
                    {"member": "alpha", "agent_id": "worker-a",
                     "manifest_version": "worker/1.0.0", "kind": "work",
                     "input": {"kind": "inline", "value": {"part": "alpha"}},
                     "effect": "idempotent"},
                    {"member": "beta", "agent_id": "worker-b",
                     "manifest_version": "worker/1.0.0", "kind": "work",
                     "input": {"kind": "inline", "value": {"part": "beta"}},
                     "effect": "idempotent"}
                ],
                "max_in_flight": 2,
                "on_member_failure": "partial"
            }
        }))
        .send()
        .await
        .expect("fan-out submission");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let created = response.json::<Value>().await.unwrap();
    assert_eq!(
        created["start_event"],
        json!("coordination:default:fo-1:0"),
        "the team's root spawn event: {created}"
    );

    // alpha's effect fires and its turn completes; the settlement hook
    // (awaited inside the complete route, after the settlement is durable)
    // journals alpha's MailboxReceive in fo-1's journal. The receive's
    // event id is DERIVED, not read back: the creation drive journaled
    // start(0), send alpha(1), send beta(2) — asserted at submission and
    // pinned by the golden at the end — so alpha's receive is seq 3. (The
    // test deliberately never issues a reconcile-driving
    // `GET /coordination/fo-1` while a settlement hook for fo-1 could be
    // in flight: the read is also a drive, and two drives of one pattern
    // are the file backend's documented one-writer boundary. Task-status
    // and ledger polls are pure reads.)
    let alpha_fired = wait_ledger_records(&ledger, alpha_key, 1).await;
    assert_eq!(alpha_fired[0]["attempt"], json!(1));
    let alpha_completed = wait_task_status(&client, &base1, alpha_task, "completed").await;
    assert_eq!(alpha_completed["result"]["deduplicated"], json!(false));
    let alpha_receive_id = "coordination:default:fo-1:3";

    // The delegated follow-up: on the supervisor's behalf, follow-up work
    // is delegated back to worker-a, parented causally onto alpha's
    // journaled settlement — the cross-journal link the team trace
    // stitches on. It settles before the crash: its evidence is the second
    // journal that must survive the kill.
    let response = client
        .post(format!("{base1}/coordination/delegate"))
        .json(&json!({
            "coordination_id": "d-1",
            "delegator": "supervisor",
            "parent": alpha_receive_id,
            "delegate": {
                "delegate": {"member": "follow", "agent_id": "worker-a",
                             "manifest_version": "worker/1.0.0", "kind": "work",
                             "input": {"kind": "inline", "value": {"part": "follow-up"}},
                             "effect": "idempotent"}
            }
        }))
        .send()
        .await
        .expect("follow-up submission");
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let follow_fired = wait_ledger_records(&ledger, follow_key, 1).await;
    assert_eq!(follow_fired[0]["attempt"], json!(1));
    let follow_completed = wait_task_status(&client, &base1, follow_task, "completed").await;
    assert_eq!(follow_completed["result"]["deduplicated"], json!(false));
    // d-1 settles on follow's settlement hook: the outcome message task
    // appearing in the queue (a pure read) proves the settle committed —
    // journaled end + outcome row are one commit. No supervisor host
    // exists yet, so the message waits in the supervisor's mailbox; it is
    // itself a survivor of the crash.
    let d1_outcome = wait_task_visible(&client, &base1, "default--d-1--outcome").await;
    assert_eq!(d1_outcome["recipient"], json!("agent:supervisor"));

    // beta's effect has fired at the provider (fsynced); its host is now
    // inside the 30 s pause — completion can never race the SIGKILL.
    let beta_fired = wait_ledger_records(&ledger, beta_key, 1).await;
    assert_eq!(beta_fired[0]["attempt"], json!(1));
    let beta_provider_id = beta_fired[0]["provider_id"].as_str().unwrap().to_string();
    let beta_leased: Value = client
        .get(format!("{base1}/tasks/{beta_task}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        beta_leased["status"],
        json!("leased"),
        "task record: {beta_leased}"
    );
    assert_eq!(beta_leased["attempt"], json!(1));
    assert_eq!(beta_leased["lease"]["owner"], json!("worker-b-host-1"));
    assert_eq!(beta_leased["recipient"], json!("agent:worker-b"));
    assert_eq!(beta_leased["idempotency_key"], json!(beta_key));

    // --- THE CRASH WINDOW: the fan-out partially settled. ---------------
    // alpha complete (effect fsynced), beta in flight (effect fired,
    // completion never reported). SIGKILL worker-b's host first (it holds
    // both leases and is mid-pause), then the server. No drains, no
    // signals handled.
    let worker_b1_status = worker_b1.sigkill().await;
    let server1_status = server1.sigkill().await;
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            worker_b1_status.signal(),
            Some(9),
            "worker-b host died by SIGKILL"
        );
        assert_eq!(server1_status.signal(), Some(9), "server-1 died by SIGKILL");
    }
    let _ = (worker_b1_status, server1_status);
    // worker-a's generation-1 host is generation-1 teardown: its work is
    // done, and a live host against the dead port can never reconnect.
    let _ = worker_a1.sigkill().await;

    // --- Boot generation 2: same store, same provider ledger. -----------
    // The 1 s leases from generation 1 expire while the replacement boots:
    // the replacement worker-b host steals the dead host's activation and
    // re-claims the turn at the task lease's expiry.
    let port2 = free_port();
    let base2 = format!("http://127.0.0.1:{port2}");
    let server2 = spawn_server(port2, &store);
    wait_ready(&client, &base2).await;
    // worker-b's replacement runs the SAME 30 s pause: it is the
    // provider's dedup path (no pause), not the configuration, that makes
    // the re-attempt safe.
    let worker_b2 = spawn_host(
        &base2,
        "worker-b-host-2",
        "worker-b",
        Some(&ledger),
        EFFECT_PAUSE_MS,
    );
    // The rest of the team restarts too: worker-a (mailbox empty — nothing
    // to re-deliver) and the supervisor (provider-less, draining the
    // outcome messages its patterns delivered).
    let worker_a2 = spawn_host(
        &base2,
        "worker-a-host-2",
        "worker-a",
        Some(&ledger),
        FAST_PAUSE_MS,
    );
    let supervisor2 = spawn_host(
        &base2,
        "supervisor-host-2",
        "supervisor",
        None,
        FAST_PAUSE_MS,
    );

    // The in-flight child's message is re-delivered: beta's expired lease
    // returns to visibility, attempt 2 runs, and the message ends
    // completed — polled, not slept.
    let beta_completed = wait_task_status(&client, &base2, beta_task, "completed").await;

    // --- Assertion 2: re-delivery under the idempotency key. ------------
    assert_eq!(
        beta_completed["attempt"],
        json!(2),
        "task record: {beta_completed}"
    );
    assert_eq!(beta_completed["idempotency_key"], json!(beta_key));
    assert_eq!(beta_completed["recipient"], json!("agent:worker-b"));
    // Attempt 2 hit the provider's dedup: the stored result and the effect
    // receipt carry the FIRST attempt's provider confirmation under the
    // same derived key.
    assert_eq!(beta_completed["result"]["deduplicated"], json!(true));
    assert_eq!(
        beta_completed["result"]["provider_id"],
        json!(beta_provider_id)
    );
    assert_eq!(
        beta_completed["receipt"]["provider"],
        json!("file-provider")
    );
    assert_eq!(
        beta_completed["receipt"]["provider_id"],
        json!(beta_provider_id)
    );
    assert_eq!(
        beta_completed["receipt"]["idempotency_key"],
        json!(beta_key)
    );

    // --- The team completes. --------------------------------------------
    // The fan-out settles on beta's settlement hook. The gate for that
    // being committed is again a pure read: the outcome message tasks
    // reaching the supervisor's mailbox and its host draining them —
    // fo-1's delivered after the restart, d-1's queued since generation 1
    // and itself a survivor of the server's SIGKILL. The supervisor's
    // completion of fo-1's outcome can only happen after the settle
    // drive's commit (the outcome task exists only from it), and outcome
    // tasks are not pattern members, so draining them drives nothing —
    // after this loop no settlement hook for either pattern can still be
    // in flight, and the journal reads below are race-free.
    for outcome_task in ["default--fo-1--outcome", "default--d-1--outcome"] {
        let outcome = wait_task_status(&client, &base2, outcome_task, "completed").await;
        assert_eq!(outcome["recipient"], json!("agent:supervisor"));
    }

    // The settled fan-out, read once now that it is quiescent: the merge
    // is in member task-id order (alpha, beta), with beta's dedup flag as
    // the evidence its re-attempt was a no-op at the effect.
    let fo1_settled = get_coordination(&client, &base2, "fo-1").await;
    assert_eq!(fo1_settled["settled"], json!(true), "record: {fo1_settled}");
    assert_eq!(fo1_settled["outcome"]["status"], json!("completed"));
    let merged = &fo1_settled["outcome"]["result"]["value"];
    assert_eq!(merged.as_array().unwrap().len(), 2);
    assert_eq!(merged[0]["deduplicated"], json!(false), "alpha fired once");
    assert_eq!(merged[1]["deduplicated"], json!(true), "beta deduplicated");
    // The delegated follow-up survived the crash settled.
    let d1_settled = get_coordination(&client, &base2, "d-1").await;
    assert_eq!(d1_settled["settled"], json!(true), "record: {d1_settled}");
    assert_eq!(d1_settled["outcome"]["status"], json!("completed"));

    // --- Assertion 1: no idempotent effect duplicated. ------------------
    // Across ALL host generations, the provider ledger holds exactly ONE
    // invocation per idempotency key — three keys, three invocations,
    // every one of them the first attempt's fire.
    for (key, task_id) in [
        (alpha_key, alpha_task),
        (beta_key, beta_task),
        (follow_key, follow_task),
    ] {
        let records = ledger_records(&ledger, key);
        assert_eq!(
            records.len(),
            1,
            "the effect for `{key}` (task {task_id}) fired more than once: {records:?}"
        );
        assert_eq!(
            records[0]["attempt"],
            json!(1),
            "ledger record: {records:?}"
        );
    }
    let total_lines = std::fs::read_to_string(&ledger)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert_eq!(total_lines, 3, "the provider ledger holds stray effects");

    // --- Assertion 3: ONE connected causal tree. ------------------------
    // Server-side, per pattern: both traces are connected; fo-1's root is
    // its CoordinationStart — the team's root spawn event.
    let trace_fo1: Value = client
        .get(format!("{base2}/coordination/fo-1/trace"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(trace_fo1["connected"], json!(true), "trace: {trace_fo1}");
    assert_eq!(
        trace_fo1["trace"]["roots"],
        json!(["coordination:default:fo-1:0"])
    );
    assert_eq!(trace_fo1["trace"]["nodes"].as_array().unwrap().len(), 6);
    // d-1's own trace is connected too — its root is its start event,
    // whose parent link names alpha's receive in fo-1's journal: the
    // cross-journal stitch exists in the persisted evidence.
    let trace_d1: Value = client
        .get(format!("{base2}/coordination/d-1/trace"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(trace_d1["connected"], json!(true), "trace: {trace_d1}");
    assert_eq!(
        trace_d1["trace"]["roots"],
        json!(["coordination:default:d-1:0"])
    );
    let d1_start = trace_d1["trace"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["kind"] == json!("coordination_start"))
        .expect("d-1's start event");
    assert_eq!(d1_start["parent"], json!("coordination:default:fo-1:3"));

    // The release-gate read: the UNION of the team's persisted journals,
    // assembled client-side with TeamTrace's exact semantics. The golden
    // expectation — every event id, its kind, and its parent link:
    //
    //   fo-1:0 CoordinationStart (the team's root spawn event)
    //   fo-1:1 MailboxSend alpha     ← fo-1:0
    //   fo-1:2 MailboxSend beta      ← fo-1:0
    //   fo-1:3 MailboxReceive alpha  ← fo-1:1   (journaled pre-crash)
    //   fo-1:4 MailboxReceive beta   ← fo-1:2   (journaled post-restart)
    //   fo-1:5 CoordinationEnd       ← fo-1:0
    //   d-1:0  CoordinationStart     ← fo-1:3   (the cross-journal stitch)
    //   d-1:1  MailboxSend follow    ← d-1:0
    //   d-1:2  MailboxReceive follow ← d-1:1
    //   d-1:3  CoordinationEnd       ← d-1:0
    // Both journals survived the SIGKILL intact: contiguous seqs from 0.
    for (record, len) in [(&fo1_settled, 6), (&d1_settled, 4)] {
        let events = journal_events(record);
        assert_eq!(events.len(), len, "journal events: {events:?}");
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event["seq"], json!(i as u64), "journal event seqs");
        }
    }
    let team_events: Vec<Value> = journal_events(&fo1_settled)
        .iter()
        .chain(journal_events(&d1_settled).iter())
        .cloned()
        .collect();
    assert_one_connected_tree(
        &team_events,
        &[
            ("coordination:default:fo-1:0", "coordination_start", None),
            (
                "coordination:default:fo-1:1",
                "mailbox_send",
                Some("coordination:default:fo-1:0"),
            ),
            (
                "coordination:default:fo-1:2",
                "mailbox_send",
                Some("coordination:default:fo-1:0"),
            ),
            (
                "coordination:default:fo-1:3",
                "mailbox_receive",
                Some("coordination:default:fo-1:1"),
            ),
            (
                "coordination:default:fo-1:4",
                "mailbox_receive",
                Some("coordination:default:fo-1:2"),
            ),
            (
                "coordination:default:fo-1:5",
                "coordination_end",
                Some("coordination:default:fo-1:0"),
            ),
            (
                "coordination:default:d-1:0",
                "coordination_start",
                Some("coordination:default:fo-1:3"),
            ),
            (
                "coordination:default:d-1:1",
                "mailbox_send",
                Some("coordination:default:d-1:0"),
            ),
            (
                "coordination:default:d-1:2",
                "mailbox_receive",
                Some("coordination:default:d-1:1"),
            ),
            (
                "coordination:default:d-1:3",
                "coordination_end",
                Some("coordination:default:d-1:0"),
            ),
        ],
        "coordination:default:fo-1:0",
    );

    // Generation 2 is drained by SIGKILL too (the guard's Drop would do
    // it; an explicit kill keeps the teardown symmetric and immediate).
    let _ = supervisor2.sigkill().await;
    let _ = worker_a2.sigkill().await;
    let _ = worker_b2.sigkill().await;
    let _ = server2.sigkill().await;

    let _ = std::fs::remove_dir_all(root);
}
