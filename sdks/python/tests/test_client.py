"""End-to-end tests for rusty_client against a LIVE rusty-server.

The suite starts the real ``server_demo`` example binary as a subprocess
(fixed bind ``127.0.0.1:8100``), waits for ``/ok``, exercises the full
API surface, and kills the server in ``tearDownClass``.

Graphs registered by ``rusty-server/examples/server_demo.rs``:

- ``pipeline``    — ``first -> second``, appending to a ``log`` channel.
- ``react_agent`` — scripted-model ReAct agent (no network) with an
  ``echo`` tool; the script is one tool call then a final answer, then
  it answers "done" forever.

If the demo binary is missing, the suite attempts
``cargo build --example server_demo`` (release, then debug) before
skipping. If port 8100 is already taken, the suite skips.

Note: server_demo registers no interrupting graph, so the
interrupt/resume round trip cannot be exercised here — see
``test_interrupt_resume_skipped`` for the documented skip.
"""

import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO_ROOT / "sdks" / "python"))

from rusty_client import RustyClient, RustyError, SSEEvent  # noqa: E402

BASE_URL = "http://127.0.0.1:8100"
CARGO = shutil.which("cargo") or str(Path.home() / ".cargo" / "bin" / "cargo")


def _port_in_use(port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        return sock.connect_ex(("127.0.0.1", port)) == 0


def _ensure_binary() -> Path:
    """Locate (or build) the server_demo example binary."""
    # The repo is a Cargo workspace: examples build into the workspace-root
    # target/. Fall back to the legacy per-crate path for pre-workspace clones.
    candidates = [
        REPO_ROOT / "target" / "release" / "examples" / "server_demo",
        REPO_ROOT / "target" / "debug" / "examples" / "server_demo",
        REPO_ROOT / "rusty-server" / "target" / "release" / "examples" / "server_demo",
        REPO_ROOT / "rusty-server" / "target" / "debug" / "examples" / "server_demo",
    ]
    for path in candidates:
        if path.exists():
            return path
    if not (shutil.which("cargo") or Path(CARGO).exists()):
        raise unittest.SkipTest("cargo not available to build server_demo")
    manifest = REPO_ROOT / "rusty-server" / "Cargo.toml"
    for profile in (["--release"], []):
        try:
            subprocess.run(
                [CARGO, "build", "--manifest-path", str(manifest),
                 "--example", "server_demo", *profile],
                check=True, capture_output=True, timeout=600,
            )
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired):
            continue
        for path in candidates:
            if path.exists():
                return path
    raise unittest.SkipTest("could not build server_demo example binary")


class LiveServerTestCase(unittest.TestCase):
    """Base class: boots one server_demo subprocess for the whole class."""

    server: subprocess.Popen
    workdir: str
    client: RustyClient

    @classmethod
    def setUpClass(cls) -> None:
        if _port_in_use(8100):
            raise unittest.SkipTest(
                "port 8100 already in use; server_demo binds it fixed"
            )
        binary = _ensure_binary()
        # Run from a scratch dir so ./data/server-demo-checkpoints is isolated.
        cls.workdir = tempfile.mkdtemp(prefix="rusty-sdk-test-")
        cls._log = open(os.path.join(cls.workdir, "server.log"), "wb")
        cls.server = subprocess.Popen(
            [str(binary)],
            cwd=cls.workdir,
            stdout=cls._log,
            stderr=subprocess.STDOUT,
        )
        cls.client = RustyClient(BASE_URL, timeout=30)
        deadline = time.time() + 30
        while time.time() < deadline:
            if cls.server.poll() is not None:
                cls._log.flush()
                with open(cls._log.name, "rb") as fh:
                    tail = fh.read()[-2000:].decode("utf-8", "replace")
                raise RuntimeError(f"server_demo exited early:\n{tail}")
            if cls.client.ok():
                break
            time.sleep(0.1)
        else:
            cls.tearDownClass()
            raise RuntimeError("server_demo did not become ready in 30s")

    @classmethod
    def tearDownClass(cls) -> None:
        proc = getattr(cls, "server", None)
        if proc is not None:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
        log = getattr(cls, "_log", None)
        if log is not None:
            log.close()
        shutil.rmtree(getattr(cls, "workdir", ""), ignore_errors=True)

    # -- helpers --------------------------------------------------------

    def new_thread(self, graph: str = "pipeline") -> str:
        return self.client.create_thread(graph)["thread_id"]


