"""Run the swarm peers inside Rooms clones against a Redis server on the host.

Run as root on a rooms host with redis-server installed. Each clone unpacks
store.py and peer.py from its command line, takes a random peer id, and works the
shared task list through resp:HOST_IP:PORT. With --faults the host kills one
clone mid-run and restarts the store; the usual invariants are then checked
against the server's own history. wall_s covers restore and teardown; active_s
runs from the first claim to the last completion any surviving peer logged; a
killed clone's log is never collected. The output directory must not exist.
"""

import argparse
import base64
import io
import json
import os
import subprocess
import sys
import tarfile
import threading
import time

import bench
from peer import task_ids
from store import RespStore

HERE = os.path.dirname(os.path.abspath(__file__))
GUEST = """set -eu
export PATH=/nix/var/rooms/env/bin:$PATH
mkdir -p /tmp/swarm /workspace/out
cd /tmp/swarm
echo {bundle} | base64 -d | tar xz
python3 peer.py --store resp:{host}:{port} --peer-id "$(cat /proc/sys/kernel/random/uuid)" \\
  --tasks {tasks} --work-ms {work_ms} --ttl-ms {ttl_ms} --retries 14 --log /workspace/out/peer.ndjson
"""


def bundle():
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w:gz") as tar:
        for name in ("store.py", "peer.py"):
            tar.add(os.path.join(HERE, name), arcname=name)
    return base64.b64encode(raw.getvalue()).decode()


def start_server(opts, port, scratch):
    argv = ["redis-server", "--port", str(port), "--bind", opts.host_ip, "--protected-mode", "no",
            "--dir", scratch, "--save", "", "--appendonly", "yes", "--appendfsync", "always"]
    server = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    probe = RespStore(opts.host_ip, port)
    for _ in range(100):
        try:
            probe.call("PING")
            probe.close()
            return server
        except OSError:
            time.sleep(0.1)
    probe.close()
    server.kill()
    raise OSError("redis-server did not start on %s:%d" % (opts.host_ip, port))


def rooms_cli(opts, *args):
    """stdout of one rooms command; empty if it hangs, so the injector cannot block on it."""
    try:
        done = subprocess.run([opts.rooms] + list(args), capture_output=True, text=True,
                              check=False, timeout=30)
    except subprocess.TimeoutExpired:
        return ""
    return done.stdout


def running_rooms(listing):
    return [line.split()[0] for line in listing.splitlines() if " running" in line]


def quarter_done(probe, tasks):
    """A store that does not answer is "not there yet": the poll outlives a store hiccup."""
    try:
        return probe.call("HLEN", "swarm:done") >= tasks // 4
    except OSError:
        return False


def await_progress(opts, port, stop):
    """Block until a quarter of the tasks are done, the wall deadline passes, or `stop` is set."""
    probe = RespStore(opts.host_ip, port)
    deadline = time.monotonic() + opts.wall_s
    try:
        while time.monotonic() < deadline and not quarter_done(probe, opts.tasks):
            if stop.wait(0.05):
                return
    finally:
        probe.close()


def inject(opts, port, scratch, holder, log, stop):
    """Once a quarter of the tasks are done, kill one clone, then take the store away for
    two seconds. Does nothing if the run ended first."""
    await_progress(opts, port, stop)
    if stop.is_set():
        return
    rooms = running_rooms(rooms_cli(opts, "ls"))
    if rooms:
        rooms_cli(opts, "kill", rooms[0])
        log.append({"t": time.time(), "fault": "kill-room", "room": rooms[0]})
    holder["server"].kill()
    holder["server"].wait()
    log.append({"t": time.time(), "fault": "kill-store"})
    time.sleep(2)
    holder["server"] = start_server(opts, port, scratch)
    log.append({"t": time.time(), "fault": "restart-store"})


def peer_events(out_dir):
    events = []
    for folder, _dirs, files in os.walk(out_dir):
        if "peer.ndjson" in files:
            events.extend(bench.read_ndjson(os.path.join(folder, "peer.ndjson")))
    return events


def finish_injector(injector, stop):
    """Safe to call twice, and on a thread that was never started."""
    stop.set()
    if injector.is_alive():
        injector.join()


