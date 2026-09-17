"""Experiment 2b: how much memory do clones of one snapshot share, with and without host KSM?

Run as root on the rooms host. Each condition forks N clones that either idle or
read part or all of the toolstore into guest page cache, then hold. While they hold, every
firecracker process's smaps_rollup is sampled each second. KSM is opted into with
prctl(PR_SET_MEMORY_MERGE) before the clone launcher is executed, which the VMMs
inherit; Rooms itself is unchanged.
"""
import argparse
import ctypes
import json
import os
import statistics
import subprocess
import threading
import time
from pathlib import Path

KSM = Path("/sys/kernel/mm/ksm")
PR_SET_MEMORY_MERGE = 67
ROLLUP_FIELDS = ("Rss", "Pss", "Pss_Anon", "Pss_File", "Shared_Clean", "Private_Clean", "Private_Dirty", "Anonymous")
KSM_FIELDS = ("pages_shared", "pages_sharing", "pages_unshared", "pages_volatile", "full_scans")
SUBSET = "/nix/store/*-python3-* /nix/store/*-glibc-*"  # about 185 MiB, fits beside a 512 MiB guest's kernel
# A representative task: start the interpreter, import widely, byte-compile a package, run its tests.
PYTASK = ("export PATH=/nix/var/rooms/env/bin:$PATH; d=$(mktemp -d); "
          "python3 -c 'import json,sqlite3,ssl,asyncio,unittest,decimal,email,xml.dom.minidom,http.client,zipfile,csv,argparse,logging' && "
          "cp -r $(python3 -c 'import email,os;print(os.path.dirname(email.__file__))') $d/pkg && "
          "python3 -m compileall -q $d/pkg >/dev/null && "
          "python3 -m unittest -q test.test_json >/dev/null 2>&1 || true")
WORKLOADS = {
    "pytask": "set -eu\n" + PYTASK.replace("{", "{{").replace("}", "}}") + "\nsleep {hold}\n",
    "idle": "set -eu\nsleep {hold}\n",
    "subset": "set -eu\nfind " + SUBSET + " -xdev -type f -exec cat {{}} + >/dev/null 2>&1 || true\nsleep {hold}\n",
    "scan": "set -eu\nfind /nix/store -xdev -type f -exec cat {{}} + >/dev/null 2>&1 || true\nsleep {hold}\n",
}


def kib_fields(text, wanted):
    rows = (line.split() for line in text.splitlines())
    return {row[0].rstrip(":"): int(row[1]) for row in rows if len(row) > 1 and row[0].rstrip(":") in wanted}


def vmm_pids():
    found = []
    for proc in Path("/proc").glob("[0-9]*"):
        try:
            if proc.joinpath("comm").read_text().strip() == "firecracker":
                found.append(proc)
        except OSError:
            continue
    return found


def read_vmm(proc):
    try:
        row = kib_fields(proc.joinpath("smaps_rollup").read_text(), ROLLUP_FIELDS)
        row["ksm_merging_pages"] = int(proc.joinpath("ksm_merging_pages").read_text())
    except (OSError, ValueError):
        return None
    row["pid"] = int(proc.name)
    return row


