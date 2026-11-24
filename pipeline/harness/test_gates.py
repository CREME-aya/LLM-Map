import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import gate2
import gate3
import gate5
import sample
from common import read_json, rows
from sidecar import inspect


class TemporaryRun(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name) / "run"


class PackTest(TemporaryRun):
    def setUp(self):
        super().setUp()
        sample.gate2(self.root)
        self.key = self.root / "test-public.pem"
        self.pointer = self.root / "active"
        self.pointer.write_text("v1")

    def run_switch(self):
        return gate2.rehearse(self.root, self.key, self.pointer)

    def adjust(self, **fields):
        run = read_json(self.root / "run.json")
        run.update(fields)
        sample.write(self.root / "run.json", run)

    def test_verified_switch_keeps_pinned_generation_and_reuses_shared(self):
        r = self.run_switch()
        self.assertTrue(r["criteria_met"])
        self.assertEqual((r["pinned_version"], self.pointer.read_text()), ("v1", "v2"))
        self.assertGreater(r["reused_bytes"], 0)
        self.assertEqual(len(r["od_runs"]), 12)

    def test_peak_excess_and_device_shortage_keep_current(self):
        original = gate2.budget
        with patch.object(
            gate2,
            "budget",
            side_effect=lambda old, new, region: original(
                {**old, "additional": {"bytes": 9_000_000_001}}, new, region
            ),
        ):
            self.assertFalse(self.run_switch()["criteria_met"])
        self.adjust(device_free_bytes=1)
        self.assertFalse(self.run_switch()["criteria_met"])
        self.assertEqual(self.pointer.read_text(), "v1")

    def test_signed_manifest_tamper_does_not_activate(self):
        path = self.root / "new.json"
        path.write_bytes(path.read_bytes() + b" ")
        with self.assertRaisesRegex(ValueError, "署名"):
            self.run_switch()
        self.assertEqual(self.pointer.read_text(), "v1")

    def test_corrupt_chunk_does_not_activate(self):
        key = read_json(self.root / "new.json")["chunks"][0]["sha256"]
        path = self.root / "blobs" / key
        path.write_bytes(b"x" * path.stat().st_size)
        with self.assertRaisesRegex(ValueError, "破損"):
            self.run_switch()
        self.assertEqual(self.pointer.read_text(), "v1")

    def test_version_mixing_is_rejected_even_in_verified_manifest(self):
        m = read_json(self.root / "new.json")
        m["chunks"][0]["graph_version"] = "v1"
        with self.assertRaisesRegex(ValueError, "版不一致"):
            gate2.check_manifest(m, self.root / "blobs")

    def test_missing_od_and_fake_guidance(self):
        run = read_json(self.root / "run.json")
        run["reroutes"]["outside_safe"][3] = False
        self.adjust(reroutes=run["reroutes"])
        self.assertFalse(self.run_switch()["criteria_met"])
        self.adjust(od_runs=run["od_runs"][1:])
        with self.assertRaisesRegex(ValueError, "12 OD"):
            self.run_switch()
        self.assertEqual(self.pointer.read_text(), "v1")

    def test_threshold_boundaries_with_logical_sizes(self):
        # 巨大オブジェクトを書き込まず、署名検証後の容量計算だけを検証。
        new = {str(i): {"bytes": 250_000_000} for i in range(30)}
        old = {str(i): {"bytes": 250_000_000} for i in range(30, 36)}
        first = ["0", "1", "2"]
        self.assertTrue(gate2.budget(old, new, first)["within_budget"])
        self.assertFalse(
            gate2.budget({**old, "extra": {"bytes": 1}}, new, first)["within_budget"]
        )
        self.assertFalse(
            gate2.budget(old, {**new, "extra": {"bytes": 1}}, first)["within_budget"]
        )
        self.assertFalse(gate2.budget(old, new, first + ["3"])["within_budget"])
        self.assertFalse(
            gate2.budget({}, {"0": {"bytes": 256_000_001}}, ["0"])["within_budget"]
        )

    def test_unknown_width_is_distinct_from_corrupt_or_wrong_tile(self):
        raw = (Path(__file__).parent / "handoff" / "fixture.dsc").read_bytes()
        self.assertEqual(inspect(raw, "jp-test-1", 0), 2)
        for buf, ver, tile in (
            (raw[:-1], "jp-test-1", 0),
            (raw, "old", 0),
            (raw, "jp-test-1", 4),
            (raw[:-1] + b"\x02", "jp-test-1", 0),
        ):
            with self.assertRaises(ValueError):
                inspect(buf, ver, tile)

    def test_all_r2_objects_include_models_and_both_version_manifests(self):
        old, new = (read_json(self.root / name) for name in ("old.json", "new.json"))
        model = b"model fixture"
        key = sample.digest(model)
        (self.root / "blobs" / key).write_bytes(model)
        for m in (old, new):
            m["model"] = {"storage": "r2", "revision": "pinned", "sha256": key}
            m["additional_r2_objects"] = [
                {"object_key": key, "sha256": key, "bytes": len(model), "kind": "model"}
            ]
        a, b = (
            gate2.objects(self.root, name, m, {c["sha256"]: c for c in m["chunks"]})
            for name, m in (("old.json", old), ("new.json", new))
        )
        self.assertEqual(
            {k for k in a.keys() | b.keys() if k.startswith("manifests/")},
            {f"manifests/v{v}/manifest.json{s}" for s in ("", ".sig") for v in (1, 2)},
        )
        self.assertEqual(
            sum(v["bytes"] for k, v in (a | b).items() if k == key), len(model)
        )
        new["additional_r2_objects"] = []
        with self.assertRaises(ValueError):
            gate2.objects(self.root, "new.json", new, {})

    def test_cell_misalignment_and_missing_national_tile_are_rejected(self):
        m = read_json(self.root / "new.json")
        chunk = next(c for c in m["chunks"] if c["kind"] == "L2")
        chunk["cell"][0] += 1
        with self.assertRaisesRegex(ValueError, "セル不一致"):
            gate2.check_manifest(m, self.root / "blobs")
        chunk["cell"][0] -= 1
        m["builder_tile_inventory"].append(123)
        with self.assertRaisesRegex(ValueError, "タイル欠損"):
            gate2.check_manifest(m, self.root / "blobs")


