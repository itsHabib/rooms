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
    server.kill()
    raise OSError("redis-server did not start on %s:%d" % (opts.host_ip, port))


def running_rooms(opts):
    listed = subprocess.run([opts.rooms, "ls"], capture_output=True, text=True, check=False).stdout
    return [line.split()[0] for line in listed.splitlines() if " running" in line]


def inject(opts, port, scratch, holder, log):
    """Once a quarter of the tasks are done, kill one clone, then take the store away for two seconds."""
    probe = RespStore(opts.host_ip, port)
    deadline = time.monotonic() + opts.wall_s
    while probe.call("HLEN", "swarm:done") < opts.tasks // 4 and time.monotonic() < deadline:
        time.sleep(0.05)
    probe.close()
    rooms = running_rooms(opts)
    if rooms:
        subprocess.run([opts.rooms, "kill", rooms[0]], capture_output=True, check=False)
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
        if "peer.ndjson" not in files:
            continue
        with open(os.path.join(folder, "peer.ndjson"), encoding="utf-8") as fh:
            events.extend(json.loads(line) for line in fh if line.endswith("\n"))
    return events


def run_once(opts, count, faulted):
    name = "rooms-n%d%s" % (count, "-faults" if faulted else "")
    run_dir = os.path.join(opts.out, name)
    scratch = os.path.join(run_dir, "redis")
    os.makedirs(scratch)
    port = bench.free_port()
    holder = {"server": start_server(opts, port, scratch)}
    fault_log = []
    try:
        seeder = RespStore(opts.host_ip, port)
        seeder.seed(task_ids(opts.tasks))
        seeder.close()
        command = GUEST.format(bundle=bundle(), host=opts.host_ip, port=port, tasks=opts.tasks,
                               work_ms=opts.work_ms, ttl_ms=opts.ttl_ms)
        argv = [opts.rooms, "clone", opts.snapshot, "--image", opts.image, "--toolstore", opts.toolstore,
                "-n", str(count), "--command", command, "--max-wall", "%ds" % opts.wall_s,
                "--out", os.path.join(run_dir, "out"), "--json"]
        injector = threading.Thread(target=inject, args=(opts, port, scratch, holder, fault_log))
        if faulted:
            injector.start()
        started = time.monotonic()
        done = subprocess.run(argv, capture_output=True, text=True, check=False)
        wall_s = time.monotonic() - started
        if faulted:
            injector.join()
        reader = RespStore(opts.host_ip, port)
        history = reader.history()
        reader.close()
    finally:
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
                rows.append(run_once(opts, count, faulted))
                print(json.dumps(rows[-1], sort_keys=True), flush=True)
    finally:
        with open(os.path.join(opts.out, "summary.json"), "w", encoding="utf-8") as fh:
            json.dump(rows, fh, indent=1, sort_keys=True)
    return 0 if all(r["invariants"]["ok"] for r in rows) else 1


if __name__ == "__main__":
    sys.exit(main())
