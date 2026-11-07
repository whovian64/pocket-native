"""Synthetic harness checks; these never claim real ROM or game compatibility."""

import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

MODULE = Path(__file__).with_name("evaluate.py")
SPEC = importlib.util.spec_from_file_location("evaluate", MODULE)
evaluate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(evaluate)


def fixture() -> dict:
    envelope = {"schema_version": 1, "known": True, "constraints": {"cpu.max": [], "memory.max": []}}
    screenshots = {f"frame-{i:05}.bmp": hashlib.sha256(str(i).encode()).hexdigest() for i in range(8)}
    env = {"ZAKURO_CLOCK": "4000000000000", "ZAKURO_INPUT": "0:A", "ZAKURO_RESOLUTION": "1.0",
           "ZAKURO_FRAMESKIP": "0", "ZAKURO_RECOMPILED": "/synthetic/title.so", "RUST_LOG": "info", "LC_ALL": "C", "TZ": "UTC"}
    trial = {"valid": True, "resource_envelope": envelope, "resource_envelope_after": envelope,
             "environment": env, "renderer_log_lines": ["drawing with OpenGL ES on synthetic"],
             "host_activity": {"schema_version": 1, "known": True, "other_cpu_fraction": 0.0},
             "renderer_identity": ["synthetic"], "probe": {"valid": True, "errors": [], "report_sha256": "d" * 64,
             "warnings_errors": {"section": "None.\n", "entries": [], "report_md_sha256": "f" * 64}, "screenshots": screenshots,
             "report": {"schema_version": 1, "requested_frames": 8, "attempted_frames": 8, "elapsed_seconds": .01,
                        "stop_reason": "frame_limit", "skipped_frames": 0, "screenshots": list(screenshots),
                        "top_pictures": 8, "bottom_pictures": 8, "completed_frames": 8,
                        "interpreted_instructions": 100, "recompiled_instructions": 1000}},
             "metrics": {"exit_code": 0, "timed_out": False, "memory_limit_exceeded": False,
                         "wall_seconds": 4.0, "cpu_seconds": 2.0, "peak_rss_kib": 100000,
                         "user_cpu_seconds": 1.5, "system_cpu_seconds": .5}}
    return {"schema_version": 1, "harness": {"sha256": "synthetic-harness"},
            "status": "complete", "experiment": {"frames": 8, "resource_envelope": envelope,
                                                   "clock_ms_since_1900": 4000000000000, "input": "0:A", "resolution": 1.0,
                                                   "renderer_environment": {}, "guest_trace_interval_frames": None,
                                                   "max_other_cpu_fraction": 0.10},
            "host": {"cpu": "synthetic", "meminfo": "varies"},
            "rom": {"sha256": "synthetic-rom"}, "library": {"path": "/synthetic/title.so"},
            "trials": [copy.deepcopy(trial) for _ in range(3)],
            "scene_review": {"schema_version": 1, "verified_gameplay": True,
                             "scene": "synthetic test fixture", "reviewed_by": "test",
                             "rom_sha256": "synthetic-rom", "frames": 8, "screenshots": screenshots}}


def scale_metrics(trial: dict, factor: float) -> None:
    for key in ("wall_seconds", "cpu_seconds", "peak_rss_kib", "user_cpu_seconds", "system_cpu_seconds"):
        trial["metrics"][key] *= factor


