#!/usr/bin/env python3
"""Benchmark chunked snapshot distribution on one machine, with no VMs.

Generates two related synthetic snapshots, accounts for the bytes each transfer
strategy would move to a cold and a warm host, then times real fetches from
throttled loopback seeds, including one run with a seed that serves bad bytes.
"""

import argparse
import contextlib
import hashlib
import json
import os
import platform
import random
import shutil
import sys
import tempfile
import time

import chunks
import fetch
import serve

PAGE = 4096
KIB = 1024
SET_A = "snap-a"
SET_B = "snap-b"
ZERO_SHARE = 0.55
REPEAT_SHARE = 0.20
DIRTY_SHARE = 0.10
HOT_SHARE = 0.75
HOT_WINDOWS = 8


def synth_memory(size, seed):
    """Guest-RAM-like bytes: runs of zero pages, runs of one repeated page, random runs."""
    rng = random.Random(seed)
    pool = [rng.randbytes(PAGE) for _ in range(16)]
    pages = size // PAGE
    longest = max(16, pages // 64)
    buf = bytearray(size)
    pos = 0
    while pos < pages:
        run = min(rng.randint(4, longest), pages - pos)
        _fill_run(buf, pos, run, rng, pool)
        pos += run
    return buf


def _fill_run(buf, pos, run, rng, pool):
    kind = rng.random()
    if kind < ZERO_SHARE:
        return
    start = pos * PAGE
    if kind < ZERO_SHARE + REPEAT_SHARE:
        buf[start:start + run * PAGE] = rng.choice(pool) * run
        return
    buf[start:start + run * PAGE] = rng.randbytes(run * PAGE)


def derive_related(buf, seed):
    """Mutate a snapshot in place into a later, related one.

    About DIRTY_SHARE of pages are rewritten in short runs, most of them inside
    a few hot windows. Then the span between 40% and 60% of the file is shifted
    by an unaligned insertion, with the same number of bytes removed after it,
    so the size is unchanged and fixed-offset chunking loses that span.
    """
    rng = random.Random(seed + 1)
    size = len(buf)
    pages = size // PAGE
    window = max(1, pages // (HOT_WINDOWS * 8))
    hot = [rng.randrange(pages - window) for _ in range(HOT_WINDOWS)]
    dirty = 0
    while dirty < pages * DIRTY_SHARE:
        run = rng.randint(1, 32)
        start = rng.randrange(pages)
        if rng.random() < HOT_SHARE:
            start = rng.choice(hot) + rng.randrange(window)
        run = min(run, pages - start)
        buf[start * PAGE:(start + run) * PAGE] = rng.randbytes(run * PAGE)
        dirty += run
    shift = size // 14000 + 11
    cut = size * 3 // 5
    buf[size * 2 // 5:size * 2 // 5] = rng.randbytes(shift)
    del buf[cut:cut + shift]


def write_sparse(path, buf, block=chunks.MIB):
    with open(path, "wb") as handle:
        for offset in range(0, len(buf), block):
            piece = buf[offset:offset + block]
            if piece.count(0) == len(piece):
                continue
            handle.seek(offset)
            handle.write(piece)
        handle.truncate(len(buf))


def write_set(directory, mem, seed):
    """Write one snapshot-shaped artifact set and return its file paths."""
    os.makedirs(directory)
    paths = [os.path.join(directory, name) for name in ("snapshot.mem", "snapshot.vmstate", "snapshot.json")]
    write_sparse(paths[0], mem)
    with open(paths[1], "wb") as handle:
        handle.write(random.Random(seed + 2).randbytes(30 * KIB))
    with open(paths[2], "w") as handle:
        json.dump({"synthetic": True, "mem_bytes": len(mem), "seed": seed}, handle)
    return paths


def generate(work, size, seed):
    mem = synth_memory(size, seed)
    first = write_set(os.path.join(work, SET_A), mem, seed)
    derive_related(mem, seed)
    second = write_set(os.path.join(work, SET_B), mem, seed)
    return first, second


def file_sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for piece in iter(lambda: handle.read(chunks.MIB), b""):
            digest.update(piece)
    return digest.hexdigest()


def nonzero_bytes(paths, block=PAGE):
    total = 0
    for path in paths:
        with open(path, "rb") as handle:
            pieces = iter(lambda: handle.read(block), b"")
            total += sum(len(piece) for piece in pieces if piece.count(0) != len(piece))
    return total


def measure_strategy(label, spec, first, second):
    """Bytes the second snapshot costs a cold host and a host holding the first."""
    started = time.monotonic()
    base = chunks.build_manifest(SET_A, first, spec)
    later = chunks.build_manifest(SET_B, second, spec)
    seconds = time.monotonic() - started
    cold = chunks.transfer_plan(later, set())
    warm = chunks.transfer_plan(later, set(chunks.manifest_hashes(base)))
    return {
        "kind": "transfer",
        "strategy": label,
        "chunker": spec,
        "cold_bytes": cold["needed_bytes"],
        "cold_chunks": cold["needed_chunks"],
        "warm_bytes": warm["needed_bytes"],
        "warm_chunks": warm["needed_chunks"],
        "data_chunks": cold["data_chunks"],
        "hole_bytes": cold["hole_bytes"],
        "chunking_seconds": round(seconds, 3),
    }


def whole_file_record(second):
    logical = sum(os.path.getsize(path) for path in second)
    record = {"kind": "transfer", "strategy": "whole-file", "cold_bytes": logical, "warm_bytes": logical}
    record["sparse_aware_bytes"] = nonzero_bytes(second)
    return record


def run_fetch(label, source, work, expect, args, seeds, corrupt=None, warm=None, rate=None):
    """Fetch SET_B from `seeds` loopback servers into a fresh store and check the result."""
    rate = args.rate_mbps if rate is None else rate
    local = chunks.Store(os.path.join(work, label, "store"))
    if warm is not None:
        chunks.build_manifest(SET_A, warm, source.load_manifest(SET_B)["chunker"], local)
    out_dir = os.path.join(work, label, "out")
    record = {"kind": "fetch", "run": label, "seed_count": seeds,"rate_mbps": rate, "corrupt_seed": corrupt}
    with contextlib.ExitStack() as stack:
        urls = [
            stack.enter_context(serve.serving(source.root, rate=int(rate * 1e6), corrupt=(index == corrupt)))
            for index in range(seeds)
        ]
        try:
            record.update(fetch.fetch(SET_B, urls, local, out_dir, args.workers))
        except (fetch.FetchError, chunks.ChunkError) as err:
            record.update({"ok": False, "error": str(err)})
            return record
    restored = {name: file_sha256(os.path.join(out_dir, name)) for name in expect}
    record["ok"] = restored == expect
    if corrupt is not None:
        bad = record["seeds"][corrupt]
        record["ok"] = record["ok"] and bad["dropped"] is not None and bad["bad_chunks"] > 0
    shutil.rmtree(os.path.join(work, label))
    return record


def machine_facts():
    return {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "cpu_count": os.cpu_count(),
        "python": platform.python_version(),
    }


def strategies(sizes_kib):
    for kib in sizes_kib:
        yield f"fixed-{kib}k", chunks.fixed_spec(kib * KIB)
    for kib in sizes_kib:
        yield f"cdc-{kib}k", chunks.cdc_spec(kib * KIB)


def fetch_runs(source, work, expect, args, first):
    yield run_fetch("unthrottled-1-seed", source, work, expect, args, 1, rate=0)
    for seeds in (1, 2, 3):
        yield run_fetch(f"cold-{seeds}-seed", source, work, expect, args, seeds)
    yield run_fetch("warm-1-seed", source, work, expect, args, 1, warm=first)
    yield run_fetch("corrupt-1-of-3", source, work, expect, args, 3, corrupt=0)


def run(args, work, emit):
    started = time.monotonic()
    first, second = generate(work, args.size_mib * chunks.MIB, args.seed)
    emit({"kind": "generate", "seconds": round(time.monotonic() - started, 3), "size_mib": args.size_mib})
    transfers = [emit(whole_file_record(second))]
    for label, spec in strategies(args.sizes_kib):
        transfers.append(emit(measure_strategy(label, spec, first, second)))
    source = chunks.Store(os.path.join(work, "seed-store"))
    source.save_manifest(chunks.build_manifest(SET_B, second, chunks.fixed_spec(args.fetch_chunk_kib * KIB), source))
    expect = {os.path.basename(path): file_sha256(path) for path in second}
    fetches = [emit(record) for record in fetch_runs(source, work, expect, args, first)]
    return {"transfers": transfers, "fetches": fetches}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", required=True, help="new directory for summary.json and raw.ndjson")
    parser.add_argument("--size-mib", type=int, default=512)
    parser.add_argument("--seed", type=int, default=1)
    parser.add_argument("--rate-mbps", type=float, default=20, help="per-seed throttle in MB/s")
    parser.add_argument("--workers", type=int, default=8)
    parser.add_argument("--sizes-kib", type=int, nargs="+", default=[64, 256, 1024, 4096])
    parser.add_argument("--fetch-chunk-kib", type=int, default=1024, help="fixed chunk size for the fetch runs")
    args = parser.parse_args(argv)
    try:
        os.makedirs(args.out)
    except FileExistsError:
        print(f"bench: refusing to reuse output directory {args.out}", file=sys.stderr)
        return 2
    work = tempfile.mkdtemp(prefix="chunkstore-bench-")
    with open(os.path.join(args.out, "raw.ndjson"), "w") as raw:

        def emit(record):
            raw.write(json.dumps(record, sort_keys=True) + "\n")
            raw.flush()
            return record

        try:
            results = run(args, work, emit)
        finally:
            shutil.rmtree(work, ignore_errors=True)
    summary = {"machine": machine_facts(), "params": vars(args), "ok": all(f["ok"] for f in results["fetches"])}
    summary.update(results)
    with open(os.path.join(args.out, "summary.json"), "w") as handle:
        json.dump(summary, handle, indent=2, sort_keys=True)
        handle.write("\n")
    print(json.dumps(summary, indent=2, sort_keys=True))
    return 0 if summary["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
