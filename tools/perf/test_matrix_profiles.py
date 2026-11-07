"""Synthetic prospective-plan tests; no runtime, game or device evidence."""

import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import matrix_profiles as matrix


def identities(names, value="a"):
    return {name: value * 64 for name in names.split()}


def specification():
    runtimes = {}
    for variant, value in (("original", "a"), ("current", "b")):
        runtimes[variant] = {
            "identities": identities("executable build_receipt compiler", value),
            "capability": {"schema_version": 1, "runtime_sha256": value * 64,
                           "internal_scales": [1.0, 1.5, 2.0],
                           "aspects": ["Original", "FourThree"],
                           "layouts": ["Original", "TopOnly", "SideBySide"],
                           "device_audio": True, "effective_settings_reporting": True,
                           "observed_internal_scale_reporting": True, "presentation_capture_reporting": True}}
    profile = json.loads((Path(__file__).parent / "profiles/common-serialized.json").read_text())
    scenes = [
        {"id": "route", "phase": "warmed-gameplay", "available": True, "cache": "warm",
         "frames": 6000, "measure_start": 1800,
         "identities": identities("seed_data input scene_review clock original_cache_seed current_cache_seed")},
        {"id": "boot", "phase": "cold-start", "available": True, "cache": "fresh",
         "frames": 6000, "measure_start": 0,
         "identities": identities("seed_data input scene_review clock original_cache_seed current_cache_seed", "c")},
        {"id": "transition", "phase": "transition", "available": False, "cache": "fresh",
         "frames": None, "measure_start": None, "identities": None}]
    return {"schema_version": 1, "comparison_factor": "runtime", "order_seed": 42,
            "pairs": 3, "aa_repeats": 3, "max_cells": 128, "coverage": "screen",
            "implementation_profile": profile,
            "host_policy": {"max_busy_fraction": 0.1, "max_other_cpu_fraction": 0.15,
                            "aa_max_span_fraction": 0.1},
            "layout_size": [1280, 720], "renderer_environment": {"EGL_PLATFORM": "x11"},
            "runtimes": runtimes,
            "titles": [{"id": title,
                        "identities": identities("rom title_profile original_library current_library original_codegen current_codegen"),
                        "scenes": copy.deepcopy(scenes)} for title in sorted(matrix.TITLES)]}


