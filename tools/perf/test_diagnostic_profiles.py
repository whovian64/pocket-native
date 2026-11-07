"""Synthetic diagnostic contract checks; no ROM, GPU, audio device or build."""

import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("diagnostic_profiles", Path(__file__).with_name("diagnostic_profiles.py"))
diagnostics = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(diagnostics)


def audio_fixture(mode="paced"):
    result = {"schema_version": 1, "requested": mode, "pacing": "frontend" if mode == "paced" else "unpaced",
              "mixing_requested": "async", "backend": "ALSA", "delivery": "device", "drained": True,
              "mixed_frames": 100, "received_frames": 100, "discarded_frames": 10,
              "output_frames": 90, "written_frames": 90, "device_rate": 48000,
              "device_frames_offered": 120, "device_frames_accepted": 120, "device_writes": 3,
              "partial_writes": 0, "failed_writes": 0, "write_errors": 1, "write_recoveries": 1,
              "stretch_ratio_min": 1.0, "stretch_ratio_max": 1.0,
              "queued_seconds_mean": 0.01, "queued_seconds_max": 0.02, "underruns_measured": False}
    if mode == "paced":
        result.update(paced_batches=8, late_batches=1)
    return result


def finish_fixture(**changes):
    row = audio_fixture()
    del row["drained"]
    row.update(schema_version=2, channel_finished=True, backend_finish="succeeded",
               backend_finish_operation="snd_pcm_drain", backend_finish_code=0)
    row.update(changes)
    return row


def presentation_fixture(**changes):
    row = {"sequence": 1, "output": "offscreen", "panel": [64, 64], "turned": False,
           "target": [64, 64], "layout": [640, 640], "layout_set": True, "fit": [1.0, 0.0, 0.0],
           "top_crop": 0.0, "second_window": False, "panel_shot_status": "written",
           "panel_shot_retained": True, "panel_shot_index": 0, "panel_shot_numbered": True,
           "panel_budget_status": "ready", "panel_budget_limit_bytes": "12301",
           "panel_budget_charged_bytes": "12301"}
    row.update(changes)
    return row


def packet_bytes(start=None):
    """A synthetic empty census, not a substitute for missing runtime data."""
    gpu = diagnostics.gpu_reader()
    scope = {table: dict.fromkeys(fields, 0) for table, fields in gpu.PACKET_LAYOUT.items()}
    scope.update(executed_words=0, accounted_words=0, unaccounted_words=0)
    row = {"schema": gpu.PACKET_SCHEMA, "schema_version": 1, "source": gpu.PACKET_SOURCE,
           "crate_version": "synthetic", "batches_seen": 8, "total": scope}
    if start is not None:
        row.update(window_start_batch=start, window_marked=True, window=copy.deepcopy(scope))
    lines = []

    def emit(table, prefix=()):
        if prefix:
            lines.append("[" + ".".join(json.dumps(key) for key in prefix) + "]")
        for key, value in table.items():
            if type(value) is not dict:
                lines.append(json.dumps(key) + " = " + json.dumps(value))
        for key, value in table.items():
            if type(value) is dict:
                emit(value, (*prefix, key))

    emit(row)
    return ("\n".join(lines) + "\n").encode()


GEOMETRY = b'''schema_version = 1
unobserved = ["physical scanout"]
[renderer]
kind = "gles"
readback_scale = 1
[requested]
resolution = 1.0
layout = "TopBottom"
aspect = "Default"
swapped = false
stretched = "No"
stacked = true
second_window = false
[[shots]]
frame = 0
file = "frame-00000.bmp"
[shots.top]
readback = [400, 240]
[shots.bottom]
readback = [320, 240]
'''


