"""Protocol tests use synthetic records only; no game/device claim."""

import copy
import csv
from pathlib import Path
import tempfile
import unittest

import batch_metrics
import host_controls
import paired


class BatchMetricsTests(unittest.TestCase):
    def parse(self, rows, start=1, end=3):
        with tempfile.TemporaryDirectory() as folder:
            p = Path(folder) / "batch.csv"
            with p.open("w", newline="") as out:
                writer = csv.writer(out)
                writer.writerow(batch_metrics.FIELDS)
                writer.writerows(rows)
            return batch_metrics.read(p, 3, start, end)

    def rows(self):
        return [
            [
                i,
                i * 1_000_000,
                500_000,
                i * 1_000_000,
                500_000,
                i * 100,
                i * 10,
                i * 1000,
                0,
            ]
            for i in (1, 2, 3)
        ]

    def test_named_range_excludes_startup_and_uses_boundary_work(self):
        data = self.parse(self.rows())
        measured = data["measurement"]
        self.assertAlmostEqual(measured["wall_seconds"], 0.005)
        self.assertAlmostEqual(measured["process_cpu_seconds"], 0.001)
        self.assertEqual(measured["guest_work_delta"]["recompiled_instructions"], 2000)
        self.assertEqual(measured["batch_wall_ms"]["median"], 2.5)
        self.assertEqual(data["whole_loop"]["batches"], 3)

    def test_per_batch_wait_deltas_may_fall_without_guest_time_going_backward(self):
        rows = self.rows()
        rows[0][-1], rows[1][-1], rows[2][-1] = 100, 50, 0
        data = self.parse(rows)
        self.assertAlmostEqual(data["measurement"]["gpu_wait_seconds"], 0.00005)

    def test_missing_duplicate_backward_nonfinite_and_extra_rows_fail_closed(self):
        bad = [self.rows()[:2], [self.rows()[0]] * 3, self.rows() + [self.rows()[2]]]
        for field, value in [(1, 0), (2, "nan"), (2, ""), (5, -1), (7, 0)]:
            rows = self.rows()
            rows[1][field] = value
            bad.append(rows)
        for rows in bad:
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                self.parse(rows)
        with self.assertRaises(ValueError):
            self.parse(self.rows(), start=3)


class AdmissionTests(unittest.TestCase):
    def snapshot(self, busy=0, temperature=60):
        return {
            "cpu": {
                "cpu": {"busy": busy, "total": 100},
                "cpu2": {"busy": busy, "total": 100},
            },
            "cpu_temperatures_c": [temperature],
            "governors": {"policy2": "performance"},
            "boost": "1",
            "ac_online": {"AC": "1"},
        }

    def policy(self):
        return {
            "cpus": [2],
            "siblings": [],
            "max_busy_fraction": 0.03,
            "max_core_fraction": 0.1,
            "max_cpu_c": 85,
            "max_temperature_span_c": 3,
        }

    def test_aggregate_and_local_contention_have_distinct_gates(self):
        a = self.snapshot()
        b = self.snapshot(4)
        b["cpu"]["cpu"]["total"] = b["cpu"]["cpu2"]["total"] = 200
        result = host_controls.check_window(a, b, self.policy())
        self.assertFalse(result["passed"])
        self.assertEqual(result["errors"], ["host CPU busy"])
        b["cpu"]["cpu"]["busy"] = 0
        b["cpu"]["cpu2"]["busy"] = 20
        self.assertIn(
            "selected core or SMT sibling busy/unknown",
            host_controls.check_window(a, b, self.policy())["errors"],
        )

    def test_unknown_temperature_power_and_changed_boost_are_not_quiet(self):
        a = self.snapshot()
        for key, value in [
            ("cpu_temperatures_c", []),
            ("ac_online", {}),
            ("boost", "0"),
        ]:
            b = copy.deepcopy(a)
            b[key] = value
            self.assertFalse(host_controls.check_window(a, b, self.policy())["passed"])
        self.assertFalse(
            host_controls.check_window(a, self.snapshot(temperature=90), self.policy())[
                "passed"
            ]
        )

    def test_stable_cpu_sensors_need_not_have_the_same_temperature(self):
        a = self.snapshot()
        a["cpu_temperatures_c"] = [45, 60]
        self.assertTrue(
            host_controls.check_window(a, copy.deepcopy(a), self.policy())["passed"]
        )

    def test_post_run_thermal_and_smt_gate(self):
        a = self.snapshot()
        b = self.snapshot(temperature=90)
        self.assertIn(
            "CPU temperature unknown/above ceiling during run",
            host_controls.observation_errors([a, b], self.policy()),
        )

    def test_relaxed_desktop_policy_tolerates_moderate_load_and_thermal_drift(self):
        example = paired.json.loads(
            Path(paired.__file__).with_name("paired.example.json").read_text()
        )
        policy = {**example["host_policy"], "cpus": [2], "siblings": []}
        a = self.snapshot()
        b = self.snapshot(8, temperature=64)
        for counter in b["cpu"].values():
            counter["total"] = 200
        b["cpu"]["cpu2"]["busy"] = 20
        self.assertFalse(host_controls.check_window(a, b, self.policy())["passed"])
        self.assertTrue(host_controls.check_window(a, b, policy)["passed"])
        self.assertEqual(host_controls.observation_errors([a, b], policy), [])
        for aggregate, local, temperature in [(11, 20, 64), (8, 26, 64), (8, 20, 86)]:
            b["cpu"]["cpu"]["busy"] = aggregate
            b["cpu"]["cpu2"]["busy"] = local
            b["cpu_temperatures_c"] = [temperature]
            self.assertFalse(host_controls.check_window(a, b, policy)["passed"])


