#!/usr/bin/env python3
"""Offline CPU/log census and capture plans; never starts a game or profiler."""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import sys

_SPEC = importlib.util.spec_from_file_location("summarize_profile", Path(__file__).with_name("summarize_profile.py"))
summarize_profile = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(summarize_profile)

SCHEMA = 1
MAX_BYTES = 64 * 1024 * 1024
MAX_LINES = 1_000_000
HEADER = re.compile(r"^\s*(.*?)\s+(\d+)\s+(\d+\.\d+):\s+(.+?):\s+([0-9a-fA-F]+)\s+(.*?)\s+\((.*)\)\s*$")
FRAME = re.compile(r"^\s+([0-9a-fA-F]+)\s+(.*?)\s+\((.*)\)\s*$")
TEXTURES = re.compile(r"textures: (\d+) looked at, (\d+) decoded, (\d+) KiB hashed;")
WAIT = re.compile(r"waiting on gpu (\d+(?:\.\d+)?)%")


def identity(path: Path) -> dict:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": digest.hexdigest()}


def snapshot_input(path: Path) -> tuple[bytes, dict]:
    """Read once with a hard bound; identity and parsing use the same bytes."""
    with path.open("rb") as source:
        data = source.read(MAX_BYTES + 1)
    if len(data) > MAX_BYTES:
        raise ValueError(f"input exceeds {MAX_BYTES} byte budget: {path}")
    return data, {"path": str(path), "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


def input_identity(path: Path) -> dict:
    return snapshot_input(path)[1]


def bounded_lines(source):
    data = snapshot_input(source)[0] if isinstance(source, Path) else source
    lines = data.splitlines(keepends=True)
    if len(lines) > MAX_LINES or any(len(line) > 65536 for line in lines):
        raise ValueError("input exceeds line/count budget")
    for line in lines:
        yield line.decode("utf-8", errors="strict").rstrip("\n")


def category(symbol: str, dso: str, guest_dsos: set[str]) -> str:
    """Exclusive leaf classification, never an avoidable-cost estimate."""
    if dso in guest_dsos:
        return "guest_native"
    name = symbol.lower()
    if any(part in name for part in ("memcpy", "memmove", "memcmp", "memset")):
        return "data_movement_compare"
    if any(part in name for part in ("symphonia", "dsp_aac", "aac::", "zakuro_frontend::sound")):
        return "audio"
    if any(part in name for part in ("futex", "pthread_cond", "nanosleep", "poll@@", "poll@glibc")):
        return "wait_or_poll_cpu"
    if "zakuro_gpu::" in name:
        return "gles_submission" if "::raster::gles" in name else "pica_frontend"
    if any(part in dso.lower() for part in ("libnvidia", "libmali", "libgallium", "libglx", "libegl")):
        return "userspace_gl_driver"
    if any(part in name for part in ("zakuro_core::", "zakuro_kernel::")):
        return "core_kernel_services"
    if symbol in ("[unknown]", "??", "?") or dso in ("[unknown]", "?", ""):
        return "unknown"
    return "other_cpu"


def cpu_summary(samples: list[dict], malformed: int, source: str, guests: set[str]) -> dict:
    groups = defaultdict(list)
    for sample in samples:
        groups[sample["event"]].append(sample)
    events = {}
    for event, selected in sorted(groups.items()):
        leaves, threads, callers = Counter(), Counter(), Counter()
        stack_samples = 0
        for sample in selected:
            leaf = sample["frames"][0]
            leaves[category(leaf["symbol"], leaf["dso"], guests)] += 1
            threads[(sample["thread"], sample["tid"])] += 1
            stack = sample["frames"][1:]
            # A link-register suffix is not an unwound stack.
            if stack and source == "perf-script" and stack[0]["symbol"] not in ("[unknown]", "??", "?"):
                stack_samples += 1
                if any(x in leaf["symbol"].lower() for x in ("memcpy", "memmove", "memcmp", "memset")):
                    first = stack[0]
                    callers[(first["symbol"], first["dso"])] += 1
        denominator = len(selected)
        events[event] = {
            "sample_denominator": denominator,
            "exclusive_leaf_categories": [{"category": k, "samples": n, "percent_all_event_samples": 100 * n / denominator}
                                          for k, n in leaves.most_common()],
            "threads": [{"comm": comm, "tid": tid, "samples": n} for (comm, tid), n in threads.most_common()],
            "unwound_caller_samples": stack_samples,
            "unwound_caller_coverage": stack_samples / denominator,
            "data_movement_first_callers": [{"symbol": symbol, "dso": dso, "samples": n} for (symbol, dso), n in callers.most_common(40)],
        }
    return {"format": source, "events": events, "malformed_nonempty_lines": malformed,
            "complete_parse": malformed == 0, "all_parsed_samples": len(samples),
            "scope": "CPU sample observations; categories are exclusive leaf attribution, not time saved or GPU execution time",
            "limitations": ["sample rates, event weighting and lost samples require the original capture receipt",
                            "caller coverage counts a supplied resolved frame, not proof that unwinding is complete",
                            "events retain separate denominators and must not be summed as CPU time"]}


def parse_perf(path: Path, guests: set[str]) -> dict:
    samples, current, malformed = [], None, 0
    for line in bounded_lines(path):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        header = HEADER.fullmatch(line)
        if header:
            comm, tid, timestamp, event, address, symbol, dso = header.groups()
            current = {"thread": comm.strip(), "tid": int(tid), "timestamp": timestamp, "event": event,
                       "frames": [{"symbol": symbol, "dso": dso, "ip": int(address, 16)}]}
            samples.append(current)
            continue
        frame = FRAME.fullmatch(line)
        if frame and current:
            address, symbol, dso = frame.groups()
            value = {"symbol": symbol, "dso": dso, "ip": int(address, 16)}
            if value != current["frames"][-1]:
                current["frames"].append(value)
        else:
            malformed += 1
            current = None
    return cpu_summary(samples, malformed, "perf-script", guests)


def parse_host(path: Path, guests: set[str]) -> dict:
    samples, malformed = [], 0
    for line in bounded_lines(path):
        if not line.strip():
            continue
        sample = summarize_profile.parse_sample(line)
        if sample is None:
            malformed += 1
            continue
        thread, dso, offset = sample
        samples.append({"thread": thread, "tid": None, "event": "ITIMER_PROF",
                        "frames": [{"symbol": "?", "dso": dso, "ip": f"{offset:x}"}]})
    result = cpu_summary(samples, malformed, "host-ip", guests)
    result["sampler_capacity_may_have_been_reached"] = len(samples) + malformed >= summarize_profile.CAPACITY
    result["limitations"].append("host-IP input has no resolved symbols or unwound callers; use summarize_profile.py for safe ELF-offset symbolization")
    return result


def parse_log(path: Path) -> dict:
    intervals, waits, sound_unavailable = [], [], False
    for line in bounded_lines(path):
        match = TEXTURES.search(line)
        if match:
            looked, decoded, kib = map(int, match.groups())
            if decoded > looked:
                raise ValueError("decoded texture count exceeds looked count")
            intervals.append({"lookups": looked, "decoded": decoded,
                              "hashed_bytes_lower_bound": kib * 1024,
                              "hashed_bytes_upper_bound_exclusive": (kib + 1) * 1024})
        match = WAIT.search(line)
        if match:
            waits.append(float(match[1]))
        sound_unavailable |= "could not open the sound device" in line
    return {"texture_intervals": intervals, "texture_interval_count": len(intervals),
            "texture_hashed_bytes_lower_bound": sum(row["hashed_bytes_lower_bound"] for row in intervals),
            "queue_wait_percent_observations": waits,
            "audio_device_open_failure_observed": sound_unavailable,
            "audio_playback_verified": False,
            "hash_reasons": None, "gl_calls": None, "upload_bytes": None,
            "scope": "texture intervals flush after 5000 lookups and omit the unflushed tail; KiB is integer-truncated; wait samples have unknown wall durations and cannot be averaged as a run fraction"}


def manifest_context(path: Path) -> dict:
    data, input_id = snapshot_input(path) if isinstance(path, Path) else path
    # Apply the same byte and line limits as the other inputs.
    list(bounded_lines(data))
    value = json.loads(data)
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise ValueError("unsupported evaluator manifest schema")
    if not isinstance(value.get("trials", []), list):
        raise ValueError("manifest trials must be a list")
    trials = []
    for trial in value.get("trials", []):
        if not isinstance(trial, dict) or not isinstance(trial.get("probe", {}), dict):
            raise ValueError("manifest trial/probe must be objects")
        probe = trial.get("probe", {})
        report = probe.get("report", {})
        if not isinstance(report, dict):
            raise ValueError("manifest report must be an object")
        trials.append({"valid_in_original_evaluator": trial.get("valid"),
                       "work": {key: report.get(key) for key in ("requested_frames", "completed_frames", "attempted_frames", "skipped_frames", "interpreted_instructions", "recompiled_instructions", "top_pictures", "bottom_pictures")},
                       "guest_trace": probe.get("guest_trace"),
                       "measurement": trial.get("batch_telemetry", {}).get("summary", {}).get("measurement")})
    return {"identity": input_id, "source": value.get("source"),
            "executable": value.get("executable"), "rom": value.get("rom"), "library": value.get("library"),
            "experiment": value.get("experiment"), "build_metadata": value.get("build_metadata"),
            "inputs_changed_during_run": value.get("inputs_changed_during_run"), "trials": trials,
            "profile_capture_bound_to_manifest": False,
            "scope": "supplied context only; hash/profile timing linkage, equality, noise and audio admission are not established by this importer"}


def capture_plan(command: list[str], output: str) -> dict:
    if not command:
        raise ValueError("plan requires a runtime command after --")
    return {"status": "plan_only_not_executed", "output_prefix": output,
            "permission_status": "unknown_not_probed",
            "capture_argv": ["perf", "record", "--event", "cpu-clock:u", "--freq", "199", "--call-graph", "dwarf,16384", "--output", f"{output}.data", "--", *command],
            "export_argv": ["perf", "script", "--input", f"{output}.data", "--ns", "--fields", "comm,tid,time,event,ip,sym,dso"],
            "must_record": ["exact perf version, command/environment, capture exit/stderr and lost-sample diagnostics",
                            "source/build/runtime/ROM/native-library/settings/replay/save identities before and after capture",
                            "profile-window batch bounds, guest work, images/traces, audio-device/PCM proof",
                            "host preflight/noise receipts and CPU/GPU slot ownership"],
            "limits": ["diagnostic sampling changes overhead; exclude these runs from uninstrumented timing comparisons",
                       "cpu-clock samples omit off-CPU waits; use batch GPU_WAITED and separate scheduling traces if needed",
                       "userspace driver CPU is not GPU execution time; DWARF stacks may truncate or fail",
                       "do not elevate privileges or alter perf_event_paranoid automatically",
                       "hold production flags; frame-pointer rebuilds require separate identity and correctness validation"]}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    sub = parser.add_subparsers(dest="mode", required=True)
    plan = sub.add_parser("plan")
    plan.add_argument("--prefix", default="census")
    plan.add_argument("command", nargs=argparse.REMAINDER)
    read = sub.add_parser("read")
    read.add_argument("--host-profile", type=Path)
    read.add_argument("--perf-script", type=Path)
    read.add_argument("--log", type=Path)
    read.add_argument("--manifest", type=Path)
    read.add_argument("--guest-dso", action="append", default=[])
    args = parser.parse_args()
    try:
        result = {"schema_version": SCHEMA, "tool_identity": identity(Path(__file__)),
                  "host_parser_identity": identity(Path(summarize_profile.__file__))}
        if args.mode == "plan":
            command = args.command[1:] if args.command[:1] == ["--"] else args.command
            result["plan"] = capture_plan(command, args.prefix)
        else:
            if not any((args.host_profile, args.perf_script, args.log, args.manifest)):
                raise ValueError("read requires at least one explicit input")
            result["inputs"] = {}
            snapshots = {}
            for name, path in (("host_profile", args.host_profile), ("perf_script", args.perf_script), ("log", args.log), ("manifest", args.manifest)):
                if path:
                    if path.resolve() == args.output.resolve():
                        raise ValueError("output must not overwrite an input")
                    snapshots[name], result["inputs"][name] = snapshot_input(path)
            guests = set(args.guest_dso)
            if args.host_profile:
                result["host_profile"] = parse_host(snapshots["host_profile"], guests)
            if args.perf_script:
                result["perf_script"] = parse_perf(snapshots["perf_script"], guests)
            if args.log:
                result["log"] = parse_log(snapshots["log"])
            if args.manifest:
                result["context"] = manifest_context((snapshots["manifest"], result["inputs"]["manifest"]))
            result["timing_or_correctness_admitted"] = False
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
        print(json.dumps({"output": str(args.output), "mode": args.mode}))
        return 0
    except (OSError, ValueError, UnicodeError) as error:
        parser.exit(2, f"census error: {error}\n")


if __name__ == "__main__":
    sys.exit(main())
