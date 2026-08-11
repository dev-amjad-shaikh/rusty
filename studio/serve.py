#!/usr/bin/env python3
"""Rusty Studio local server — typed product app + API proxy.

The typed application under ``studio/ui`` is the default experience. This
lightweight host serves its deep routes and proxies ``/api/*`` to a Rusty
server for same-origin local development. The specialist console remains
available at ``/advanced/legacy`` while its workflows migrate.

Usage:
    python3 studio/serve.py [--port 8000] [--target http://127.0.0.1:8100]

Then open http://127.0.0.1:8000/. The default connection points directly to
http://127.0.0.1:8100; the proxy remains available at /api/*.
"""

import argparse
import http.client
import http.server
import pathlib
import urllib.parse

STUDIO_ROOT = pathlib.Path(__file__).resolve().parent
ROOT = STUDIO_ROOT / "ui" / "dist"
LEGACY = STUDIO_ROOT / "index.html"


class Handler(http.server.SimpleHTTPRequestHandler):
    target_host = "127.0.0.1"
    target_port = 8100
    target_secure = False

    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=str(ROOT), **kwargs)

    # -- static -----------------------------------------------------------
    def do_GET(self):
        path = urllib.parse.urlsplit(self.path).path
        if path == "/api" or path.startswith("/api/"):
            self._proxy()
        elif path in ("/advanced/legacy", "/advanced/legacy/"):
            self._serve_legacy()
        else:
            target = ROOT / path.lstrip("/")
            if path in ("/", "") or not target.is_file():
                self.path = "/index.html"  # SPA route fallback
            super().do_GET()

    def _serve_legacy(self):
        try:
            body = LEGACY.read_bytes()
        except OSError:
            self.send_error(404, "legacy Studio is unavailable")
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # -- proxy ------------------------------------------------------------
    def do_POST(self):
        self._proxy()

    def do_PUT(self):
        self._proxy()

    def do_DELETE(self):
        self._proxy()

    def _proxy(self):
        upstream = self.path[len("/api"):] or "/"
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            self.send_error(400, "malformed Content-Length header")
            return
        body = self.rfile.read(length) if length else None

        connection_type = http.client.HTTPSConnection if self.target_secure else http.client.HTTPConnection
        conn = connection_type(self.target_host, self.target_port, timeout=600)
        headers = {}
        for name in ("Content-Type", "X-Api-Key", "Accept", "Last-Event-ID"):
            if self.headers.get(name):
                headers[name] = self.headers[name]
        try:
            conn.request(self.command, upstream, body=body, headers=headers)
            resp = conn.getresponse()
        except OSError as exc:
            self.send_error(502, f"proxy cannot reach {self.target_host}:{self.target_port} — {exc}")
            return

        self.send_response(resp.status)
        # Forward content type; never forward content-length — we stream.
        ct = resp.getheader("Content-Type")
        if ct:
            self.send_header("Content-Type", ct)
        # SSE: disable any buffering so events flush per frame.
        if ct and "text/event-stream" in ct:
            self.send_header("Cache-Control", "no-cache")
            self.send_header("X-Accel-Buffering", "no")
        self.end_headers()

        try:
            while True:
                chunk = resp.read1(4096) if hasattr(resp, "read1") else resp.read(4096)
                if not chunk:
                    break
                self.wfile.write(chunk)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        finally:
            conn.close()

    def log_message(self, fmt, *args):  # quieter logs
        pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8000)
    ap.add_argument("--target", default="http://127.0.0.1:8100")
    args = ap.parse_args()

    if not (ROOT / "index.html").is_file():
        raise SystemExit("typed Studio build is missing — run `npm ci && npm run build` in studio/ui")

    parsed = urllib.parse.urlparse(args.target)
    if parsed.scheme not in ("http", "https") or not parsed.hostname or parsed.username or parsed.password \
            or parsed.path not in ("", "/") or parsed.query or parsed.fragment:
        raise SystemExit("--target must be an http(s) origin without credentials, path, query, or fragment")
    Handler.target_host = parsed.hostname or "127.0.0.1"
    Handler.target_secure = parsed.scheme == "https"
    Handler.target_port = parsed.port or (443 if Handler.target_secure else 80)

    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"Rusty Studio       →  http://127.0.0.1:{args.port}/")
    print(f"Advanced legacy    →  http://127.0.0.1:{args.port}/advanced/legacy")
    print(f"proxying /api/*    →  {args.target}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