class ProfileTests(unittest.TestCase):
    def test_closed_schema_and_dependencies(self):
        invalid = ({"schema_version": True}, {"schema_version": 1, "env": {"HOME": "/tmp"}},
                   {"schema_version": 1, "geometry": 1}, {"schema_version": 1, "audio": "on"},
                   {"schema_version": 1, "audio": "paced"}, {"schema_version": 1, "texture_timing": True},
                   {"schema_version": 1, "packet_window_start": 0},
                   {"schema_version": 1, "packet_census": True, "packet_window_start": True},
                   {"schema_version": 1, "packet_census": True, "packet_window_start": 2**64},
                   {"schema_version": 1, "audio_backend": {}})
        for value in invalid:
            with self.subTest(value=value), self.assertRaises(ValueError):
                diagnostics.validate(value)

    def test_effective_environment_is_finite_and_packet_flag_cannot_disable_binary(self):
        profile = diagnostics.validate({"schema_version": 1, "geometry": True, "packet_census": True,
                                        "packet_window_start": 1800, "texture_stats": True, "texture_timing": True})
        self.assertEqual(diagnostics.environment(profile), {"ZAKURO_PROBE_AUDIO": "off", "ZAKURO_PROBE_GEOMETRY": "1",
            "ZAKURO_PACKET_CENSUS_FROM": "1800", "ZAKURO_TEXTURE_CACHE_STATS": "1", "ZAKURO_TEXTURE_CACHE_TIMING": "1"})
        self.assertEqual(diagnostics.environment(diagnostics.validate({"schema_version": 1})), {"ZAKURO_PROBE_AUDIO": "off"})
        self.assertEqual(diagnostics.declaration_hash(profile), diagnostics.declaration_hash(dict(reversed(list(profile.items())))))

    def test_duplicate_json_and_symlink_sources_are_rejected(self):
        with tempfile.TemporaryDirectory() as name:
            path = Path(name) / "profile.json"
            path.write_text('{"schema_version":1,"schema_version":1}')
            with self.assertRaises(ValueError):
                diagnostics.load(path)
            link = path.with_name("link.json")
            link.symlink_to(path)
            with self.assertRaises(ValueError):
                diagnostics.load(link)

    def backend_profile(self, root, kind="alsa_null"):
        source = root / "declared.asoundrc"
        source.write_text("pcm.!default { type null }\n")
        return {"schema_version": 1, "audio": "unpaced", "audio_backend": {"kind": kind,
            "asoundrc": {"path": str(source), "sha256": hashlib.sha256(source.read_bytes()).hexdigest()}}}

    def test_backend_is_cloned_and_source_change_fails(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            profile = diagnostics.validate(self.backend_profile(root))
            trial = root / "trial"
            (trial / "home").mkdir(parents=True)
            receipt = diagnostics.stage_backend(profile, trial)
            self.assertEqual(receipt["asoundrc_source"]["sha256"], receipt["asoundrc_clone"]["sha256"])
            self.assertEqual((trial / "home/.asoundrc").read_bytes(), (root / "declared.asoundrc").read_bytes())
            self.assertIsNone(receipt["runtime_directory"])
            self.assertNotIn("HOME", diagnostics.environment(profile))
            (root / "declared.asoundrc").write_text("changed")
            with self.assertRaises(ValueError):
                diagnostics.backend_inputs(profile)

    def test_runtime_directory_is_explicit_real_device_stat_identity(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            profile = self.backend_profile(root, "real_device")
            runtime = root / "runtime"
            runtime.mkdir()
            info = runtime.stat()
            binding = {"path": str(runtime), "uid": info.st_uid, "device": info.st_dev, "inode": info.st_ino}
            profile["audio_backend"]["xdg_runtime_dir"] = binding
            profile = diagnostics.validate(profile)
            self.assertEqual(diagnostics.backend_inputs(profile)["runtime_directory"], binding)
            profile["audio_backend"]["xdg_runtime_dir"]["inode"] += 1
            with self.assertRaises(ValueError):
                diagnostics.backend_inputs(profile)
            profile["audio_backend"]["kind"] = "alsa_null"
            with self.assertRaises(ValueError):
                diagnostics.validate(profile)


class FinishAndCapSchemaTests(unittest.TestCase):
    def test_historical_delivery_is_readable_with_finish_explicitly_unknown(self):
        original = audio_fixture()
        saved = copy.deepcopy(original)
        result = diagnostics.audio_admission(original, "paced", completion_required=True)
        self.assertTrue(result["full_write_admitted"])
        self.assertFalse(result["backend_completion_admitted"])
        self.assertFalse(result["admitted"])
        self.assertEqual(result["backend_finish"], "unknown_historical_schema1")
        self.assertEqual(original, saved)

    def test_completion_does_not_replace_write_or_channel_gates(self):
        result = diagnostics.audio_admission(finish_fixture(), "paced", completion_required=True)
        self.assertTrue(result["admitted"])
        self.assertEqual(result["acoustic_quality"], "unknown_not_measured")
        self.assertNotIn("underruns", result["report"])
        for changes in ({"channel_finished": False}, {"written_frames": 89}, {"received_frames": 99},
                        {"device_frames_accepted": 119}, {"delivery": "incomplete", "partial_writes": 1}):
            with self.subTest(changes=changes):
                self.assertFalse(diagnostics.audio_admission(finish_fixture(**changes), "paced", True)["admitted"])

    def test_failed_or_unobserved_alsa_return_does_not_erase_full_write_delivery(self):
        for code in (-32, 1):
            result = diagnostics.audio_admission(finish_fixture(backend_finish="failed", backend_finish_code=code), "paced", True)
            self.assertTrue(result["full_write_admitted"])
            self.assertFalse(result["admitted"])
            self.assertEqual(result["report"]["backend_finish_code"], code)
        row = finish_fixture(backend_finish="in_progress", channel_finished=False)
        del row["backend_finish_code"]
        self.assertFalse(diagnostics.audio_admission(row, "paced", True)["admitted"])

    def test_aaudio_noop_never_claims_backend_or_acoustic_completion(self):
        row = finish_fixture(backend="AAudio", backend_finish="not_attempted", backend_finish_operation="none")
        del row["backend_finish_code"]
        result = diagnostics.audio_admission(row, "paced", True)
        self.assertTrue(result["full_write_admitted"])
        self.assertFalse(result["backend_completion_admitted"])
        self.assertEqual(result["acoustic_quality"], "unknown_not_measured")

    def test_finish_status_operation_and_exact_signed_code_are_closed(self):
        for changes in ({"backend_finish": "drained"}, {"backend_finish": "failed"},
                        {"backend_finish_code": -1}, {"backend_finish_code": True}, {"backend_finish_code": 2**31},
                        {"backend_finish_code": -(2**31)-1}, {"backend_finish_code": "0"},
                        {"backend_finish_operation": "request_stop"}, {"backend": "AAudio"},
                        {"backend_finish": "not_attempted", "backend_finish_operation": "none"},
                        {"backend_finish": "in_progress"}, {"channel_finished": 1},
                        {"drained": True}, {"device_frames_accepted": 121}, {"written_frames": 91},
                        {"queued_seconds_mean": 1.0}, {"underruns_measured": True, "underruns": 0}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                diagnostics.audio_admission(finish_fixture(**changes), "paced", True)
        for missing in ("channel_finished", "backend_finish", "backend_finish_operation", "backend_finish_code"):
            row = finish_fixture()
            del row[missing]
            with self.subTest(missing=missing), self.assertRaises(ValueError):
                diagnostics.audio_admission(row, "paced", True)

    def test_unavailable_backend_can_finish_channel_without_delivery(self):
        row = finish_fixture(backend="unavailable", backend_finish="unavailable", backend_finish_operation="none", delivery="mixing_only")
        del row["backend_finish_code"], row["device_rate"]
        for name in ("received_frames", "discarded_frames", "output_frames", "written_frames", "device_frames_offered",
                     "device_frames_accepted", "device_writes", "partial_writes", "failed_writes", "write_errors", "write_recoveries"):
            row[name] = 0
        self.assertFalse(diagnostics.audio_admission(row, "paced", True)["admitted"])
        row["device_frames_accepted"] = 1
        with self.assertRaises(ValueError):
            diagnostics.audio_admission(row, "paced", True)

    def test_optional_completion_declaration_preserves_old_hashes(self):
        with tempfile.TemporaryDirectory() as directory:
            profile = ProfileTests().backend_profile(Path(directory))
            old = diagnostics.validate(profile)
            self.assertNotIn("audio_completion", old)
            new = diagnostics.validate({**profile, "audio_completion": "backend"})
            self.assertNotEqual(diagnostics.declaration_hash(old), diagnostics.declaration_hash(new))
            self.assertEqual(diagnostics.environment(old), diagnostics.environment(new))
        for profile in ({"schema_version": 1, "audio_completion": "backend"},
                        {"schema_version": 1, "audio_completion": True}):
            with self.assertRaises(ValueError):
                diagnostics.validate(profile)

    def test_collection_holds_historical_or_failed_finish_when_completion_requested(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            probe = ObserverTests().trial(root)
            profile = diagnostics.validate({**ProfileTests().backend_profile(root), "audio_completion": "backend"})
            for audio, admitted in ((audio_fixture("unpaced"), False),
                                    (finish_fixture(requested="unpaced", pacing="unpaced"), True),
                                    (finish_fixture(requested="unpaced", pacing="unpaced", backend_finish="failed",
                                                    backend_finish_code=-32), False)):
                audio.pop("paced_batches", None)
                audio.pop("late_batches", None)
                lines = ["schema_version = 1", "[audio]"] + [f"{key} = {json.dumps(value)}" for key, value in audio.items()]
                (probe / "report.toml").write_text("\n".join(lines) + "\n")
                result = diagnostics.collect(root, {"schema_version": 1, "audio": audio}, {}, profile)
                with self.subTest(schema=audio["schema_version"], admitted=admitted):
                    self.assertEqual(result["valid"], admitted)
                    self.assertEqual(result["audio"]["backend_completion_admitted"], admitted)
                    self.assertFalse(result["timing_eligible"])

    def test_geometry_schema1_rejects_v2_budget_and_attempt_index_semantics(self):
        row = presentation_fixture()
        with self.assertRaises(ValueError):
            diagnostics.presentation_record(row, 1)
        for name in ("panel_budget_status", "panel_budget_limit_bytes", "panel_budget_charged_bytes"):
            row.pop(name)
        diagnostics.presentation_record(row, 1)
        row.update(panel_shot_status="write_failed", panel_shot_retained=False)
        with self.assertRaises(ValueError):
            diagnostics.presentation_record(row, 1)
        row.pop("panel_shot_index")
        diagnostics.presentation_record(row, 1)  # historical failed capture remains readable
        diagnostics.presentation_record({**presentation_fixture(), "panel_shot_status": "write_failed",
                                        "panel_shot_retained": False, "panel_budget_status": "write_failed"}, 2)

    def test_sidecar_versions_apply_the_closed_presentation_schema(self):
        body = b"\n[final_presentation]\n" + "\n".join(
            f"{key} = {json.dumps(value)}" for key, value in presentation_fixture().items()).encode() + b"\n"
        v2 = GEOMETRY.replace(b"schema_version = 1", b"schema_version = 2") + body
        result = diagnostics.geometry_record(v2, {"frame-00000.bmp": "synthetic"})
        self.assertEqual(result["record"]["schema_version"], 2)
        self.assertEqual(result["record"]["final_presentation"]["panel_budget_charged_bytes"], "12301")
        with self.assertRaises(ValueError):
            diagnostics.geometry_record(GEOMETRY + body, {"frame-00000.bmp": "synthetic"})

    def test_canonical_decimal_strings_and_u64_overflow(self):
        for value in ("", "00", "01", "+1", "-1", " 1", "1 ", "1.0", "1e3", "١", "18446744073709551616", 1, True):
            with self.subTest(value=value), self.assertRaises(ValueError):
                diagnostics.canonical_u64(value)
        self.assertEqual(diagnostics.canonical_u64("18446744073709551615"), 2**64-1)
        diagnostics.presentation_record(presentation_fixture(panel_budget_limit_bytes=str(2**64-1)), 2)

    def test_geometry_status_counter_and_retention_contradictions_fail_closed(self):
        for changes in ({"panel_budget_status": "unknown"}, {"panel_budget_status": "absent"},
                        {"panel_budget_limit_bytes": "0"}, {"panel_budget_charged_bytes": "12302"},
                        {"panel_budget_charged_bytes": "0"}, {"panel_budget_status": "exhausted"},
                        {"panel_shot_retained": False}, {"panel_shot_numbered": False}, {"new_field": 1}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                diagnostics.presentation_record(presentation_fixture(**changes), 2)
        for name in ("panel_budget_status", "panel_budget_limit_bytes", "panel_budget_charged_bytes", "panel_shot_index"):
            row = presentation_fixture()
            row.pop(name)
            with self.subTest(missing=name), self.assertRaises(ValueError):
                diagnostics.presentation_record(row, 2)
        for status in ("absent", "invalid"):
            row = presentation_fixture(panel_budget_status=status)
            del row["panel_budget_limit_bytes"], row["panel_budget_charged_bytes"]
            if status == "invalid":
                row.update(panel_shot_status="write_failed", panel_shot_retained=False)
            diagnostics.presentation_record(row, 2)


class ObserverTests(unittest.TestCase):
    def test_audio_domains_discards_and_unknown_underruns_are_retained(self):
        for mode in ("paced", "unpaced"):
            row = audio_fixture(mode)
            result = diagnostics.audio_admission(row, mode)
            self.assertTrue(result["full_write_admitted"])
            self.assertNotEqual(row["output_frames"], row["device_frames_accepted"])
            self.assertEqual(result["report"]["discarded_frames"], 10)
            self.assertNotIn("underruns", result["report"])
            self.assertEqual(result["acoustic_quality"], "unknown_not_measured")

    def test_mixing_request_is_observed_not_forced_to_async(self):
        for mixing in ("sync", "async"):
            row = {**audio_fixture(), "mixing_requested": mixing}
            self.assertTrue(diagnostics.audio_admission(row, "paced")["full_write_admitted"])
        with self.assertRaises(ValueError):
            diagnostics.audio_admission({**audio_fixture(), "mixing_requested": "unknown"}, "paced")

    def test_audio_partial_drain_and_short_device_write_cannot_admit(self):
        for edit in ({"partial_writes": 1, "delivery": "incomplete"}, {"drained": False},
                     {"received_frames": 99}, {"device_frames_accepted": 119}, {"written_frames": 89}):
            with self.subTest(edit=edit):
                self.assertFalse(diagnostics.audio_admission({**audio_fixture(), **edit}, "paced")["full_write_admitted"])
        for edit in ({"schema_version": True}, {"received_frames": True}, {"requested": "unpaced"},
                     {"underruns": 0}, {"queued_seconds_max": float("nan")}):
            with self.subTest(edit=edit), self.assertRaises(ValueError):
                diagnostics.audio_admission({**audio_fixture(), **edit}, "paced")

    def test_geometry_binds_source_readbacks_without_scanout_claim(self):
        result = diagnostics.geometry_record(GEOMETRY, {"frame-00000.bmp": "retained-by-probe"})
        self.assertEqual(result["record"]["shots"][0]["bottom"]["readback"], [320, 240])
        self.assertEqual(result["presentation_pixels"], "unknown_not_captured")
        with self.assertRaises(ValueError):
            diagnostics.geometry_record(GEOMETRY, {"frame-00001.bmp": "different"})
        with self.assertRaises(ValueError):
            diagnostics.geometry_record(GEOMETRY.replace(b"schema_version = 1", b"schema_version = 3"), {"frame-00000.bmp": "hash"})

    def trial(self, root):
        probe = root / "probe-synthetic"
        probe.mkdir()
        (probe / "report.toml").write_text("schema_version = 1\n")
        (root / "stderr.log").write_text("INFO synthetic short run\n")
        return probe

    def test_absent_profile_retains_ordinary_behavior_and_detects_compile_time_packet(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            probe = self.trial(root)
            self.assertIsNone(diagnostics.collect(root, {}, {}))
            (probe / "packet-census.toml").write_bytes(packet_bytes())
            result = diagnostics.collect(root, {}, {})
            self.assertTrue(result["valid"])
            self.assertFalse(result["timing_eligible"])
            self.assertFalse(result["packet"]["requested"])
            self.assertEqual(result["packet"]["status"], "observed")
            self.assertEqual(result["packet"]["source"]["sha256"], hashlib.sha256(packet_bytes()).hexdigest())

    def test_requested_feature_files_are_required_and_packet_window_is_checked(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            probe = self.trial(root)
            for field in ("packet_census", "geometry"):
                result = diagnostics.collect(root, {}, {}, diagnostics.validate({"schema_version": 1, field: True}))
                self.assertFalse(result["valid"])
            profile = diagnostics.validate({"schema_version": 1, "packet_census": True, "packet_window_start": 2})
            (probe / "packet-census.toml").write_bytes(packet_bytes(1))
            self.assertFalse(diagnostics.collect(root, {}, {}, profile)["valid"])
            (probe / "packet-census.toml").write_bytes(packet_bytes(2))
            self.assertTrue(diagnostics.collect(root, {}, {}, profile)["valid"])

    def test_short_texture_run_is_unknown_and_raw_log_identity_is_retained(self):
        with tempfile.TemporaryDirectory() as name:
            root = Path(name)
            self.trial(root)
            profile = diagnostics.validate({"schema_version": 1, "texture_stats": True})
            result = diagnostics.collect(root, {}, {}, profile)
            self.assertTrue(result["valid"])
            self.assertEqual(result["texture"]["status"], "unknown_no_rows")
            self.assertFalse(result["texture"]["coverage"]["missing_rows_are_zero"])
            self.assertIsNone(result["texture"]["timed_input"])
            self.assertEqual(result["texture"]["source"]["sha256"], hashlib.sha256((root / "stderr.log").read_bytes()).hexdigest())


if __name__ == "__main__":
    unittest.main()
