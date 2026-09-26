"""No-I/O unit tests for the TasksClient (R0.6 durable task queue,
control plane).

Transport is faked at the same two seams the SSE parser tests use:
``mock.patch.object(client, "_request")`` to capture method/path/body
and answer canned JSON, and ``urllib.request.urlopen`` for the
error-translation path. The wire shapes asserted here are read from
rusty-server/src/routes.rs (enqueue_task, get_task, list_tasks,
cancel_task, cancel_run) — the server API is the frozen contract.
"""

import io
import json
import sys
import unittest
import urllib.error
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO_ROOT / "sdks" / "python"))

from rusty_client import RustyClient, RustyError, TasksClient  # noqa: E402

TASK_RECORD = {
    "task_id": "t-123",
    "kind": "send_email",
    "payload": {"to": "a@b.c"},
    "pool": "default",
    "status": "queued",
    "attempt": 0,
    "max_attempts": 3,
    "error_class": None,
    "effect": "idempotent",
    "last_error": None,
    "idempotency_key": "order-42",
    "result": None,
    "receipt": None,
    "run_id": None,
    "thread_id": None,
    "cancel_requested": False,
    "deadline": None,
    "lease": None,
    "next_attempt_at": None,
    "created_at": "2026-08-10T09:00:00Z",
    "updated_at": "2026-08-10T09:00:00Z",
}


def make_client() -> RustyClient:
    return RustyClient("http://unit.test")


class TestTasksAccessor(unittest.TestCase):
    def test_tasks_returns_cached_client_bound_to_parent(self) -> None:
        client = make_client()
        first, second = client.tasks, client.tasks
        self.assertIsInstance(first, TasksClient)
        self.assertIs(first, second, "the accessor caches one instance")
        self.assertIs(first._client, client)


class TestEnqueue(unittest.TestCase):
    def test_minimal_enqueue_omits_optional_fields(self) -> None:
        client = make_client()
        with mock.patch.object(
            client, "_request", return_value={"task_id": "t-1", "deduplicated": False}
        ) as req:
            out = client.tasks.enqueue("send_email", {"to": "a@b.c"})
        req.assert_called_once_with(
            "POST", "/tasks", {"kind": "send_email", "payload": {"to": "a@b.c"}}
        )
        self.assertEqual(out, {"task_id": "t-1", "deduplicated": False})

    def test_full_enqueue_maps_every_option(self) -> None:
        client = make_client()
        with mock.patch.object(
            client, "_request", return_value={"task_id": "t-2", "deduplicated": True}
        ) as req:
            out = client.tasks.enqueue(
                "charge_card",
                {"amount": 100},
                pool="billing",
                max_attempts=5,
                idempotency_key="order-42",
                effect="idempotent",
                run_id="r-9",
                thread_id="th-7",
                deadline="2026-08-11T00:00:00Z",
            )
        req.assert_called_once_with(
            "POST",
            "/tasks",
            {
                "kind": "charge_card",
                "payload": {"amount": 100},
                "pool": "billing",
                "max_attempts": 5,
                "idempotency_key": "order-42",
                "effect": "idempotent",
                "run_id": "r-9",
                "thread_id": "th-7",
                "deadline": "2026-08-11T00:00:00Z",
            },
        )
        # The dedup case (HTTP 200) folds into the boolean — same shape.
        self.assertTrue(out["deduplicated"])

    def test_enqueue_outbox_uses_the_outbox_route(self) -> None:
        client = make_client()
        with mock.patch.object(
            client, "_request", return_value={"task_id": "t-3", "deduplicated": False}
        ) as req:
            client.tasks.enqueue_outbox("reindex", {"shard": 3})
        req.assert_called_once_with(
            "POST", "/tasks/outbox", {"kind": "reindex", "payload": {"shard": 3}}
        )


class TestGetListCancel(unittest.TestCase):
    def test_get_quotes_the_task_id(self) -> None:
        client = make_client()
        with mock.patch.object(client, "_request", return_value=TASK_RECORD) as req:
            out = client.tasks.get("t/odd id")
        req.assert_called_once_with("GET", "/tasks/t%2Fodd%20id")
        self.assertEqual(out["task_id"], "t-123")

    def test_list_without_filter_hits_bare_route(self) -> None:
        client = make_client()
        with mock.patch.object(client, "_request", return_value=[TASK_RECORD]) as req:
            out = client.tasks.list()
        req.assert_called_once_with("GET", "/tasks")
        self.assertEqual(len(out), 1)

    def test_list_with_status_sends_query(self) -> None:
        client = make_client()
        with mock.patch.object(client, "_request", return_value=[]) as req:
            client.tasks.list(status="dead")
        req.assert_called_once_with("GET", "/tasks?status=dead")

    def test_list_rejects_unknown_status_client_side(self) -> None:
        client = make_client()
        with mock.patch.object(client, "_request") as req:
            with self.assertRaises(ValueError):
                client.tasks.list(status="dlq")  # the DLQ is status="dead"
        req.assert_not_called()

    def test_cancel_posts_to_the_cancel_route(self) -> None:
        client = make_client()
        record = dict(TASK_RECORD, status="cancelled", error_class="cancelled")
        with mock.patch.object(client, "_request", return_value=record) as req:
            out = client.tasks.cancel("t-123")
        req.assert_called_once_with("POST", "/tasks/t-123/cancel")
        self.assertEqual(out["status"], "cancelled")

    def test_cancel_run_tasks_posts_to_the_run_route(self) -> None:
        client = make_client()
        body = {"run_id": "r-9", "cancelled": ["t-1"], "signalled": ["t-2"]}
        with mock.patch.object(client, "_request", return_value=body) as req:
            out = client.tasks.cancel_run_tasks("r-9")
        req.assert_called_once_with("POST", "/runs/r-9/cancel")
        self.assertEqual(out["cancelled"], ["t-1"])
        self.assertEqual(out["signalled"], ["t-2"])


class TestErrorTranslation(unittest.TestCase):
    """A 404 from GET /tasks/{id} must surface as RustyError with the
    status and body attached — the everything-is-RustyError contract the
    rest of the client honors."""

    def test_unknown_task_raises_rusty_error_404(self) -> None:
        client = make_client()
        body = b'{"error":"not_found","message":"task `nope` not found"}'
        err = urllib.error.HTTPError(
            "http://unit.test/tasks/nope", 404, "Not Found", None, io.BytesIO(body)
        )
        with mock.patch("urllib.request.urlopen", side_effect=err):
            with self.assertRaises(RustyError) as ctx:
                client.tasks.get("nope")
        self.assertEqual(ctx.exception.status, 404)
        self.assertEqual(json.loads(ctx.exception.body)["error"], "not_found")

    def test_terminal_cancel_raises_rusty_error_409(self) -> None:
        client = make_client()
        body = (
            b'{"error":"conflict","message":"task `t-1` is already terminal '
            b'(completed) and cannot be cancelled"}'
        )
        err = urllib.error.HTTPError(
            "http://unit.test/tasks/t-1/cancel", 409, "Conflict", None, io.BytesIO(body)
        )
        with mock.patch("urllib.request.urlopen", side_effect=err):
            with self.assertRaises(RustyError) as ctx:
                client.tasks.cancel("t-1")
        self.assertEqual(ctx.exception.status, 409)


if __name__ == "__main__":
    unittest.main(verbosity=2)