class TestServiceAndThreads(LiveServerTestCase):
    def test_01_ok_and_info(self) -> None:
        self.assertTrue(self.client.ok())
        info = self.client.info()
        self.assertEqual(info["service"], "rusty-server")
        graphs = {g["name"]: g for g in info["graphs"]}
        self.assertIn("pipeline", graphs)
        self.assertIn("react_agent", graphs)
        self.assertEqual(graphs["pipeline"]["channels"], ["log"])

    def test_02_create_thread(self) -> None:
        tid = f"sdk-{uuid.uuid4()}"
        thread = self.client.create_thread(
            "pipeline", thread_id=tid, metadata={"origin": "sdk-test"}
        )
        self.assertEqual(thread["thread_id"], tid)
        self.assertEqual(thread["graph"], "pipeline")
        self.assertEqual(thread["metadata"], {"origin": "sdk-test"})

    def test_03_error_unknown_graph(self) -> None:
        with self.assertRaises(RustyError) as ctx:
            self.client.create_thread("no_such_graph")
        self.assertIn(ctx.exception.status, (400, 404))
        self.assertIsNotNone(ctx.exception.body)


class TestRuns(LiveServerTestCase):
    def test_10_run_wait_pipeline(self) -> None:
        tid = self.new_thread()
        result = self.client.run_wait(tid)
        self.assertEqual(result["status"], "success")
        self.assertEqual(result["output"], {"log": ["first", "second"]})

    def test_11_state_and_history(self) -> None:
        tid = self.new_thread()
        self.client.run_wait(tid)

        state = self.client.get_state(tid)
        self.assertEqual(state["values"], {"log": ["first", "second"]})
        self.assertEqual(state["next"], [])
        self.assertIn("checkpoint_id", state["checkpoint"])

        history = self.client.history(tid)
        self.assertGreaterEqual(len(history), 2)
        # Newest first: steps must be non-increasing.
        steps = [h["checkpoint"]["step"] for h in history]
        self.assertEqual(steps, sorted(steps, reverse=True))
        # limit is honored.
        limited = self.client.history(tid, limit=1)
        self.assertEqual(len(limited), 1)
        self.assertEqual(limited[0]["checkpoint"]["step"], steps[0])

    def test_12_update_state(self) -> None:
        tid = self.new_thread()
        written = self.client.update_state(
            tid, {"log": ["manual"]}, as_node="first"
        )
        self.assertIn("checkpoint", written)
        state = self.client.get_state(tid)
        self.assertEqual(state["values"], {"log": ["manual"]})

    def test_13_background_run_and_status(self) -> None:
        tid = self.new_thread()
        run = self.client.run(tid)
        self.assertIn("run_id", run)
        self.assertEqual(run["thread_id"], tid)

        deadline = time.time() + 15
        status = {}
        while time.time() < deadline:
            status = self.client.run_status(run["run_id"])
            if status["status"] in ("success", "error", "interrupted"):
                break
            time.sleep(0.1)
        self.assertEqual(status["status"], "success")
        self.assertEqual(status["output"], {"log": ["first", "second"]})

    def test_14_react_agent(self) -> None:
        tid = self.new_thread("react_agent")
        result = self.client.run_wait(
            tid, input={"messages": [{"role": "user", "content": "say pong"}]}
        )
        self.assertEqual(result["status"], "success")
        messages = result["output"]["messages"]
        self.assertTrue(any(m.get("role") == "assistant" for m in messages))

    def test_15_run_via_assistant(self) -> None:
        assistant = self.client.create_assistant(
            name=f"pipe-bot-{uuid.uuid4().hex[:8]}",
            graph="pipeline",
            config={"recursion_limit": 10},
        )
        self.assertIn("assistant_id", assistant)
        tid = self.new_thread()
        result = self.client.run_wait(
            tid, assistant_id=assistant["assistant_id"]
        )
        self.assertEqual(result["status"], "success")


