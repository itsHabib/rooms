"""Exercise real fixture behavior and fail-closed comparison admission."""
import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import lab


class RehearsalTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "run"
        self.manifest = lab.prepare(self.root, "local")
        self.execution = lab.local_run(self.root, self.manifest)
        lab.save(self.root / "execution.json", self.execution)
        lab.save(self.root / "process.json", {"exit_code": 0})

    def corrupt_execution(self, mutate):
        mutate(self.execution)
        lab.save(self.root / "execution.json", self.execution)
        return lab.compare(self.root)

    def test_rehearsal_exposes_bug_that_normal_delivery_misses(self):
        report = lab.compare(self.root)
        self.assertEqual(report["status"], "complete")
        statuses = {row["id"]: row["status"] for row in report["cases"]}
        self.assertEqual(statuses.pop("baseline-lost-ack"), "failed")
        self.assertEqual(set(statuses.values()), {"passed"})
        self.assertTrue(all(record["exit_code"] == 0 for record in self.execution["clones"]))
        lost = self.root / "evidence/baseline-lost-ack/ledger.tsv"
        self.assertEqual(lost.read_text().count("payment-001"), 2)

    def test_amount_dedup_mutant_loses_a_legitimate_payment(self):
        # Exercise an actual wrong handler, not a manually invented outcome.
        handler = (lab.HERE / "handler.sh").read_text().replace("$1 == event", "$2 == 2500")
        target = self.root / "evidence/idempotent-distinct-events"
        (target / "mutant.sh").write_text(handler)
        subprocess.run(["sh", str(lab.HERE / "scenario.sh"), "idempotent", "distinct-events",
                        str(target), str(target / "mutant.sh")], check=True)
        row = lab.compare(self.root)["cases"][-1]
        self.assertEqual(row["status"], "failed")
        self.assertNotIn("payment-002", row["ledger"])

    def test_guest_claim_of_success_cannot_override_observations(self):
        (self.root / "evidence/baseline-lost-ack/verdict.json").write_text('{"passed":true}')
        report = lab.compare(self.root)
        self.assertEqual(report["cases"][1]["status"], "failed")

    def test_missing_empty_malformed_and_oversized_evidence(self):
        target = self.root / "evidence/idempotent-normal/ledger.tsv"
        for content, expected in ((None, "inconclusive"), ("", "failed"), ("nonsense", "inconclusive"),
                                  ("x" * (lab.LIMIT + 1), "inconclusive")):
            with self.subTest(expected=expected, size=len(content or "")):
                if target.exists():
                    target.unlink()
                if content is not None:
                    target.write_text(content)
                row = lab.compare(self.root)["cases"][3]
                self.assertEqual(row["status"], expected)

    def test_execution_identity_and_completeness_are_required(self):
        original = copy.deepcopy(self.execution)
        mutations = [lambda x: x.update(status="cancelled"),
                     lambda x: x.update(matrix_sha256="sha256:wrong"),
                     lambda x: x.update(schema="rooms.matrix.result.v1"),
                     lambda x: x["clones"].pop(),
                     lambda x: x["clones"].append(x["clones"][0]),
                     lambda x: x["clones"][0].update(command_sha256="sha256:wrong"),
                     lambda x: x["clones"][0].update(case_id="../../escape")]
        for mutate in mutations:
            self.execution = copy.deepcopy(original)
            self.assertEqual(self.corrupt_execution(mutate)["status"], "inconclusive")

    def test_command_failure_and_boolean_exit_are_inconclusive(self):
        for code in (1, False, None):
            report = self.corrupt_execution(lambda x: x["clones"][0].update(exit_code=code))
            self.assertEqual(report["cases"][0]["status"], "inconclusive")

    def test_changed_manifest_is_never_executed_by_report(self):
        marker = self.root / "should-not-exist"
        self.manifest["cases"][0]["command"] = f"touch {marker}"
        lab.save(self.root / "matrix.json", self.manifest)
        self.assertEqual(lab.compare(self.root)["status"], "inconclusive")
        self.assertFalse(marker.exists())

    def test_invalid_json_and_duplicate_keys_are_inconclusive(self):
        for content in ('{"status":', '{"status":"failed","status":"completed"}', 'null'):
            (self.root / "execution.json").write_text(content)
            self.assertEqual(lab.compare(self.root)["status"], "inconclusive")

    def test_report_escapes_observed_content_and_has_no_external_assets(self):
        target = self.root / "evidence/baseline-normal/ledger.tsv"
        target.write_text('<script>alert("bad")</script>')
        lab.compare(self.root)
        html = (self.root / "report.html").read_text()
        self.assertIn("&lt;script&gt;", html)
        self.assertNotIn("<script>", html)
        self.assertNotIn('src="https:', html)
        self.assertNotIn('href="https:', html)
        self.assertIn("Local processes", html)

    def test_evidence_placeholders_stay_literal_and_missing_counts_stay_unknown(self):
        target = self.root / "evidence/baseline-normal/ledger.tsv"
        target.write_text("{{DIGEST}}")
        lab.compare(self.root)
        html = (self.root / "report.html").read_text()
        self.assertIn("<pre>{{DIGEST}}</pre>", html)
        self.assertIn("evidence incomplete", html)
        self.assertNotIn("0 ledger entries", html)
        self.assertIn("Expected trace", html)
        self.assertIn("Trace sha256:", html)

    def test_symlink_evidence_is_refused(self):
        target = self.root / "evidence/baseline-normal/ledger.tsv"
        target.unlink()
        target.symlink_to(self.root / "evidence/idempotent-normal/ledger.tsv")
        self.assertEqual(lab.compare(self.root)["cases"][0]["status"], "inconclusive")

    def test_rooms_identity_checks(self):
        records = [{"room_id": str(i), "namespace": f"ns{i}", "host_veth": f"veth{i}",
                    "clone_net_index": i + 1, "snapshot_id": "shared"} for i in range(6)]
        lab.validate_rooms(records)
        for field in ("room_id", "namespace", "host_veth", "clone_net_index"):
            mutant = copy.deepcopy(records)
            mutant[0][field] = mutant[1][field]
            with self.assertRaises(ValueError):
                lab.validate_rooms(mutant)
        records[0]["snapshot_id"] = "different"
        with self.assertRaises(ValueError):
            lab.validate_rooms(records)

    def test_rooms_collected_result_is_required_and_checked(self):
        experiment = lab.load(self.root, "experiment.json")
        experiment["backend"] = "rooms"
        lab.save(self.root / "experiment.json", experiment)
        self.execution["schema"] = "rooms.matrix.result.v1"
        for i, record in enumerate(self.execution["clones"]):
            record.update(room_id=str(i), namespace=f"ns{i}", host_veth=f"veth{i}",
                          clone_net_index=i + 1, snapshot_id="shared")
            lab.save(self.root / "evidence" / record["case_id"] / "result.json",
                     {"schema_version": 1, "status": "succeeded", "exit_code": 0})
        lab.save(self.root / "execution.json", self.execution)
        self.assertEqual(lab.compare(self.root)["status"], "complete")
        (self.root / "evidence/idempotent-normal/result.json").unlink()
        self.assertEqual(lab.compare(self.root)["cases"][3]["status"], "inconclusive")

    def test_cli_refuses_reuse_and_returns_two_for_incomplete_report(self):
        command = [sys.executable, str(lab.HERE / "lab.py")]
        result = subprocess.run(command + ["demo", "--out", str(self.root)], capture_output=True)
        self.assertEqual(result.returncode, 2)
        (self.root / "execution.json").write_text('{}')
        result = subprocess.run(command + ["report", "--out", str(self.root)], capture_output=True)
        self.assertEqual(result.returncode, 2)
        self.assertTrue((self.root / "report.html").exists())


if __name__ == "__main__":
    unittest.main()
