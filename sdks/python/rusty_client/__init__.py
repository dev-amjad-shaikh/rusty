"""rusty-agent-runtime: zero-dependency Python SDK for rusty-server.

A stdlib-only (``urllib.request`` + ``json``) HTTP/SSE client for the
``rusty-server`` HTTP API: threads, runs (background / blocking /
SSE-streaming), checkpoint history, time travel (fork + replay),
assistants, crons, the cross-thread KV store, and the R0.6 durable
task queue's control plane (``client.tasks``).

Quickstart::

    from rusty_client import RustyClient

    client = RustyClient("http://127.0.0.1:8100")
    print(client.info())

    thread = client.create_thread("pipeline")
    result = client.run_wait(thread["thread_id"])
    print(result["status"], result["output"])

No third-party packages are required — Python 3.8+ is enough.
"""

from .client import RustyClient, RustyError, SSEEvent, TasksClient

__all__ = ["RustyClient", "TasksClient", "RustyError", "SSEEvent"]
__version__ = "0.1.0"
