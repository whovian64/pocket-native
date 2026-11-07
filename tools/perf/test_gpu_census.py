"""Synthetic source-contract cases; no ROM, GPU, profiler, build or network.

Prepared for root-owned CPU GO. This file does not assert execution receipts.
"""

import copy
from contextlib import redirect_stderr, redirect_stdout
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("gpu_census", Path(__file__).with_name("gpu_census.py"))
gpu = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gpu)


def packet_fixture(window=False):
    scope = {table: dict.fromkeys(fields, 0) for table, fields in gpu.PACKET_LAYOUT.items()}
    scope.update(executed_words=8, accounted_words=8, unaccounted_words=0)
    scope["summary"].update(lists=1, list_words=12, packets=3, declared_words=8,
                            padding_words=1, twin_packets=1)
    scope["mask"]["0xF"] = 3
    scope["extra"].update({"0": 1, "2-3": 2})
    scope["shape"].update(single_full_mask=1, same_register_full_mask=1, consecutive_full_mask=1)
    scope["first_full_mask"].update(gs_uniform_data=1, vs_uniform_index=1)
    scope["scalar_same_register_full_clear"]["gs_uniform_data"] = 3
    scope["scalar_consecutive_full_clear"]["vs_uniform_data"] = 2
    scope["index_data_packets"]["vs_uniform"] = 1
    scope["index_data_words"]["vs_uniform"] = 2
    result = {"schema": gpu.PACKET_SCHEMA, "schema_version": 1, "source": gpu.PACKET_SOURCE,
              "crate_version": "0.1.0", "batches_seen": 2, "total": scope}
    if window:
        result.update(window_start_batch=1, window_marked=True, window=copy.deepcopy(scope))
    return result


def packet_fixture_v2(window=False):
    """The v1 fixture as the packet-batching parser reports it: its consecutive
    uniform data words one burst, its GS same-register words one fast burst."""
    row = packet_fixture(window)
    row["schema_version"] = 2
    for scope in [row["total"], *([row["window"]] if window else [])]:
        scope["consecutive_uniform_burst"] = dict.fromkeys(gpu.FAMILIES, 0)
        scope["summary"].update(consecutive_uniform_bursts=1, fast_packets=1)
        scope["scalar_consecutive_full_clear"]["vs_uniform_data"] = 0
        scope["consecutive_uniform_burst"]["vs_uniform_data"] = 2
        scope["scalar_same_register_full_clear"]["gs_uniform_data"] = 0
        scope["fast"]["gs_uniform_data"] = 3
    return row


def expected_v1_output(row):
    """Freeze the pre-v2 public output shape, including total/window coverage."""
    def scope(source):
        return {
            "accepted_header_payload": {"declared_words": 8, "truncated_words": 0,
                                        "executed_words": 8, "accounted_words": 8,
                                        "unaccounted_words": 0},
            "loaded_list_words": 12,
            "raw_list_conservation": "unknown_no_incomplete_header_or_abandoned_suffix_counters",
            "summary": source["summary"],
            "histograms": {key: source[key] for key in ("mask", "extra", "shape")},
            "path_words_by_family": {key: source[key] for key in gpu.PATH_TABLES},
            "opportunities": {
                "index_plus_data_packets": source["index_data_packets"],
                "index_plus_data_extra_words": source["index_data_words"],
                "scalar_same_register_full_clear_words": source["scalar_same_register_full_clear"],
                "adjacent_same_register_packets": source["chain_same_register"],
                "adjacent_next_register_packets": source["chain_next_register"],
            },
            "acts_by_family": source["acts"],
            "opportunity_overlap": "index/data and adjacency are not disjoint from path words; do not sum",
            "pending_scope": "sampled after first write when extra-word path is selected",
            "mirrored_scope": "fast-path extra program words only; first/scalar mirrors unknown",
        }

    window = row.get("window")
    return {
        "status": "observed", "batches_begun": 2, "crate_version": "0.1.0",
        "total": scope(row["total"]), "window": scope(window) if window else None,
        "window_coverage": {
            "status": "observed_start_fence_only" if window else "unknown_not_requested",
            "start_batch": 1 if window else None,
            "unit": "zero-based next_frame batch", "end_batch_fence": None,
            "includes_interrupted_final_attempted_batch": True, "completed_batch_count": None,
        },
        "overflow_coverage": "u64 ranges and conservation checked; undetectable source wrap remains unknown",
    }


