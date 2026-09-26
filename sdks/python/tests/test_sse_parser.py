"""No-I/O unit tests for rusty_client's hand-rolled SSE parser and
transport-error translation.

These mirror the JS SDK's parser fixtures (sdks/typescript/test/
client.test.js "SSE parser unit tests"): the live server only ever emits
well-formed LF frames, so CRLF, multi-line ``data:``, comment/keepalive,
id-only blocks, EOF flush, and read-time timeouts are exercised here
against fake response objects.
"""

import io
import socket
import sys
import unittest
from pathlib import Path
from unittest import mock

REPO_ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(REPO_ROOT / "sdks" / "python"))

from rusty_client import RustyClient, RustyError, SSEEvent  # noqa: E402
from rusty_client.client import _iter_sse  # noqa: E402


class FakeSseResponse:
    """Minimal addinfourl stand-in: line-iterable byte stream + close()."""

    def __init__(self, payload: bytes) -> None:
        self._buf = io.BytesIO(payload)
        self.closed = False

    def __iter__(self):
        return iter(self._buf)

    def close(self) -> None:
        self.closed = True


class TimeoutSseResponse:
    """Yields one line, then stalls the way a timed-out socket read does."""

    def __init__(self) -> None:
        self.closed = False

    def __iter__(self):
        yield b"event: updates\n"
        raise socket.timeout("timed out")

    def close(self) -> None:
        self.closed = True


class FakeJsonResponse:
    """Body-read stand-in for _request: read()/close()/getcode()."""

    def __init__(self, payload: bytes = b"{}", read_error: Exception = None) -> None:
        self._payload = payload
        self._read_error = read_error
        self.closed = False

    def read(self, *args) -> bytes:
        if self._read_error is not None:
            raise self._read_error
        return self._payload

    def close(self) -> None:
        self.closed = True

    def getcode(self) -> int:
        return 200


class TestIterSse(unittest.TestCase):
    def test_crlf_frames_with_keepalive_and_id(self) -> None:
        resp = FakeSseResponse(
            b": keepalive\r\n\r\n"
            b"event: metadata\r\n"
            b"id: -:0:1\r\n"
            b'data: {"run_id":"r1"}\r\n'
            b"\r\n"
        )
        frames = list(_iter_sse(resp))
        self.assertEqual(len(frames), 1)
        self.assertEqual(frames[0].event, "metadata")
        self.assertEqual(frames[0].data, {"run_id": "r1"})
        self.assertEqual(frames[0].id, "-:0:1")
        self.assertTrue(resp.closed, "response closed after full iteration")

    def test_multi_line_data_joined_and_json_decoded(self) -> None:
        resp = FakeSseResponse(
            b"event: updates\n"
            b"id: cp1:0:2\n"
            b'data: {"step":0,\n'
            b'data: "updates":{"first":{}}}\n'
            b"\n"
        )
        frames = list(_iter_sse(resp))
        self.assertEqual(len(frames), 1)
        self.assertEqual(frames[0].event, "updates")
        self.assertEqual(frames[0].id, "cp1:0:2")
        self.assertEqual(frames[0].data, {"step": 0, "updates": {"first": {}}})

    def test_non_json_payload_passes_through_as_string(self) -> None:
        resp = FakeSseResponse(b"event: end\ndata: not-json\n\n")
        frames = list(_iter_sse(resp))
        self.assertEqual(len(frames), 1)
        self.assertEqual(frames[0].event, "end")
        self.assertEqual(frames[0].data, "not-json")

    def test_eof_flush_without_trailing_blank_line(self) -> None:
        resp = FakeSseResponse(b'event: end\ndata: {"status":"success"}')
        frames = list(_iter_sse(resp))
        self.assertEqual(len(frames), 1)
        self.assertEqual(frames[0].event, "end")
        self.assertEqual(frames[0].data, {"status": "success"})

    def test_comment_and_id_only_blocks_dispatch_nothing(self) -> None:
        # Per the SSE spec, blocks without data: lines update parser
        # state (last-event-id) but must not dispatch an event.
        resp = FakeSseResponse(
            b": ping\n\n"
            b"id: abc\n\n"
            b"event: updates\ndata: {}\n\n"
        )
        frames = list(_iter_sse(resp))
        self.assertEqual(len(frames), 1)
        self.assertEqual(frames[0].event, "updates")
        self.assertEqual(frames[0].data, {})

    def test_malformed_and_unknown_field_lines_ignored(self) -> None:
        resp = FakeSseResponse(
            b"garbage line without colon... has one: but odd field\n"
            b"retry: 1000\n"
            b"data: {}\n\n"
        )
        frames = list(_iter_sse(resp))
        self.assertEqual(len(frames), 1)
        self.assertEqual(frames[0].event, "message")
        self.assertEqual(frames[0].data, {})

    def test_early_close_closes_response(self) -> None:
        resp = FakeSseResponse(
            b'event: updates\ndata: {"a":1}\n\n'
            b'event: updates\ndata: {"b":2}\n\n'
        )
        gen = _iter_sse(resp)
        first = next(gen)
        self.assertIsInstance(first, SSEEvent)
        self.assertFalse(resp.closed)
        gen.close()  # consumer abandons the stream early
        self.assertTrue(resp.closed, "response closed on generator.close()")

    def test_read_timeout_translated_to_rusty_error(self) -> None:
        resp = TimeoutSseResponse()
        with self.assertRaises(RustyError) as ctx:
            list(_iter_sse(resp))
        self.assertIsNone(ctx.exception.status)
        self.assertTrue(resp.closed, "response closed even on read failure")


