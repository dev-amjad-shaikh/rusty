#!/usr/bin/env python3
"""Container entrypoint for Rusty Studio under `docker compose up`.

Reuses studio/serve.py's request handler (static host + /api proxy) with two
adjustments for the container layout in docker-compose.yml:

- binds 0.0.0.0 — serve.py binds 127.0.0.1, which Docker's published ports
  cannot reach — and
- targets 127.0.0.1:8100 for the API, because the studio service shares the
  server container's network namespace (network_mode: service:server) and the
  server demo listens on loopback inside that namespace.

It additionally runs a small TCP relay on 0.0.0.0:8100 -> 127.0.0.1:8100 so
the raw HTTP/SSE API is reachable from the host exactly as the local
quickstart documents it (e.g. `curl localhost:8100/info`).
"""

import socket
import sys
import threading
from http.server import ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "studio"))
import serve  # noqa: E402 — studio/serve.py; its Handler is reused unchanged

STUDIO_PORT = 8000
API_PORT = 8100


def _pipe(src: socket.socket, dst: socket.socket) -> None:
    try:
        while chunk := src.recv(65536):
            dst.sendall(chunk)
    except OSError:
        pass
    finally:
        for s in (src, dst):
            try:
                s.close()
            except OSError:
                pass


def _relay_api() -> None:
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(("0.0.0.0", API_PORT))
    listener.listen()
    while True:
        client, _ = listener.accept()
        try:
            upstream = socket.create_connection(("127.0.0.1", API_PORT))
        except OSError:
            client.close()
            continue
        threading.Thread(target=_pipe, args=(client, upstream), daemon=True).start()
        threading.Thread(target=_pipe, args=(upstream, client), daemon=True).start()


def main() -> None:
    serve.Handler.target_host = "127.0.0.1"
    serve.Handler.target_port = API_PORT
    threading.Thread(target=_relay_api, daemon=True).start()
    httpd = ThreadingHTTPServer(("0.0.0.0", STUDIO_PORT), serve.Handler)
    print(f"Rusty Studio  ->  http://localhost:{STUDIO_PORT}/  (API proxied at /api)")
    print(f"Rusty API     ->  http://localhost:{API_PORT}/  (relayed to the server demo)")
    httpd.serve_forever()


if __name__ == "__main__":
    main()