class SurveyTest(TemporaryRun):
    def setUp(self):
        super().setUp()
        sample.gate3(self.root)

    def rewrite(self, name, fn):
        path = self.root / f"{name}.ndjson"
        sample.ndjson(path, fn(list(rows(path))))

    def test_fixed_pairs_and_weighted_denominator(self):
        r = gate3.evaluate(self.root)
        self.assertTrue(r["criteria_met"])
        self.assertEqual(
            (r["audited_od_count"], len(r["paired_od_ids"]), len(r["no_alternative"])),
            (40, 36, 4),
        )
        self.assertEqual(r["baseline"]["crossings"], 360)
        widths = [s for s in r["tag_strata"] if s["tag"] == "width"]
        self.assertEqual(len(widths), 47)
        self.assertIsNone(widths[0]["transfer_rate"])
        self.assertEqual(
            (widths[0]["eligible_cm"], widths[0]["distance_detection_rate"]), (1000, 0)
        )

    def test_non_ok_and_missing_result_are_pending(self):
        records = list(rows(self.root / "routes.ndjson"))
        sample.ndjson(
            self.root / "routes.ndjson",
            [
                dict(r, outcome="timeout") if r["od_id"] == "urban_right-0" else r
                for r in records
                if r["od_id"] != "urban_right-1"
                and (r["od_id"] != "urban_right-0" or r["kind"] == "result")
            ],
        )
        self.rewrite(
            "audits",
            lambda a: [
                r for r in a if r["od_id"] not in {"urban_right-0", "urban_right-1"}
            ],
        )
        r = gate3.evaluate(self.root)
        self.assertFalse(r["criteria_met"])
        self.assertEqual(r["audited_od_count"], 38)
        self.assertEqual(r["pending"]["urban_right-1"], "missing_result")

    def test_missing_audit_and_unknown_are_not_zero(self):
        self.rewrite(
            "audits",
            lambda a: [
                dict(r, crossings=None) if r["od_id"] == "urban_right-1" else r
                for r in a[1:]
            ],
        )
        r = gate3.evaluate(self.root)
        self.assertFalse(r["criteria_met"])
        self.assertEqual(r["audited_od_count"], 38)
        self.assertNotIn("urban_right-1", r["paired_od_ids"])
        self.assertEqual(r["pending"]["urban_right-0"], "missing_audit")

    def test_false_left_or_illegal_or_insufficient_reduction_fails(self):
        self.rewrite(
            "routes",
            lambda rs: [
                dict(r, entrance_display="left") if r["kind"] == "candidate" else r
                for r in rs
            ],
        )
        self.rewrite(
            "audits",
            lambda rs: [
                dict(r, legality="illegal")
                if r["od_id"] == "urban_right-2"
                else dict(r, narrow_cm=1000)
                for r in rs
            ],
        )
        r = gate3.evaluate(self.root)
        self.assertFalse(r["criteria_met"])
        self.assertGreater(r["observed_false_left"], 0)
        self.assertGreater(r["observed_illegal"], 0)
        self.assertEqual(r["reductions"]["narrow_cm"], 0)

    def test_duplicate_result_rejected(self):
        self.rewrite("routes", lambda r: r + [r[0]])
        with self.assertRaises(ValueError):
            gate3.evaluate(self.root)

    def test_tag_population_version_and_sidecar_mismatch(self):
        path = self.root / "tags.ndjson"
        values = list(rows(path))
        for replacement in (
            values[:-1],
            [dict(values[0], graph_version="old")] + values[1:],
            [dict(values[0], sidecar_status="corrupt")] + values[1:],
        ):
            sample.ndjson(path, replacement)
            with self.assertRaises(ValueError):
                gate3.evaluate(self.root)
        values[0]["source"]["turn:lanes"] = None
        sample.ndjson(path, values)
        self.assertFalse(gate3.evaluate(self.root)["criteria_met"])