def toml_bytes(row):
    """Serialize only this finite synthetic fixture's scalar/table types."""
    lines = []

    def emit(table, prefix=()):
        if prefix:
            lines.append("[" + ".".join(json.dumps(part) for part in prefix) + "]")
        for key, value in table.items():
            if not isinstance(value, dict):
                lines.append(json.dumps(key) + " = " + json.dumps(value))
        for key, value in table.items():
            if isinstance(value, dict):
                emit(value, (*prefix, key))

    emit(row)
    return ("\n".join(lines) + "\n").encode()


def texture_fixture(first=1, end=600, complete=True, timed=False):
    return {
        "schema": gpu.TEXTURE_SCHEMA,
        "interval": {"first_list": first, "end_list": end, "lists": end - first, "complete": complete},
        "checked": {"calls": 6, "absent": 1, "untrusted": 2, "trusted": 3},
        "decoded": {"calls": 4, "hits": 1, "misses": 3, "hashed_bytes": 128, "oversized": 1},
        "misses_by_format": {
            "Rgba8": {"misses": 3, "replaced": 1, "new_keys": 2, "source_bytes": 96,
                      "rgba_bytes": 384, "packed_bytes": 0, "packs_asked": 1, "packed": 0,
                      "decode_ns": 90 if timed else None, "decode_max_ns": 40 if timed else None}},
        "evictions": {"inserts": 2, "evicted": 2, "evicted_bytes": 128,
                      "per_insert": {"0": 1, "1": 0, "2": 1, "3": 0, "4-7": 0, "8-15": 0, "16+": 0},
                      "scanned_entries": 5, "scan_max_entries": 3,
                      "evict_insert_ns": 20 if timed else None},
        "idle_pruned": 1,
        "live": {"entries": 2, "max_entries": 3, "capacity": 3, "accounted_bytes": 256,
                 "full_hash_duplicates": 1},
        "list_peaks": {"misses": 2, "decoded_bytes": 256, "evictions": 2,
                       "decode_ns": 60 if timed else None},
        "timed": timed,
    }


def log_bytes(*rows):
    return "\n".join("INFO " + gpu.TEXTURE_PREFIX + json.dumps(row) for row in rows).encode()