class ThresholdPolicyTests(unittest.TestCase):
    def plan(self):
        plan = paired.json.loads(
            Path(paired.__file__).with_name("paired.example.json").read_text()
        )
        # The validator needs existing inputs, but never executes these files.
        existing = str(Path(__file__).resolve())
        for title in plan["titles"]:
            for key in ("rom", "library", "input", "scene", "seed_data"):
                title[key] = existing
        for variant in ("original", "current"):
            for key in ("executable", "build_metadata"):
                plan[variant][key] = existing
        plan["host_policy"]["cpus"] = [min(paired.os.sched_getaffinity(0))]
        return plan

    def test_revised_example_and_explicit_policy_are_not_capped_at_ten_percent(self):
        plan = self.plan()
        self.assertEqual(plan["max_other_cpu_fraction"], 0.15)
        self.assertEqual(plan["aa_max_span_fraction"], 0.10)
        paired.validate(plan)
        plan.update(max_other_cpu_fraction=0.20, aa_max_span_fraction=0.12)
        paired.validate(plan)

    def test_acceptance_fractions_must_be_finite_positive_numbers(self):
        for key in ("max_other_cpu_fraction", "aa_max_span_fraction"):
            for value in (0, -0.1, 1.01, float("nan"), float("inf"), True, "0.1"):
                with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                    plan = self.plan()
                    plan[key] = value
                    paired.validate(plan)