class TestStreaming(LiveServerTestCase):
    def test_20_stream_collect_frames(self) -> None:
        tid = self.new_thread()
        frames = list(
            self.client.run_stream(tid, stream_mode=["updates", "values"])
        )
        self.assertTrue(all(isinstance(f, SSEEvent) for f in frames))

        # First frame is always run metadata.
        self.assertEqual(frames[0].event, "metadata")
        self.assertEqual(frames[0].data["graph"], "pipeline")
        self.assertEqual(frames[0].data["thread_id"], tid)

        # One `updates` frame per executed node, in order. Each payload
        # carries the post-reducer channel value read back from the merged
        # state (per the server README), so the Append `log` channel
        # accumulates across steps.
        updates = [f.data for f in frames if f.event == "updates"]
        self.assertEqual(
            [u["updates"]["log"] for u in updates],
            [["first"], ["first", "second"]],
        )

        # `values` frames carry the full state per step.
        values = [f.data for f in frames if f.event == "values"]
        self.assertEqual(values[-1], {"log": ["first", "second"]})

        # Terminal frame.
        self.assertEqual(frames[-1].event, "end")
        self.assertEqual(frames[-1].data["status"], "success")

        # Every frame carries an id usable for Last-Event-ID resume.
        self.assertTrue(all(f.id for f in frames))

    def test_21_stream_mode_filter(self) -> None:
        tid = self.new_thread()
        events = [
            f.event
            for f in self.client.run_stream(tid, stream_mode=["values"])
        ]
        self.assertIn("values", events)
        self.assertNotIn("updates", events)
        # metadata / end are always emitted regardless of filter.
        self.assertEqual(events[0], "metadata")
        self.assertEqual(events[-1], "end")


class TestTimeTravel(LiveServerTestCase):
    def test_30_fork_and_replay(self) -> None:
        tid = self.new_thread()
        self.client.run_wait(tid)

        # Find the mid-graph checkpoint (step 0, `second` still pending).
        history = self.client.history(tid)
        mid = next(
            h for h in history
            if h["checkpoint"]["step"] == 0 and h["next"] == ["second"]
        )
        cp_id = mid["checkpoint"]["checkpoint_id"]
        self.assertEqual(mid["values"], {"log": ["first"]})

        # Mid-history fork: copy only up to that checkpoint.
        fork_id = f"fork-{uuid.uuid4()}"
        fork = self.client.fork(tid, checkpoint_id=cp_id, new_thread_id=fork_id)
        self.assertEqual(fork["thread_id"], fork_id)
        self.assertGreaterEqual(fork["checkpoints_copied"], 1)

        # Replay on the fork from that checkpoint: only `second` re-runs.
        result = self.client.run_wait(fork_id, checkpoint_id=cp_id)
        self.assertEqual(result["status"], "success")
        self.assertEqual(result["output"], {"log": ["first", "second"]})

        # The fork grew its own history; the source thread is untouched.
        self.assertGreaterEqual(len(self.client.history(fork_id)), 2)
        self.assertEqual(
            len(self.client.history(tid)), len(history)
        )

    def test_31_fork_unknown_checkpoint_404(self) -> None:
        tid = self.new_thread()
        self.client.run_wait(tid)
        with self.assertRaises(RustyError) as ctx:
            self.client.fork(tid, checkpoint_id=str(uuid.uuid4()))
        self.assertEqual(ctx.exception.status, 404)


