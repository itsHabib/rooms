#!/usr/bin/env python3
"""Restore a named artifact set onto this host from one or more seed servers.

Chunks already in the local store are skipped; the rest are fetched in parallel,
spread across seeds, and hash-checked before they enter the store. A seed that
returns bad bytes is dropped and the chunk is fetched elsewhere. Afterwards the
local store holds the manifest and every chunk, so `serve.py` can seed from it.
"""

import argparse
import http.client
import json
import sys
import threading
import time
import urllib.parse
from concurrent.futures import ThreadPoolExecutor

from chunks import ChunkError, Store, manifest_hashes, reassemble, sha256_hex, validate_manifest

TIMEOUT = 30
MAX_ERRORS = 3


class FetchError(Exception):
    """The artifact set could not be fetched from the seeds given."""


class Seed:
    def __init__(self, url):
        parts = urllib.parse.urlsplit(url)
        if parts.scheme != "http" or not parts.hostname:
            raise FetchError(f"seed must be an http url: {url!r}")
        self.url = url.rstrip("/")
        self.host = parts.hostname
        self.port = parts.port or 80
        self.have = set()
        self.inflight = 0
        self.chunks = 0
        self.bytes = 0
        self.errors = 0
        self.bad_chunks = 0
        self.dropped = None

    def stats(self):
        keys = ("url", "chunks", "bytes", "errors", "bad_chunks", "dropped")
        return {key: getattr(self, key) for key in keys}


class Swarm:
    """The seeds for one fetch, with per-thread keep-alive connections."""

    def __init__(self, urls):
        if not urls:
            raise FetchError("at least one seed is required")
        self.seeds = [Seed(url) for url in urls]
        self.lock = threading.Lock()
        self.local = threading.local()
        self.conns = []
        self.retries = 0

    def get(self, seed, path):
        conns = self.local.__dict__.setdefault("conns", {})
        conn = conns.pop(seed.url, None) or self._connect(seed)
        try:
            conn.request("GET", path)
            response = conn.getresponse()
            body = response.read()
        except (OSError, http.client.HTTPException) as err:
            conn.close()
            raise FetchError(f"{seed.url}{path}: {err}") from err
        conns[seed.url] = conn
        if response.status != 200:
            raise FetchError(f"{seed.url}{path}: http {response.status}")
        return body

    def _connect(self, seed):
        conn = http.client.HTTPConnection(seed.host, seed.port, timeout=TIMEOUT)
        with self.lock:
            self.conns.append(conn)
        return conn

    def close(self):
        for conn in self.conns:
            conn.close()

    def acquire(self, sha):
        """Pick the least busy live seed that advertises the chunk."""
        with self.lock:
            live = [seed for seed in self.seeds if seed.dropped is None and sha in seed.have]
            if not live:
                return None
            seed = min(live, key=lambda candidate: (candidate.inflight, candidate.chunks))
            seed.inflight += 1
            return seed

    def release(self, seed, fetched=0, error=None, bad=False):
        with self.lock:
            seed.inflight -= 1
            if error is None:
                seed.chunks += 1
                seed.bytes += fetched
                return
            self.retries += 1
            seed.errors += 1
            seed.bad_chunks += int(bad)
            if bad or seed.errors >= MAX_ERRORS:
                seed.dropped = seed.dropped or error

    def drop(self, seed, reason):
        with self.lock:
            seed.dropped = seed.dropped or reason


def fetch_manifest(swarm, name, expect_sha=None):
    """Return the first manifest a seed serves that validates (and matches the pinned hash)."""
    problems = []
    for seed in swarm.seeds:
        try:
            raw = swarm.get(seed, f"/manifest/{name}")
            if expect_sha is not None and sha256_hex(raw) != expect_sha:
                raise ChunkError("manifest does not match the pinned sha256")
            manifest = json.loads(raw)
            validate_manifest(manifest)
            if manifest["name"] != name:
                raise ChunkError(f"manifest is for {manifest['name']!r}")
            return manifest
        except (FetchError, ChunkError, ValueError) as err:
            swarm.drop(seed, f"manifest: {err}")
            problems.append(f"{seed.url}: {err}")
    raise FetchError(f"no seed served a valid manifest for {name!r}: {'; '.join(problems)}")


def load_have(swarm):
    for seed in swarm.seeds:
        if seed.dropped is not None:
            continue
        try:
            seed.have = set(json.loads(swarm.get(seed, "/have")))
        except (FetchError, ValueError, TypeError) as err:
            swarm.drop(seed, f"have: {err}")


def fetch_chunk(swarm, store, sha, length):
    """Fetch one chunk, moving to another seed until a verified copy is stored."""
    while True:
        seed = swarm.acquire(sha)
        if seed is None:
            raise FetchError(f"no live seed holds chunk {sha}")
        try:
            data = swarm.get(seed, f"/chunk/{sha}")
        except FetchError as err:
            swarm.release(seed, error=str(err))
            continue
        if len(data) != length or sha256_hex(data) != sha:
            swarm.release(seed, error=f"served bad bytes for chunk {sha}", bad=True)
            continue
        store.put(data, expect=sha)
        swarm.release(seed, fetched=len(data))
        return


def fetch(name, seed_urls, store, out_dir=None, workers=8, manifest_sha=None):
    """Fetch an artifact set into `store`, optionally reassemble it, and return stats."""
    swarm = Swarm(seed_urls)
    try:
        return _fetch(swarm, name, store, out_dir, workers, manifest_sha)
    finally:
        swarm.close()


def _fetch(swarm, name, store, out_dir, workers, manifest_sha):
    started = time.monotonic()
    manifest = fetch_manifest(swarm, name, manifest_sha)
    load_have(swarm)
    wanted = manifest_hashes(manifest)
    needed = {sha: length for sha, length in wanted.items() if not store.has(sha)}
    with ThreadPoolExecutor(max_workers=workers) as pool:
        jobs = [pool.submit(fetch_chunk, swarm, store, sha, length) for sha, length in needed.items()]
        failures = [job.exception() for job in jobs if job.exception() is not None]
    if failures:
        raise FetchError(f"{len(failures)} chunks could not be fetched; first: {failures[0]}")
    stats = {
        "name": name,
        "workers": workers,
        "chunks_total": len(wanted),
        "chunks_local": len(wanted) - len(needed),
        "chunks_fetched": len(needed),
        "bytes_fetched": sum(seed.bytes for seed in swarm.seeds),
        "retries": swarm.retries,
        "seeds": [seed.stats() for seed in swarm.seeds],
    }
    stats["fetch_seconds"] = round(time.monotonic() - started, 3)
    store.save_manifest(manifest)
    if out_dir is not None:
        reassemble(manifest, store, out_dir)
    stats["wall_seconds"] = round(time.monotonic() - started, 3)
    return stats


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--store", required=True, help="local store; reusable as a seed afterwards")
    parser.add_argument("--out", help="directory to reassemble the files into")
    parser.add_argument("--seed", action="append", required=True, dest="seeds", metavar="URL")
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--manifest-sha256", help="refuse any manifest whose bytes do not hash to this")
    parser.add_argument("name")
    args = parser.parse_args(argv)
    try:
        stats = fetch(args.name, args.seeds, Store(args.store), args.out, args.workers, args.manifest_sha256)
    except (FetchError, ChunkError, OSError) as err:
        print(f"fetch: {err}", file=sys.stderr)
        return 1
    json.dump(stats, sys.stdout, indent=2)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