class PacketTests(unittest.TestCase):
    def rejected(self, edit):
        row = packet_fixture()
        edit(row)
        with self.assertRaises(ValueError):
            gpu.parse_packet(toml_bytes(row))

    def test_actual_layout_and_opportunities(self):
        result = gpu.parse_packet(toml_bytes(packet_fixture(window=True)))
        self.assertEqual(result["total"]["accepted_header_payload"]["executed_words"], 8)
        self.assertEqual(result["total"]["loaded_list_words"], 12)
        self.assertTrue(result["total"]["raw_list_conservation"].startswith("unknown"))
        self.assertEqual(result["total"]["opportunities"]["index_plus_data_extra_words"]["vs_uniform"], 2)
        self.assertEqual(result["total"]["opportunities"]["scalar_same_register_full_clear_words"]["gs_uniform_data"], 3)
        self.assertIsNone(result["window_coverage"]["end_batch_fence"])
        self.assertTrue(result["window_coverage"]["includes_interrupted_final_attempted_batch"])
        self.assertIsNone(result["window_coverage"]["completed_batch_count"])

    def test_schema_2_reports_actual_uniform_bursts(self):
        result = gpu.parse_packet(toml_bytes(packet_fixture_v2(window=True)))
        self.assertEqual(result["schema_version"], 2)
        for scope in (result["total"], result["window"]):
            self.assertEqual(scope["accepted_header_payload"]["accounted_words"], 8)
            actual = scope["actual_fast_paths"]
            self.assertEqual(actual["consecutive_uniform_burst_packets"], 1)
            self.assertEqual(actual["consecutive_uniform_burst_words"]["vs_uniform_data"], 2)
            self.assertEqual(actual["same_register_fast_words"]["gs_uniform_data"], 3)
            self.assertIn("consecutive_uniform_burst", scope["path_words_by_family"])
            self.assertEqual(scope["opportunities"]["index_plus_data_extra_words"]["vs_uniform"], 2)
        v1 = gpu.parse_packet(toml_bytes(packet_fixture()))
        self.assertNotIn("schema_version", v1)
        self.assertNotIn("actual_fast_paths", v1["total"])
        self.assertNotIn("consecutive_uniform_burst", v1["total"]["path_words_by_family"])

    def test_schema_2_fails_closed(self):
        def rejected(edit):
            row = packet_fixture_v2()
            edit(row)
            with self.assertRaises(ValueError):
                gpu.parse_packet(toml_bytes(row))

        rejected(lambda row: row["total"].pop("consecutive_uniform_burst"))
        rejected(lambda row: row["total"]["summary"].pop("consecutive_uniform_bursts"))
        rejected(lambda row: row["total"]["consecutive_uniform_burst"].update(vs_uniform_data=1))
        rejected(lambda row: row["total"]["consecutive_uniform_burst"].update(vs_uniform_data=0, vs_program_data=2))
        rejected(lambda row: row["total"]["summary"].update(consecutive_uniform_bursts=0))
        rejected(lambda row: row["total"]["summary"].update(consecutive_uniform_bursts=2))
        rejected(lambda row: row.update(schema_version=3))
        row = packet_fixture()
        row["total"]["consecutive_uniform_burst"] = dict.fromkeys(gpu.FAMILIES, 0)
        with self.assertRaises(ValueError):
            gpu.parse_packet(toml_bytes(row))

    def test_v1_output_is_exactly_unchanged(self):
        for window in (False, True):
            with self.subTest(window=window):
                row = packet_fixture(window)
                self.assertEqual(gpu.parse_packet(toml_bytes(row)), expected_v1_output(row))

    def test_versions_cannot_mix_between_total_and_window(self):
        for version in (1, 2):
            row = packet_fixture(window=True) if version == 1 else packet_fixture_v2(window=True)
            other = packet_fixture_v2(window=True) if version == 1 else packet_fixture(window=True)
            row["window"] = other["window"]
            with self.subTest(version=version), self.assertRaisesRegex(ValueError, "missing or unknown fields"):
                gpu.parse_packet(toml_bytes(row))

    def test_schema_2_requires_complete_counts_and_zero_unaccounted(self):
        for edit in (
            lambda row: row["total"].update(unaccounted_words=1),
            lambda row: row["total"].update(accounted_words=7),
            lambda row: row["total"]["consecutive_uniform_burst"].pop("gs_uniform_data"),
            lambda row: row["total"]["consecutive_uniform_burst"].update(gs_uniform_data=True),
        ):
            row = packet_fixture_v2()
            edit(row)
            with self.subTest(edit=edit), self.assertRaises(ValueError):
                gpu.parse_packet(toml_bytes(row))

    def test_schema_2_empty_scope_and_truncated_packet(self):
        row = packet_fixture_v2()
        scope = row["total"]
        # Keep an index-first packet's declared extras, but truncate all of them.
        # It supplies no burst words and therefore must not count as a burst.
        scope["summary"].update(truncated_words=2, consecutive_uniform_bursts=0)
        scope.update(executed_words=6, accounted_words=6)
        scope["consecutive_uniform_burst"]["vs_uniform_data"] = 0
        scope["index_data_packets"]["vs_uniform"] = 0
        scope["index_data_words"]["vs_uniform"] = 0
        result = gpu.parse_packet(toml_bytes(row))
        self.assertEqual(result["total"]["accepted_header_payload"]["unaccounted_words"], 0)
        self.assertEqual(result["total"]["actual_fast_paths"]["consecutive_uniform_burst_packets"], 0)
        # A complete v2 schema can represent a process with no parsed packets.
        for value in scope.values():
            if isinstance(value, dict):
                value.update(dict.fromkeys(value, 0))
        scope.update(executed_words=0, accounted_words=0, unaccounted_words=0)
        self.assertEqual(gpu.parse_packet(toml_bytes(row))["total"]["accepted_header_payload"]["executed_words"], 0)

    def test_schema_2_opportunities_are_a_subset_of_actual_bursts(self):
        for edit, message in (
            # Move words to a scalar path to keep the payload conserved while
            # making the index-first opportunity larger than the actual burst.
            (lambda scope: (scope["consecutive_uniform_burst"].update(vs_uniform_data=1),
                            scope["scalar_consecutive_full_clear"].update(vs_uniform_data=1)),
             "index/data uniform words exceed burst words"),
            # The same total burst size in the wrong shader unit is not enough.
            (lambda scope: scope["consecutive_uniform_burst"].update(vs_uniform_data=0, gs_uniform_data=2),
             "index/data uniform words exceed burst words"),
        ):
            row = packet_fixture_v2()
            edit(row["total"])
            with self.subTest(message=message), self.assertRaisesRegex(ValueError, message):
                gpu.parse_packet(toml_bytes(row))

    def test_schema_2_window_burst_count_cannot_exceed_total(self):
        row = packet_fixture_v2(window=True)
        scope = row["window"]
        # Two conserved burst words with no index-first opportunity: this
        # window is valid alone but cannot contain more GS words than total.
        scope["consecutive_uniform_burst"].update(vs_uniform_data=0, gs_uniform_data=2)
        scope["index_data_packets"]["vs_uniform"] = 0
        scope["index_data_words"]["vs_uniform"] = 0
        with self.assertRaisesRegex(ValueError, "window counter exceeds total"):
            gpu.parse_packet(toml_bytes(row))

    def test_schema_2_burst_width_is_bounded_by_the_eight_data_registers(self):
        for words in (8, 9):
            row = packet_fixture_v2()
            scope = row["total"]
            scope["summary"]["declared_words"] = 6 + words
            scope.update(executed_words=6 + words, accounted_words=6 + words)
            scope["extra"].update({"2-3": 1, "8-15": 1})
            scope["consecutive_uniform_burst"]["vs_uniform_data"] = words
            scope["index_data_words"]["vs_uniform"] = words
            with self.subTest(words=words):
                if words == 8:
                    result = gpu.parse_packet(toml_bytes(row))
                    self.assertEqual(result["total"]["accepted_header_payload"]["unaccounted_words"], 0)
                else:
                    with self.assertRaisesRegex(ValueError, "uniform burst payload bound mismatch"):
                        gpu.parse_packet(toml_bytes(row))

    def test_v1_and_v2_preserve_header_and_scalar_equivalent_family_words(self):
        old, new = packet_fixture(), packet_fixture_v2()
        # Actual scalar act counts for the fixture's two ordinary packets;
        # the stereo twin has its separate payload accounting path.
        old["total"]["acts"].update(gs_uniform_data=4, vs_uniform_index=1, vs_uniform_data=2)
        new["total"]["acts"].update(gs_uniform_data=1, vs_uniform_index=1)
        before = gpu.parse_packet(toml_bytes(old))["total"]
        after = gpu.parse_packet(toml_bytes(new))["total"]
        for key in ("accepted_header_payload", "histograms", "loaded_list_words"):
            self.assertEqual(before[key], after[key])
        for family in gpu.FAMILIES:
            self.assertEqual(
                before["acts_by_family"][family] + before["path_words_by_family"]["fast"][family],
                after["acts_by_family"][family] + after["path_words_by_family"]["fast"][family]
                + after["path_words_by_family"]["consecutive_uniform_burst"][family],
                family,
            )
        self.assertEqual(after["accepted_header_payload"]["unaccounted_words"], 0)

    def test_missing_window_is_unknown(self):
        row = packet_fixture()
        self.assertIsNone(gpu.parse_packet(toml_bytes(row))["window"])
        row.update(window_start_batch=20, window_marked=False)
        result = gpu.parse_packet(toml_bytes(row))
        self.assertEqual(result["window_coverage"]["status"], "unknown_start_not_reached")

    def test_unknown_schema_source_and_legacy_counter(self):
        self.rejected(lambda row: row.update(schema_version=3))
        self.rejected(lambda row: row.update(schema_version=True))
        self.rejected(lambda row: row.update(source="another producer"))
        self.rejected(lambda row: row["total"]["summary"].update(gs_mirrored_program_words=0))
        self.rejected(lambda row: row["total"].update(future_table={}))

    def test_missing_histogram_or_family_is_not_zero(self):
        self.rejected(lambda row: row["total"].pop("mask"))
        self.rejected(lambda row: row["total"]["fast"].pop("gs_program_data"))

    def test_conservation_and_histogram_corruption(self):
        self.rejected(lambda row: row["total"].update(unaccounted_words=1))
        self.rejected(lambda row: row["total"]["summary"].update(truncated_words=9))
        self.rejected(lambda row: row["total"]["mask"].update({"0xF": 2}))
        self.rejected(lambda row: row["total"]["extra"].update({"2-3": 1, "128-255": 1}))
        self.rejected(lambda row: row["total"]["index_data_words"].update(vs_uniform=0))
        self.rejected(lambda row: row["total"]["summary"].update(gs_mirrored_fast_extra_program_words=1))

    def test_u64_and_boolean_counts_fail_closed(self):
        for value in (-1, gpu.U64_MAX + 1, True):
            self.rejected(lambda row, value=value: row["total"]["summary"].update(list_words=value))

    def test_window_marker_and_subtraction_consistency(self):
        row = packet_fixture(window=True)
        row["window"]["summary"]["list_words"] += 1
        with self.assertRaises(ValueError):
            gpu.parse_packet(toml_bytes(row))
        row = packet_fixture(window=True)
        row["window_start_batch"] = 2
        with self.assertRaises(ValueError):
            gpu.parse_packet(toml_bytes(row))

    def test_malformed_toml_and_duplicate_fields(self):
        for data in (b"[not valid", b"schema=1\nschema=1\n", b"\xff"):
            with self.assertRaises(ValueError):
                gpu.parse_packet(data)