def read_history(opts, port):
    reader = RespStore(opts.host_ip, port)
    try:
        return reader.history()
    finally:
        reader.close()


def run_ok(row, tasks):
    """A run passes only if it did the work: invariants hold, every task was accepted, and at
    least one peer reported. A clone that never started must not pass on an empty history."""
    verdict = row["invariants"]
    return bool(verdict["ok"] and verdict["accepted"] == tasks and row["peers_reporting"] > 0)


def run_once(opts, count, faulted):
    name = "rooms-n%d%s" % (count, "-faults" if faulted else "")
    run_dir = os.path.join(opts.out, name)
    scratch = os.path.join(run_dir, "redis")
    os.makedirs(scratch)
    port = bench.free_port()
    holder = {"server": start_server(opts, port, scratch)}
    fault_log = []
    stop = threading.Event()
    injector = threading.Thread(target=inject, args=(opts, port, scratch, holder, fault_log, stop))
    try:
        seeder = RespStore(opts.host_ip, port)
        seeder.seed(task_ids(opts.tasks))
        seeder.close()
        command = GUEST.format(bundle=bundle(), host=opts.host_ip, port=port, tasks=opts.tasks,
                               work_ms=opts.work_ms, ttl_ms=opts.ttl_ms)
        argv = [opts.rooms, "clone", opts.snapshot, "--image", opts.image, "--toolstore", opts.toolstore,
                "-n", str(count), "--command", command, "--max-wall", "%ds" % opts.wall_s,
                "--out", os.path.join(run_dir, "out"), "--json"]
        if faulted:
            injector.start()
        started = time.monotonic()
        done = subprocess.run(argv, capture_output=True, text=True, check=False)
        wall_s = time.monotonic() - started
        finish_injector(injector, stop)
        history = read_history(opts, port)
    finally:
        finish_injector(injector, stop)
        holder["server"].kill()
        holder["server"].wait()
    with open(os.path.join(run_dir, "clone-stdout.json"), "w", encoding="utf-8") as fh:
        fh.write(done.stdout)
    with open(os.path.join(run_dir, "clone-stderr.log"), "w", encoding="utf-8") as fh:
        fh.write(done.stderr)
    seen = peer_events(os.path.join(run_dir, "out"))
    bench.write_ndjson(os.path.join(run_dir, "peers.ndjson"), seen)
    bench.write_ndjson(os.path.join(run_dir, "history.ndjson"), history)
    survivors = count - sum(1 for f in fault_log if f["fault"] == "kill-room")
    row = bench.score(opts.tasks, seen, history, survivors, wall_s)
    stamps = [e["t"] for e in seen if e["ev"] in ("claim", "complete")]
    active_s = (max(stamps) - min(stamps)) / 1000 if stamps else 0
    rate = round(row["invariants"]["accepted"] / active_s, 2) if active_s else None
    return dict(row, name=name, clones=count, clone_exit=done.returncode, faults=fault_log,
                active_s=round(active_s, 3), active_tasks_per_s=rate,
                peers_reporting=len({e["peer"] for e in seen}))


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    for name in ("rooms", "snapshot", "image", "toolstore", "host-ip", "out"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--counts", default="2,4,6")
    parser.add_argument("--tasks", type=int, default=120)
    parser.add_argument("--work-ms", type=int, default=200)
    parser.add_argument("--ttl-ms", type=int, default=2000)
    parser.add_argument("--wall-s", type=int, default=180)
    parser.add_argument("--faults", action="store_true", help="also run each count with a kill and a store restart")
    return parser.parse_args(argv)


def main(argv=None):
    opts = parse_args(argv)
    os.makedirs(opts.out)
    rows = []
    try:
        for count in (int(c) for c in opts.counts.split(",")):
            for faulted in ([False, True] if opts.faults else [False]):
                row = run_once(opts, count, faulted)
                rows.append(dict(row, ok=run_ok(row, opts.tasks)))
                print(json.dumps(rows[-1], sort_keys=True), flush=True)
    finally:
        with open(os.path.join(opts.out, "summary.json"), "w", encoding="utf-8") as fh:
            json.dump(rows, fh, indent=1, sort_keys=True)
    return 0 if rows and all(r["ok"] for r in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