class ResolutionTests(unittest.TestCase):
    def test_only_actual_quarter_step_requests_are_labeled(self):
        for value in (1, 1.25, 1.5, 1.75, 2, 3.5, 4):
            evaluate.validate_resolution(value)
        for value in (0.5, 1.3, 4.25, float("nan"), float("inf"), True, "1.5"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                evaluate.validate_resolution(value)

    def test_matching_mislabeled_receipts_still_cannot_compare(self):
        baseline, candidate = fixture(), fixture()
        for manifest in (baseline, candidate):
            manifest["experiment"]["resolution"] = 1.3
            for trial in manifest["trials"]:
                trial["environment"]["ZAKURO_RESOLUTION"] = "1.3"
        result = evaluate.compare_values(baseline, candidate)
        self.assertEqual(result["status"], "incomparable")
        self.assertTrue(any("quarter step" in error for error in result["errors"]))


class DiagnosticComparisonTests(unittest.TestCase):
    def test_identical_diagnostics_cannot_become_throughput_wins(self):
        for marker in ("profile", "packet_artifact", "audio_report", "diagnostic_receipt", "environment"):
            with self.subTest(marker=marker):
                baseline, candidate = fixture(), fixture()
                for manifest in (baseline, candidate):
                    if marker == "profile":
                        manifest["diagnostic_profile"] = {"declaration": {"schema_version": 1}}
                    for trial in manifest["trials"]:
                        if marker == "packet_artifact":
                            trial["probe"]["diagnostic_artifacts"] = ["packet-census.toml"]
                        elif marker == "audio_report":
                            trial["probe"]["report"]["audio"] = {"requested": "paced"}
                        elif marker == "diagnostic_receipt":
                            trial["diagnostics"] = {"valid": True, "timing_eligible": False}
                        elif marker == "environment":
                            trial["environment"]["ZAKURO_PROBE_AUDIO"] = "off"
                for trial in candidate["trials"]:
                    scale_metrics(trial, .1)
                result = evaluate.compare_values(baseline, candidate)
                self.assertEqual(result["status"], "incomparable")
                self.assertFalse(result["target_met"])
                self.assertTrue(any("NON-TIMING" in error for error in result["errors"]))

    def test_no_profile_or_artifact_preserves_ordinary_comparison(self):
        baseline, candidate = fixture(), fixture()
        for trial in candidate["trials"]:
            scale_metrics(trial, .49)
        self.assertEqual(evaluate.diagnostic_rules.timing_reasons(baseline), [])
        self.assertTrue(evaluate.compare_values(baseline, candidate)["target_met"])


def render_profile(scale=1.5):
    return evaluate.render_rules.validate({"schema_version": 1, "id": "scale-cell", "internal_scale": scale,
        "layout": "BigTop", "aspect": "Original", "swapped": False, "stretched": "Off",
        "presentation_capture": None})


def with_render_receipts(manifest, profile):
    manifest["render_profile"] = {"declaration": profile,
        "declaration_sha256": evaluate.render_rules.declaration_hash(profile)}
    manifest["harness"]["render_profiles_sha256"] = evaluate.digest(Path(evaluate.render_rules.__file__))
    manifest["experiment"]["render_profile"] = profile
    manifest["experiment"]["resolution"] = profile["internal_scale"]
    for trial in manifest["trials"]:
        trial["environment"].update(ZAKURO_RESOLUTION=str(profile["internal_scale"]),
            ZAKURO_DATA="/synthetic/trial/data", POCKET_NATIVE_CONFIG="/synthetic/trial/config")
        trial["renderer_log_lines"].append(f"drawing on synthetic at {profile['internal_scale']:g}x through OpenGL ES")
        trial["render_settings"] = None
        trial["render_observation"] = evaluate.render_rules.observe(profile,
            renderer_log_lines=trial["renderer_log_lines"], trial=Path("/synthetic/trial"))
    return manifest


class RenderReceiptTests(unittest.TestCase):
    def test_scale_only_receipt_can_compare_when_actual_scale_is_proven(self):
        baseline = with_render_receipts(fixture(), render_profile())
        candidate = copy.deepcopy(baseline)
        for trial in candidate["trials"]:
            scale_metrics(trial, .49)
        result = evaluate.compare_values(baseline, candidate)
        self.assertEqual(result["errors"], [])
        self.assertTrue(result["target_met"])

    def test_admitted_flag_cannot_override_missing_or_conflicting_scale_evidence(self):
        for mutation in ("missing-log", "wrong-log", "hash", "declaration", "settings", "env", "observation"):
            with self.subTest(mutation=mutation):
                manifest = with_render_receipts(fixture(), render_profile())
                trial = manifest["trials"][0]
                if mutation == "missing-log": trial["renderer_log_lines"].pop()
                elif mutation == "wrong-log": trial["renderer_log_lines"][-1] = "drawing on synthetic at 1x through OpenGL ES"
                elif mutation == "hash": manifest["render_profile"]["declaration_sha256"] = "f" * 64
                elif mutation == "declaration": manifest["experiment"]["render_profile"] = render_profile(2)
                elif mutation == "settings": trial["render_settings"] = {"sha256": "f" * 64}
                elif mutation == "env": trial["environment"]["ZAKURO_PANEL_SHOT"] = "/tmp/foreign.ppm"
                else: trial["render_observation"]["scanout"] = "observed"
                self.assertTrue(evaluate.validate_trial_receipt(manifest, trial))

    def test_distinct_or_missing_profiles_and_support_cannot_compare(self):
        baseline = with_render_receipts(fixture(), render_profile())
        for candidate in (fixture(), with_render_receipts(fixture(), render_profile(2))):
            result = evaluate.compare_values(baseline, candidate)
            self.assertTrue(any("different render profile" in error for error in result["errors"]))
        candidate = copy.deepcopy(baseline)
        candidate["harness"]["render_profiles_sha256"] = "f" * 64
        self.assertTrue(any("support SHA-256" in error for error in evaluate.compare_values(baseline, candidate)["errors"]))

    def test_panel_environment_is_non_timing_even_without_declaration(self):
        baseline, candidate = fixture(), fixture()
        for manifest in (baseline, candidate):
            for trial in manifest["trials"]:
                trial["environment"]["ZAKURO_PANEL_SHOT"] = "/tmp/panel.ppm"
        result = evaluate.compare_values(baseline, candidate)
        self.assertTrue(any("NON-TIMING" in error for error in result["errors"]))
        self.assertTrue(any("undeclared render" in error for error in result["errors"]))

    def test_recorded_adapter_is_bound_to_loaded_code(self):
        baseline = with_render_receipts(fixture(), render_profile())
        # Matching historical identities do not bind the code revalidating them.
        baseline["harness"]["render_profiles_sha256"] = "f" * 64
        candidate = copy.deepcopy(baseline)
        result = evaluate.compare_values(baseline, candidate)
        self.assertEqual(result["status"], "incomparable")
        self.assertTrue(any("support SHA-256" in error for error in result["errors"]))

        baseline = with_render_receipts(fixture(), render_profile())
        with mock.patch.object(evaluate, "digest", side_effect=OSError("adapter unreadable")):
            result = evaluate.compare_values(baseline, copy.deepcopy(baseline))
        self.assertEqual(result["status"], "incomparable")
        self.assertTrue(any("adapter unreadable" in error for error in result["errors"]))


class MemoryObservationTests(unittest.TestCase):
    def test_peak_components_come_from_one_snapshot(self):
        observations = {"samples": 0, "at_sampled_peak_rss": None}
        evaluate.record_rss_sample(observations,
            "VmRSS: 100 kB\nRssAnon: 90 kB\nRssFile: 10 kB\nRssShmem: 0 kB\n", 1.0)
        evaluate.record_rss_sample(observations,
            "VmRSS: 99 kB\nRssAnon: 1 kB\nRssFile: 98 kB\nRssShmem: 0 kB\n", 2.0)
        peak = observations["at_sampled_peak_rss"]
        self.assertEqual(observations["samples"], 2)
        self.assertEqual((peak["VmRSS"], peak["RssAnon"], peak["RssFile"], peak["RssShmem"]), (100, 90, 10, 0))
        self.assertEqual(peak["elapsed_seconds"], 1.0)
        # Summing independent component maxima would invent 188 KiB here.
        evaluate.record_rss_sample(observations, "VmRSS: 1000 kB\n", 3.0)
        self.assertEqual(observations["samples"], 2)
        self.assertEqual(observations["at_sampled_peak_rss"], peak)

    def test_live_child_exposes_memory_and_fault_observations(self):
        with tempfile.TemporaryDirectory() as folder:
            metrics = evaluate.measure([sys.executable, "-c",
                "import time; data=bytearray(8*1024*1024); data[0]=1; time.sleep(0.12)"],
                Path(folder), dict(os.environ), 10, [], None)
        self.assertEqual(metrics["exit_code"], 0)
        self.assertGreater(metrics["rss_components"]["samples"], 0)
        self.assertGreater(metrics["rss_components"]["at_sampled_peak_rss"]["RssAnon"], 0)
        self.assertGreater(metrics["minor_page_faults"], 0)


class ComparisonTests(unittest.TestCase):
    def test_valid_true_cannot_override_execution_probe_or_environment_facts(self):
        mutations = ("timeout", "exit", "memory", "missing-status", "bool-metric", "nan", "infinity", "cpu-total", "bool-schema",
                     "schema", "requested", "attempted", "stop", "skips", "probe-errors", "missing-warnings",
                     "warning-change", "image-hash", "frameskip-env", "resolution-env", "clock-env", "input-env",
                     "renderer-env", "extra-env")
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                a, b = fixture(), fixture()
                trial = b["trials"][1]
                metrics, report, env = trial["metrics"], trial["probe"]["report"], trial["environment"]
                if mutation == "timeout": metrics["timed_out"] = True
                elif mutation == "exit": metrics["exit_code"] = 1
                elif mutation == "memory": metrics["memory_limit_exceeded"] = True
                elif mutation == "missing-status": del metrics["timed_out"]
                elif mutation == "bool-metric": metrics["cpu_seconds"] = True
                elif mutation == "nan": metrics["wall_seconds"] = float("nan")
                elif mutation == "infinity": metrics["cpu_seconds"] = float("inf")
                elif mutation == "bool-schema": b["schema_version"] = True
                elif mutation == "cpu-total": metrics["cpu_seconds"] += 1
                elif mutation == "schema": report["schema_version"] = 2
                elif mutation == "requested": report["requested_frames"] -= 1
                elif mutation == "attempted": report["attempted_frames"] -= 1
                elif mutation == "stop": report["stop_reason"] = "fault"
                elif mutation == "skips": report["skipped_frames"] = 1
                elif mutation == "probe-errors": trial["probe"]["errors"] = ["failure"]
                elif mutation == "missing-warnings": del trial["probe"]["warnings_errors"]
                elif mutation == "warning-change":
                    section = "- 1 × (from frame 0) WARN new warning\n"
                    trial["probe"]["warnings_errors"].update(section=section, entries=evaluate.warning_receipt(section, 8))
                elif mutation == "image-hash": trial["probe"]["screenshots"]["frame-00000.bmp"] = "not-a-hash"
                elif mutation == "frameskip-env": env["ZAKURO_FRAMESKIP"] = "1"
                elif mutation == "resolution-env": env["ZAKURO_RESOLUTION"] = "0.5"
                elif mutation == "clock-env": env["ZAKURO_CLOCK"] = "0"
                elif mutation == "input-env": env["ZAKURO_INPUT"] = "0:B"
                elif mutation == "renderer-env": env["LIBGL_ALWAYS_SOFTWARE"] = "1"
                else: env["ZAKURO_SPEED"] = "100"
                self.assertTrue(trial["valid"])
                self.assertEqual(evaluate.compare_values(a, b)["status"], "incomparable")

    def test_matching_normal_warnings_are_preserved_and_short_cadence_matches_probe(self):
        a, b = fixture(), fixture()
        section = "- 2 × (from frame 0) WARN known service limitation\n"
        for manifest in (a, b):
            for trial in manifest["trials"]:
                trial["probe"]["warnings_errors"].update(section=section, entries=evaluate.warning_receipt(section, 8))
        self.assertEqual(evaluate.compare_values(a, b)["status"], "comparable")
        self.assertEqual(evaluate.expected_screenshots(1), {"frame-00000.bmp"})
        self.assertEqual(evaluate.expected_screenshots(7), {f"frame-{i:05}.bmp" for i in range(7)})

    def test_matching_pixels_cannot_hide_a_different_guest_instruction_workload(self):
        for key in ("interpreted_instructions", "recompiled_instructions"):
            for change in ("different", "missing"):
                with self.subTest(key=key, change=change):
                    baseline, candidate = fixture(), fixture()
                    for trial in candidate["trials"]:
                        scale_metrics(trial, .49)
                    report = candidate["trials"][1]["probe"]["report"]
                    if change == "different":
                        report[key] += 1
                    else:
                        del report[key]
                    result = evaluate.compare_values(baseline, candidate)
                    self.assertEqual(result["status"], "incomparable")
                    self.assertFalse(result["target_met"])

    def test_host_busy_missing_unknown_and_different_budgets_are_incomparable(self):
        for mutation in ("busy", "missing", "unknown", "budget"):
            with self.subTest(mutation=mutation):
                baseline, candidate = fixture(), fixture()
                if mutation == "busy":
                    candidate["trials"][1]["host_activity"]["other_cpu_fraction"] = 0.11
                elif mutation == "missing":
                    del candidate["trials"][0]["host_activity"]
                elif mutation == "unknown":
                    candidate["trials"][0]["host_activity"]["known"] = False
                else:
                    candidate["experiment"]["max_other_cpu_fraction"] = 0.05
                result = evaluate.compare_values(baseline, candidate)
                self.assertEqual(result["status"], "incomparable")
                self.assertFalse(result["target_met"])

    def test_cgroup_pressure_harness_and_schema_confounds_are_rejected(self):
        baseline = fixture()
        for mutation in ("pressure", "unknown", "missing", "harness", "schema"):
            with self.subTest(mutation=mutation):
                candidate = fixture()
                if mutation == "pressure":
                    candidate["experiment"]["resource_envelope"]["constraints"] = {
                        "cpu.max": ["400000 100000"], "memory.max": ["805306368"]}
                elif mutation == "unknown":
                    candidate["experiment"]["resource_envelope"]["known"] = False
                elif mutation == "missing":
                    del candidate["experiment"]["resource_envelope"]
                elif mutation == "harness":
                    candidate["harness"]["sha256"] = "different-harness"
                else:
                    candidate["schema_version"] = 2
                self.assertEqual(evaluate.compare_values(baseline, candidate)["status"], "incomparable")

    def test_requires_speed_cpu_and_memory_halving_with_noise_margin(self):
        baseline = fixture()
        candidate = fixture()
        for trial in candidate["trials"]:
            scale_metrics(trial, .49)
        result = evaluate.compare_values(baseline, candidate)
        self.assertTrue(result["target_met"])
        candidate["trials"][2]["metrics"]["peak_rss_kib"] = 51000
        self.assertFalse(evaluate.compare_values(baseline, candidate)["target_met"])

    def test_changed_output_early_exit_rom_or_host_never_passes(self):
        baseline = fixture()
        for mutation in ("pixels", "frames", "rom", "host", "invalid", "one_trial"):
            with self.subTest(mutation=mutation):
                candidate = fixture()
                if mutation == "pixels":
                    candidate["trials"][1]["probe"]["screenshots"]["frame-00000.bmp"] = "changed"
                elif mutation == "frames":
                    candidate["trials"][0]["probe"]["report"]["completed_frames"] = 7
                elif mutation == "rom":
                    candidate["rom"]["sha256"] = "other"
                elif mutation == "host":
                    candidate["host"]["cpu"] = "different"
                elif mutation == "invalid":
                    candidate["trials"][0]["valid"] = False
                else:
                    candidate["trials"] = candidate["trials"][:1]
                self.assertEqual(evaluate.compare_values(baseline, candidate)["status"], "incomparable")

    def test_scene_review_is_bound_to_rom_frames_and_pixels(self):
        baseline, candidate = fixture(), fixture()
        for trial in candidate["trials"]:
            scale_metrics(trial, .1)
        candidate["scene_review"]["rom_sha256"] = "wrong"
        result = evaluate.compare_values(baseline, candidate)
        self.assertTrue(result["measured_workload_target_met"])
        self.assertFalse(result["target_met"])


class CgroupEnvelopeTests(unittest.TestCase):
    def hierarchy(self, root: Path, unit: str) -> tuple[Path, Path]:
        root.mkdir()
        (root / "cgroup.controllers").write_text("cpu memory cpuset")
        (root / "cpuset.cpus.effective").write_text("0-3")
        (root / "cpuset.mems.effective").write_text("0")
        ancestor = root / "shared.slice"
        leaf = ancestor / unit
        leaf.mkdir(parents=True)
        for node in (ancestor, leaf):
            # cpuset absent at the leaf means inherited, not an unreadable enabled file.
            controllers = "cpu memory cpuset" if node == ancestor else "cpu memory"
            (node / "cgroup.controllers").write_text(controllers)
            for name, default in evaluate.CGROUP_DEFAULTS.items():
                if name.split(".", 1)[0] not in controllers.split():
                    continue
                value = {"cpu.max": "max 100000", "cpuset.cpus.effective": "0,1,2,3",
                         "cpuset.mems.effective": "0"}.get(name, default)
                (node / name).write_text(value)
        (ancestor / "memory.max").write_text("805306368")
        (ancestor / "cpu.max").write_text("400000 100000")
        proc = root.parent / f"{unit}.proc-cgroup"
        proc.write_text(f"0::/shared.slice/{unit}\n")
        return proc, leaf

    def test_ancestral_limits_survive_normalization_without_unit_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            proc_a, _ = self.hierarchy(base / "a", "run-a.scope")
            proc_b, _ = self.hierarchy(base / "b", "run-b.scope")
            left = evaluate.cgroup_envelope(proc_a, base / "a")
            right = evaluate.cgroup_envelope(proc_b, base / "b")
            self.assertTrue(left["comparable"]["known"], left["errors"])
            self.assertEqual(left["comparable"], right["comparable"])
            self.assertNotEqual(left["ancestry"], right["ancestry"])
            self.assertEqual(left["comparable"]["constraints"]["memory.max"], ["805306368"])
            self.assertEqual(left["comparable"]["constraints"]["cpu.max"], ["400000 100000"])
            self.assertEqual(left["comparable"]["constraints"]["cpuset.cpus.effective"], ["0,1,2,3"])

    def test_missing_enabled_limit_is_unknown(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "hierarchy"
            proc, leaf = self.hierarchy(root, "run.scope")
            (leaf / "memory.max").unlink()
            result = evaluate.cgroup_envelope(proc, root)
            self.assertFalse(result["comparable"]["known"])
            self.assertTrue(any("memory.max" in error for error in result["errors"]))


class HostActivityTests(unittest.TestCase):
    def snapshot(self) -> dict:
        result = evaluate.parse_cpu_stat("cpu 100 20 30 400 50 6 7 8 9 10\n" +
                                         "".join(f"cpu{cpu} 1 1 1 1 1 1 1 1\n" for cpu in range(4)))
        result.update(known=True, user_hz=100, governors={"policy0": "performance"})
        return result

    def test_cpu_ticks_exclude_idle_iowait_and_double_counted_guest(self):
        result = self.snapshot()
        self.assertEqual(result["busy_ticks"], 171)
        self.assertEqual(result["online_cpus"], 4)
        self.assertEqual(result["counters_ticks"]["guest"], 9)
        for text in ("", "cpu 1 2\ncpu0 1", "cpu 1 2 3 -1 5 6 7 8\ncpu0 1",
                     "cpu 1 2 3 4 5 6 7 8"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                evaluate.parse_cpu_stat(text)

    def test_external_cpu_delta_subtracts_child_and_normalizes_host_capacity(self):
        before = self.snapshot()
        after = copy.deepcopy(before)
        after["counters_ticks"]["user"] += 200
        after["counters_ticks"]["guest"] += 100  # Already included in user.
        after["busy_ticks"] += 200
        result = evaluate.host_activity_delta(before, after, 2.0, 0.5)
        self.assertTrue(result["known"])
        self.assertEqual(result["estimated_other_cpu_seconds"], 1.5)
        self.assertEqual(result["other_cpu_fraction"], 0.1875)
        # Sub-tick child/accounting disagreement is retained as zero residual.
        after["counters_ticks"]["user"] = before["counters_ticks"]["user"]
        after["busy_ticks"] = before["busy_ticks"]
        quantized = evaluate.host_activity_delta(before, after, 0.1, 0.01)
        self.assertTrue(quantized["known"])
        self.assertEqual(quantized["other_cpu_fraction"], 0.0)

    def test_unknown_regressing_or_changed_accounting_is_not_valid_telemetry(self):
        for mutation in ("unknown", "cpus", "ticks", "regression", "governor"):
            with self.subTest(mutation=mutation):
                before = self.snapshot()
                after = copy.deepcopy(before)
                if mutation == "unknown":
                    after["known"] = False
                elif mutation == "cpus":
                    after["online_cpus"] = 8
                elif mutation == "ticks":
                    after["user_hz"] = 1000
                elif mutation == "regression":
                    after["counters_ticks"]["user"] -= 1
                else:
                    after["governors"]["policy0"] = "powersave"
                self.assertFalse(evaluate.host_activity_delta(before, after, 1.0, 0.0)["known"])


@unittest.skipUnless(sys.platform == "linux", "Linux measurement API")
class ExecutionTests(unittest.TestCase):
    def test_preload_mapping_proof_observes_the_exact_child_library(self):
        paths = [line.split(maxsplit=5)[5] for line in Path("/proc/self/maps").read_text().splitlines()
                 if len(line.split(maxsplit=5)) == 6 and line.split(maxsplit=5)[5].endswith("/libc.so.6")]
        if not paths:
            self.skipTest("no libc mapping available for the native loader fixture")
        library = Path(paths[0]).resolve()
        with tempfile.TemporaryDirectory() as directory:
            result = evaluate.measure([sys.executable, "-c", "import time; time.sleep(0.05)"], Path(directory),
                {**os.environ, "LD_PRELOAD": str(library)}, 3, [], None, library)
        self.assertEqual(result["exit_code"], 0)
        proof = result["preload_proof"]
        self.assertTrue(proof["observed"])
        self.assertEqual(proof["requested_path"], str(library))
        self.assertTrue(all(line.split(maxsplit=5)[5] == str(library) for line in proof["mapping_lines"]))

    def test_timeout_and_rss_watchdog_are_failures(self):
        for code, timeout, limit, flag in (("import time; time.sleep(10)", 0.05, None, "timed_out"),
                                          ("import time; x=bytearray(60*1024*1024); time.sleep(10)", 3, 35, "memory_limit_exceeded")):
            with self.subTest(flag=flag), tempfile.TemporaryDirectory() as directory:
                result = evaluate.measure([sys.executable, "-c", code], Path(directory), dict(os.environ), timeout, [], limit)
                self.assertTrue(result[flag])
                self.assertNotEqual(result["exit_code"], 0)
                self.assertGreater(result["peak_rss_kib"], 0)

    def test_cli_isolates_seed_preserves_source_and_rejects_reuse(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            game = root / "fake-game"
            game.write_text("#!" + sys.executable + "\n" + '''
import os, pathlib, struct, sys, tomllib
if pathlib.Path("data/settings.toml").exists():
    advanced = tomllib.loads(pathlib.Path("data/settings.toml").read_text())["advanced"]
    assert advanced == dict(gpu_thread=False, generic_shaders=False, generic_lighting=False,
                           stand_ins=False, separable_shaders=True, background_shaders=False)
    assert os.environ["ZAKURO_TEXTURE_CACHE_MIB"] == "16"
    assert "LD_PRELOAD" not in os.environ and "MALLOC_CONF" not in os.environ
    print("switch: GPU thread off, ZAKURO_SYNC_GPU", file=sys.stderr)
    print("switch: shader stages built apart and put together when drawn, ZAKURO_SEPARABLE_SHADERS", file=sys.stderr)
    print("switch: shaders compiled when a draw needs them, ZAKURO_SYNC_SHADERS", file=sys.stderr)
    print("decoded texture cache budget: 16 MiB (ZAKURO_TEXTURE_CACHE_MIB)", file=sys.stderr)
print("drawing with OpenGL ES on synthetic", file=sys.stderr)
print("drawing on synthetic at " + os.environ["ZAKURO_RESOLUTION"] + "x through OpenGL ES", file=sys.stderr)
p = pathlib.Path("probe-0000000000000000")
p.mkdir()
assert not pathlib.Path("data/new-save").exists()
assert pathlib.Path("data/seed").read_text() == "original"
pathlib.Path("data/new-save").write_text("local")
for n in range(8):
    bmp = bytearray(58)
    bmp[:2] = b"BM"
    struct.pack_into("<I", bmp, 2, 58)
    struct.pack_into("<I", bmp, 10, 54)
    struct.pack_into("<II", bmp, 18, 1, 1)
    (p / f"frame-{n:05}.bmp").write_bytes(bmp)
(p / "report.toml").write_text("""schema_version = 1
requested_frames = 8
attempted_frames = 8
completed_frames = 8
elapsed_seconds = 0.01
stop_reason = "frame_limit"
recompiled_instructions = 10
interpreted_instructions = 2
top_pictures = 8
bottom_pictures = 8
skipped_frames = 0
screenshots = [""" + ",".join('"frame-%05d.bmp"' % n for n in range(8)) + "]")
(p / "report.md").write_text("## Warnings and errors\\n\\nNone.\\n\\n## Threads at the end\\n")
if os.environ.get("ZAKURO_PROBE_GEOMETRY") == "1":
    nl = chr(10)
    settings_file = pathlib.Path(os.environ["POCKET_NATIVE_CONFIG"]) / "settings.toml"
    settings = tomllib.loads(settings_file.read_text()) if settings_file.exists() else {}
    aspect, layout = settings.get("aspect", "Original"), settings.get("layout", "BigTop")
    resolution = float(os.environ["ZAKURO_RESOLUTION"])
    g = ['schema_version = 1', 'unobserved = ["physical scanout"]', '[renderer]', 'kind = "gles"',
         'name = "synthetic"', 'readback_scale = 1', 'effective_quarters = ' + str(round(resolution * 4)),
         '[requested]', 'resolution = ' + str(resolution), 'layout = "' + layout + '"',
         'aspect = "' + aspect + '"', 'swapped = false', 'stretched = "Off"', 'stacked = false', 'second_window = false']
    for n in range(8):
        g += ['[[shots]]', 'frame = ' + str(n), 'file = "frame-%05d.bmp"' % n,
              '[shots.top]', 'readback = [1, 1]', '[shots.bottom]', 'readback = [1, 1]']
    if "ZAKURO_PANEL_SHOT" in os.environ:
        assert aspect == "FourThree" and layout == "BigTop"
        assert os.environ["ZAKURO_PANEL_SIZE"] == "64x64"
        pathlib.Path(os.environ["ZAKURO_PANEL_SHOT"]).write_bytes(("P6" + nl + "64 64" + nl + "255" + nl).encode() + bytes(64 * 64 * 3))
        g += ['[final_presentation]', 'sequence = 1', 'output = "offscreen"', 'panel = [64, 64]',
              'target = [64, 64]', 'layout = [640, 640]', 'layout_set = true', 'turned = false',
              'second_window = false', 'fit = [1.0, 0.0, 0.0]', 'top_crop = 0.0', 'panel_shot_status = "written"',
              'panel_shot_numbered = false', 'panel_shot_retained = false',
              '[final_presentation.top]', 'area = [0, 0, 1, 1]', '[final_presentation.bottom]', 'area = [0, 1, 1, 1]']
    (p / "render-geometry.toml").write_text(nl.join(g) + nl)
    (p / "report.toml").write_text((p / "report.toml").read_text() + nl + 'render_geometry_file = "render-geometry.toml"' + nl)
''')
            game.chmod(0o755)
            (root / "rom.3ds").write_bytes(b"synthetic, not a ROM")
            (root / "title.so").write_bytes(b"synthetic, not a library")
            seed = root / "seed"
            seed.mkdir()
            (seed / "seed").write_text("original")
            command = [sys.executable, str(MODULE), "run", "--rom", str(root / "rom.3ds"),
                       "--library", str(root / "title.so"), "--executable", str(game),
                       "--output", str(root / "result"), "--seed-data", str(seed), "--frames", "8", "--repeats", "3"]
            inherited = {**os.environ, "ZAKURO_PANEL_SHOT": "/tmp/foreign.ppm", "ZAKURO_PANEL_SIZE": "64x64"}
            result = subprocess.run(command, capture_output=True, text=True, env=inherited)
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
            manifest = json.loads((root / "result/manifest.json").read_text())
            self.assertNotIn("render_profile", manifest)
            self.assertNotIn("render_profile", manifest["experiment"])
            for trial in manifest["trials"]:
                self.assertNotIn("render_observation", trial)
                self.assertFalse(set(trial["environment"]) & evaluate.render_rules.ENV_KEYS)
                self.assertFalse((root / "result" / trial["trial"] / "config/settings.toml").exists())
            render = root / "render.json"
            render.write_text(json.dumps(render_profile()))
            rendered_command = command.copy()
            rendered_command[rendered_command.index("--output") + 1] = str(root / "render-result")
            rendered_command += ["--render-profile", str(render), "--resolution", "1.5"]
            rendered = subprocess.run(rendered_command, capture_output=True, text=True)
            self.assertEqual(rendered.returncode, 0, rendered.stderr + rendered.stdout)
            recorded = json.loads((root / "render-result/manifest.json").read_text())
            self.assertEqual(recorded["status"], "complete")
            self.assertNotIn("evidence_class", recorded)
            for trial in recorded["trials"]:
                self.assertTrue(trial["render_observation"]["admitted"])
                self.assertEqual(evaluate.validate_trial_receipt(recorded, trial), [])
                self.assertIsNone(trial["render_settings"])
            panel_profile = render_profile()
            panel_profile.update(id="aspect-cell", aspect="FourThree",
                presentation_capture={"panel_size": [64, 64], "file": "panel.ppm", "retain": "last", "max_bytes": 12301})
            panel_file = root / "panel-profile.json"
            panel_file.write_text(json.dumps(panel_profile))
            diagnostics_file = root / "geometry-profile.json"
            diagnostics_file.write_text(json.dumps({"schema_version": 1, "geometry": True}))
            panel_command = command.copy()
            panel_command[panel_command.index("--output") + 1] = str(root / "panel-result")
            panel_command += ["--render-profile", str(panel_file), "--resolution", "1.5",
                              "--diagnostic-profile", str(diagnostics_file)]
            panel_run = subprocess.run(panel_command, capture_output=True, text=True)
            self.assertEqual(panel_run.returncode, 0, panel_run.stderr + panel_run.stdout)
            recorded_panel = json.loads((root / "panel-result/manifest.json").read_text())
            self.assertEqual(recorded_panel["evidence_class"], evaluate.diagnostic_rules.NON_TIMING)
            for trial in recorded_panel["trials"]:
                self.assertEqual(evaluate.validate_trial_receipt(recorded_panel, trial), [])
                self.assertTrue(trial["render_observation"]["admitted"])
                self.assertFalse(trial["render_observation"]["matrix_presentation_capability_admitted"])
                self.assertIn('aspect = "FourThree"', (root / "panel-result" / trial["trial"] / "config/settings.toml").read_text())
                self.assertEqual(trial["render_settings"], trial["render_settings_after"])
            self.assertTrue(any("NON-TIMING" in error for error in
                                evaluate.compare_values(recorded_panel, recorded_panel)["errors"]))
            corrupt = recorded_panel["trials"][0]
            corrupt["render_settings_after"]["sha256"] = "f" * 64
            self.assertTrue(evaluate.validate_trial_receipt(recorded_panel, corrupt))
            config = root / "config-seed"
            config.mkdir()
            (config / "settings.toml").write_text('layout = "Original"\n')
            rejected_command = rendered_command + ["--seed-config", str(config)]
            rejected_command[rejected_command.index("--output") + 1] = str(root / "render-rejected")
            rejected = subprocess.run(rejected_command, capture_output=True, text=True)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("seeded configuration", rejected.stderr)
            self.assertEqual(manifest["status"], "complete")
            self.assertEqual(len(manifest["trials"]), 3)
            self.assertFalse((seed / "new-save").exists())
            reused = subprocess.run(command, capture_output=True, text=True)
            self.assertNotEqual(reused.returncode, 0)
            # A fault/shortened run can exit 0 in older probes: the independent gate rejects it.
            report = root / "result/trial-01/probe-0000000000000000/report.toml"
            report.write_text(report.read_text().replace("completed_frames = 8", "completed_frames = 7"))
            self.assertFalse(evaluate.probe_result(report.parents[1], 8)["valid"])
            report.write_text(report.read_text().replace("completed_frames = 7", "completed_frames = 8").replace("recompiled_instructions = 10", "recompiled_instructions = 0"))
            self.assertFalse(evaluate.probe_result(report.parents[1], 8)["valid"])
            report.write_text(report.read_text().replace("recompiled_instructions = 0", "recompiled_instructions = 10") + '\nguest_trace_file = "guest-trace.toml"\n')
            trace = report.parent / "guest-trace.toml"
            trace.write_text("schema_version = 1\nsampling_interval_frames = 4\ndropped_checkpoints = 0\n"
                             "[[checkpoints]]\ncompleted_frames = 4\n[[checkpoints]]\ncompleted_frames = 8\n")
            traced = evaluate.probe_result(report.parents[1], 8)
            self.assertTrue(traced["valid"], traced["errors"])
            self.assertEqual(traced["guest_trace"]["checkpoint_count"], 2)
            trace.write_text(trace.read_text().replace("dropped_checkpoints = 0", "dropped_checkpoints = 1"))
            self.assertFalse(evaluate.probe_result(report.parents[1], 8)["valid"])
            # Exercise real CLI settings/env generation, rather than only synthetic comparator receipts.
            profile = root / "profile.json"
            profile.write_text(json.dumps({"schema_version": 1, "id": "integration", "reviewed_by": "test",
                "rationale": "synthetic CLI contract", "constraints": {"gpu_thread": False,
                "generic_shaders": False, "generic_lighting": False, "stand_ins": False},
                "knobs": {"shader_linking": "separable", "background_shaders": False,
                "texture_cache_mib": 16, "gles_ring_kib": None, "rom_cache_mib": None}}))
            profiled_command = command.copy()
            profiled_command[profiled_command.index("--output") + 1] = str(root / "profile-result")
            profiled_command += ["--implementation-profile", str(profile)]
            completed = subprocess.run(profiled_command, capture_output=True, text=True)
            self.assertEqual(completed.returncode, 0, completed.stderr + completed.stdout)
            recorded = json.loads((root / "profile-result/manifest.json").read_text())
            self.assertEqual(recorded["status"], "complete")
            self.assertTrue(all(t["implementation_profile_proof"]["verified"] for t in recorded["trials"]))
            self.assertEqual(recorded["implementation_profile"]["source"]["sha256"], evaluate.digest(profile))
            self.assertFalse((seed / "settings.toml").exists())
            (seed / "settings.toml").write_text("[advanced]\nstand_ins = true\n")
            profiled_command[profiled_command.index("--output") + 1] = str(root / "conflicting-settings")
            rejected = subprocess.run(profiled_command, capture_output=True, text=True)
            self.assertNotEqual(rejected.returncode, 0)
            self.assertIn("undeclared overrides", rejected.stderr)


if __name__ == "__main__":
    unittest.main()