class PssTest(TemporaryRun):
    def setUp(self):
        super().setUp()
        sample.gate5(self.root)
        self.package = "jp.example.nav"

    def test_steady_p95_and_event_peak_use_contemporaneous_sums(self):
        r = gate5.evaluate(self.root)
        self.assertTrue(r["criteria_met"])
        self.assertEqual(r["steady_samples"], 721)
        self.assertEqual(
            (r["p95_bytes"], r["peak_bytes"]), (1_100_000 * 1024, 1_130_000 * 1024)
        )

    def test_event_only_excess_is_in_maximum(self):
        (self.root / "112.txt").write_text(sample.dump(self.package, 900000, 600000))
        r = gate5.evaluate(self.root)
        self.assertFalse(r["criteria_met"])
        self.assertEqual(r["p95_bytes"], 1_100_000 * 1024)
        self.assertGreater(r["peak_bytes"], 1_500_000_000)

    def test_steady_p95_excess(self):
        for sec in range(0, 185, 5):
            (self.root / f"{sec}.txt").write_text(
                sample.dump(self.package, 900000, 400000)
            )
        r = gate5.evaluate(self.root)
        self.assertFalse(r["criteria_met"])
        self.assertLess(r["peak_bytes"], 1_500_000_000)

    def test_gap_missing_inference_and_short_run(self):
        (self.root / "5.txt").write_text(
            "** MEMINFO in pid 11 [jp.example.nav] **\n TOTAL PSS: 55"
        )
        (self.root / "111.txt").unlink()
        info = read_json(self.root / "run.json")
        sample.write(self.root / "run.json", dict(info, observed_duration_ms=3599999))
        r = gate5.evaluate(self.root)
        self.assertFalse(r["criteria_met"])
        self.assertEqual(set(r["missing"]), {5, 111})

    def test_missing_event_schedule_or_unreviewed_exit_log(self):
        info = read_json(self.root / "run.json")
        for modified in (dict(info, events=[]), dict(info, abnormal_exits=None)):
            sample.write(self.root / "run.json", modified)
            with self.assertRaises(ValueError):
                gate5.evaluate(self.root)

    def test_device_matrix_requires_distinct_conditions(self):
        self.assertFalse(gate5.matrix(self.root)["criteria_met"])
        with self.assertRaises(ValueError):
            gate5.matrix(self.root, self.root)

    def test_pid_restart_detected_even_if_pss_below_envelope(self):
        (self.root / "15.txt").write_text(
            sample.dump(self.package).replace("pid 12", "pid 13")
        )
        r = gate5.evaluate(self.root)
        self.assertFalse(r["criteria_met"])
        self.assertEqual(r["pid_changes"], [15, 20])


if __name__ == "__main__":
    unittest.main()
