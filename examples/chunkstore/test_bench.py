import contextlib
import io
import json
import os
import tempfile
import unittest

import bench

MIB = 1 << 20


class GeneratorTest(unittest.TestCase):
    def test_same_seed_gives_identical_snapshots(self):
        first = bench.synth_memory(2 * MIB, seed=5)
        again = bench.synth_memory(2 * MIB, seed=5)
        self.assertEqual(first, again)
        bench.derive_related(first, seed=5)
        bench.derive_related(again, seed=5)
        self.assertEqual(first, again)
        self.assertNotEqual(first, bench.synth_memory(2 * MIB, seed=6))

    def test_memory_mixes_zero_repeated_and_random_pages(self):
        mem = bench.synth_memory(8 * MIB, seed=1)
        pages = [bytes(mem[offset:offset + bench.PAGE]) for offset in range(0, len(mem), bench.PAGE)]
        zero = sum(1 for page in pages if page.count(0) == bench.PAGE)
        distinct = len(set(pages))
        self.assertGreater(zero, len(pages) // 4)
        self.assertLess(zero, len(pages) * 3 // 4)
        self.assertLess(distinct, len(pages) - zero)
        self.assertGreater(distinct, len(pages) // 10)

    def test_related_snapshot_keeps_size_and_most_pages(self):
        base = bench.synth_memory(8 * MIB, seed=1)
        later = bytearray(base)
        bench.derive_related(later, seed=1)
        self.assertEqual(len(later), len(base))
        head = range(0, len(base) * 2 // 5, bench.PAGE)
        changed = sum(1 for offset in head if base[offset:offset + bench.PAGE] != later[offset:offset + bench.PAGE])
        self.assertGreater(changed, 0)
        self.assertLess(changed, len(head) // 2)

    def test_sparse_writer_round_trips(self):
        mem = bench.synth_memory(2 * MIB, seed=2)
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "mem")
            bench.write_sparse(path, mem, block=64 * 1024)
            with open(path, "rb") as handle:
                self.assertEqual(handle.read(), bytes(mem))


class BenchRunTest(unittest.TestCase):
    def run_bench(self, out):
        argv = ["--out", out, "--size-mib", "4", "--rate-mbps", "200", "--sizes-kib", "64", "--fetch-chunk-kib", "64"]
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return bench.main(argv)

    def test_small_run_writes_summary_and_refuses_to_reuse_the_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "run")
            self.assertEqual(self.run_bench(out), 0)
            with open(os.path.join(out, "summary.json")) as handle:
                summary = json.load(handle)
            with open(os.path.join(out, "raw.ndjson")) as handle:
                raw = [json.loads(line) for line in handle]
            self.assertEqual(self.run_bench(out), 2)
        self.assertTrue(summary["ok"])
        self.assertEqual(len(raw), 1 + len(summary["transfers"]) + len(summary["fetches"]))
        by_name = {record["strategy"]: record for record in summary["transfers"]}
        self.assertLess(by_name["fixed-64k"]["warm_bytes"], by_name["fixed-64k"]["cold_bytes"])
        self.assertLess(by_name["fixed-64k"]["cold_bytes"], by_name["whole-file"]["cold_bytes"])
        corrupt = [record for record in summary["fetches"] if record["run"] == "corrupt-1-of-3"][0]
        self.assertIsNotNone(corrupt["seeds"][0]["dropped"])
        self.assertEqual(corrupt["seeds"][0]["chunks"], 0)


if __name__ == "__main__":
    unittest.main()