class TextureTests(unittest.TestCase):
    def rejected(self, edit):
        row = texture_fixture()
        edit(row)
        with self.assertRaises(ValueError):
            gpu.parse_texture_log(log_bytes(row))

    def test_valid_rows_separate_lists_and_keep_unknown_tail(self):
        result = gpu.parse_texture_log(log_bytes(texture_fixture(), texture_fixture(600, 1200)))
        segment = result["segments"][0]
        self.assertEqual(segment["lists_observed"], 1199)
        self.assertEqual(segment["map_api_operations"], 42)
        self.assertEqual(segment["evictions"]["scanned_entries"], 10)
        self.assertEqual(segment["evictions_per_insert_histogram"]["2"], 2)
        self.assertEqual(segment["misses_by_format"]["Rgba8"]["misses"], 6)
        self.assertTrue(result["coverage"]["tail"].startswith("unknown"))
        self.assertFalse(result["coverage"]["missing_rows_are_zero"])
        self.assertNotIn("decode_ns", segment["list_peaks"])

    def test_missing_rows_unknown_including_short_runs(self):
        result = gpu.parse_texture_log(b"INFO no texture boundary reached\n")
        self.assertEqual(result["status"], "unknown_no_rows")
        self.assertEqual(result["segments"], [])
        self.assertIsNone(result["timed_input"])

    def test_gap_and_reset_do_not_create_whole_run_totals(self):
        # Exact duplicate rows cannot be distinguished from replay and must fail closed.
        with self.assertRaises(ValueError):
            gpu.parse_texture_log(log_bytes(texture_fixture(), texture_fixture(1200, 1800),
                                           texture_fixture(1, 600, timed=False)))

    def test_gap_and_ambiguous_restart_segments(self):
        reset = texture_fixture()
        reset["idle_pruned"] = 2
        result = gpu.parse_texture_log(log_bytes(texture_fixture(), texture_fixture(1200, 1800), reset))
        self.assertEqual(len(result["segments"]), 3)
        self.assertEqual(result["interval_gaps"], [{"after_end_list": 600, "before_first_list": 1200}])
        self.assertEqual(result["segments"][2]["start_reason"], "restart_or_overlap_identity_unknown")
        self.assertEqual(result["coverage"]["whole_run"], "unknown")

    def test_drop_tail_observed_but_whole_run_unknown(self):
        result = gpu.parse_texture_log(log_bytes(texture_fixture(), texture_fixture(600, 605, complete=False)))
        self.assertTrue(result["segments"][0]["tail_row_observed"])
        self.assertEqual(result["coverage"]["whole_run"], "unknown")
        with self.assertRaises(ValueError):
            gpu.parse_texture_log(log_bytes(texture_fixture(1, 605, complete=False), texture_fixture(605, 1200)))

    def test_unknown_fields_versions_formats_and_types(self):
        self.rejected(lambda row: row.update(schema="pocket-native.texture-cache-stats.v2"))
        self.rejected(lambda row: row.update(unknown_metric=0))
        self.rejected(lambda row: row["checked"].pop("trusted"))
        self.rejected(lambda row: row["misses_by_format"].update(FutureFormat={}))
        self.rejected(lambda row: row["interval"].update(lists=True))
        self.rejected(lambda row: row["checked"].update(calls=gpu.U64_MAX + 1))
        self.rejected(lambda row: row["interval"].update(first_list=0))

    def test_partitions_histograms_scans_and_peaks(self):
        self.rejected(lambda row: row["checked"].update(trusted=4))
        self.rejected(lambda row: row["decoded"].update(hits=2))
        self.rejected(lambda row: row["misses_by_format"]["Rgba8"].update(new_keys=3))
        self.rejected(lambda row: row["evictions"]["per_insert"].update({"2": 0, "16+": 1}))
        self.rejected(lambda row: row["evictions"].update(scanned_entries=100))
        self.rejected(lambda row: row["list_peaks"].update(decoded_bytes=385))
        self.rejected(lambda row: row["live"].update(full_hash_duplicates=2))

    def test_u64_overflowing_partition_sums(self):
        self.rejected(lambda row: row["checked"].update(calls=gpu.U64_MAX, absent=gpu.U64_MAX,
                                                        untrusted=gpu.U64_MAX, trusted=0))

    def test_empty_miss_histogram_means_zero_only_in_a_complete_valid_row(self):
        row = texture_fixture()
        row["decoded"].update(calls=1, hits=1, misses=0, oversized=0)
        row["misses_by_format"] = {}
        row["evictions"].update(inserts=0, evicted=0, evicted_bytes=0, scanned_entries=0, scan_max_entries=0)
        row["evictions"]["per_insert"] = dict.fromkeys(gpu.EVICTION_BUCKETS, 0)
        row["list_peaks"].update(misses=0, decoded_bytes=0, evictions=0)
        result = gpu.parse_texture_log(log_bytes(row))
        self.assertEqual(result["segments"][0]["decoded"]["misses"], 0)
        self.assertEqual(result["coverage"]["whole_run"], "unknown")

    def test_timing_validated_but_not_published(self):
        result = gpu.parse_texture_log(log_bytes(texture_fixture(timed=True)))
        self.assertTrue(result["timed_input"])
        self.assertNotIn("decode_ns", result["segments"][0]["misses_by_format"]["Rgba8"])
        self.rejected(lambda row: row["list_peaks"].update(decode_ns=0))
        self.rejected(lambda row: row.update(timed=True))
        with self.assertRaises(ValueError):
            gpu.parse_texture_log(log_bytes(texture_fixture(), texture_fixture(600, 1200, timed=True)))

    def test_duplicate_keys_replay_malformed_and_unknown_prefix(self):
        for data in (b"texture cache stats v2 {}", b"texture cache stats v1 {",
                     b'texture cache stats v1 {"schema":1,"schema":2}',
                     log_bytes(texture_fixture(), texture_fixture())):
            with self.assertRaises(ValueError):
                gpu.parse_texture_log(data)