class TestPlatformSurface(LiveServerTestCase):
    def test_40_assistants(self) -> None:
        name = f"sdk-bot-{uuid.uuid4().hex[:8]}"
        created = self.client.create_assistant(
            name=name, graph="pipeline", metadata={"suite": "e2e"}
        )
        aid = created["assistant_id"]

        fetched = self.client.get_assistant(aid)
        self.assertEqual(fetched["name"], name)
        self.assertEqual(fetched["graph"], "pipeline")

        listed = self.client.list_assistants()
        self.assertIn(aid, [a["assistant_id"] for a in listed])

        with self.assertRaises(RustyError) as ctx:
            self.client.get_assistant(str(uuid.uuid4()))
        self.assertEqual(ctx.exception.status, 404)

    def test_41_crons(self) -> None:
        created = self.client.create_cron(
            graph="pipeline",
            interval_secs=3600,  # long: must not fire during the suite
            metadata={"suite": "e2e"},
        )
        cron_id = created.get("cron_id") or created.get("id")
        self.assertTrue(cron_id)

        listed = self.client.list_crons()
        match = [
            c for c in listed
            if (c.get("cron_id") or c.get("id")) == cron_id
        ]
        self.assertEqual(len(match), 1)
        self.assertEqual(match[0]["graph"], "pipeline")

        self.client.delete_cron(cron_id)
        listed = self.client.list_crons()
        self.assertNotIn(
            cron_id, [(c.get("cron_id") or c.get("id")) for c in listed]
        )
        with self.assertRaises(RustyError) as ctx:
            self.client.delete_cron(cron_id)
        self.assertEqual(ctx.exception.status, 404)

        # Exactly one schedule kind is enforced client-side too.
        with self.assertRaises(ValueError):
            self.client.create_cron(graph="pipeline")

    def test_42_kv_store(self) -> None:
        ns = f"suite-{uuid.uuid4().hex[:8]}"

        # Unwritten namespace lists as empty.
        self.assertEqual(self.client.kv_list(ns), [])

        self.client.kv_put(ns, "user-1", {"preference": "dark-mode"})
        self.client.kv_put(ns, "user-2", [1, 2, 3])

        item = self.client.kv_get(ns, "user-1")
        self.assertEqual(item["value"], {"preference": "dark-mode"})
        self.assertEqual(item["key"], "user-1")
        self.assertEqual(item["namespace"], ns)

        # Replace preserves the key; listing is sorted by key.
        self.client.kv_put(ns, "user-1", {"preference": "light-mode"})
        self.assertEqual(
            self.client.kv_get(ns, "user-1")["value"],
            {"preference": "light-mode"},
        )
        keys = [i["key"] for i in self.client.kv_list(ns)]
        self.assertEqual(keys, ["user-1", "user-2"])

        self.client.kv_delete(ns, "user-1")
        with self.assertRaises(RustyError) as ctx:
            self.client.kv_get(ns, "user-1")
        self.assertEqual(ctx.exception.status, 404)
        self.assertEqual([i["key"] for i in self.client.kv_list(ns)], ["user-2"])

    def test_43_api_key_header_sent(self) -> None:
        # Dev-mode server has auth disabled, so a key-bearing client must
        # still work — this exercises the X-Api-Key code path end to end.
        keyed = RustyClient(BASE_URL, api_key="test-key", timeout=10)
        self.assertTrue(keyed.ok())
        self.assertEqual(keyed.info()["service"], "rusty-server")


class TestFlightRecorder(LiveServerTestCase):
    """Flight Recorder: `RustyClient.run_events` against journaled demo runs."""

    def test_50_run_events_pipeline(self) -> None:
        tid = self.new_thread()
        terminal = self.client.run_wait(tid)
        self.assertEqual(terminal["status"], "success")
        run_id = terminal["run_id"]

        body = self.client.run_events(run_id)
        self.assertEqual(body["run_id"], run_id)
        self.assertIs(body["complete"], True)
        events = body["events"]
        self.assertTrue(events, "a journaled run must have events")

        # Total order + deterministic ids: seq is 0..n, id is {run_id}:{seq}.
        for seq, event in enumerate(events):
            for field in (
                "id", "run_id", "thread_id", "node_id", "seq", "kind",
                "effect", "input", "output", "latency_ms", "tokens",
                "cost_usd", "status", "parent", "recorded_at",
            ):
                self.assertIn(field, event)
            self.assertEqual(event["seq"], seq)
            self.assertEqual(event["id"], f"{run_id}:{seq}")
            self.assertEqual(event["run_id"], run_id)
            self.assertEqual(event["thread_id"], tid)

        # The executor journaled the full lifecycle.
        kinds = [e["kind"] for e in events]
        for expected in (
            "super_step_start", "super_step_end", "node_input",
            "node_output", "routing_decision", "checkpoint_written",
        ):
            self.assertIn(expected, kinds)
        self.assertEqual(kinds[0], "super_step_start")

        # Payload refs are adjacently tagged; demo pipeline nodes are pure.
        node_input = next(e for e in events if e["kind"] == "node_input")
        self.assertEqual(node_input["input"]["kind"], "inline")
        self.assertEqual(node_input["effect"], "pure")

    def test_51_run_events_react_agent(self) -> None:
        tid = self.new_thread("react_agent")
        terminal = self.client.run_wait(
            tid, input={"messages": [{"role": "user", "content": "say pong"}]}
        )
        self.assertEqual(terminal["status"], "success")
        body = self.client.run_events(terminal["run_id"])
        self.assertIs(body["complete"], True)
        kinds = [e["kind"] for e in body["events"]]
        self.assertIn("checkpoint_written", kinds)

    def test_52_run_events_unknown_run_404(self) -> None:
        with self.assertRaises(RustyError) as ctx:
            self.client.run_events(str(uuid.uuid4()))
        self.assertEqual(ctx.exception.status, 404)


