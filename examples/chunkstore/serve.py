#!/usr/bin/env python3
"""Serve manifests and chunks from a store directory over HTTP.

Routes: GET /manifest/<name>, GET /chunk/<sha256>, GET /have.
"""

import argparse
import contextlib
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from chunks import ChunkError, Store

SEND_PIECE = 64 * 1024


class TokenBucket:
    """Byte-rate limiter shared by every connection of one server.

    Callers may drive the balance negative and then sleep off their own debt,
    which keeps the long-run rate exact however many threads are sending.
    """

    def __init__(self, rate):
        self.rate = float(rate)
        self.burst = self.rate / 20
        self.tokens = 0.0
        self.stamp = time.monotonic()
        self.lock = threading.Lock()

    def take(self, count):
        with self.lock:
            now = time.monotonic()
            self.tokens = min(self.burst, self.tokens + (now - self.stamp) * self.rate)
            self.stamp = now
            self.tokens -= count
            debt = -self.tokens
        if debt > 0:
            time.sleep(debt / self.rate)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):  # noqa: N802 - name fixed by http.server
        kind, _, arg = self.path[1:].partition("/")
        route = {"manifest": self._manifest, "chunk": self._chunk, "have": self._have}.get(kind)
        if route is None:
            return self._reply(404, b"unknown route\n")
        try:
            return route(arg)
        except ChunkError as err:
            return self._reply(400, f"{err}\n".encode())

    def _manifest(self, name):
        self._send_file(self.server.store.manifest_path(name))

    def _chunk(self, sha):
        self._send_file(self.server.store.object_path(sha), self.server.corrupt)

    def _have(self, arg):
        if arg:
            return self._reply(404, b"unknown route\n")
        body = json.dumps(sorted(self.server.store.hashes())).encode()
        return self._reply(200, body)

    def _send_file(self, path, corrupt=False):
        # Names and hashes are already pattern-checked; this also stops a
        # symlink inside the store from pointing anywhere else.
        real = os.path.realpath(path)
        if not real.startswith(self.server.store.root + os.sep):
            return self._reply(400, b"path escapes the store\n")
        if not os.path.isfile(real):
            return self._reply(404, b"not found\n")
        with open(real, "rb") as handle:
            body = handle.read()
        if corrupt:
            body = bytes([body[0] ^ 0xFF]) + body[1:]
        return self._reply(200, body)

    def _reply(self, status, body):
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        bucket = self.server.bucket
        for start in range(0, len(body), SEND_PIECE):
            piece = body[start:start + SEND_PIECE]
            if bucket is not None:
                bucket.take(len(piece))
            self.wfile.write(piece)

    def log_message(self, format, *args):  # noqa: A002 - signature fixed by http.server
        pass


def make_server(store_dir, host="127.0.0.1", port=0, rate=0, corrupt=False):
    """Build a server; `rate` is bytes per second (0 is unlimited), `corrupt` injects bad chunks."""
    server = ThreadingHTTPServer((host, port), Handler)
    server.daemon_threads = True
    server.store = Store(store_dir)
    server.bucket = TokenBucket(rate) if rate > 0 else None
    server.corrupt = corrupt
    return server


@contextlib.contextmanager
def serving(store_dir, **options):
    """Run a server on a free loopback port for the life of the block; yields its URL."""
    server = make_server(store_dir, **options)
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}"
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--store", required=True)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--rate-mbps", type=float, default=0, help="throttle to this many MB/s (0 is unlimited)")
    parser.add_argument("--corrupt", action="store_true", help="fault injection: flip a byte in every chunk")
    args = parser.parse_args(argv)
    server = make_server(args.store, args.host, args.port, int(args.rate_mbps * 1e6), args.corrupt)
    print(f"serving {args.store} on http://{args.host}:{server.server_address[1]}", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