class TestTransportWrapping(unittest.TestCase):
    """_request must translate read-time failures, matching _open's
    connect-time translation (the module's documented contract)."""

    def test_body_read_timeout_wrapped(self) -> None:
        client = RustyClient("http://unit.test")
        resp = FakeJsonResponse(read_error=socket.timeout("timed out"))
        with mock.patch.object(client, "_open", return_value=resp):
            with self.assertRaises(RustyError) as ctx:
                client.info()
        self.assertIsNone(ctx.exception.status)
        self.assertTrue(resp.closed)

    def test_invalid_json_reports_status_via_getcode(self) -> None:
        # Regression: the JSON-decode-error branch must not rely on the
        # addinfourl.status attribute, which only exists on Python 3.9+.
        client = RustyClient("http://unit.test")
        resp = FakeJsonResponse(payload=b"<html>not json</html>")
        with mock.patch.object(client, "_open", return_value=resp):
            with self.assertRaises(RustyError) as ctx:
                client.info()
        self.assertEqual(ctx.exception.status, 200)
        self.assertEqual(ctx.exception.body, "<html>not json</html>")


class TestTimeoutResolution(unittest.TestCase):
    """timeout=None falls back to the client default; 0 disables."""

    def _capture(self, client: RustyClient, timeout=None) -> float:
        captured = {}

        def fake_urlopen(req, timeout=None):
            captured["timeout"] = timeout
            return FakeJsonResponse(payload=b'{"ok": true}')

        with mock.patch("urllib.request.urlopen", side_effect=fake_urlopen):
            client._request("GET", "/ok", timeout=timeout)
        return captured["timeout"]

    def test_default_applies_when_unset(self) -> None:
        client = RustyClient("http://unit.test", timeout=30)
        self.assertEqual(self._capture(client), 30)

    def test_explicit_timeout_wins(self) -> None:
        client = RustyClient("http://unit.test", timeout=30)
        self.assertEqual(self._capture(client, timeout=5), 5)

    def test_zero_disables(self) -> None:
        client = RustyClient("http://unit.test", timeout=30)
        self.assertIsNone(self._capture(client, timeout=0))


class TestRunStreamHeaders(unittest.TestCase):
    def test_accept_and_last_event_id_headers_sent(self) -> None:
        captured = {}

        def fake_open(method, path, body=None, headers=None, timeout=None):
            captured.update(method=method, path=path, headers=headers)
            return FakeSseResponse(b'event: end\ndata: {"status":"success"}\n\n')

        client = RustyClient("http://unit.test")
        with mock.patch.object(client, "_open", side_effect=fake_open):
            frames = list(client.run_stream("t1", last_event_id="cp0:0:1"))

        self.assertEqual(captured["method"], "POST")
        self.assertEqual(captured["path"], "/threads/t1/runs/stream")
        self.assertEqual(captured["headers"]["Accept"], "text/event-stream")
        self.assertEqual(captured["headers"]["Last-Event-ID"], "cp0:0:1")
        self.assertEqual(len(frames), 1)
        self.assertEqual(frames[0].data["status"], "success")


if __name__ == "__main__":
    unittest.main(verbosity=2)