class TestReplayAndDiff(LiveServerTestCase):
    """Server-side replay + branch diff: `replay_run`, `diff_runs`,
    `get_fixture` against journaled demo runs."""

    def test_60_replay_pipeline_run_verifies(self) -> None:
        tid = self.new_thread()
        terminal = self.client.run_wait(tid)
        self.assertEqual(terminal["status"], "success")
        run_id = terminal["run_id"]
        event_count = len(self.client.run_events(run_id)["events"])

        report = self.client.replay_run(run_id)
        self.assertEqual(
            sorted(report.keys()),
            [
                "actual_events",
                "expected_events",
                "first_divergence",
                "run_id",
                "verified",
            ],
        )
        self.assertEqual(report["run_id"], run_id)
        self.assertIs(report["verified"], True)
        self.assertEqual(report["expected_events"], event_count)
        self.assertEqual(report["actual_events"], event_count)
        self.assertIsNone(report["first_divergence"])

    def test_61_replay_unknown_run_404(self) -> None:
        with self.assertRaises(RustyError) as ctx:
            self.client.replay_run(str(uuid.uuid4()))
        self.assertEqual(ctx.exception.status, 404)

    def test_62_diff_of_a_run_and_its_fork_diverges(self) -> None:
        tid = self.new_thread()
        base_run = self.client.run_wait(tid, input={"seed": 1})["run_id"]

        fork = self.client.fork(tid)
        branch_run = self.client.run_wait(
            fork["thread_id"], input={"seed": 2}
        )["run_id"]

        diff = self.client.diff_runs(base_run, branch_run)
        for field in (
            "first_divergent_seq", "added", "removed",
            "step_diffs", "base_totals", "branch_totals",
        ):
            self.assertIn(field, diff)
        self.assertIsNotNone(
            diff["first_divergent_seq"],
            f"forks with different inputs must diverge: {diff}",
        )
        self.assertTrue(diff["added"])
        self.assertTrue(diff["removed"])
        self.assertEqual(
            diff["base_totals"]["events"], diff["branch_totals"]["events"]
        )

        # A run diffed against itself is logically identical.
        same = self.client.diff_runs(base_run, base_run)
        self.assertIsNone(same["first_divergent_seq"])
        self.assertEqual(same["added"], [])
        self.assertEqual(same["removed"], [])

    def test_63_diff_unknown_run_404(self) -> None:
        tid = self.new_thread()
        run_id = self.client.run_wait(tid)["run_id"]
        with self.assertRaises(RustyError) as ctx:
            self.client.diff_runs(run_id, str(uuid.uuid4()))
        self.assertEqual(ctx.exception.status, 404)

    def test_64_get_fixture_downloads_a_replay_bundle(self) -> None:
        tid = self.new_thread()
        terminal = self.client.run_wait(tid)
        run_id = terminal["run_id"]

        fixture = self.client.get_fixture(run_id)
        self.assertEqual(fixture["format_version"], 1)
        self.assertTrue(fixture["graph_hash"])
        self.assertEqual(fixture["journal"]["run_id"], run_id)
        self.assertEqual(fixture["journal"]["thread_id"], tid)
        self.assertTrue(fixture["journal"]["events"])
        # The demo run wrote checkpoints, so the bundle carries the final one.
        self.assertEqual(fixture["final_checkpoint"]["thread_id"], tid)

        with self.assertRaises(RustyError) as ctx:
            self.client.get_fixture(str(uuid.uuid4()))
        self.assertEqual(ctx.exception.status, 404)


class TestInterruptResume(unittest.TestCase):
    @unittest.skip(
        "server_demo registers no interrupting graph (pipeline and "
        "react_agent both run to completion), so the interrupt/resume "
        "round trip is not exercisable against this binary. The client's "
        "resume path is run_wait(tid, command={'resume': value})."
    )
    def test_interrupt_resume_round_trip(self) -> None:
        pass


if __name__ == "__main__":
    unittest.main(verbosity=2)
