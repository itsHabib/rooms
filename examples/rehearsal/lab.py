#!/usr/bin/env python3
"""A six-case, offline payment rehearsal consuming the Rooms matrix contract."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import stat
import subprocess
import sys
import tempfile

from report import publish

HERE = Path(__file__).resolve().parent
MODES = ("baseline", "idempotent")
SCENARIOS = ("normal", "lost-ack", "distinct-events")
LIMIT = 128 * 1024


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def canonical(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()


def write(path, data):
    """Publish a complete artifact by rename within its destination directory."""
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as stream:
        temporary = Path(stream.name)
        stream.write(data)
    temporary.replace(path)


def save(path, value):
    write(path, (json.dumps(value, indent=2) + "\n").encode())


def read(root, relative):
    path = root
    for part in Path(relative).parts:
        if part in ("..", "/"):
            raise ValueError("artifact path escapes run directory")
        path = path / part
        if path.is_symlink():
            raise ValueError(f"symlink artifact: {relative}")
    if not stat.S_ISREG(path.stat().st_mode):
        raise ValueError(f"artifact is not a regular file: {relative}")
    with path.open("rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError(f"artifact is not a regular file: {relative}")
        data = stream.read(LIMIT + 1)
    if len(data) > LIMIT:
        raise ValueError(f"artifact exceeds {LIMIT} bytes: {relative}")
    return data


def unique_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON key: {key}")
        result[key] = value
    return result


def load(root, relative):
    return json.loads(read(root, relative), object_pairs_hook=unique_keys)


def build_manifest():
    handler = (HERE / "handler.sh").read_text()
    scenario = (HERE / "scenario.sh").read_text()
    cases = []
    for mode in MODES:
        for name in SCENARIOS:
            # Identical command bytes on both backends; only the output env differs.
            command = (
                'set -eu; out="${REHEARSAL_OUT:-/workspace/out}"; '
                'mkdir -p "$out"; '
                f"printf %s {shlex.quote(handler)} > \"$out/handler.sh\"; "
                f"sh -c {shlex.quote(scenario)} rehearsal {mode} {name} "
                '"$out" "$out/handler.sh"'
            )
            cases.append({"id": f"{mode}-{name}", "command": command})
    return {"schema": "rooms.matrix.v1", "cases": cases}


def prepare(root, backend):
    root.mkdir(parents=True, exist_ok=False)
    manifest = build_manifest()
    save(root / "matrix.json", manifest)
    save(root / "experiment.json", {
        "schema": "rooms.rehearsal.v1", "specimen": "payment-redelivery-v1",
        "backend": backend, "matrix_sha256": digest(canonical(manifest)),
        "oracle_sha256": digest((HERE / "lab.py").read_bytes()),
    })
    return manifest


def local_run(root, manifest):
    records = []
    for case in manifest["cases"]:
        out = root / "evidence" / case["id"]
        out.mkdir(parents=True)
        env = {"PATH": os.defpath, "REHEARSAL_OUT": str(out)}
        with (out / "stdout.log").open("wb") as stdout, (out / "stderr.log").open("wb") as stderr:
            result = subprocess.run(["sh", "-c", case["command"]], env=env,
                                    stdout=stdout, stderr=stderr, timeout=30, check=False)
        records.append({"case_id": case["id"], "command_sha256": digest(case["command"].encode()),
                        "status": "exited", "exit_code": result.returncode})
    return {"schema": "rooms.rehearsal.local.v1", "status": "completed",
            "matrix_sha256": digest(canonical(manifest)), "clones": records}


def rooms_run(args, root):
    argv = [args.rooms, "matrix", str(args.snapshot.resolve()), "--image", str(args.image.resolve()),
            "--cases", str(root / "matrix.json"), "--out", str(root / "evidence"),
            "--witness", "--egress", "none", "--max-wall", "30s", "--json"]
    save(root / "invocation.json", {"argv": argv})
    # Rooms owns cancellation/teardown and its workload timeout. Do not SIGKILL it
    # from a second timeout wrapper. Preserve even unsuccessful terminal output.
    with (root / "execution.partial.json").open("wb") as stdout, (root / "rooms.stderr.log").open("wb") as stderr:
        result = subprocess.run(argv, stdout=stdout, stderr=stderr, check=False)
    (root / "execution.partial.json").replace(root / "execution.json")
    save(root / "process.json", {"exit_code": result.returncode})


def admitted(root):
    experiment = load(root, "experiment.json")
    manifest = load(root, "matrix.json")
    execution = load(root, "execution.json")
    expected = build_manifest()
    if experiment.get("schema") != "rooms.rehearsal.v1" or experiment.get("specimen") != "payment-redelivery-v1":
        raise ValueError("unsupported experiment")
    if manifest != expected or experiment.get("oracle_sha256") != digest((HERE / "lab.py").read_bytes()):
        raise ValueError("specimen or oracle changed; use the original checkout to inspect this run")
    identity = digest(canonical(expected))
    if experiment.get("matrix_sha256") != identity or execution.get("matrix_sha256") != identity:
        raise ValueError("matrix digest mismatch")
    backend = experiment.get("backend")
    schemas = {"local": "rooms.rehearsal.local.v1", "rooms": "rooms.matrix.result.v1"}
    if backend not in schemas or execution.get("schema") != schemas[backend]:
        raise ValueError("execution backend/schema mismatch")
    process = load(root, "process.json")
    if execution.get("status") != "completed" or type(process.get("exit_code")) is not int or process["exit_code"] != 0:
        raise ValueError("execution did not complete successfully; inspect execution.json and logs")
    records = execution.get("clones", [])
    if len(records) != len(expected["cases"]):
        raise ValueError("missing or extra execution cases")
    indexed = {item["case_id"]: item for item in records}
    if len(indexed) != len(records) or set(indexed) != {case["id"] for case in expected["cases"]}:
        raise ValueError("duplicate or unknown execution case")
    for case in expected["cases"]:
        record = indexed[case["id"]]
        if record.get("command_sha256") != digest(case["command"].encode()):
            raise ValueError(f"command digest mismatch: {case['id']}")
    if backend == "rooms":
        validate_rooms(records)
    return experiment, indexed


def validate_rooms(records):
    for field in ("room_id", "namespace", "host_veth", "clone_net_index"):
        values = [record.get(field) for record in records]
        if any(value is None or value == "" for value in values) or len(set(values)) != len(records):
            raise ValueError(f"missing or reused clone identity: {field}")
    snapshots = {record.get("snapshot_id") for record in records}
    if len(snapshots) != 1 or None in snapshots or "" in snapshots:
        raise ValueError("snapshot lineage mismatch")


def expected_observations(scenario):
    ledger = "payment-001\t2500\n"
    trace = "deliver\tpayment-001\nack\tpayment-001\n"
    if scenario == "lost-ack":
        trace = "deliver\tpayment-001\nreply-lost\tpayment-001\ndeliver\tpayment-001\nack\tpayment-001\n"
    if scenario == "distinct-events":
        ledger += "payment-002\t2500\n"
        trace += "deliver\tpayment-002\nack\tpayment-002\n"
    return ledger, trace


def validate_observation(name, content):
    for line in content.splitlines():
        fields = line.split("\t")
        if len(fields) != 2:
            raise ValueError(f"malformed {name} row")
        if name == "ledger" and (not fields[0].startswith("payment-") or not fields[1].isascii() or not fields[1].isdigit()):
            raise ValueError("malformed ledger entry")
        if name == "trace" and fields[0] not in ("deliver", "reply-lost", "ack"):
            raise ValueError("malformed delivery trace")


def observe(root, mode, scenario, record, backend):
    case_id = f"{mode}-{scenario}"
    row = {"id": case_id, "mode": mode, "scenario": scenario, "status": "inconclusive",
           "reason": "", "ledger": "", "trace": "", "hashes": {}, "execution": record}
    try:
        if type(record.get("exit_code")) is not int or record["exit_code"] != 0 or record.get("status") != "exited":
            raise ValueError("command did not complete successfully")
        for name in ("ledger", "trace"):
            data = read(root, f"evidence/{case_id}/{name}.tsv")
            row[name] = data.decode("utf-8")
            row["hashes"][name] = digest(data)
            validate_observation(name, row[name])
        if backend == "rooms":
            result = load(root, f"evidence/{case_id}/result.json")
            if type(result.get("schema_version")) is not int or result["schema_version"] != 1:
                raise ValueError("unsupported collected runner schema")
            if type(result.get("exit_code")) is not int or result["exit_code"] != 0 or result.get("status") != "succeeded":
                raise ValueError("collected runner result did not succeed")
        ledger, trace = expected_observations(scenario)
        row["expected_ledger"] = ledger
        row["expected_trace"] = trace
        row["status"] = "passed"
        row["reason"] = "one ledger entry per event; delivery trace matches the scenario"
        if row["ledger"] != ledger or row["trace"] != trace:
            row["status"] = "failed"
            row["reason"] = "ledger or delivery trace differs from the independent expectation"
    except (OSError, ValueError, TypeError, KeyError, AttributeError) as error:
        row["status"] = "inconclusive"
        row["reason"] = str(error)
    return row


def compare(root):
    report = {"schema": "rooms.rehearsal.report.v1", "status": "inconclusive",
              "backend": "unknown", "reason": "", "cases": []}
    try:
        experiment, records = admitted(root)
        report.update(backend=experiment["backend"], matrix_sha256=experiment["matrix_sha256"],
                      oracle_sha256=experiment["oracle_sha256"])
        report["cases"] = [observe(root, mode, scenario, records[f"{mode}-{scenario}"], experiment["backend"])
                           for mode in MODES for scenario in SCENARIOS]
        report["status"] = "complete"
        if any(row["status"] == "inconclusive" for row in report["cases"]):
            report["status"] = "inconclusive"
    except (OSError, ValueError, TypeError, KeyError, AttributeError) as error:
        report["reason"] = str(error)
    save(root / "comparison.json", report)
    publish(root, report, write)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="action", required=True)
    for action in ("demo", "run", "report"):
        command = sub.add_parser(action)
        command.add_argument("--out", type=Path, required=True, help="new run directory; existing run for report")
        if action == "run":
            command.add_argument("--snapshot", type=Path, required=True)
            command.add_argument("--image", type=Path, required=True)
            command.add_argument("--rooms", default="rooms", help="Rooms executable (no shell arguments)")
    args = parser.parse_args()
    root = args.out.absolute()
    try:
        if root.is_symlink():
            raise ValueError("run directory must not be a symlink")
        if args.action != "report":
            backend = "local"
            if args.action == "run":
                backend = "rooms"
            manifest = prepare(root, backend)
            if args.action == "demo":
                save(root / "execution.json", local_run(root, manifest))
                save(root / "process.json", {"exit_code": 0})
            if args.action == "run":
                rooms_run(args, root)
        report = compare(root)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"rehearsal: {error}", file=sys.stderr)
        return 2
    print(f"{report['status']}: {root / 'report.html'}")
    if report["status"] != "complete":
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