class MatrixTests(unittest.TestCase):
    def test_scale_aspect_are_independent_and_layout_scope_is_explicit(self):
        spec = specification()
        result = matrix.build_matrix(spec)
        warm = [c for c in result["cells"] if c["binding"]["scene"]["phase"] == "warmed-gameplay"]
        self.assertEqual(len(warm), 24)
        top = [c for c in warm if c["binding"]["settings"]["layout"] == "Original"]
        self.assertEqual({(c["binding"]["settings"]["internal_scale"], c["binding"]["settings"]["aspect"]) for c in top},
                         {(s, a) for s in matrix.SCALES for a in matrix.ASPECTS})
        self.assertEqual(len(result["cells"]), 27)
        self.assertEqual(len(result["unavailable_scenes"]), 3)
        self.assertEqual(result["planned_processes_excluding_warmups"], 243)
        self.assertFalse(result["timing_admitted"])
        self.assertTrue(all(not c["runnable"] for c in result["cells"]))
        self.assertTrue(all(c["binding"]["settings"]["audio"] == "device-on" for c in result["cells"]))
        self.assertEqual(spec, specification())

    def test_unsupported_geometry_probe_audio_and_reporting_fail_closed(self):
        spec = specification()
        for runtime in spec["runtimes"].values():
            runtime["capability"].update(internal_scales=[1.0], device_audio=False,
                                          effective_settings_reporting=False)
        result = matrix.build_matrix(spec)
        self.assertTrue(all(c["status"] == "blocked" for c in result["cells"]))
        upscale = next(c for c in result["cells"] if c["binding"]["settings"]["internal_scale"] == 1.5)
        self.assertIn("original: unsupported internal_scales=1.5", upscale["blocked_reasons"])
        self.assertIn("current: missing device_audio", upscale["blocked_reasons"])

    def test_runtime_factor_cannot_hide_a_changed_game_library(self):
        spec = specification()
        spec["titles"][0]["identities"]["current_library"] = "d" * 64
        with self.assertRaisesRegex(ValueError, "same native library"):
            matrix.build_matrix(spec)
        spec["comparison_factor"] = "native-library"
        with self.assertRaisesRegex(ValueError, "same runtime"):
            matrix.build_matrix(spec)
        spec["runtimes"]["current"] = copy.deepcopy(spec["runtimes"]["original"])
        self.assertEqual(len(matrix.build_matrix(spec)["cells"]), 27)

    def test_hash_and_order_are_deterministic_and_bind_all_settings(self):
        spec = specification()
        result = matrix.build_matrix(spec)
        self.assertEqual(result, matrix.build_matrix(copy.deepcopy(spec)))
        for cell in result["cells"]:
            self.assertEqual(cell["binding_sha256"], matrix.digest(cell["binding"]))
            self.assertEqual(cell["proposed_order"]["aa"], ["current"] * 3)
            self.assertEqual(cell["proposed_order"]["pairs"][0], cell["proposed_order"]["pairs"][2])
            self.assertEqual(cell["proposed_order"]["pairs"][0], list(reversed(cell["proposed_order"]["pairs"][1])))
        spec["host_policy"]["max_busy_fraction"] = 0.09
        changed = matrix.build_matrix(spec)
        self.assertNotEqual(result["specification_sha256"], changed["specification_sha256"])
        self.assertNotEqual(result["cells"][0]["binding_sha256"], changed["cells"][0]["binding_sha256"])

    def test_host_policy_requires_complete_bound_source_fields(self):
        spec = specification()
        result = matrix.build_matrix(spec)
        self.assertTrue(all(c["status"] == "blocked" for c in result["cells"]))
        self.assertTrue(all("complete frozen evaluator policy missing" in c["blocked_reasons"] for c in result["cells"]))
        example = json.loads((Path(__file__).parent / "paired.example.json").read_text())
        complete = {**example["host_policy"], "max_other_cpu_fraction": example["max_other_cpu_fraction"],
                    "aa_max_span_fraction": example["aa_max_span_fraction"]}
        spec["host_policy"] = complete
        result = matrix.build_matrix(spec)
        self.assertTrue(all(c["status"] == "needs-execution-adapter" for c in result["cells"]))
        self.assertTrue(all(not c["runnable"] for c in result["cells"]))
        self.assertFalse(result["timing_admitted"])
        self.assertEqual(result["cells"][0]["binding"]["host_policy"], complete)
        for name, value in (("max_busy_fraction", .11), ("max_core_fraction", .26),
                            ("max_cpu_c", 86), ("max_temperature_span_c", 6),
                            ("max_other_cpu_fraction", .16), ("aa_max_span_fraction", .11),
                            ("cpus", []), ("cpus", [True]), ("siblings", complete["cpus"]),
                            ("window_seconds", 11), ("timeout_seconds", 61),
                            ("consecutive_windows", True)):
            bad = copy.deepcopy(complete)
            bad[name] = value
            with self.subTest(name=name), self.assertRaises(ValueError):
                matrix.validate_host_policy(bad)
        for policy in ({"anything": True}, {}, {**complete, "extra": 1},
                       {k: v for k, v in complete.items() if k != "max_core_fraction"}):
            with self.subTest(policy=policy), self.assertRaises(ValueError):
                matrix.validate_host_policy(policy)

    def test_unavailable_transitions_do_not_manufacture_workload_evidence(self):
        spec = specification()
        spec["titles"][0]["scenes"][2]["frames"] = 6000
        with self.assertRaisesRegex(ValueError, "unavailable"):
            matrix.build_matrix(spec)

    def test_cold_start_and_warmed_cache_contracts_cannot_be_swapped(self):
        for index, field, value in ((0, "cache", "fresh"), (1, "measure_start", 1800), (1, "cache", "warm")):
            spec = specification()
            spec["titles"][0]["scenes"][index][field] = value
            with self.subTest(index=index, field=field), self.assertRaises(ValueError):
                matrix.build_matrix(spec)

    def test_capability_bound_to_binary_and_exact_closed_schema(self):
        for mutation in ("binary", "unknown", "scale-bool", "revision", "missing-hash", "pairs-bool", "nan"):
            spec = specification()
            cap = spec["runtimes"]["current"]["capability"]
            if mutation == "binary":
                cap["runtime_sha256"] = "e" * 64
            elif mutation == "unknown":
                spec["ignore_fields"] = ["resolution"]
            elif mutation == "scale-bool":
                cap["internal_scales"] = [True]
            elif mutation == "revision":
                spec["runtimes"]["current"]["identities"]["executable"] = "88ce1a3"
            elif mutation == "missing-hash":
                del spec["titles"][0]["scenes"][0]["identities"]["input"]
            elif mutation == "pairs-bool":
                spec["pairs"] = True
            else:
                spec["host_policy"]["max_busy_fraction"] = float("nan")
            with self.subTest(mutation=mutation), self.assertRaises(ValueError):
                matrix.build_matrix(spec)

    def test_full_interactions_require_explicit_budget(self):
        spec = specification()
        spec["coverage"] = "full"
        self.assertEqual(len(matrix.build_matrix(spec)["cells"]), 108)
        spec["max_cells"] = 100
        with self.assertRaisesRegex(ValueError, "max_cells"):
            matrix.build_matrix(spec)

    def test_cli_refuses_to_overwrite_a_frozen_plan(self):
        with tempfile.TemporaryDirectory() as folder:
            source, output = Path(folder) / "spec.json", Path(folder) / "matrix.json"
            source.write_text(json.dumps(specification()))
            command = [sys.executable, str(Path(matrix.__file__)), "--specification", str(source), "--output", str(output)]
            first = subprocess.run(command, capture_output=True, text=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            frozen = output.read_bytes()
            second = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(second.returncode, 0)
            self.assertEqual(output.read_bytes(), frozen)


if __name__ == "__main__":
    unittest.main()