def ksmd_cpu_seconds():
    for proc in Path("/proc").glob("[0-9]*"):
        try:
            if proc.joinpath("comm").read_text().strip() != "ksmd":
                continue
            stat = proc.joinpath("stat").read_text().rsplit(")", 1)[1].split()
        except (OSError, IndexError):
            continue
        return (int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK")
    return None


def host_sample():
    meminfo = kib_fields(Path("/proc/meminfo").read_text(), ("MemAvailable", "MemFree", "Cached", "AnonPages"))
    ksm = {name: int(KSM.joinpath(name).read_text()) for name in KSM_FIELDS}
    return {"meminfo_kib": meminfo, "ksm": ksm, "ksmd_cpu_seconds": ksmd_cpu_seconds()}


def sample_until(stop, sink):
    while not stop.is_set():
        vmms = [row for row in map(read_vmm, vmm_pids()) if row]
        sink.append({"unix": time.time(), "vmms": vmms, "host": host_sample()})
        stop.wait(1)


def set_ksm(enabled):
    if not enabled:
        KSM.joinpath("run").write_text("2")  # unmerge everything a previous condition merged
        KSM.joinpath("run").write_text("0")
        return
    KSM.joinpath("pages_to_scan").write_text("4000")
    KSM.joinpath("sleep_millisecs").write_text("20")
    KSM.joinpath("run").write_text("1")


def opt_into_ksm():
    if ctypes.CDLL(None, use_errno=True).prctl(PR_SET_MEMORY_MERGE, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "prctl(PR_SET_MEMORY_MERGE) refused")


def steady_state(samples, count):
    """Median of the last five samples in which every requested VMM was present."""
    full = [s for s in samples if len(s["vmms"]) == count][-5:]
    if not full:
        return None
    totals = {f: statistics.median(sum(v[f] for v in s["vmms"]) for s in full) for f in ROLLUP_FIELDS}
    totals["ksm_merging_pages"] = statistics.median(sum(v["ksm_merging_pages"] for v in s["vmms"]) for s in full)
    return {"samples_used": len(full), "sum_kib": totals, "host": full[-1]["host"]}


def run_condition(args, workload, ksm, count, out):
    out.mkdir(parents=True)
    set_ksm(ksm)
    before = host_sample()
    argv = [args.rooms, "clone", args.snapshot, "--image", args.image, "--toolstore", args.toolstore,
            "-n", str(count), "--command", WORKLOADS[workload].format(hold=args.hold),
            "--max-wall", f"{args.hold + args.slack}s", "--out", str(out / "out"), "--json"]
    out.joinpath("argv.json").write_text(json.dumps(argv, indent=1))
    samples, stop = [], threading.Event()
    sampler = threading.Thread(target=sample_until, args=(stop, samples))
    sampler.start()
    started = time.monotonic()
    print(out.name, "started", flush=True)
    try:
        # preexec_fn is unsafe with threads running if it can take a lock; opt_into_ksm
        # makes one bare prctl syscall and must stay that small.
        done = subprocess.run(argv, capture_output=True, text=True, timeout=args.hold + args.slack + 300,
                              preexec_fn=opt_into_ksm if ksm else None, check=False)
    finally:
        stop.set()
        sampler.join()
    out.joinpath("stdout.json").write_text(done.stdout)
    out.joinpath("stderr.log").write_text(done.stderr)
    out.joinpath("memory.ndjson").write_text("".join(json.dumps(s) + "\n" for s in samples))
    row = {"workload": workload, "ksm": ksm, "clones": count, "exit": done.returncode,
           "wall_seconds": time.monotonic() - started, "before": before,
           "steady": steady_state(samples, count), "leftover_vmms": len(vmm_pids())}
    out.joinpath("summary.json").write_text(json.dumps(row, indent=1))
    return row


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("rooms", "snapshot", "image", "toolstore", "out"):
        parser.add_argument(f"--{name}", required=True)
    parser.add_argument("--counts", default="1,2,4")
    parser.add_argument("--hold", type=int, default=45, help="seconds each clone holds after its workload")
    parser.add_argument("--slack", type=int, default=90, help="wall seconds allowed beyond the hold")
    parser.add_argument("--workloads", default=",".join(WORKLOADS))
    parser.add_argument("--ksm", default="0,1", help="0, 1 or 0,1")
    args = parser.parse_args()
    if not KSM.joinpath("run").exists():
        parser.error("this kernel has no KSM (/sys/kernel/mm/ksm); every sample would fail")
    root = Path(args.out)
    root.mkdir(parents=True)  # refuses to reuse a previous run's directory
    rows = []
    try:
        for workload in args.workloads.split(","):
            for ksm in (flag == "1" for flag in args.ksm.split(",")):
                for count in map(int, args.counts.split(",")):
                    name = f"{workload}-ksm{int(ksm)}-n{count}"
                    rows.append(run_condition(args, workload, ksm, count, root / name))
                    print(name, "exit", rows[-1]["exit"], flush=True)
    finally:
        set_ksm(False)
        root.joinpath("summary.json").write_text(json.dumps(rows, indent=1))


if __name__ == "__main__":
    main()
