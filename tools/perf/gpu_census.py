#!/usr/bin/env python3
"""Offline, private diagnostic census reader (Python 3.11+, standard library).

Read bounded snapshots only. Counts describe instrumented parser/cache activity;
they cannot establish speedups, normal-runtime cost, or matching time windows.
"""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys
import tomllib

SCHEMA = "pocket-native.gpu-census-reader.v1"
PACKET_SCHEMA = "pocket-native.gpu-packet-census"
PACKET_SOURCE = "zakuro-gpu run_command_list, packet-census feature"
TEXTURE_SCHEMA = "pocket-native.texture-cache-stats.v1"
TEXTURE_PREFIX = "texture cache stats v1 "
U64_MAX = (1 << 64) - 1
MAX_PACKET_BYTES = 2 * 1024 * 1024
MAX_LOG_BYTES = 64 * 1024 * 1024
MAX_LINES = 200_000
MAX_LINE_BYTES = 65_536
MAX_ROWS = 20_000

FAMILIES = (
    "draw", "cmdbuf_jump", "fixed_attribute_index", "fixed_attribute_data",
    "light_table_data", "proctex_table_data", "fog_table_data", "vs_uniform_index",
    "vs_uniform_data", "vs_program_index", "vs_program_data", "vs_descriptor_index",
    "vs_descriptor_data", "vs_other", "gs_uniform_index", "gs_uniform_data",
    "gs_program_index", "gs_program_data", "gs_descriptor_index", "gs_descriptor_data",
    "gs_other", "passive_fragment", "passive_other", "beyond_register_file",
)
SUMMARY_FIELDS = (
    "lists", "jumps", "list_words", "packets", "out_of_range_stops", "declared_words",
    "truncated_words", "padding_words", "twin_packets", "fast_packets",
    "gs_mirrored_fast_extra_program_words",
)
MASKS = tuple(f"0x{value:X}" for value in range(16))
EXTRAS = ("0", "1", "2-3", "4-7", "8-15", "16-31", "32-63", "64-127", "128-255")
EXTRA_MIN = (0, 1, 2, 4, 8, 16, 32, 64, 128)
EXTRA_MAX = (0, 1, 3, 7, 15, 31, 63, 127, 255)
SHAPES = tuple(f"{shape}_{mask}_mask" for shape in ("single", "same_register", "consecutive")
               for mask in ("partial", "full"))
SCALAR = tuple(f"scalar_{shape}_{mask}_{state}" for shape in ("same_register", "consecutive")
               for mask in ("full", "partial") for state in ("clear", "pending"))
INDEX_DATA = tuple(f"{unit}_{kind}" for unit in ("vs", "gs")
                   for kind in ("uniform", "program", "descriptor"))
PACKET_LAYOUT = {
    "summary": SUMMARY_FIELDS, "mask": MASKS, "extra": EXTRAS, "shape": SHAPES,
    **{key: FAMILIES for key in ("first_partial_mask", "first_full_mask", "fast", "batched",
                                *SCALAR, "chain_same_register", "chain_next_register", "acts")},
    "index_data_packets": INDEX_DATA, "index_data_words": INDEX_DATA,
}
PATH_TABLES = ("first_partial_mask", "first_full_mask", "fast", "batched", *SCALAR)
# Schema 2 (packet-batching parser) adds actual consecutive uniform bursts as
# their own path table and packet count; every version 1 table keeps its meaning.
UNIFORM_DATA_REGISTERS = 8
UNIFORM_BURST_FAMILIES = ("vs_uniform_data", "gs_uniform_data")
PACKET_LAYOUTS = {
    1: PACKET_LAYOUT,
    2: {**PACKET_LAYOUT, "summary": (*SUMMARY_FIELDS, "consecutive_uniform_bursts"),
        "consecutive_uniform_burst": FAMILIES},
}
PATH_TABLES_BY_VERSION = {1: PATH_TABLES, 2: (*PATH_TABLES, "consecutive_uniform_burst")}
MIRRORED_SCOPE = {
    1: "fast-path extra program words only; first/scalar mirrors unknown",
    2: "VS same-register fast-path extra program words mirrored into GS only; "
       "GS own program bursts and first/scalar mirrors excluded",
}
FORMATS = ("Rgba8", "Rgb8", "Rgba5551", "Rgb565", "Rgba4", "La8", "Hilo8", "L8",
           "A8", "La4", "L4", "A4", "Etc1", "Etc1A4", "Unknown")
