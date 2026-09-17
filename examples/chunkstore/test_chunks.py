import contextlib
import io
import json
import os
import random
import tempfile
import unittest

import chunks

KIB = 1024


def sample_bytes(seed=7):
    """Data, a long hole, repeated data, a short zero gap, and a ragged tail."""
    rng = random.Random(seed)
    block = rng.randbytes(40 * KIB)
    parts = [rng.randbytes(300 * KIB), bytes(512 * KIB), block * 8, bytes(3 * KIB), rng.randbytes(77 * KIB + 13)]
    return b"".join(parts)


class ChunkCase(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.dir = tmp.name
        self.store = chunks.Store(os.path.join(self.dir, "store"))

    def write(self, name, data):
        path = os.path.join(self.dir, name)
        with open(path, "wb") as handle:
            handle.write(data)
        return path

    def read(self, path):
        with open(path, "rb") as handle:
            return handle.read()


class RoundTripTest(ChunkCase):
    def round_trip(self, spec):
        data = sample_bytes()
        source = self.write("snapshot.mem", data)
        empty = self.write("empty.bin", b"")
        manifest = chunks.build_manifest("snap", [source, empty], spec, self.store)
        self.store.save_manifest(manifest)
        out = os.path.join(self.dir, "out")
        paths = chunks.reassemble(self.store.load_manifest("snap"), self.store, out)
        self.assertEqual(self.read(paths[0]), data)
        self.assertEqual(self.read(paths[1]), b"")
        return manifest

    def test_fixed_round_trip_records_holes(self):
        manifest = self.round_trip(chunks.fixed_spec(64 * KIB))
        records = manifest["files"][0]["chunks"]
        holes = [rec for rec in records if rec[2] is None]
        self.assertEqual(len(holes), 7)
        plan = chunks.transfer_plan(manifest, set())
        self.assertEqual(plan["hole_bytes"], 7 * 64 * KIB)
        self.assertEqual(plan["unique_bytes"], sum(os.path.getsize(p) for p in self._objects()))

    def test_cdc_round_trip_finds_exact_hole(self):
        manifest = self.round_trip(chunks.cdc_spec(32 * KIB))
        records = manifest["files"][0]["chunks"]
        self.assertIn([300 * KIB, 512 * KIB, None], records)
        lengths = [length for _, length, sha in records if sha is not None]
        self.assertLessEqual(max(lengths), 4 * 32 * KIB)

    def test_holes_are_never_stored(self):
        source = self.write("zeros.bin", bytes(1 << 20))
        manifest = chunks.build_manifest("zeros", [source], chunks.fixed_spec(64 * KIB), self.store)
        self.assertEqual(self.store.hashes(), set())
        out = chunks.reassemble(manifest, self.store, os.path.join(self.dir, "out"))
        self.assertEqual(self.read(out[0]), bytes(1 << 20))

    def _objects(self):
        for sha in self.store.hashes():
            yield self.store.object_path(sha)


class RefusalTest(ChunkCase):
    def test_corrupt_object_fails_reassembly_and_leaves_no_file(self):
        source = self.write("data.bin", sample_bytes())
        manifest = chunks.build_manifest("snap", [source], chunks.fixed_spec(64 * KIB), self.store)
        victim = self.store.object_path(manifest["files"][0]["chunks"][0][2])
        with open(victim, "r+b") as handle:
            handle.write(b"\xff")
        out = os.path.join(self.dir, "out")
        with self.assertRaises(chunks.ChunkError):
            chunks.reassemble(manifest, self.store, out)
        self.assertEqual(os.listdir(out), [])

    def test_missing_object_fails_loudly(self):
        source = self.write("data.bin", sample_bytes())
        manifest = chunks.build_manifest("snap", [source], chunks.fixed_spec(64 * KIB), self.store)
        os.unlink(self.store.object_path(manifest["files"][0]["chunks"][0][2]))
        with self.assertRaises(OSError):
            chunks.reassemble(manifest, self.store, os.path.join(self.dir, "out"))

    def test_put_refuses_unexpected_bytes(self):
        with self.assertRaises(chunks.ChunkError):
            self.store.put(b"not it", expect=chunks.sha256_hex(b"it"))
        self.assertEqual(self.store.hashes(), set())

    def test_failed_write_leaves_no_temp_file(self):
        with self.assertRaises(TypeError):
            self.store._write_atomic(os.path.join(self.store.objects, "x"), "not bytes")
        self.assertEqual(os.listdir(self.store.tmp), [])
        self.assertEqual(os.listdir(self.store.objects), [])

    def test_manifest_cannot_name_a_path_outside_the_output(self):
        for path in ("../escape", "/etc/passwd", "a/b", "..", ""):
            manifest = {"version": 1, "name": "snap", "files": [{"path": path, "size": 0, "chunks": []}]}
            with self.assertRaises(chunks.ChunkError, msg=path):
                chunks.reassemble(manifest, self.store, os.path.join(self.dir, "out"))

    def test_manifest_with_gap_or_bad_hash_is_refused(self):
        sha = chunks.sha256_hex(b"x")
        for records in ([[0, 1, sha], [2, 1, sha]], [[0, 1, "zz"]], [[0, 1, sha]]):
            manifest = {"version": 1, "name": "snap", "files": [{"path": "f", "size": 3, "chunks": records}]}
            with self.assertRaises(chunks.ChunkError):
                chunks.validate_manifest(manifest)

    def test_store_refuses_traversal_names(self):
        for name in ("../x", "a/b", ".hidden", ""):
            with self.assertRaises(chunks.ChunkError):
                self.store.manifest_path(name)
        with self.assertRaises(chunks.ChunkError):
            self.store.object_path("../" + "a" * 61)


class DedupTest(ChunkCase):
    def test_repeated_blocks_are_stored_once(self):
        rng = random.Random(3)
        block = rng.randbytes(64 * KIB)
        source = self.write("data.bin", block * 5 + rng.randbytes(64 * KIB))
        manifest = chunks.build_manifest("snap", [source], chunks.fixed_spec(64 * KIB), self.store)
        plan = chunks.transfer_plan(manifest, set())
        self.assertEqual((plan["data_chunks"], plan["unique_chunks"]), (6, 2))
        self.assertEqual(plan["needed_bytes"], 2 * 64 * KIB)
        self.assertEqual(len(self.store.hashes()), 2)

    def test_warm_host_needs_only_changed_chunks(self):
        rng = random.Random(4)
        data = bytearray(rng.randbytes(640 * KIB))
        first = self.write("a.bin", bytes(data))
        data[130 * KIB:131 * KIB] = rng.randbytes(KIB)
        second = self.write("b.bin", bytes(data))
        spec = chunks.fixed_spec(64 * KIB)
        base = chunks.build_manifest("a", [first], spec)
        later = chunks.build_manifest("b", [second], spec)
        plan = chunks.transfer_plan(later, set(chunks.manifest_hashes(base)))
        self.assertEqual((plan["needed_chunks"], plan["needed_bytes"]), (1, 64 * KIB))
        self.assertEqual(chunks.transfer_plan(later, set())["needed_bytes"], 640 * KIB)

    def test_insertion_defeats_fixed_but_not_cdc(self):
        rng = random.Random(5)
        data = rng.randbytes(2 << 20)
        first = self.write("a.bin", data)
        second = self.write("b.bin", data[:1000] + rng.randbytes(37) + data[1000:])
        needed = {}
        for spec in (chunks.fixed_spec(32 * KIB), chunks.cdc_spec(32 * KIB)):
            base = chunks.build_manifest("a", [first], spec)
            later = chunks.build_manifest("b", [second], spec)
            plan = chunks.transfer_plan(later, set(chunks.manifest_hashes(base)))
            needed[spec["kind"]] = plan["needed_bytes"]
        self.assertGreater(needed["fixed"], 2 << 20)
        self.assertLess(needed["cdc"], 256 * KIB)

    def test_cdc_cuts_depend_only_on_content(self):
        source = self.write("a.bin", sample_bytes())
        spec = chunks.cdc_spec(32 * KIB)
        self.assertEqual(chunks.chunk_file(source, spec), chunks.chunk_file(source, spec))


class CliTest(ChunkCase):
    def run_cli(self, *argv):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = chunks.main(list(argv))
        self.assertEqual(code, 0)
        return json.loads(out.getvalue())

    def test_ingest_then_stats(self):
        data = sample_bytes()
        first = self.write("a.mem", data)
        second = self.write("b.mem", data[:100 * KIB] + bytes(KIB) + data[101 * KIB:])
        root = self.store.root
        report = self.run_cli("ingest", "--store", root, "--name", "a", "--size", str(64 * KIB), first)
        self.assertRegex(report["manifest_sha256"], chunks.HASH_RE)
        self.run_cli("ingest", "--store", root, "--name", "b", "--size", str(64 * KIB), second)
        stats = self.run_cli("stats", "--store", root, "a", "b")
        self.assertEqual(stats["b_given_a"]["needed_chunks"], 1)
        self.assertEqual(stats["b"]["logical_bytes"], len(data))

    def test_unknown_name_is_an_error_not_a_traceback(self):
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(chunks.main(["stats", "--store", self.store.root, "nope", "nada"]), 1)


if __name__ == "__main__":
    unittest.main()
