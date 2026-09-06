"""Offline views of independently checked rehearsal observations."""
from html import escape
from pathlib import Path

LABELS = {"normal": "Ordinary delivery", "lost-ack": "The reply disappears", "distinct-events": "Same amount, different events"}
MODES = {"baseline": "Original", "idempotent": "With idempotency"}


def text(value):
    return escape(str(value), quote=True)


def cell(row):
    status = row["status"]
    ledger = row["ledger"].splitlines()
    noun = "ledger entries"
    if len(ledger) == 1:
        noun = "ledger entry"
    return (f'<a class="cell {status}" href="#{row["id"]}">'
            f'<strong>{text(status.upper())}</strong><span>{len(ledger)} {noun}</span>'
            '<span class="inspect">Inspect evidence ↗</span></a>')


def detail(row):
    expected = row.get("expected_ledger", "unavailable")
    return (f'<details id="{row["id"]}"><summary><span class="dot {row["status"]}"></span>'
            f'{text(MODES[row["mode"]])} / {text(LABELS[row["scenario"]])}'
            f'<span>{text(row["status"])}</span></summary>'
            f'<p>{text(row["reason"])}</p><div class="evidence">'
            f'<section><h3>Observed ledger</h3><pre>{text(row["ledger"])}</pre></section>'
            f'<section><h3>Expected ledger</h3><pre>{text(expected)}</pre></section>'
            f'<section><h3>Delivery trace</h3><pre>{text(row["trace"])}</pre></section></div>'
            f'<p class="hash">Command {text(row["execution"].get("command_sha256", "unavailable"))}</p>'
            f'<p class="hash">Ledger {text(row["hashes"].get("ledger", "unavailable"))}</p></details>')


def summary(report):
    lines = ["# Payment redelivery rehearsal", "", f"Evidence: {report['backend']}. Comparison: {report['status']}.",
             "", "Synthetic, sequential fixture. A command can exit zero while its behavior fails.", "",
             "| Scenario | Original | With idempotency |", "| --- | --- | --- |"]
    rows = {row["id"]: row for row in report["cases"]}
    for scenario, label in LABELS.items():
        statuses = [rows.get(f"{mode}-{scenario}", {}).get("status", "inconclusive") for mode in MODES]
        lines.append(f"| {label} | {' | '.join(statuses)} |")
    lines.extend(["", report["reason"], "", "No claim of concurrent safety, disk durability, hostile-guest attestation, or merge authorization.",
                  "", "See comparison.json for raw observations, expectations, and hashes.", ""])
    return "\n".join(lines)


def publish(root, report, write):
    rows = {row["id"]: row for row in report["cases"]}
    matrix = []
    for scenario, label in LABELS.items():
        cells = ['<div class="row-label">' + text(label) + '</div>']
        for mode in MODES:
            row = rows.get(f"{mode}-{scenario}")
            cells.append('<div class="cell inconclusive">INCONCLUSIVE</div>')
            if row:
                cells[-1] = cell(row)
        matrix.extend(cells)
    backend = "Local processes · no VM isolation evidence"
    if report["backend"] == "rooms":
        backend = "Rooms matrix · six isolated clones · shared snapshot"
    headline = "The happy path hides a double charge."
    narrative = "Lose one acknowledgement after a payment is recorded. Redeliver it in a fresh process. Inspect what actually reached the ledger."
    if report["status"] != "complete":
        headline = "The evidence is incomplete."
        narrative = report["reason"] or "One or more cases could not be evaluated. Inspect the retained execution record and case evidence."
    baseline = rows.get("baseline-lost-ack", {})
    candidate = rows.get("idempotent-lost-ack", {})
    if report["status"] == "complete" and (baseline.get("status") != "failed" or candidate.get("status") != "passed"):
        headline = "The observed results need a closer look."
        narrative = "The comparison completed, but the expected baseline failure and candidate recovery did not both appear. Read the case evidence."
    template = (Path(__file__).parent / "report.html").read_text()
    replacements = {"BACKEND": text(backend), "STATUS": text(report["status"]), "HEADLINE": text(headline),
                    "NARRATIVE": text(narrative), "MATRIX": "\n".join(matrix),
                    "DETAILS": "\n".join(detail(row) for row in report["cases"]),
                    "DIGEST": text(report.get("matrix_sha256", "unavailable"))}
    for key, value in replacements.items():
        template = template.replace("{{" + key + "}}", value)
    write(root / "summary.md", summary(report).encode())
    write(root / "report.html", template.encode())
