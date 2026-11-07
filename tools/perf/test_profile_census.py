"""Synthetic census contracts; no compiler, profiler, game or GL processes."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("profile_census", Path(__file__).with_name("profile_census.py"))
census = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(census)


class CensusTests(unittest.TestCase):
    def input(self, text, function, *args):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "input.txt"
            source.write_text(text)
            return function(source, *args)

    def test_perf_keeps_unknowns_events_and_threads_in_denominator(self):
        result = self.input("""CPU worker 12 2.000000000: cpu-clock:u: 7 memcpy (/lib/libc.so)
           0007 memcpy (/lib/libc.so)
           18 zakuro_gpu::raster::draw (/bin/runtime)

CPU worker 12 2.010000000: cpu-clock:u: 9 [unknown] ([unknown])
GPU 13 2.020000000: cycles:u: a f_00100000 (/tmp/game.so)
""", census.parse_perf, {"/tmp/game.so"})
        event = result["events"]["cpu-clock:u"]
        self.assertEqual(event["sample_denominator"], 2)
        self.assertEqual(event["unwound_caller_samples"], 1)
        self.assertEqual(event["unwound_caller_coverage"], .5)
        self.assertEqual(event["data_movement_first_callers"][0]["symbol"], "zakuro_gpu::raster::draw")
        self.assertEqual(event["exclusive_leaf_categories"][0]["percent_all_event_samples"], 50)
        self.assertEqual(result["events"]["cycles:u"]["exclusive_leaf_categories"][0]["category"], "guest_native")
        self.assertEqual(result["all_parsed_samples"], 3)

    def test_malformed_records_do_not_join_prior_stack(self):
        result = self.input("""CPU 1 1.0: cpu-clock:u: a memcpy (/lib/libc.so)
unrecognized record
           b not_a_caller (/bad.so)
""", census.parse_perf, set())
        self.assertFalse(result["complete_parse"])
        self.assertEqual(result["malformed_nonempty_lines"], 2)
        self.assertEqual(result["events"]["cpu-clock:u"]["unwound_caller_samples"], 0)

    def test_unknown_stack_is_not_claimed_resolved(self):
        result = self.input("CPU 1 1.0: cpu-clock:u: a memcpy (/lib/libc.so)\n           b [unknown] ([unknown])\n", census.parse_perf, set())
        self.assertEqual(result["events"]["cpu-clock:u"]["unwound_caller_samples"], 0)

    def test_host_lr_suffix_does_not_become_stack_evidence(self):
        result = self.input("CPU worker /tmp/game.so 10 /tmp/runtime 20\nCPU ? 0 ? 0\nbad\n", census.parse_host, {"/tmp/game.so"})
        self.assertEqual(result["events"]["ITIMER_PROF"]["unwound_caller_samples"], 0)
        self.assertEqual(result["all_parsed_samples"], 2)
        self.assertEqual(result["malformed_nonempty_lines"], 1)

    def test_log_intervals_are_quantized_and_never_audio_proof(self):
        result = self.input("""INFO textures: 5000 looked at, 20 decoded, 12 KiB hashed; most decoded []
INFO textures: 5000 looked at, 1 decoded, 3 KiB hashed; most decoded []
INFO waiting on gpu 22%, memory 200 MiB
WARN could not open the sound device, playing without sound
""", census.parse_log)
        self.assertEqual(result["texture_hashed_bytes_lower_bound"], 15 * 1024)
        self.assertEqual(result["texture_intervals"][0]["hashed_bytes_upper_bound_exclusive"], 13 * 1024)
        self.assertEqual(result["queue_wait_percent_observations"], [22])
        self.assertTrue(result["audio_device_open_failure_observed"])
        self.assertFalse(result["audio_playback_verified"])
        self.assertIsNone(result["hash_reasons"])
        with self.assertRaises(ValueError):
            self.input("textures: 1 looked at, 2 decoded, 0 KiB hashed;", census.parse_log)

    def test_plan_preserves_argv_and_never_probes_or_runs(self):
        command = ["/private path/runtime", "--rom", "$(do-not-run)"]
        result = census.capture_plan(command, "private/census")
        self.assertEqual(result["capture_argv"][-3:], command)
        self.assertEqual(result["permission_status"], "unknown_not_probed")
        self.assertEqual(result["status"], "plan_only_not_executed")
        with self.assertRaises(ValueError):
            census.capture_plan([], "x")

    def test_manifest_context_cannot_admit_capture_or_equal_work(self):
        value = {"schema_version": 1, "executable": {"sha256": "a" * 64},
                 "source": {"head": "b" * 40}, "trials": [{"valid": True,
                 "probe": {"report": {"completed_frames": 10, "skipped_frames": 0,
                 "recompiled_instructions": 100}}}]}
        result = self.input(json.dumps(value), census.manifest_context)
        self.assertFalse(result["profile_capture_bound_to_manifest"])
        self.assertEqual(result["trials"][0]["work"]["recompiled_instructions"], 100)
        self.assertEqual(result["executable"], value["executable"])

    def test_snapshot_pins_parsed_bytes_after_replacement(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "profile"
            original = b"CPU 1 1.0: cpu-clock:u: a memcpy (/lib/libc.so)\n"
            source.write_bytes(original)
            data, identity = census.snapshot_input(source)
            source.write_bytes(b"changed\n")
            parsed = census.parse_perf(data, set())
            self.assertEqual(parsed["all_parsed_samples"], 1)
            self.assertEqual(identity["sha256"], __import__("hashlib").sha256(original).hexdigest())
            self.assertEqual(identity["bytes"], len(original))

    def test_bounded_read_catches_growth_after_open(self):
        from unittest.mock import mock_open
        fake = mock_open(read_data=b"abcdef")
        with patch.object(census, "MAX_BYTES", 2), patch.object(Path, "open", fake):
            with self.assertRaises(ValueError):
                census.snapshot_input(Path("growing"))
        fake().read.assert_called_once_with(3)

    def test_input_budgets_fail_instead_of_silent_truncation(self):
        with patch.object(census, "MAX_BYTES", 2):
            with self.assertRaises(ValueError):
                self.input("too big", census.parse_host, set())
        with patch.object(census, "MAX_LINES", 1):
            with self.assertRaises(ValueError):
                self.input("line1\nline2\n", census.parse_host, set())
        with patch.object(census, "MAX_BYTES", 2), patch.object(census, "identity") as hashed:
            with self.assertRaises(ValueError):
                self.input("too big", census.input_identity)
            hashed.assert_not_called()


if __name__ == "__main__":
    unittest.main()
