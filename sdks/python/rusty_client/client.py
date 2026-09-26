"""Stdlib-only HTTP/SSE client for rusty-server.

Everything here is built on ``urllib.request``, ``json``, and
``urllib.error`` — no ``requests``, no ``httpx``, no ``sseclient``.
The module has four public names:

- :class:`RustyClient` — the API client.
- :class:`TasksClient` — the control-plane face of the R0.6 durable
  task queue, reached as ``client.tasks``.
- :class:`RustyError` — raised for any non-2xx response or
  transport failure; carries ``status`` and ``body``.
- :class:`SSEEvent` — one parsed Server-Sent-Events frame
  (``event``, ``data``, ``id``), yielded by
  :meth:`RustyClient.run_stream`.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any, Dict, Generator, Iterable, List, Optional

__all__ = ["RustyClient", "TasksClient", "RustyError", "SSEEvent"]


class RustyError(Exception):
    """Error returned by rusty-server or the transport layer.

    Attributes:
        status: HTTP status code (``None`` for transport-level failures
            such as connection refused).
        body: Raw response body text (may be empty).
        message: Human-readable summary.
    """

    def __init__(
        self,
        message: str,
        status: Optional[int] = None,
        body: Optional[str] = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.status = status
        self.body = body

    def __repr__(self) -> str:  # pragma: no cover - cosmetic
        return f"RustyError(status={self.status!r}, message={self.message!r})"


@dataclass
class SSEEvent:
    """One parsed SSE frame from ``POST /threads/{id}/runs/stream``.

    Attributes:
        event: The SSE ``event:`` field (``metadata``, ``updates``,
            ``values``, ``messages``, ``error``, ``end``; ``"message"``
            when the server omits the field).
        data: The ``data:`` payload, JSON-decoded when possible,
            otherwise the raw string.
        id: The SSE ``id:`` field (``{checkpoint_id}:{step}:{seq}``),
            usable as ``last_event_id`` for a resumable reconnect.
    """

    event: str
    data: Any
    id: Optional[str] = None


class RustyClient:
    """Client for the rusty-server HTTP API.

    Args:
        base_url: Server origin, e.g. ``"http://127.0.0.1:8100"``.
        api_key: Optional static API key; sent as the ``X-Api-Key``
            header on every request (the LangSmith managed-deployment
            convention). Leave ``None`` against a dev-mode server.
        timeout: Default per-request timeout in seconds. Streaming
            requests use this as the socket read timeout between frames.
            ``0`` disables the timeout (blocking with no deadline).

    Example:
        >>> client = RustyClient("http://127.0.0.1:8100")
        >>> client.info()["service"]
        'rusty-server'
    """

    def __init__(
        self,
        base_url: str,
        api_key: Optional[str] = None,
        timeout: float = 30.0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.timeout = timeout
        self._tasks: Optional[TasksClient] = None

    @property
    def tasks(self) -> "TasksClient":
        """The durable task queue's control plane (R0.6).

        Returns a cached :class:`TasksClient` bound to this client's
        transport — same base URL, API key, and timeout.
        """
        if self._tasks is None:
            self._tasks = TasksClient(self)
        return self._tasks

    # ------------------------------------------------------------------
    # Low-level transport
    # ------------------------------------------------------------------

    def _headers(
        self,
        extra: Optional[Dict[str, str]] = None,
        has_body: bool = False,
    ) -> Dict[str, str]:
        headers = {"Accept": "application/json"}
        if has_body:
            # Bodiless GET/DELETE requests carry no Content-Type.
            headers["Content-Type"] = "application/json"
        if self.api_key is not None:
            headers["X-Api-Key"] = self.api_key
        if extra:
            headers.update(extra)
        return headers

    def _open(
        self,
        method: str,
        path: str,
        body: Any = None,
        headers: Optional[Dict[str, str]] = None,
        timeout: Optional[float] = None,
    ) -> urllib.response.addinfourl:
        """Open a request, translating failures into RustyError.

        ``timeout`` falls back to the client default when ``None``;
        ``0`` disables the timeout entirely (parity with the JS client).
        """
        url = self.base_url + path
        payload = None if body is None else json.dumps(body).encode("utf-8")
        req = urllib.request.Request(
            url,
            data=payload,
            headers=self._headers(headers, has_body=payload is not None),
            method=method,
        )
        effective = self.timeout if timeout is None else timeout
        if effective == 0:
            effective = None  # 0 = no timeout (urllib: None = blocking)
        try:
            return urllib.request.urlopen(req, timeout=effective)
        except urllib.error.HTTPError as exc:
            raw = exc.read().decode("utf-8", errors="replace") if exc.fp else ""
            raise RustyError(
                f"{method} {path} -> HTTP {exc.code}: {raw}",
                status=exc.code,
                body=raw,
            ) from exc
        except urllib.error.URLError as exc:
            raise RustyError(
                f"{method} {path} -> transport error: {exc.reason}",
                status=None,
                body=None,
            ) from exc

    def _request(
        self,
        method: str,
        path: str,
        body: Any = None,
        timeout: Optional[float] = None,
    ) -> Any:
        """Perform a JSON request and decode the JSON response."""
        resp = self._open(method, path, body=body, timeout=timeout)
        try:
            raw = resp.read().decode("utf-8")
        except OSError as exc:
            # Read-time transport failure (socket.timeout, connection
            # reset mid-body); honor the everything-is-RustyError
            # contract that _open establishes for connect-time failures.
            raise RustyError(
                f"{method} {path} -> transport error while reading response: {exc}",
                status=None,
                body=None,
            ) from exc
        finally:
            resp.close()
        if not raw:
            return None
        try:
            return json.loads(raw)
        except json.JSONDecodeError as exc:
            raise RustyError(
                f"{method} {path} -> invalid JSON response: {raw[:200]}",
                # getcode(), not .status: the attribute is 3.9+ only.
                status=resp.getcode(),
                body=raw,
            ) from exc

    # ------------------------------------------------------------------
    # Service
    # ------------------------------------------------------------------

    def ok(self) -> bool:
        """Liveness probe (``GET /ok``). Returns True when the server is up."""
        try:
            return bool(self._request("GET", "/ok").get("ok"))
        except RustyError:
            return False

    def info(self) -> Dict[str, Any]:
        """Service metadata: version, checkpointer kind, store path, and
        the registered graphs with their channels (``GET /info``)."""
        return self._request("GET", "/info")

    # ------------------------------------------------------------------
    # Threads, state, history
    # ------------------------------------------------------------------

    def create_thread(
        self,
        graph: str,
        thread_id: Optional[str] = None,
        metadata: Optional[Dict[str, Any]] = None,
    ) -> Dict[str, Any]:
        """Create a thread bound to a registered graph (``POST /threads``).

        Args:
            graph: Registered graph name (see :meth:`info`).
            thread_id: Optional client-chosen id; re-using an id after a
                server restart re-attaches to its persisted checkpoints.
            metadata: Optional free-form metadata.

        Returns:
            ``{"thread_id": ..., "graph": ..., "metadata": ..., "created_at": ...}``.
        """
        body: Dict[str, Any] = {"graph": graph}
        if thread_id is not None:
            body["thread_id"] = thread_id
        if metadata is not None:
            body["metadata"] = metadata
        return self._request("POST", "/threads", body)

    def get_state(self, thread_id: str) -> Dict[str, Any]:
        """Latest checkpoint of a thread: ``{values, next, checkpoint}``
        (``GET /threads/{id}/state``)."""
        return self._request("GET", f"/threads/{_q(thread_id)}/state")

    def update_state(
        self,
        thread_id: str,
        values: Dict[str, Any],
        as_node: Optional[str] = None,
        next_nodes: Optional[List[str]] = None,
    ) -> Dict[str, Any]:
        """Write a new checkpoint — the ``update_state`` analog
        (``POST /threads/{id}/state``).

        Args:
            values: Channel values to record.
            as_node: Optional node name to attribute the write to.
            next_nodes: Optional next-node set for the new checkpoint.
        """
        body: Dict[str, Any] = {"values": values}
        if as_node is not None:
            body["as_node"] = as_node
        if next_nodes is not None:
            body["next_nodes"] = next_nodes
        return self._request("POST", f"/threads/{_q(thread_id)}/state", body)

    def history(
        self,
        thread_id: str,
        limit: Optional[int] = None,
        before: Optional[str] = None,
    ) -> List[Dict[str, Any]]:
        """Checkpoint history, newest first (``POST /threads/{id}/history``).

        Args:
            limit: Maximum number of checkpoints to return.
            before: Only return checkpoints before this checkpoint id.
        """
        body: Dict[str, Any] = {}
        if limit is not None:
            body["limit"] = limit
        if before is not None:
            body["before"] = before
        return self._request("POST", f"/threads/{_q(thread_id)}/history", body)

    def fork(
        self,
        thread_id: str,
        checkpoint_id: Optional[str] = None,
        new_thread_id: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Time travel: copy a thread's checkpoint history into a new
        thread (``POST /threads/{id}/fork``).

        Args:
            checkpoint_id: Fork mid-history — copy only up to and
                including this checkpoint, so the fork branches off at
                that point in the timeline.
            new_thread_id: Optional client-chosen id for the fork.

        Returns:
            ``{"thread_id": ..., "checkpoints_copied": n}``.
        """
        body: Dict[str, Any] = {}
        if checkpoint_id is not None:
            body["checkpoint_id"] = checkpoint_id
        if new_thread_id is not None:
            body["new_thread_id"] = new_thread_id
        return self._request("POST", f"/threads/{_q(thread_id)}/fork", body)

    # ------------------------------------------------------------------
    # Runs
    # ------------------------------------------------------------------

    @staticmethod
    def _run_body(
        input: Optional[Dict[str, Any]],
        command: Optional[Dict[str, Any]],
        checkpoint_id: Optional[str],
        multitask_strategy: Optional[str],
        config: Optional[Dict[str, Any]],
        metadata: Optional[Dict[str, Any]],
        stream_mode: Optional[Iterable[str]],
        assistant_id: Optional[str],
    ) -> Dict[str, Any]:
        body: Dict[str, Any] = {}
        if input is not None:
            body["input"] = input
        if command is not None:
            body["command"] = command
        if checkpoint_id is not None:
            body["checkpoint"] = {"checkpoint_id": checkpoint_id}
        if multitask_strategy is not None:
            body["multitask_strategy"] = multitask_strategy
        if config is not None:
            body["config"] = config
        if metadata is not None:
            body["metadata"] = metadata
        if stream_mode is not None:
            body["stream_mode"] = list(stream_mode)
        if assistant_id is not None:
            body["assistant_id"] = assistant_id
        return body

    def run(
        self,
        thread_id: str,
        input: Optional[Dict[str, Any]] = None,
        command: Optional[Dict[str, Any]] = None,
        checkpoint_id: Optional[str] = None,
        multitask_strategy: Optional[str] = None,
        config: Optional[Dict[str, Any]] = None,
        metadata: Optional[Dict[str, Any]] = None,
        assistant_id: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Start a **background** run (``POST /threads/{id}/runs`` -> 202).

        Args:
            input: Graph input (e.g. ``{"messages": [...]}``).
            command: e.g. ``{"resume": value}`` for human-in-the-loop.
            checkpoint_id: Replay from this checkpoint instead of the
                latest (time travel).
            multitask_strategy: ``"enqueue"`` (default) or ``"reject"``
                (409 while another run is active on the thread).
            config: e.g. ``{"recursion_limit": 25}``.
            metadata: Free-form run metadata.
            assistant_id: Run through a named assistant (must be bound
                to the thread's graph).

        Returns:
            ``{"run_id": ..., "thread_id": ..., "status": ...}``.
            Poll with :meth:`run_status`.
        """
        body = self._run_body(
            input, command, checkpoint_id, multitask_strategy,
            config, metadata, None, assistant_id,
        )
        return self._request("POST", f"/threads/{_q(thread_id)}/runs", body)

    def run_wait(
        self,
        thread_id: str,
        input: Optional[Dict[str, Any]] = None,
        command: Optional[Dict[str, Any]] = None,
        checkpoint_id: Optional[str] = None,
        multitask_strategy: Optional[str] = None,
        config: Optional[Dict[str, Any]] = None,
        metadata: Optional[Dict[str, Any]] = None,
        assistant_id: Optional[str] = None,
        timeout: Optional[float] = None,
    ) -> Dict[str, Any]:
        """Run to completion and return the terminal JSON
        (``POST /threads/{id}/runs/wait``).

        The terminal payload is ``{"status": "success"|"interrupted"|"error",
        "output": ...}`` plus ``interrupt`` / ``checkpoint_id`` / ``state``
        fields when the run was interrupted. ``timeout`` defaults to the
        client timeout — raise it for long-running graphs.
        """
        body = self._run_body(
            input, command, checkpoint_id, multitask_strategy,
            config, metadata, None, assistant_id,
        )
        return self._request(
            "POST", f"/threads/{_q(thread_id)}/runs/wait", body, timeout=timeout
        )

    def run_stream(
        self,
        thread_id: str,
        input: Optional[Dict[str, Any]] = None,
        command: Optional[Dict[str, Any]] = None,
        checkpoint_id: Optional[str] = None,
        multitask_strategy: Optional[str] = None,
        config: Optional[Dict[str, Any]] = None,
        metadata: Optional[Dict[str, Any]] = None,
        stream_mode: Optional[Iterable[str]] = None,
        assistant_id: Optional[str] = None,
        last_event_id: Optional[str] = None,
        timeout: Optional[float] = None,
    ) -> Generator[SSEEvent, None, None]:
        """Run with SSE streaming (``POST /threads/{id}/runs/stream``).

        Returns a **generator** of :class:`SSEEvent` frames. Frames arrive
        as the graph executes; the stream ends after the terminal
        ``end`` frame (or an ``error`` frame).

        Args:
            stream_mode: Frame families to receive — any of
                ``"updates"``, ``"values"``, ``"messages"``; default
                ``["values", "updates"]``. ``metadata``, ``error``, and
                ``end`` frames are always emitted.
            last_event_id: Resume support — send ``Last-Event-ID`` so
                the server replays only frames after the given
                ``{checkpoint_id}:{step}:{seq}`` id.
            timeout: Socket read timeout between frames (defaults to the
                client timeout; raise it for slow LLM graphs; ``0``
                disables).

        Example:
            >>> for frame in client.run_stream(tid):
            ...     print(frame.event, frame.data)
        """
        body = self._run_body(
            input, command, checkpoint_id, multitask_strategy,
            config, metadata, stream_mode, assistant_id,
        )
        headers = {"Accept": "text/event-stream"}
        if last_event_id is not None:
            headers["Last-Event-ID"] = last_event_id
        resp = self._open(
            "POST",
            f"/threads/{_q(thread_id)}/runs/stream",
            body=body,
            headers=headers,
            timeout=timeout,
        )
        return _iter_sse(resp)

    def run_status(self, run_id: str) -> Dict[str, Any]:
        """Poll a run (``GET /runs/{run_id}``).

        Returns ``{"run_id", "thread_id", "graph", "attempt", "status"}``;
        terminal runs also carry ``output`` / ``error`` / ``interrupt``.
        """
        return self._request("GET", f"/runs/{_q(run_id)}")

    def run_events(self, run_id: str) -> Dict[str, Any]:
        """Fetch a run's Flight Recorder journal
        (``GET /runs/{run_id}/events``).

        Returns ``{"run_id", "events", "complete"}`` where ``events`` is the
        journaled ``RunEvent`` list in ``seq`` order (super-step boundaries,
        node inputs/outputs, model/tool calls, interrupts, routing
        decisions, checkpoint writes) and ``complete`` is ``True`` once the
        run is terminal — i.e. the served snapshot is the final journal.
        While a run is active the snapshot trails the live journal by at
        most one checkpoint boundary. Unknown runs raise
        :class:`RustyError` with ``status == 404``.
        """
        return self._request("GET", f"/runs/{_q(run_id)}/events")

    def get_fixture(self, run_id: str) -> Dict[str, Any]:
        """Download a run as a portable replay fixture
        (``GET /runs/{run_id}/fixture``).

        Returns core's ``ReplayFixture`` envelope:
        ``{"format_version", "graph_hash", "graph_version", "journal",
        "final_checkpoint", "metadata"}`` — feed it to
        ``ReplayFixture::import`` to re-drive the run in CI. Raises
        :class:`RustyError` with ``status == 404`` for unknown runs and
        ``status == 409`` when the run has no persisted journal yet
        (queued, or before its first checkpoint boundary).
        """
        return self._request("GET", f"/runs/{_q(run_id)}/fixture")

    def replay_run(self, run_id: str) -> Dict[str, Any]:
        """Re-drive a journaled run server-side and verify the replayed
        evidence (``POST /runs/replay``).

        The server re-executes the run's registered graph against the
        persisted journal (zero outbound calls) and compares the replayed
        evidence against the recorded journal. Returns exactly::

            {"run_id": ..., "verified": bool, "expected_events": int,
             "actual_events": int, "first_divergence": int | None}

        ``verified`` compares kinds, nodes, sequences, effect classes,
        statuses, and payloads (per-run minted checkpoint ids and wall-clock
        measurements excluded); ``first_divergence`` is the ``seq`` of the
        first disagreeing event. Raises :class:`RustyError` with
        ``status == 404`` for unknown runs, ``status == 409`` when no
        journal is persisted or the run is still executing, and
        ``status == 422`` when the run's graph is not registered in the
        server process or the journal carries recorded model/tool calls
        (those replay through the CI fixture instead).
        """
        return self._request("POST", "/runs/replay", {"run_id": run_id})

    def diff_runs(self, base: str, branch: str) -> Dict[str, Any]:
        """Structural diff of two runs' journals
        (``GET /runs/diff?base=…&branch=…``).

        Returns core's ``BranchDiff`` shape::

            {"first_divergent_seq": int | None,
             "added": [...],   # branch events at/after the divergence
             "removed": [...], # base events at/after the divergence
             "step_diffs": [{"step", "channels": [{"channel", "base", "branch"}]}],
             "base_totals": {"events", "tokens", "cost_usd"},
             "branch_totals": {"events", "tokens", "cost_usd"}}

        Events compare logically (identity/timing excluded), so two runs
        that forked from one point show their shared prefix as equal; a run
        diffed against itself has ``first_divergent_seq`` ``None``. Raises
        :class:`RustyError` with ``status == 404`` for unknown runs (either
        side) and ``status == 409`` when either run has no persisted
        journal yet.
        """
        query = urllib.parse.urlencode({"base": base, "branch": branch})
        return self._request("GET", f"/runs/diff?{query}")

    def delete_run(self, thread_id: str, run_id: str) -> Any:
        """Rollback: delete a **finished** run's checkpoints, re-anchoring
        the thread to the pre-run checkpoint
        (``DELETE /threads/{id}/runs/{run_id}``; 409 while active)."""
        return self._request(
            "DELETE", f"/threads/{_q(thread_id)}/runs/{_q(run_id)}"
        )

    # ------------------------------------------------------------------
    # Assistants
    # ------------------------------------------------------------------

    def create_assistant(
        self,
        name: str,
        graph: str,
        config: Optional[Dict[str, Any]] = None,
        metadata: Optional[Dict[str, Any]] = None,
        assistant_id: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Create a named graph alias (``POST /assistants`` -> 201).

        Assistants bind a name plus free-form ``config`` / ``metadata``
        to a registered graph so clients can create runs by
        ``assistant_id``. ``config.recursion_limit`` applies as a run
        default.
        """
        body: Dict[str, Any] = {"name": name, "graph": graph}
        if config is not None:
            body["config"] = config
        if metadata is not None:
            body["metadata"] = metadata
        if assistant_id is not None:
            body["assistant_id"] = assistant_id
        return self._request("POST", "/assistants", body)

    def list_assistants(self) -> List[Dict[str, Any]]:
        """List all assistants (``GET /assistants``)."""
        return self._request("GET", "/assistants")

    def get_assistant(self, assistant_id: str) -> Dict[str, Any]:
        """Fetch one assistant (``GET /assistants/{id}``)."""
        return self._request("GET", f"/assistants/{_q(assistant_id)}")

    # ------------------------------------------------------------------
    # Crons
    # ------------------------------------------------------------------

    def create_cron(
        self,
        graph: str,
        interval_secs: Optional[int] = None,
        cron_expr: Optional[str] = None,
        input: Optional[Dict[str, Any]] = None,
        metadata: Optional[Dict[str, Any]] = None,
        on_run_completed: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Schedule recurring runs (``POST /crons`` -> 201).

        Exactly one schedule kind is required: ``interval_secs`` (fixed
        interval, >= 1 s) or ``cron_expr`` (5-field cron, UTC). Each due
        cron fires a run on a **fresh thread** bound to ``graph``.
        ``on_run_completed="delete"`` makes the cron a one-shot.
        """
        if (interval_secs is None) == (cron_expr is None):
            raise ValueError(
                "exactly one of interval_secs / cron_expr is required"
            )
        body: Dict[str, Any] = {"graph": graph}
        if interval_secs is not None:
            body["interval_secs"] = interval_secs
        if cron_expr is not None:
            body["cron_expr"] = cron_expr
        if input is not None:
            body["input"] = input
        if metadata is not None:
            body["metadata"] = metadata
        if on_run_completed is not None:
            body["on_run_completed"] = on_run_completed
        return self._request("POST", "/crons", body)

    def list_crons(self) -> List[Dict[str, Any]]:
        """List crons with ``runs_fired`` / ``last_run_at`` bookkeeping
        (``GET /crons``)."""
        return self._request("GET", "/crons")

    def delete_cron(self, cron_id: str) -> Any:
        """Delete a cron (``DELETE /crons/{id}``; 404 when unknown)."""
        return self._request("DELETE", f"/crons/{_q(cron_id)}")

    # ------------------------------------------------------------------
    # KV store
    # ------------------------------------------------------------------

    def kv_put(self, namespace: str, key: str, value: Any) -> Dict[str, Any]:
        """Upsert a JSON value in a namespace
        (``PUT /store/{ns}/{key}``; 201 on create, 200 on replace).
        The request body *is* the value — any JSON-serializable object.
        Namespace and key are restricted to ``[A-Za-z0-9._-]``."""
        return self._request("PUT", f"/store/{_q(namespace)}/{_q(key)}", value)

    def kv_get(self, namespace: str, key: str) -> Dict[str, Any]:
        """Fetch one store item (``GET /store/{ns}/{key}``; 404 when absent)."""
        return self._request("GET", f"/store/{_q(namespace)}/{_q(key)}")

    def kv_delete(self, namespace: str, key: str) -> Any:
        """Delete one store item (``DELETE /store/{ns}/{key}``)."""
        return self._request("DELETE", f"/store/{_q(namespace)}/{_q(key)}")

    def kv_list(self, namespace: str) -> List[Dict[str, Any]]:
        """List a namespace's items, sorted by key (``GET /store/{ns}``;
        empty array for an unwritten namespace)."""
        return self._request("GET", f"/store/{_q(namespace)}")


# The lifecycle values `GET /tasks?status=…` accepts. `dead` is the DLQ
# listing. Mirrored from the server's TaskStatus::parse so a typo fails
# client-side with a clear error instead of a round trip answered 400.
_TASK_STATUSES = frozenset(
    {"queued", "leased", "failed", "completed", "dead", "cancelled"}
)


class TasksClient:
    """Control-plane client for the R0.6 durable task queue.

    Obtained as ``client.tasks``; shares the parent client's transport
    (base URL, API key, timeout). Covers the submit/observe/cancel
    operations a controlling application needs:

    - :meth:`enqueue` / :meth:`enqueue_outbox` — submit work.
    - :meth:`get` / :meth:`list` — observe records (``list`` with
      ``status="dead"`` is the dead-letter queue).
    - :meth:`cancel` / :meth:`cancel_run_tasks` — cancellation.

    Why ``claim`` / ``heartbeat`` / ``complete`` / ``fail`` are
    deliberately absent: those four endpoints are the queue's
    *worker-machine* half — lease-guarded by ``worker_id``, they exist
    so a worker process can hold, renew, and settle exactly one lease
    at a time. A control-plane caller that claimed a lease would either
    sit on it (starving real workers until the visibility timeout
    reclaims the task) or race a real worker's settlement and answer
    409s. That surface belongs to the worker SDK (``rusty-worker``'s
    ``ActivityWorker``); this client never holds leases.
    """

    def __init__(self, client: RustyClient) -> None:
        self._client = client

    @staticmethod
    def _enqueue_body(
        kind: str,
        payload: Any,
        pool: Optional[str],
        max_attempts: Optional[int],
        idempotency_key: Optional[str],
        effect: Optional[str],
        run_id: Optional[str],
        thread_id: Optional[str],
        deadline: Optional[str],
    ) -> Dict[str, Any]:
        body: Dict[str, Any] = {"kind": kind, "payload": payload}
        if pool is not None:
            body["pool"] = pool
        if max_attempts is not None:
            body["max_attempts"] = max_attempts
        if idempotency_key is not None:
            body["idempotency_key"] = idempotency_key
        if effect is not None:
            body["effect"] = effect
        if run_id is not None:
            body["run_id"] = run_id
        if thread_id is not None:
            body["thread_id"] = thread_id
        if deadline is not None:
            body["deadline"] = deadline
        return body

    def enqueue(
        self,
        kind: str,
        payload: Any,
        *,
        pool: Optional[str] = None,
        max_attempts: Optional[int] = None,
        idempotency_key: Optional[str] = None,
        effect: Optional[str] = None,
        run_id: Optional[str] = None,
        thread_id: Optional[str] = None,
        deadline: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Enqueue a durable task (``POST /tasks``).

        Args:
            kind: Work classification the worker fleet dispatches on
                (free-form label, e.g. ``"send_email"``).
            payload: Work payload — any JSON-serializable value, stored
                verbatim.
            pool: Named pool (default ``"default"``); workers claim
                from named pools.
            max_attempts: Attempt ceiling before dead-lettering
                (server default 3, max 100).
            idempotency_key: Dedup key, unique per tenant across live
                tasks. Re-enqueueing with the same key returns the
                existing task instead of creating a duplicate — check
                ``deduplicated`` in the response.
            effect: Declared effect classification (``"pure"``,
                ``"read_only"``, ``"idempotent"``, ``"compensatable"``,
                ``"non_idempotent"``). A declared non-repeatable effect
                is never silently retried; when absent, the worker's
                per-attempt ``retryable`` flag decides.
            run_id: Run this task belongs to. ``cancel_run_tasks`` (and
                ``POST /runs/{run_id}/cancel``) cancels every
                non-terminal task carrying it.
            thread_id: Thread linkage (companion to ``run_id``).
            deadline: Whole-task deadline as an RFC 3339 timestamp,
                across attempts. Past it the task is finalized as
                cancelled instead of being leased again.

        Returns:
            ``{"task_id": ..., "deduplicated": bool}`` — ``True`` when
            the idempotency key already named a live task (HTTP 200)
            rather than a fresh one being created (HTTP 201). The
            status code is intentionally folded into the boolean: the
            two cases differ in meaning, not in shape.
        """
        body = self._enqueue_body(
            kind, payload, pool, max_attempts, idempotency_key,
            effect, run_id, thread_id, deadline,
        )
        return self._client._request("POST", "/tasks", body)

    def enqueue_outbox(
        self,
        kind: str,
        payload: Any,
        *,
        pool: Optional[str] = None,
        max_attempts: Optional[int] = None,
        idempotency_key: Optional[str] = None,
        effect: Optional[str] = None,
        run_id: Optional[str] = None,
        thread_id: Optional[str] = None,
        deadline: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Enqueue through the transactional outbox (``POST
        /tasks/outbox`` -> 202 accepted).

        Same arguments as :meth:`enqueue`. The task is written to the
        outbox and becomes claimable only when the relay publishes it
        into the queue (within one poll interval). Delivery is
        at-least-once: the relay dedupes on the task's idempotency key,
        so a crash anywhere in the pipe neither loses nor doubles the
        task. Prefer this when the submission must commit atomically
        with other state; prefer :meth:`enqueue` when the task should
        be claimable immediately.
        """
        body = self._enqueue_body(
            kind, payload, pool, max_attempts, idempotency_key,
            effect, run_id, thread_id, deadline,
        )
        return self._client._request("POST", "/tasks/outbox", body)

    def get(self, task_id: str) -> Dict[str, Any]:
        """Fetch one task record (``GET /tasks/{id}``).

        Unknown or cross-tenant ids raise :class:`RustyError` with
        ``status == 404``.
        """
        return self._client._request("GET", f"/tasks/{_q(task_id)}")

    def list(self, status: Optional[str] = None) -> List[Dict[str, Any]]:
        """List the tenant's tasks, oldest first (``GET /tasks``).

        Args:
            status: Optional lifecycle filter — one of ``"queued"``,
                ``"leased"``, ``"failed"``, ``"completed"``, ``"dead"``,
                ``"cancelled"``. ``"dead"`` is the dead-letter queue.
                Validated client-side (``ValueError``) because an
                unknown filter could never match a task.
        """
        if status is not None and status not in _TASK_STATUSES:
            raise ValueError(
                f"unknown task status {status!r} "
                f"(expected {'|'.join(sorted(_TASK_STATUSES))})"
            )
        path = "/tasks"
        if status is not None:
            path += "?" + urllib.parse.urlencode({"status": status})
        return self._client._request("GET", path)

    def cancel(self, task_id: str) -> Dict[str, Any]:
        """Cancel a non-terminal task (``POST /tasks/{id}/cancel``).

        Queued and retry-scheduled tasks move to the terminal
        ``cancelled`` state immediately; a leased task keeps its lease
        with ``cancel_requested`` set, so its holder learns on the next
        heartbeat and reports the attempt as cancelled. Cancellation is
        a hint for promptness — lease expiry stays the correctness
        mechanism: a holder that never asks is finalized as cancelled
        once its lease lapses.

        Returns the updated task record. Raises :class:`RustyError`
        with ``status == 409`` when the task is already terminal and
        ``status == 404`` for unknown or cross-tenant ids.
        """
        return self._client._request("POST", f"/tasks/{_q(task_id)}/cancel")

    def cancel_run_tasks(self, run_id: str) -> Dict[str, Any]:
        """Cancel every non-terminal task belonging to a run
        (``POST /runs/{run_id}/cancel``).

        Returns ``{"run_id": ..., "cancelled": [...], "signalled":
        [...]}`` — task ids that moved to the terminal ``cancelled``
        state immediately (queued, or failed with a retry scheduled)
        versus leased task ids whose holders were signalled via
        ``cancel_requested``. Note the scope: this waves the queue half
        of run cancellation; a task enqueued *after* the call is not
        retroactively cancelled. Unknown or cross-tenant runs raise
        :class:`RustyError` with ``status == 404``.
        """
        return self._client._request("POST", f"/runs/{_q(run_id)}/cancel")


# ----------------------------------------------------------------------
# Helpers
# ----------------------------------------------------------------------


def _q(segment: str) -> str:
    """URL-quote one path segment."""
    return urllib.parse.quote(str(segment), safe="")


def _iter_sse(resp: urllib.response.addinfourl) -> Generator[SSEEvent, None, None]:
    """Parse an SSE byte stream into :class:`SSEEvent` frames.

    Frames are separated by blank lines; ``data:`` lines accumulate and
    are joined with newlines; ``event:`` defaults to ``"message"``;
    comment lines (``:...``) are ignored. JSON-looking payloads are
    decoded; anything else is yielded as raw text.
    """
    event = "message"
    data_lines: List[str] = []
    event_id: Optional[str] = None
    try:
        for raw in resp:
            line = raw.decode("utf-8", errors="replace")
            line = line.rstrip("\r\n")
            if line == "":
                # Dispatch only when data was buffered; per the SSE spec,
                # event:/id:-only blocks update parser state but emit
                # nothing.
                if data_lines:
                    yield SSEEvent(
                        event=event,
                        data=_decode_data("\n".join(data_lines)),
                        id=event_id,
                    )
                event, data_lines, event_id = "message", [], None
                continue
            if line.startswith(":"):
                continue  # SSE comment / heartbeat
            field, sep, value = line.partition(":")
            if not sep:
                continue  # malformed line; ignore
            if value.startswith(" "):
                value = value[1:]
            if field == "event":
                event = value
            elif field == "data":
                data_lines.append(value)
            elif field == "id":
                event_id = value
        # Flush a trailing frame not terminated by a blank line.
        if data_lines:
            yield SSEEvent(
                event=event,
                data=_decode_data("\n".join(data_lines)),
                id=event_id,
            )
    except OSError as exc:
        # Read-time transport failure on a stalled stream
        # (socket.timeout between frames, reset by peer).
        raise RustyError(
            f"SSE stream read failed: {exc}",
            status=None,
            body=None,
        ) from exc
    finally:
        resp.close()


def _decode_data(raw: str) -> Any:
    """JSON-decode an SSE data payload when possible."""
    try:
        return json.loads(raw)
    except ValueError:  # JSONDecodeError subclasses ValueError
        return raw