FORMAT_FIELDS = ("misses", "replaced", "new_keys", "source_bytes", "rgba_bytes", "packed_bytes",
                 "packs_asked", "packed", "decode_ns", "decode_max_ns")
EVICTION_BUCKETS = ("0", "1", "2", "3", "4-7", "8-15", "16+")
EVICTION_MIN = (0, 1, 2, 3, 4, 8, 16)
EVICTION_MAX = (0, 1, 2, 3, 7, 15)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def keys(value, required, where: str, optional=()) -> dict:
    require(type(value) is dict, f"{where}: expected a table/object")
    require(set(required) <= value.keys() <= set(required) | set(optional),
            f"{where}: missing or unknown fields")
    return value


def count(value, where: str) -> int:
    require(type(value) is int and 0 <= value <= U64_MAX, f"{where}: expected a u64 count")
    return value


def counts(value, fields, where: str) -> dict:
    keys(value, fields, where)
    return {key: count(value[key], f"{where}.{key}") for key in fields}


def sum_u64(values, where: str) -> int:
    return count(sum(values), where)


def snapshot(path: Path, limit: int) -> tuple[bytes, dict]:
    """Identity and parsing share one read; output does not expose the path."""
    with path.open("rb") as source:
        data = source.read(limit + 1)
    require(len(data) <= limit, "input exceeds byte budget")
    return data, {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


def packet_scope(row: dict, where: str, version: int = 1) -> dict:
    layout, path_tables = PACKET_LAYOUTS[version], PATH_TABLES_BY_VERSION[version]
    keys(row, (*layout, "executed_words", "accounted_words", "unaccounted_words"), where)
    for table, fields in layout.items():
        counts(row[table], fields, f"{where}.{table}")
    for field in ("executed_words", "accounted_words", "unaccounted_words"):
        count(row[field], f"{where}.{field}")
    summary = row["summary"]
    packets = summary["packets"]
    require(summary["truncated_words"] <= summary["declared_words"], "truncated payload exceeds declared")
    executed = summary["declared_words"] - summary["truncated_words"]
    accounted = sum_u64([summary["twin_packets"], *(value for table in path_tables
                                                 for value in row[table].values())], "path sum")
    require(row["executed_words"] == executed == accounted == row["accounted_words"]
            and row["unaccounted_words"] == 0, "accepted-header payload conservation failed")
    for table in ("mask", "extra", "shape"):
        require(sum_u64(row[table].values(), table) == packets, f"{table}: histogram does not sum to packets")
    require(row["extra"]["0"] == row["shape"]["single_partial_mask"] + row["shape"]["single_full_mask"],
            "single packet shapes disagree with zero extras")
    require(sum(value for name, value in row["shape"].items() if name.endswith("_full_mask")) == row["mask"]["0xF"],
            "full-mask shape histogram mismatch")
    require(packets <= executed, "each accepted header supplies a first payload word")
    extras = summary["declared_words"] - packets
    require(sum(n * row["extra"][key] for key, n in zip(EXTRAS, EXTRA_MIN)) <= extras
            <= sum(n * row["extra"][key] for key, n in zip(EXTRAS, EXTRA_MAX)),
            "declared extras outside histogram bounds")
    first = sum(row[key][family] for key in ("first_partial_mask", "first_full_mask") for family in FAMILIES)
    require(first + summary["twin_packets"] == packets, "first/twin packet partition mismatch")
    require(summary["twin_packets"] <= row["extra"]["0"], "twin packets must be single-word")
    require(summary["fast_packets"] <= row["shape"]["same_register_full_mask"], "fast packet shape mismatch")
    require(summary["gs_mirrored_fast_extra_program_words"] <= row["fast"]["vs_program_data"],
            "mirrored fast extras exceed VS program fast words")
    require(sum(row["chain_same_register"].values()) + sum(row["chain_next_register"].values()) <= packets,
            "adjacency counters exceed packet count")
    require(sum(row["index_data_packets"].values()) <= row["shape"]["consecutive_full_mask"],
            "index/data packets exceed eligible shape")
    for key in INDEX_DATA:
        n, words = row["index_data_packets"][key], row["index_data_words"][key]
        require(n <= words <= n * 255 and (n != 0 or words == 0), "index/data payload bound mismatch")
    actual = None
    if version >= 2:
        bursts, burst = summary["consecutive_uniform_bursts"], row["consecutive_uniform_burst"]
        require(all(value == 0 for family, value in burst.items() if family not in UNIFORM_BURST_FAMILIES),
                "uniform burst words outside uniform data families")
        words = sum_u64([burst[family] for family in UNIFORM_BURST_FAMILIES], "uniform burst words")
        require(bursts <= row["shape"]["consecutive_full_mask"], "uniform bursts exceed eligible shape")
        require(bursts <= words <= bursts * UNIFORM_DATA_REGISTERS, "uniform burst payload bound mismatch")
        # Every index-first uniform opportunity takes the burst with the same words.
        for unit in ("vs", "gs"):
            require(row["index_data_words"][f"{unit}_uniform"] <= burst[f"{unit}_uniform_data"],
                    "index/data uniform words exceed burst words")
        require(row["index_data_packets"]["vs_uniform"] + row["index_data_packets"]["gs_uniform"] <= bursts,
                "index/data uniform packets exceed bursts")
        actual = {
            "same_register_fast_packets": summary["fast_packets"],
            "same_register_fast_words": row["fast"],
            "consecutive_uniform_burst_packets": bursts,
            "consecutive_uniform_burst_words": burst,
        }
    # Loaded words include headers, padding, invalid headers and abandoned suffixes.
    # Their difference from executed payload is deliberately not an unexplained-word count.
    result = {
        "accepted_header_payload": {"declared_words": summary["declared_words"],
                                    "truncated_words": summary["truncated_words"],
                                    "executed_words": executed, "accounted_words": accounted,
                                    "unaccounted_words": 0},
        "loaded_list_words": summary["list_words"],
        "raw_list_conservation": "unknown_no_incomplete_header_or_abandoned_suffix_counters",
        "summary": summary,
        "histograms": {key: row[key] for key in ("mask", "extra", "shape")},
        "path_words_by_family": {key: row[key] for key in path_tables},
        "opportunities": {
            "index_plus_data_packets": row["index_data_packets"],
            "index_plus_data_extra_words": row["index_data_words"],
            "scalar_same_register_full_clear_words": row["scalar_same_register_full_clear"],
            "adjacent_same_register_packets": row["chain_same_register"],
            "adjacent_next_register_packets": row["chain_next_register"],
        },
        "acts_by_family": row["acts"],
        "opportunity_overlap": "index/data and adjacency are not disjoint from path words; do not sum",
        "pending_scope": "sampled after first write when extra-word path is selected",
        "mirrored_scope": MIRRORED_SCOPE[version],
    }
    if actual is not None:
        result["actual_fast_paths"] = actual
        result["actual_overlap"] = "index/data opportunities overlap actual uniform bursts; do not sum"
    return result


def parse_packet(data: bytes) -> dict:
    require(len(data) <= MAX_PACKET_BYTES, "packet input exceeds byte budget")
    try:
        row = tomllib.loads(data.decode("utf-8"))
    except (tomllib.TOMLDecodeError, UnicodeError, RecursionError) as exc:
        raise ValueError("invalid packet TOML") from exc
    keys(row, ("schema", "schema_version", "source", "crate_version", "batches_seen", "total"), "packet",
         ("window_start_batch", "window_marked", "window"))
    require(row["schema"] == PACKET_SCHEMA and type(row["schema_version"]) is int
            and row["schema_version"] in PACKET_LAYOUTS and row["source"] == PACKET_SOURCE,
            "unknown packet schema/source")
    version = row["schema_version"]
    require(type(row["crate_version"]) is str and 0 < len(row["crate_version"]) <= 128, "invalid crate version")
    batches = count(row["batches_seen"], "batches_seen")
    total = packet_scope(row["total"], "total", version)
    window, coverage = None, {"status": "unknown_not_requested", "start_batch": None}
    if "window_start_batch" in row:
        start = count(row["window_start_batch"], "window_start_batch")
        require(type(row.get("window_marked")) is bool, "missing/invalid window marker")
        require(row["window_marked"] == (start < batches), "window marker disagrees with batches begun")
        require(("window" in row) == row["window_marked"], "window table disagrees with marker")
        coverage = {"status": "observed_start_fence_only" if row["window_marked"] else "unknown_start_not_reached",
                    "start_batch": start}
        if row["window_marked"]:
            window = packet_scope(row["window"], "window", version)
            for table in PACKET_LAYOUTS[version]:
                for key, value in row["window"][table].items():
                    require(value <= row["total"][table][key], "window counter exceeds total")
    else:
        require("window_marked" not in row and "window" not in row, "window has no requested start")
    coverage.update({"unit": "zero-based next_frame batch", "end_batch_fence": None,
                     "includes_interrupted_final_attempted_batch": True,
                     "completed_batch_count": None})
    result = {"status": "observed", "batches_begun": batches, "crate_version": row["crate_version"],
              "total": total, "window": window, "window_coverage": coverage,
              "overflow_coverage": "u64 ranges and conservation checked; undetectable source wrap remains unknown"}
    # Version 1 output stays as it was; later versions name themselves.
    if version != 1:
        result["schema_version"] = version
    return result


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate JSON field")
        result[key] = value
    return result


def ns(value, timed: bool, where: str):
    if not timed:
        require(value is None, f"{where}: untimed value must be null")
        return None
    return count(value, where)


def parse_texture_row(text: str) -> dict:
    try:
        row = json.loads(text, object_pairs_hook=unique_object)
    except (json.JSONDecodeError, RecursionError) as exc:
        raise ValueError("invalid texture JSON") from exc
    keys(row, ("schema", "interval", "checked", "decoded", "misses_by_format", "evictions",
               "idle_pruned", "live", "list_peaks", "timed"), "texture")
    require(row["schema"] == TEXTURE_SCHEMA, "unknown texture schema")
    timed = row["timed"]
    require(type(timed) is bool, "timed must be boolean")
    interval = keys(row["interval"], ("first_list", "end_list", "lists", "complete"), "interval")
    first, end, lists = (count(interval[key], key) for key in ("first_list", "end_list", "lists"))
    require(first >= 1 and end > first and lists == end - first
            and type(interval["complete"]) is bool, "invalid command-list interval")
    require(not interval["complete"] or end % 600 == 0, "complete interval must end at a 600-list fence")
    checked = counts(row["checked"], ("calls", "absent", "untrusted", "trusted"), "checked")
    require(sum_u64([checked[key] for key in ("absent", "untrusted", "trusted")], "checked outcomes")
            == checked["calls"], "checked outcome conservation failed")
    decoded = counts(row["decoded"], ("calls", "hits", "misses", "hashed_bytes", "oversized"), "decoded")
    require(sum_u64([decoded["hits"], decoded["misses"]], "decoded outcomes") == decoded["calls"],
            "decoded outcome conservation failed")
    formats = keys(row["misses_by_format"], (), "misses_by_format", FORMATS)
    for name, values in formats.items():
        keys(values, FORMAT_FIELDS, f"format.{name}")
        for field in FORMAT_FIELDS:
            if field.endswith("_ns"):
                ns(values[field], timed, field)
            else:
                count(values[field], field)
        require(values["misses"] > 0 and values["replaced"] + values["new_keys"] == values["misses"]
                and values["packed"] <= values["packs_asked"] <= values["misses"], "invalid format partition")
        if timed:
            require(values["decode_max_ns"] <= values["decode_ns"] <= values["decode_max_ns"] * values["misses"],
                    "invalid decode timing bounds")
    require(sum_u64([value["misses"] for value in formats.values()], "format misses") == decoded["misses"],
            "format miss conservation failed")
    ev = keys(row["evictions"], ("inserts", "evicted", "evicted_bytes", "per_insert", "scanned_entries",
                                "scan_max_entries", "evict_insert_ns"), "evictions")
    for field in ("inserts", "evicted", "evicted_bytes", "scanned_entries", "scan_max_entries"):
        count(ev[field], field)
    ns(ev["evict_insert_ns"], timed, "evict_insert_ns")
    hist = counts(ev["per_insert"], EVICTION_BUCKETS, "per_insert")
    require(sum_u64(hist.values(), "insertion histogram") == ev["inserts"]
            and sum_u64([ev["inserts"], decoded["oversized"]], "miss insertions") == decoded["misses"],
            "insertion conservation failed")
    require(sum(hist[key] * lower for key, lower in zip(EVICTION_BUCKETS, EVICTION_MIN)) <= ev["evicted"],
            "evictions below histogram bound")
    if hist["16+"] == 0:
        require(ev["evicted"] <= sum(hist[key] * upper for key, upper in zip(EVICTION_BUCKETS, EVICTION_MAX)),
                "evictions above histogram bound")
    require(ev["evicted"] <= ev["scanned_entries"] <= ev["evicted"] * ev["scan_max_entries"]
            and ev["scan_max_entries"] <= ev["scanned_entries"], "invalid scan bounds")
    live = counts(row["live"], ("entries", "max_entries", "capacity", "accounted_bytes", "full_hash_duplicates"), "live")
    require(live["entries"] <= live["max_entries"] and live["entries"] <= live["capacity"]
            and live["full_hash_duplicates"] <= max(0, live["entries"] - 1), "invalid live snapshot")
    peaks = keys(row["list_peaks"], ("misses", "decoded_bytes", "evictions", "decode_ns"), "list_peaks")
    for field in ("misses", "decoded_bytes", "evictions"):
        count(peaks[field], field)
    ns(peaks["decode_ns"], timed, "peak decode_ns")
    decoded_bytes = sum_u64([value[key] for value in formats.values() for key in ("rgba_bytes", "packed_bytes")],
                            "decoded bytes")
    require(peaks["misses"] <= decoded["misses"] and peaks["evictions"] <= ev["evicted"]
            and peaks["decoded_bytes"] <= decoded_bytes, "list peak exceeds interval")
    if timed:
        require(peaks["decode_ns"] <= sum_u64([v["decode_ns"] for v in formats.values()], "decode ns"),
                "list timing peak exceeds interval")
    count(row["idle_pruned"], "idle_pruned")
    # Source-derived map API calls, not hash bucket probes or comparison counts.
    row["map_api_operations"] = (checked["calls"] + 2 * decoded["calls"] + decoded["misses"]
                                 + ev["inserts"] + ev["evicted"])
    return row


def texture_segment(rows: list[dict], reason: str) -> dict:
    def total(section, field):
        return sum(row[section][field] for row in rows)

    return {
        "first_list": rows[0]["interval"]["first_list"], "end_list": rows[-1]["interval"]["end_list"],
        "lists_observed": sum(row["interval"]["lists"] for row in rows), "rows": len(rows),
        "start_reason": reason, "tail_row_observed": not rows[-1]["interval"]["complete"],
        "checked": {field: total("checked", field) for field in rows[0]["checked"]},
        "decoded": {field: total("decoded", field) for field in rows[0]["decoded"]},
        "misses_by_format": {
            name: {field: sum(row["misses_by_format"].get(name, {}).get(field, 0) for row in rows)
                   for field in FORMAT_FIELDS if not field.endswith("_ns")}
            for name in FORMATS if any(name in row["misses_by_format"] for row in rows)
        },
        "evictions": {field: total("evictions", field) for field in
                      ("inserts", "evicted", "evicted_bytes", "scanned_entries")},
        "evictions_per_insert_histogram": {key: sum(row["evictions"]["per_insert"][key] for row in rows)
                                           for key in EVICTION_BUCKETS},
        "scan_max_entries": max(row["evictions"]["scan_max_entries"] for row in rows),
        "map_api_operations": sum(row["map_api_operations"] for row in rows),
        "idle_pruned": sum(row["idle_pruned"] for row in rows),
        "last_live_snapshot": rows[-1]["live"],
        "list_peaks": {key: max(row["list_peaks"][key] for row in rows)
                       for key in ("misses", "decoded_bytes", "evictions")},
    }


def parse_texture_log(data: bytes) -> dict:
    require(len(data) <= MAX_LOG_BYTES, "texture log exceeds byte budget")
    lines = data.splitlines()
    require(len(lines) <= MAX_LINES, "texture log exceeds line budget")
    groups, reasons, gaps, seen, modes = [], [], [], set(), set()
    row_count = 0
    for line in lines:
        require(len(line) <= MAX_LINE_BYTES, "texture log line exceeds byte budget")
        try:
            text = line.decode("utf-8")
        except UnicodeError as exc:
            raise ValueError("texture log is not UTF-8") from exc
        if "texture cache stats" not in text:
            continue
        require(text.count(TEXTURE_PREFIX) == 1, "unknown or ambiguous texture log schema marker")
        payload = text.split(TEXTURE_PREFIX, 1)[1]
        row = parse_texture_row(payload)
        canonical = json.dumps(row, sort_keys=True, separators=(",", ":"))
        require(canonical not in seen, "duplicate texture row/replayed input")
        seen.add(canonical)
        row_count += 1
        require(row_count <= MAX_ROWS, "texture log exceeds row budget")
        modes.add(row["timed"])
        require(len(modes) == 1, "mixed timing modes in one log")
        first = row["interval"]["first_list"]
        if not groups:
            groups.append([])
            reasons.append("first_observed_row")
        elif first < groups[-1][-1]["interval"]["end_list"]:
            groups.append([])
            reasons.append("restart_or_overlap_identity_unknown")
        else:
            previous = groups[-1][-1]["interval"]
            require(previous["complete"], "row follows a terminal Drop tail without a reset")
            if first > previous["end_list"]:
                gaps.append({"after_end_list": previous["end_list"], "before_first_list": first})
                groups.append([])
                reasons.append("gap_unobserved_lists")
        groups[-1].append(row)
    return {
        "status": "observed_intervals" if row_count else "unknown_no_rows",
        "line_count": row_count, "timed_input": next(iter(modes)) if modes else None,
        "segments": [texture_segment(rows, reason) for rows, reason in zip(groups, reasons)],
        "interval_gaps": gaps,
        "coverage": {"unit": "process_command_list submissions [first_list, end_list)",
                     "whole_run": "unknown", "prefix": "unknown_before_first_observation",
                     "tail": "unknown_process_exit_skips_Drop_tail",
                     "missing_rows_are_zero": False, "cache_instance_identity": "unknown_no_instance_id",
                     "same_interval_as_packet_window": "unknown_different_units_no_alignment"},
        "unknown_metrics": ["bucket_probes", "key_equality_comparisons", "unique_texture_count",
                            "size_histogram", "packing_fallback_causes", "normal_runtime_cost"],
        "semantics": "checked calls include discarded render-target checks; scans count yielded entries; "
                     "full-hash duplicates are not bucket/probe collisions; absent formats in valid rows have zero misses",
        "timing_output": "omitted; instrumented ns are validated but do not establish normal-runtime cost",
        "overflow_coverage": "u64 ranges and per-row conservation checked; undetectable source wrap remains unknown",
    }


def census(packet: Path | None = None, texture_log: Path | None = None) -> dict:
    require(packet is not None or texture_log is not None, "provide at least one input")
    result = {"schema": SCHEMA, "scope": "private diagnostic counts only; no timing or speedup claims",
              "packet": {"status": "unknown_not_supplied"}, "texture": {"status": "unknown_not_supplied"}}
    if packet is not None:
        data, identity = snapshot(packet, MAX_PACKET_BYTES)
        result["packet"] = {**parse_packet(data), "input": identity}
    if texture_log is not None:
        data, identity = snapshot(texture_log, MAX_LOG_BYTES)
        result["texture"] = {**parse_texture_log(data), "input": identity}
    return result


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--packet", type=Path, help="packet-census.toml")
    parser.add_argument("--texture-log", type=Path, help="log containing texture cache stats v1 JSON rows")
    args = parser.parse_args(argv)
    try:
        result = census(args.packet, args.texture_log)
    except (ValueError, OSError) as exc:
        # Do not echo source paths, payloads, or raw parse errors into receipts.
        print(f"gpu census rejected input ({type(exc).__name__})", file=sys.stderr)
        return 2
    print(json.dumps(result, indent=2, sort_keys=True, allow_nan=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