class InputTests(unittest.TestCase):
    def test_snapshot_identity_and_one_read_per_input(self):
        data = toml_bytes(packet_fixture())
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "private-input.toml"
            path.write_bytes(data)
            with patch.object(gpu, "snapshot", wraps=gpu.snapshot) as reader:
                result = gpu.census(packet=path)
            self.assertEqual(reader.call_count, 1)
            self.assertEqual(result["packet"]["input"], {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
            self.assertEqual(result["texture"]["status"], "unknown_not_supplied")
            self.assertNotIn(directory, json.dumps(result))

    def test_byte_line_and_row_budgets(self):
        with patch.object(gpu, "MAX_PACKET_BYTES", 2):
            with self.assertRaises(ValueError):
                gpu.parse_packet(b"abc")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "input"
            path.write_bytes(b"abcd")
            with self.assertRaises(ValueError):
                gpu.snapshot(path, 3)
        for name, value, data in (("MAX_LOG_BYTES", 2, b"abc"), ("MAX_LINE_BYTES", 2, b"abc"),
                                  ("MAX_LINES", 1, b"a\nb\n"), ("MAX_ROWS", 0, log_bytes(texture_fixture()))):
            with patch.object(gpu, name, value):
                with self.assertRaises(ValueError):
                    gpu.parse_texture_log(data)

    def test_cli_rejection_has_no_raw_path_or_payload(self):
        output, error = io.StringIO(), io.StringIO()
        with redirect_stdout(output), redirect_stderr(error):
            result = gpu.main(["--packet", "/private/nonexistent/input.toml"])
        self.assertEqual(result, 2)
        self.assertEqual(output.getvalue(), "")
        self.assertNotIn("/private/", error.getvalue())


if __name__ == "__main__":
    unittest.main()