class PairScheduleTests(unittest.TestCase):
    def plan(self):
        return {
            "order_seed": 7,
            "pairs": 4,
            "aa_repeats": 3,
            "titles": [{"id": "a"}, {"id": "b"}],
            "lanes": [
                {"id": "cold", "cache": "fresh"},
                {"id": "warm", "cache": "warm"},
            ],
        }

    def test_schedule_is_frozen_balanced_and_cache_seeds_are_per_variant(self):
        p = self.plan()
        rows = paired.schedule(p)
        self.assertEqual(rows, paired.schedule(p))
        self.assertEqual(len([r for r in rows if r["kind"] == "warmup"]), 4)
        self.assertEqual(len([r for r in rows if r["kind"] == "AA"]), 12)
        for title in ("a", "b"):
            for lane in ("cold", "warm"):
                blocks = [
                    [
                        r["variant"]
                        for r in rows
                        if r["kind"] == "AB"
                        and r["title"] == title
                        and r["lane"] == lane
                        and r["pair"] == pair
                    ]
                    for pair in range(1, 5)
                ]
                self.assertEqual(blocks.count(["original", "current"]), 2)
                self.assertEqual(blocks.count(["current", "original"]), 2)
        self.assertEqual(len({r["name"] for r in rows}), len(rows))

    def test_output_cannot_mutate_an_immutable_seed_or_title_profile(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            seed = root / "seed"
            seed.mkdir()
            profiles = root / "titles"
            profiles.mkdir()
            plan = {
                "titles_directory": str(profiles),
                "titles": [{"seed_data": str(seed)}],
            }
            for parent in [seed, profiles]:
                with self.assertRaises(ValueError):
                    paired.validate_output(plan, parent / "campaign")
            paired.validate_output(plan, root / "separate")
            self.assertEqual(list(seed.iterdir()), [])

    def test_effect_resamples_pairs_and_small_budget_does_not_claim_precision(self):
        data = paired.effect([10, 20, 30], [9, 18, 27])
        self.assertAlmostEqual(data["paired_ratio_geomean"], 0.9)
        self.assertIsNone(data["confidence_interval"])
        larger = paired.effect([10] * 5, [9] * 5)
        for bound in larger["exploratory_pair_bootstrap_95pct"]:
            self.assertAlmostEqual(bound, 0.9)


class BaselinePreparationTests(unittest.TestCase):
    def test_only_known_uninstrumented_anchors_are_accepted(self):
        import prepare_baseline

        old = "\n".join(
            [
                "        let began = Instant::now();",
                "        match system.run_frame() {",
                "        if let Some(request) = system.keyboard_request() {",
                "        if let Some(limit) = slow {",
                "            let (read_us, read_bytes) = zakuro_core::services::fs::rom_reads();",
            ]
        )
        changed = prepare_baseline.instrument_frontend(old)
        self.assertIn("probe.begin_batch()", changed)
        self.assertIn("probe.checkpoint(&system, frame_number)", changed)
        self.assertIn("probe.finish_batch()", changed)
        with self.assertRaises(ValueError):
            prepare_baseline.instrument_frontend(changed)
        with self.assertRaises(ValueError):
            prepare_baseline.instrument_frontend(
                old + "\n        match system.run_frame() {"
            )


class PairedAnalysisTests(unittest.TestCase):
    def fixture(self, directory):
        import test_evaluate

        plan = {
            "pairs": 3,
            "aa_repeats": 3,
            "aa_max_span_fraction": 0.05,
            "max_other_cpu_fraction": 0.10,
            "titles": [{"id": "synthetic"}],
            "lanes": [{"id": "cold", "cache": "fresh", "role": "equal-work"}],
        }
        rows = []
        descriptors = [("AA", "current", i) for i in range(1, 4)]
        descriptors += [
            ("AB", v, i) for i in range(1, 4) for v in ("original", "current")
        ]
        for index, (kind, variant, pair) in enumerate(descriptors):
            m = test_evaluate.fixture()
            m["trials"] = m["trials"][:1]
            m["repeats"] = 1
            m["experiment"]["probe_observer"] = {
                "batch_telemetry": True,
                "capture": "legacy",
                "measure_start": 0,
                "measure_end": 8,
            }
            m["trials"][0]["environment"]["ZAKURO_PROBE_TELEMETRY"] = "1"
            m["trials"][0]["batch_telemetry"] = {
                "source": {"sha256": "c" * 64},
                "summary": {
                    "schema_version": 1,
                    "rows": 8,
                    "measurement": {
                        "start_batch_inclusive": 0,
                        "end_batch_exclusive": 8,
                        "wall_seconds": 1.0,
                        "process_cpu_seconds": 0.5,
                        "guest_work_delta": {"recompiled_instructions": 1000},
                    },
                },
            }
            manifest = directory / f"{index}.json"
            paired.write(manifest, m)
            rows.append(
                {
                    "name": str(index),
                    "title": "synthetic",
                    "lane": "cold",
                    "kind": kind,
                    "pair": pair,
                    "variant": variant,
                    "status": "complete",
                    "manifest": str(manifest),
                    "manifest_sha256": paired.evaluate.digest(manifest),
                    "preflight_passed": True,
                    "host_observation_errors": [],
                }
            )
        return plan, rows

    def analyze(self, directory, plan, rows, changes=()):
        return paired.analyze(directory, plan, rows, changes)["series"][
            "synthetic/cold"
        ]

    def mutate(self, row, change, update_hash=True):
        manifest = Path(row["manifest"])
        data = paired.json.loads(manifest.read_text())
        change(data)
        paired.write(manifest, data)
        if update_hash:
            row["manifest_sha256"] = paired.evaluate.digest(manifest)

    def test_admission_requires_equal_work_not_a_universal_speedup(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            plan, rows = self.fixture(directory)
            result = self.analyze(directory, plan, rows)
            self.assertEqual(result["status"], "admitted")
            self.assertFalse(result["comparison"]["target_met"])

    def test_raw_manifest_and_campaign_input_drift_block_admission(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            plan, rows = self.fixture(directory)
            self.assertEqual(
                self.analyze(directory, plan, rows, ["changed-source"])["status"],
                "held",
            )
            self.mutate(
                rows[-1], lambda m: m.update(unexpected_edit=True), update_hash=False
            )
            result = self.analyze(directory, plan, rows)
            self.assertEqual(result["status"], "held")
            self.assertTrue(any("raw manifest changed" in e for e in result["errors"]))

    def test_aa_renderer_changes_cannot_hide_behind_valid_true(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            plan, rows = self.fixture(directory)

            def change(m):
                m["trials"][0]["renderer_identity"] = ["other synthetic renderer"]
                m["trials"][0]["renderer_log_lines"] = [
                    "drawing with OpenGL ES on other synthetic renderer"
                ]

            self.mutate(rows[0], change)
            result = self.analyze(directory, plan, rows)
            self.assertFalse(result["AA_pass"])
            self.assertEqual(result["status"], "held")

    def test_named_window_work_must_match_even_if_final_work_matches(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            plan, rows = self.fixture(directory)

            def change(m):
                m["trials"][0]["batch_telemetry"]["summary"]["measurement"][
                    "guest_work_delta"
                ]["recompiled_instructions"] += 1

            self.mutate(rows[-1], change)
            result = self.analyze(directory, plan, rows)
            self.assertEqual(result["status"], "held")
            self.assertIn("measurement-window guest work differs", result["errors"])

    def test_diagnostic_override_does_not_waive_preflight(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            plan, rows = self.fixture(directory)
            rows[-1]["preflight_passed"] = False
            self.assertEqual(self.analyze(directory, plan, rows)["status"], "held")

    def test_relaxed_budget_admits_moderate_activity_but_not_failed_preflight(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            plan, rows = self.fixture(directory)
            plan.update(max_other_cpu_fraction=0.15, aa_max_span_fraction=0.10)

            def change(m):
                m["experiment"]["max_other_cpu_fraction"] = 0.15
                m["trials"][0]["host_activity"]["other_cpu_fraction"] = 0.12

            for row in rows:
                self.mutate(row, change)
            result = self.analyze(directory, plan, rows)
            self.assertEqual(result["status"], "admitted")
            self.assertFalse(result["strict_3pct_pass"])
            rows[-1]["preflight_passed"] = False
            self.assertEqual(self.analyze(directory, plan, rows)["status"], "held")

    def test_ten_percent_aa_budget_still_rejects_larger_spread(self):
        with tempfile.TemporaryDirectory() as folder:
            directory = Path(folder)
            plan, rows = self.fixture(directory)

            def change(m):
                m["trials"][0]["batch_telemetry"]["summary"]["measurement"][
                    "wall_seconds"
                ] = 1.08

            self.mutate(rows[0], change)
            self.assertFalse(self.analyze(directory, plan, rows)["AA_pass"])
            plan["aa_max_span_fraction"] = 0.10
            self.assertEqual(self.analyze(directory, plan, rows)["status"], "admitted")
            self.mutate(
                rows[0],
                lambda m: m["trials"][0]["batch_telemetry"]["summary"][
                    "measurement"
                ].update(wall_seconds=1.20),
            )
            self.assertFalse(self.analyze(directory, plan, rows)["AA_pass"])


if __name__ == "__main__":
    unittest.main()
