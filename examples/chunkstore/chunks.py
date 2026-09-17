#!/usr/bin/env python3
"""Chunk files into a content-addressed store and reassemble them.

A manifest names an artifact set (for example a snapshot directory) and lists,
per file, ordered ``[offset, length, sha256]`` records. All-zero chunks are
recorded with a null hash: they are holes, never stored and never transferred.
"""

import argparse
import hashlib
import json
import mmap
import os
import re
import sys
import tempfile
import zlib

MIB = 1 << 20
DEFAULT_CHUNK = MIB
HOLE_BLOCK = 64 * 1024
WINDOW = 48
# A cut may only land after one of these 8 byte values (1 position in 32 on
# random data); a checksum of the WINDOW bytes before it then decides the cut.
ANCHOR = re.compile(b"[\x07\x27\x47\x67\x87\xa7\xc7\xe7]")
ANCHOR_SPACING = 32
NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
HASH_RE = re.compile(r"^[0-9a-f]{64}$")


class ChunkError(Exception):
    """A chunk, manifest or name failed validation."""


def fixed_spec(size=DEFAULT_CHUNK):
    if size < 1:
        raise ChunkError("chunk size must be positive")
    return {"kind": "fixed", "size": size}


def cdc_spec(avg=DEFAULT_CHUNK):
    if avg < 1024:
        raise ChunkError("cdc average must be at least 1024 bytes")
    return {"kind": "cdc", "min": avg // 4, "avg": avg, "max": avg * 4}


def sha256_hex(data):
    return hashlib.sha256(data).hexdigest()


def _is_zero(data):
    return data[0] == 0 and data.count(0) == len(data)


class Store:
    """Content-addressed objects plus named manifests under one directory."""

    def __init__(self, root):
        self.root = os.path.realpath(root)
        self.objects = os.path.join(self.root, "objects")
        self.manifests = os.path.join(self.root, "manifests")
        self.tmp = os.path.join(self.root, "tmp")
        for path in (self.objects, self.manifests, self.tmp):
            os.makedirs(path, exist_ok=True)

    def object_path(self, sha):
        if not HASH_RE.match(sha):
            raise ChunkError(f"not a sha256 hex digest: {sha!r}")
        return os.path.join(self.objects, sha[:2], sha[2:])

    def manifest_path(self, name):
        if not NAME_RE.match(name):
            raise ChunkError(f"invalid artifact name: {name!r}")
        return os.path.join(self.manifests, name + ".json")

    def has(self, sha):
        return os.path.exists(self.object_path(sha))

    def hashes(self):
        found = set()
        for prefix in os.listdir(self.objects):
            rest = os.listdir(os.path.join(self.objects, prefix))
            found.update(prefix + name for name in rest)
        return found

    def put(self, data, expect=None):
        """Store data under its hash; refuse it when it is not what was expected."""
        sha = sha256_hex(data)
        if expect is not None and sha != expect:
            raise ChunkError(f"chunk hash mismatch: expected {expect}, got {sha}")
        path = self.object_path(sha)
        if not os.path.exists(path):
            os.makedirs(os.path.dirname(path), exist_ok=True)
            self._write_atomic(path, data)
        return sha

    def get(self, sha):
        with open(self.object_path(sha), "rb") as handle:
            data = handle.read()
        if sha256_hex(data) != sha:
            raise ChunkError(f"stored chunk {sha} is corrupt")
        return data

    def save_manifest(self, manifest):
        validate_manifest(manifest)
        raw = json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()
        self._write_atomic(self.manifest_path(manifest["name"]), raw)
        return sha256_hex(raw)

    def load_manifest(self, name):
        with open(self.manifest_path(name), "rb") as handle:
            manifest = json.loads(handle.read())
        validate_manifest(manifest)
        return manifest

    def _write_atomic(self, path, data):
        fd, tmp = tempfile.mkstemp(dir=self.tmp)
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
        os.replace(tmp, path)


def _fixed_spans(size, chunk):
    for offset in range(0, size, chunk):
        yield offset, min(chunk, size - offset), False


def _data_runs(buf, size):
    """Yield (start, end) of runs of HOLE_BLOCK-aligned blocks that hold data."""
    start = None
    for offset in range(0, size, HOLE_BLOCK):
        zero = _is_zero(buf[offset:offset + HOLE_BLOCK])
        if zero and start is not None:
            yield start, offset
            start = None
        if not zero and start is None:
            start = offset
    if start is not None:
        yield start, size


def _trim(buf, start, end):
    """Shrink a data run to its first and last nonzero byte."""
    head = buf[start:start + HOLE_BLOCK]
    tail = buf[max(start, end - HOLE_BLOCK):end]
    return start + len(head) - len(head.lstrip(b"\0")), end - len(tail) + len(tail.rstrip(b"\0"))


def _next_cut(buf, pos, end, spec, mask):
    if end - pos <= spec["min"]:
        return end
    limit = min(pos + spec["max"], end)
    for match in ANCHOR.finditer(buf, pos + spec["min"], limit):
        cut = match.end()
        if zlib.crc32(buf[cut - WINDOW:cut]) & mask == 0:
            return cut
    return limit


def _cdc_spans(buf, size, spec):
    """Content-defined spans: zero runs become holes, data runs are cut by content."""
    bits = max(1, ((spec["avg"] - spec["min"]) // ANCHOR_SPACING).bit_length() - 1)
    mask = (1 << bits) - 1
    pos = 0
    for run_start, run_end in _data_runs(buf, size):
        start, end = _trim(buf, run_start, run_end)
        if start > pos:
            yield pos, start - pos, True
        pos = start
        while pos < end:
            cut = _next_cut(buf, pos, end, spec, mask)
            yield pos, cut - pos, False
            pos = cut
    if pos < size:
        yield pos, size - pos, True


def _record(buf, offset, length, known_hole, store):
    if known_hole:
        return [offset, length, None]
    data = buf[offset:offset + length]
    if _is_zero(data):
        return [offset, length, None]
    if store is None:
        return [offset, length, sha256_hex(data)]
    return [offset, length, store.put(data)]


def chunk_file(path, spec, store=None):
    """Return the chunk records for one file, storing data chunks when given a store."""
    size = os.path.getsize(path)
    if size == 0:
        return []
    with open(path, "rb") as handle, mmap.mmap(handle.fileno(), 0, access=mmap.ACCESS_READ) as buf:
        spans = _fixed_spans(size, spec["size"]) if spec["kind"] == "fixed" else _cdc_spans(buf, size, spec)
        return [_record(buf, offset, length, hole, store) for offset, length, hole in spans]


def build_manifest(name, paths, spec, store=None):
    files = []
    for path in paths:
        entry = {"path": os.path.basename(path), "size": os.path.getsize(path)}
        entry["chunks"] = chunk_file(path, spec, store)
        files.append(entry)
    manifest = {"version": 1, "name": name, "chunker": spec, "files": files}
    validate_manifest(manifest)
    return manifest


def _validate_file(entry):
    path = entry["path"]
    if not NAME_RE.match(path):
        raise ChunkError(f"unsafe file name in manifest: {path!r}")
    pos = 0
    for offset, length, sha in entry["chunks"]:
        if offset != pos or length < 1:
            raise ChunkError(f"{path}: chunks are not contiguous at offset {pos}")
        if sha is not None and not HASH_RE.match(sha):
            raise ChunkError(f"{path}: bad chunk hash at offset {offset}")
        pos += length
    if pos != entry["size"]:
        raise ChunkError(f"{path}: chunks cover {pos} bytes of {entry['size']}")


def validate_manifest(manifest):
    """Reject manifests that could write outside the output directory or leave gaps."""
    try:
        if not NAME_RE.match(manifest["name"]):
            raise ChunkError(f"invalid artifact name: {manifest['name']!r}")
        paths = [entry["path"] for entry in manifest["files"]]
        if len(set(paths)) != len(paths):
            raise ChunkError("duplicate file names in manifest")
        for entry in manifest["files"]:
            _validate_file(entry)
    except (KeyError, TypeError, ValueError) as err:
        raise ChunkError(f"malformed manifest: {err!r}") from err


def manifest_hashes(manifest):
    """Map each distinct data chunk hash to its length."""
    found = {}
    for entry in manifest["files"]:
        found.update({sha: length for _, length, sha in entry["chunks"] if sha is not None})
    return found


def transfer_plan(manifest, have):
    """Account for what a host holding the hashes in `have` would still transfer."""
    records = [rec for entry in manifest["files"] for rec in entry["chunks"]]
    unique = manifest_hashes(manifest)
    needed = {sha: length for sha, length in unique.items() if sha not in have}
    return {
        "logical_bytes": sum(entry["size"] for entry in manifest["files"]),
        "hole_bytes": sum(length for _, length, sha in records if sha is None),
        "data_chunks": sum(1 for rec in records if rec[2] is not None),
        "unique_chunks": len(unique),
        "unique_bytes": sum(unique.values()),
        "needed_chunks": len(needed),
        "needed_bytes": sum(needed.values()),
    }


def _reassemble_file(entry, store, out_dir):
    fd, tmp = tempfile.mkstemp(dir=out_dir, prefix=".partial-")
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.truncate(entry["size"])
            for offset, length, sha in entry["chunks"]:
                if sha is None:
                    continue
                data = store.get(sha)
                if len(data) != length:
                    raise ChunkError(f"chunk {sha} is {len(data)} bytes, manifest says {length}")
                handle.seek(offset)
                handle.write(data)
    except BaseException:
        os.unlink(tmp)
        raise
    final = os.path.join(out_dir, entry["path"])
    os.replace(tmp, final)
    return final


def reassemble(manifest, store, out_dir):
    """Rebuild every file of a manifest, verifying each chunk and leaving holes sparse."""
    validate_manifest(manifest)
    os.makedirs(out_dir, exist_ok=True)
    return [_reassemble_file(entry, store, out_dir) for entry in manifest["files"]]


def _spec_from_args(args):
    if args.chunker == "cdc":
        return cdc_spec(args.size)
    return fixed_spec(args.size)


def _cmd_ingest(args):
    store = Store(args.store)
    manifest = build_manifest(args.name, args.files, _spec_from_args(args), store)
    digest = store.save_manifest(manifest)
    report = {"name": args.name, "manifest_sha256": digest, "chunker": manifest["chunker"]}
    report.update(transfer_plan(manifest, set()))
    json.dump(report, sys.stdout, indent=2)
    print()


def _cmd_stats(args):
    store = Store(args.store)
    first = store.load_manifest(args.name_a)
    second = store.load_manifest(args.name_b)
    report = {
        args.name_a: transfer_plan(first, set()),
        args.name_b: transfer_plan(second, set()),
        f"{args.name_b}_given_{args.name_a}": transfer_plan(second, set(manifest_hashes(first))),
        f"{args.name_a}_given_{args.name_b}": transfer_plan(first, set(manifest_hashes(second))),
    }
    json.dump(report, sys.stdout, indent=2)
    print()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    commands = parser.add_subparsers(dest="command", required=True)
    ingest = commands.add_parser("ingest", help="chunk files into a store as one named artifact set")
    ingest.add_argument("--store", required=True)
    ingest.add_argument("--name", required=True)
    ingest.add_argument("--chunker", choices=("fixed", "cdc"), default="fixed")
    ingest.add_argument("--size", type=int, default=DEFAULT_CHUNK, help="chunk size, or cdc average, in bytes")
    ingest.add_argument("files", nargs="+", metavar="FILE")
    ingest.set_defaults(run=_cmd_ingest)
    stats = commands.add_parser("stats", help="dedup between two ingested artifact sets")
    stats.add_argument("--store", required=True)
    stats.add_argument("name_a", metavar="NAME_A")
    stats.add_argument("name_b", metavar="NAME_B")
    stats.set_defaults(run=_cmd_stats)
    args = parser.parse_args(argv)
    try:
        args.run(args)
    except (ChunkError, OSError) as err:
        print(f"chunks: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
