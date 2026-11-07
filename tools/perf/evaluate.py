#!/usr/bin/env python3
"""Isolated Linux probe measurements and fail-closed baseline comparison.

No dependencies beyond Python 3.11+. ROMs and generated output stay local.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import time
import tomllib

_PROFILE_SPEC = importlib.util.spec_from_file_location("implementation_profiles", Path(__file__).with_name("implementation_profiles.py"))
profile_rules = importlib.util.module_from_spec(_PROFILE_SPEC)
_PROFILE_SPEC.loader.exec_module(profile_rules)
_BATCH_SPEC = importlib.util.spec_from_file_location("batch_metrics", Path(__file__).with_name("batch_metrics.py"))
batch_metrics = importlib.util.module_from_spec(_BATCH_SPEC)
_BATCH_SPEC.loader.exec_module(batch_metrics)
_DIAGNOSTIC_SPEC = importlib.util.spec_from_file_location("diagnostic_profiles", Path(__file__).with_name("diagnostic_profiles.py"))
diagnostic_rules = importlib.util.module_from_spec(_DIAGNOSTIC_SPEC)
_DIAGNOSTIC_SPEC.loader.exec_module(diagnostic_rules)
_RENDER_SPEC = importlib.util.spec_from_file_location("render_profiles", Path(__file__).with_name("render_profiles.py"))
render_rules = importlib.util.module_from_spec(_RENDER_SPEC)
_RENDER_SPEC.loader.exec_module(render_rules)

SCHEMA = 1
BUTTONS = set("A B X Y L R START SELECT UP DOWN LEFT RIGHT CUP CDOWN CLEFT CRIGHT".split())
RENDERER_ENV = ("DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "LD_LIBRARY_PATH",
                "LIBGL_DRIVERS_PATH", "VK_ICD_FILENAMES", "VK_DRIVER_FILES", "DRI_PRIME",
                "EGL_PLATFORM", "LIBGL_ALWAYS_SOFTWARE", "MESA_LOADER_DRIVER_OVERRIDE", "GALLIUM_DRIVER",
                "__EGL_VENDOR_LIBRARY_FILENAMES")
CGROUP_DEFAULTS = {
    "cpu.max": "max", "cpu.max.burst": "0", "cpu.weight": "100", "cpu.idle": "0",
    "cpu.uclamp.min": "0.00", "cpu.uclamp.max": "max",
    "memory.max": "max", "memory.high": "max", "memory.swap.max": "max",
    "memory.min": "0", "memory.low": "0", "memory.oom.group": "0",
    "cpuset.cpus.effective": None, "cpuset.mems.effective": None,
    "pids.max": "max", "io.max": "", "io.weight": "default 100",
}


def cgroup_envelope(proc_cgroup: Path = Path("/proc/self/cgroup"),
                    cgroup_root: Path = Path("/sys/fs/cgroup")) -> dict:
    """Observe inherited v2 limits without comparing transient unit names.

    The root has no CPU/memory bandwidth limit files by kernel design. A
    controller not delegated to a child has no new local constraint; its last
    ancestor's settings still apply. Every enabled-controller file must read.
    """
    errors, ancestry = [], []
    constraints = {name: set() for name in CGROUP_DEFAULTS}
    root_controllers = []
    try:
        entries = [line[3:] for line in proc_cgroup.read_text().splitlines() if line.startswith("0::")]
        if len(entries) != 1 or not entries[0].startswith("/") or ".." in Path(entries[0]).parts:
            raise ValueError("missing or invalid unified cgroup membership")
        root = cgroup_root.resolve(strict=True)
        node = (root / entries[0].lstrip("/")).resolve(strict=True)
        if not node.is_relative_to(root):
            raise ValueError("cgroup membership escapes mounted hierarchy")
        root_controllers = sorted((root / "cgroup.controllers").read_text().split())
        if not {"cpu", "memory", "cpuset"}.issubset(root_controllers):
            errors.append("required cpu/memory/cpuset controllers unavailable at hierarchy root")
        while True:
            observation = {"path": str(node), "values": {}}
            try:
                controllers = set((node / "cgroup.controllers").read_text().split())
                observation["controllers"] = sorted(controllers)
            except OSError as error:
                errors.append(f"{node}/cgroup.controllers: {error}")
                controllers = set(root_controllers)  # Do not silently assume inherited defaults.
            for name, default in CGROUP_DEFAULTS.items():
                controller = name.split(".", 1)[0]
                if controller not in controllers:
                    observation["values"][name] = "controller_not_delegated; inherited"
                    continue
                if node == root and controller != "cpuset":
                    observation["values"][name] = "hierarchy_root; intrinsic_unlimited"
                    continue
                try:
                    raw = (node / name).read_text().strip()
                    value = "\n".join(sorted(" ".join(line.split()) for line in raw.splitlines()))
                    if name == "cpu.max":
                        quota, period = value.split()
                        if int(period) <= 0 or (quota != "max" and int(quota) <= 0):
                            raise ValueError("invalid CPU quota/period")
                        value = "max" if quota == "max" else f"{int(quota)} {int(period)}"
                    if name.startswith("cpuset."):
                        numbers = set()
                        for span in value.split(","):
                            bounds = span.split("-")
                            first, last = int(bounds[0]), int(bounds[-1])
                            if len(bounds) > 2 or first < 0 or last < first or last > 1048575:
                                raise ValueError("invalid effective cpuset")
                            numbers.update(range(first, last + 1))
                        value = ",".join(map(str, sorted(numbers)))
                    observation["values"][name] = raw
                    if value != default:
                        constraints[name].add(value)
                except (OSError, ValueError) as error:
                    errors.append(f"{node}/{name}: {error}")
                    observation["values"][name] = "unknown"
            ancestry.append(observation)
            if node == root:
                break
            node = node.parent
    except (OSError, ValueError) as error:
        errors.append(str(error))
    return {"comparable": {"schema_version": 1, "known": not errors,
                           "root_controllers": root_controllers,
                           "constraints": {name: sorted(values) for name, values in constraints.items()}},
            "ancestry": ancestry, "errors": errors}


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def identity(path: Path) -> dict:
    return {"path": str(path.resolve()), "sha256": digest(path), "bytes": path.stat().st_size}


def tree_identity(path: Path | None) -> list[dict]:
    if path is None:
        return []
    result = []
    for item in sorted(path.rglob("*")):
        if item.is_symlink():
            raise ValueError(f"seed/profile symlinks are not supported: {item}")
        if item.is_file():
            result.append({"path": str(item.relative_to(path)), "sha256": digest(item), "bytes": item.stat().st_size})
    return result


def write_json(path: Path, value: dict) -> None:
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def validate_resolution(value: float) -> None:
    # GLES draws whole quarter steps, clamped to 1x..4x. Reject requests
    # that would label a different amount of raster work in the receipt.
    if (type(value) not in (int, float) or not math.isfinite(value)
            or not 1 <= value <= 4 or not float(value * 4).is_integer()):
        raise ValueError("resolution must be a whole quarter step from 1x through 4x")


def validate_input(spec: str) -> str:
    for entry in spec.split(","):
        frame, sep, buttons = entry.strip().partition(":")
        if not sep or not frame.isdecimal() or int(frame) >= 2**64 or not buttons:
            raise ValueError("input requires frame:BUTTON+BUTTON entries, e.g. 0:A,120:START")
        for button in buttons.split("+"):
            if button.upper() not in BUTTONS:
                match = re.fullmatch(r"@(\d+)x(\d+)", button)
                if match is None or not (int(match[1]) < 320 and int(match[2]) < 240):
                    raise ValueError(f"invalid button/touch: {button}")
    return spec


def parse_cpu_stat(text: str) -> dict:
    lines = text.splitlines()
    fields = lines[0].split() if lines else []
    if len(fields) < 9 or fields[0] != "cpu" or any(not value.isdecimal() for value in fields[1:]):
        raise ValueError("invalid /proc/stat aggregate CPU counters")
    names = ("user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal", "guest", "guest_nice")
    counters = dict(zip(names, map(int, fields[1:])))
    online = sum(bool(re.match(r"^cpu[0-9]+\s", line)) for line in lines)
    if online < 1:
        raise ValueError("/proc/stat has no online per-CPU rows")
    # guest/guest_nice are already included in user/nice; never add them twice.
    busy = sum(counters[name] for name in ("user", "nice", "system", "irq", "softirq", "steal"))
    return {"counters_ticks": counters, "busy_ticks": busy, "online_cpus": online}


def host_activity_snapshot() -> dict:
    snapshot, errors = {}, []
    try:
        snapshot.update(parse_cpu_stat(Path("/proc/stat").read_text()))
        snapshot["user_hz"] = os.sysconf("SC_CLK_TCK")
        if snapshot["user_hz"] <= 0:
            raise ValueError("invalid SC_CLK_TCK")
    except (OSError, ValueError) as error:
        errors.append(str(error))
    for name, path in (("loadavg", "/proc/loadavg"), ("meminfo", "/proc/meminfo")):
        try:
            text = Path(path).read_text()
            if name == "loadavg":
                loads = [float(value) for value in text.split()[:3]]
                if len(loads) != 3 or any(not math.isfinite(value) or value < 0 for value in loads):
                    raise ValueError("invalid load averages")
                snapshot["loadavg"] = text.strip()
            else:
                match = re.search(r"^MemAvailable:\s+(\d+)\s+kB$", text, re.MULTILINE)
                if match is None:
                    raise ValueError("MemAvailable unavailable")
                snapshot["mem_available_bytes"] = int(match[1]) * 1024
        except (OSError, ValueError) as error:
            errors.append(f"{name}: {error}")
    governors = {}
    try:
        for path in sorted(Path("/sys/devices/system/cpu/cpufreq").glob("policy*/scaling_governor")):
            governors[path.parent.name] = path.read_text().strip()
    except OSError as error:
        errors.append(f"governors: {error}")
    snapshot.update({"governors": governors, "governors_status": "read" if governors else "not_exposed",
                     "known": not errors, "errors": errors})
    return snapshot


def host_activity_delta(before: dict, after: dict, wall_seconds: float, child_cpu_seconds: float) -> dict:
    result = {"schema_version": 1, "known": False, "before": before, "after": after, "errors": []}
    try:
        if before.get("known") is not True or after.get("known") is not True:
            raise ValueError("host activity snapshot unavailable")
        if before["user_hz"] != after["user_hz"] or before["online_cpus"] != after["online_cpus"]:
            raise ValueError("CPU count or accounting tick rate changed")
        if before.get("governors") != after.get("governors"):
            raise ValueError("CPU frequency governor changed during trial")
        if (not math.isfinite(wall_seconds) or wall_seconds <= 0
                or not math.isfinite(child_cpu_seconds) or child_cpu_seconds < 0):
            raise ValueError("invalid child timing for host activity estimate")
        counters_before, counters_after = before["counters_ticks"], after["counters_ticks"]
        if any(counters_after[name] < value for name, value in counters_before.items()):
            raise ValueError("aggregate CPU counter regression")
        busy_seconds = (after["busy_ticks"] - before["busy_ticks"]) / before["user_hz"]
        # Short intervals can disagree by a tick on each online CPU.
        quantization = before["online_cpus"] / before["user_hz"]
        if busy_seconds + quantization < child_cpu_seconds:
            raise ValueError("host CPU accounting is inconsistent with child CPU usage")
        other_seconds = max(0.0, busy_seconds - child_cpu_seconds)
        result.update({"known": True, "host_online_cpus": before["online_cpus"],
                       "user_hz": before["user_hz"], "wall_seconds": wall_seconds,
                       "host_busy_cpu_seconds": busy_seconds, "child_cpu_seconds": child_cpu_seconds,
                       "estimated_other_cpu_seconds": other_seconds,
                       "other_cpu_fraction": other_seconds / (wall_seconds * before["online_cpus"]),
                       "scope": "estimated external CPU / all online host CPU capacity; not affinity isolation"})
    except (KeyError, TypeError, ValueError, ZeroDivisionError) as error:
        result["errors"].append(str(error))
    return result


def rss_components(status: str) -> dict | None:
    """Read one status snapshot; component maxima are deliberately not summed."""
    names = ("VmRSS", "RssAnon", "RssFile", "RssShmem")
    values = dict(re.findall(r"^(VmRSS|RssAnon|RssFile|RssShmem):\s+(\d+)\s+kB$", status, re.MULTILINE))
    if not all(name in values for name in names):
        return None
    return {name: int(values[name]) for name in names}


def record_rss_sample(observations: dict, status: str, elapsed: float) -> None:
    sample = rss_components(status)
    if sample is None:
        return
    observations["samples"] += 1
    peak = observations["at_sampled_peak_rss"]
    if peak is None or sample["VmRSS"] > peak["VmRSS"]:
        observations["at_sampled_peak_rss"] = {**sample, "elapsed_seconds": elapsed}


def measure(command: list[str], cwd: Path, env: dict, timeout: float,
            affinity: list[int], memory_mib: int | None, preload_library: Path | None = None) -> dict:
    """Reap this exact child, so peak RSS is not inherited from earlier trials."""
    def limits() -> None:
        if affinity:
            os.sched_setaffinity(0, affinity)

    activity_before = host_activity_snapshot()
    started = time.monotonic()
    timed_out = False
    memory_exceeded = False
    rss_observations = {"samples": 0, "sample_interval_seconds": 0.01, "units": "KiB",
                        "at_sampled_peak_rss": None,
                        "scope": "One /proc status snapshot at the largest sampled VmRSS; not component maxima or wait4 peak, and excludes GPU/OS memory."}
    preload_proof = ({"requested_path": str(preload_library), "observed": False, "mapping_lines": []}
                     if preload_library else None)
    with (cwd / "stdout.log").open("wb") as out, (cwd / "stderr.log").open("wb") as err:
        child = subprocess.Popen(command, cwd=cwd, env=env, stdout=out, stderr=err,
                                 start_new_session=True, preexec_fn=limits)
        try:
            while True:
                pid, status, usage = os.wait4(child.pid, os.WNOHANG)
                if pid:
                    break
                if preload_proof is not None and not preload_proof["observed"]:
                    try:
                        lines = Path(f"/proc/{child.pid}/maps").read_text().splitlines()
                        preload_proof["mapping_lines"] = [line for line in lines
                            if len(fields := line.split(maxsplit=5)) == 6 and fields[5] == str(preload_library)]
                        preload_proof["observed"] = bool(preload_proof["mapping_lines"])
                    except OSError:
                        pass  # Missing proof invalidates the profile rather than claiming a loaded allocator.
                try:
                    status_text = Path(f"/proc/{child.pid}/status").read_text()
                    record_rss_sample(rss_observations, status_text, time.monotonic() - started)
                    if memory_mib is not None:
                        rss = re.search(r"^VmRSS:\s+(\d+)\s+kB", status_text, re.MULTILINE)
                        memory_exceeded = rss is not None and int(rss[1]) > memory_mib * 1024
                except OSError:
                    pass  # The process may exit between wait4 and the status read.
                timed_out = time.monotonic() - started > timeout
                if timed_out or memory_exceeded:
                    os.killpg(child.pid, signal.SIGKILL)
                    _, status, usage = os.wait4(child.pid, 0)
                    break
                time.sleep(0.01)
        except BaseException:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            _, status, _ = os.wait4(child.pid, 0)
            child.returncode = os.waitstatus_to_exitcode(status)
            raise
        child.returncode = os.waitstatus_to_exitcode(status)
    wall_seconds = time.monotonic() - started
    activity_after = host_activity_snapshot()
    cpu_seconds = usage.ru_utime + usage.ru_stime
    return {"exit_code": child.returncode, "timed_out": timed_out,
            "memory_limit_exceeded": memory_exceeded,
            "wall_seconds": wall_seconds,
            "user_cpu_seconds": usage.ru_utime, "system_cpu_seconds": usage.ru_stime,
            "cpu_seconds": cpu_seconds, "peak_rss_kib": usage.ru_maxrss,
            "rss_components": rss_observations,
            "minor_page_faults": usage.ru_minflt, "major_page_faults": usage.ru_majflt,
            "block_inputs": usage.ru_inblock, "block_outputs": usage.ru_oublock,
            "preload_proof": preload_proof,
            "host_activity": host_activity_delta(activity_before, activity_after, wall_seconds, cpu_seconds)}


def expected_screenshots(frames: int) -> set[str]:
    # Same cadence/deduplication as Probe::shot_frames, including short workloads.
    return {f"frame-{n * max(frames // 8, 1) - 1:05}.bmp" for n in range(1, 9)
            if n * max(frames // 8, 1) - 1 < frames}


def validate_probe_report(report: dict, frames: int) -> list[str]:
    required = {"schema_version": int, "requested_frames": int, "completed_frames": int,
                "attempted_frames": int, "elapsed_seconds": (float, int), "stop_reason": str,
                "recompiled_instructions": int, "interpreted_instructions": int,
                "top_pictures": int, "bottom_pictures": int, "skipped_frames": int, "screenshots": list}
    if not isinstance(report, dict):
        return ["missing probe report"]
    errors = [f"missing/invalid field: {key}" for key, kind in required.items()
              if key not in report or not isinstance(report[key], kind) or isinstance(report[key], bool)]
    if errors:
        return errors
    if report["schema_version"] != SCHEMA:
        errors.append("unsupported probe schema")
    if any(report[key] != frames for key in ("requested_frames", "attempted_frames", "completed_frames")):
        errors.append("frame workload incomplete or different")
    if report["stop_reason"] != "frame_limit":
        errors.append("probe did not stop at frame limit")
    if report["recompiled_instructions"] <= 0 or report["interpreted_instructions"] < 0:
        errors.append("invalid guest instruction workload")
    if report["top_pictures"] <= 0 or report["bottom_pictures"] <= 0:
        errors.append("one or both screens drew no pictures")
    if report["skipped_frames"] != 0:
        errors.append("frames were skipped")
    if not math.isfinite(report["elapsed_seconds"]) or report["elapsed_seconds"] <= 0:
        errors.append("invalid probe duration")
    names = report["screenshots"]
    if (any(not isinstance(name, str) or Path(name).name != name for name in names)
            or len(names) != len(expected_screenshots(frames)) or set(names) != expected_screenshots(frames)):
        errors.append("missing, duplicate, unsafe or unexpected sampled frames")
    return errors


def warning_receipt(section: str, frames: int) -> list[dict]:
    if not isinstance(section, str):
        raise ValueError("missing warnings/errors section")
    rows, none = [], 0
    for line in section.strip().splitlines():
        if not line or line == "Numbers are written as #. Count, the frame it first came at, and the message.":
            continue
        if line == "None.":
            none += 1
            continue
        match = re.fullmatch(r"- ([1-9][0-9]*) × \(from frame ([0-9]+)\) ((?:WARN|ERROR) .+)", line)
        if not match or int(match[2]) > frames:
            raise ValueError("unrecognized warnings/errors entry")
        rows.append({"count": int(match[1]), "first_frame": int(match[2]), "message": match[3]})
    if (not rows and none != 1) or (rows and none) or len({row["message"] for row in rows}) != len(rows):
        raise ValueError("missing/contradictory warnings/errors entries")
    return sorted(rows, key=lambda row: row["message"])


def probe_result(trial: Path, frames: int) -> dict:
    reports = list(trial.glob("probe-*/report.toml"))
    if len(reports) != 1:
        return {"valid": False, "errors": [f"expected one report.toml, found {len(reports)}"]}
    report_path = reports[0]
    try:
        report = tomllib.loads(report_path.read_text())
        errors = validate_probe_report(report, frames)
        if errors:
            return {"valid": False, "errors": errors, "report": report}
        markdown = report_path.with_name("report.md")
        text = markdown.read_text()
        if text.count("## Warnings and errors\n") != 1 or "\n## Threads at the end" not in text:
            raise ValueError("missing/ambiguous warnings/errors report section")
        section = text.split("## Warnings and errors\n", 1)[1].split("\n## Threads at the end", 1)[0]
        warnings = {"section": section, "entries": warning_receipt(section, frames), "report_md_sha256": digest(markdown)}
        screenshots = {}
        for name in report["screenshots"]:
            if not isinstance(name, str) or Path(name).name != name:
                errors.append("unsafe screenshot filename")
                continue
            screenshot = report_path.parent / name
            if not screenshot.is_file():
                errors.append(f"missing screenshot: {name}")
            else:
                with screenshot.open("rb") as source:
                    header = source.read(54)
                if (len(header) != 54 or header[:2] != b"BM"
                        or struct.unpack_from("<I", header, 2)[0] != screenshot.stat().st_size
                        or min(struct.unpack_from("<ii", header, 18)) <= 0):
                    errors.append(f"invalid screenshot BMP: {name}")
                screenshots[name] = digest(screenshot)
        trace_receipt = None
        if trace_name := report.get("guest_trace_file"):
            if trace_name != "guest-trace.toml":
                errors.append("unexpected guest trace filename")
            else:
                trace_path = report_path.parent / trace_name
                if trace_path.stat().st_size > 128 * 1024 * 1024:
                    raise ValueError("guest trace exceeds diagnostic read budget")
                trace = tomllib.loads(trace_path.read_text())
                every = trace.get("sampling_interval_frames")
                checkpoints = trace.get("checkpoints", [])
                if (trace.get("schema_version") != 1 or not isinstance(every, int) or isinstance(every, bool)
                        or every < 1 or trace.get("dropped_checkpoints") != 0
                        or not isinstance(checkpoints, list)
                        or any(not isinstance(point, dict) for point in checkpoints)
                        or [point.get("completed_frames") for point in checkpoints] != list(range(every, frames + 1, every))):
                    errors.append("guest trace incomplete, unsupported or truncated")
                trace_receipt = {"path": str(trace_path), "sha256": digest(trace_path),
                                 "sampling_interval_frames": every, "checkpoint_count": len(checkpoints)}
        # A stopped/black boot can be fast and deterministic; require scene review separately.
        result = {"valid": not errors, "errors": errors, "report": report, "screenshots": screenshots,
                  "warnings_errors": warnings, "guest_trace": trace_receipt, "report_sha256": digest(report_path)}
        artifacts = sorted(path.name for name in ("packet-census.toml", "render-geometry.toml")
                           if (path := report_path.with_name(name)).is_file())
        if artifacts:
            result["diagnostic_artifacts"] = artifacts
        return result
    except (OSError, ValueError, TypeError) as error:
        return {"valid": False, "errors": [f"unreadable probe: {error}"]}


def git_provenance(root: Path) -> dict:
    def git(*args: str) -> str:
        result = subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True, check=False)
        return result.stdout.strip() if result.returncode == 0 else "unavailable"
    return {"head": git("rev-parse", "HEAD"), "status": git("status", "--porcelain"),
            "diff_sha256": hashlib.sha256(git("diff", "--binary", "HEAD").encode()).hexdigest(),
            "note": "checkout provenance only; supply build metadata to bind binaries to source"}


def run(args: argparse.Namespace) -> int:
    if sys.platform != "linux" or not hasattr(os, "wait4"):
        raise ValueError("measurement currently requires Linux os.wait4 (RSS units are Linux KiB)")
    if not 8 <= args.frames < 2**64 or args.repeats < 1 or not math.isfinite(args.timeout) or args.timeout <= 0:
        raise ValueError("frames >= 8, repeats >= 1 and timeout > 0 are required")
    if args.rss_limit_mib is not None and args.rss_limit_mib < 1:
        raise ValueError("memory limit must be positive")
    if not math.isfinite(args.max_other_cpu_fraction) or not 0 <= args.max_other_cpu_fraction <= 1:
        raise ValueError("--max-other-cpu-fraction must be finite and between 0 and 1")
    if args.trace_every is not None and (not 1 <= args.trace_every <= args.frames or args.frames // args.trace_every > 100000):
        raise ValueError("--trace-every must produce between 1 and 100000 checkpoints")
    validate_resolution(args.resolution)
    if not 0 <= args.clock < 2**64:
        raise ValueError("clock must be nonnegative and fit u64")
    validate_input(args.input)
    if args.batch_telemetry and not 0 <= args.measure_start < (args.measure_end or args.frames) <= args.frames <= 100000:
        raise ValueError("batch telemetry requires 0 <= start < end <= frames <= 100000")
    if not args.batch_telemetry and (args.measure_start != 0 or args.measure_end is not None):
        raise ValueError("measurement ranges require --batch-telemetry")
    diagnostic = diagnostic_source = diagnostic_data = diagnostic_backend = None
    diagnostic_path = getattr(args, "diagnostic_profile", None)
    if diagnostic_path:
        diagnostic_path = diagnostic_path.resolve(strict=True)
        diagnostic, diagnostic_source, diagnostic_data = diagnostic_rules.load(diagnostic_path)
        diagnostic_backend = diagnostic_rules.backend_inputs(diagnostic)
        if diagnostic.get("packet_window_start", 0) >= args.frames:
            raise ValueError("packet window start must be within the executed frame range")
    implementation = None
    implementation_source = None
    allocator_before = None
    if args.implementation_profile:
        args.implementation_profile = args.implementation_profile.resolve(strict=True)
        implementation_source = identity(args.implementation_profile)
        implementation = profile_rules.validate(json.loads(args.implementation_profile.read_text()))
        allocator = implementation["knobs"]["allocator"]
        if allocator["kind"] == "gnu_preload":
            allocator_path = Path(allocator["path"]).resolve(strict=True)
            allocator_before = identity(allocator_path)
            if allocator_before["sha256"] != allocator["sha256"]:
                raise ValueError("implementation-profile allocator differs from its required SHA-256")
            allocator["path"] = str(allocator_path)
    render_path = getattr(args, "render_profile", None)
    render, render_source = render_rules.load(render_path.absolute()) if render_path else (None, None)
    for key in ("rom", "executable", "library"):
        path = getattr(args, key).resolve(strict=True)
        if not path.is_file():
            raise ValueError(f"{key} must be a regular file")
        setattr(args, key, path)
    affinity = sorted({int(cpu) for cpu in args.cpu_affinity.split(",")}) if args.cpu_affinity else []
    allowed = sorted(os.sched_getaffinity(0))
    if any(cpu not in allowed for cpu in affinity):
        raise ValueError(f"CPU affinity must be a subset of {allowed}")
    args.output = args.output.resolve()
    for key in ("seed_data", "seed_config", "seed_cache", "titles"):
        source = getattr(args, key)
        if source:
            if source.is_symlink() or not source.is_dir():
                raise ValueError(f"{key} must be a real directory")
            source = source.resolve()
            if args.output.is_relative_to(source):
                raise ValueError(f"output cannot be inside {key}")
            setattr(args, key, source)
    args.output.mkdir(parents=True, exist_ok=False)
    seed = tree_identity(args.seed_data)
    profiles = tree_identity(args.titles)
    config = tree_identity(args.seed_config)
    cache = tree_identity(args.seed_cache)
    if render:
        render_rules.check_run(render, resolution=args.resolution, frames=args.frames, diagnostic=diagnostic,
            seeded_config=[item["path"] for item in config],
            implementation_settings=profile_rules.settings(implementation) if implementation else None,
            implementation_environment=profile_rules.environment(implementation) if implementation else None)
    if implementation and any(Path(item["path"]).name == "settings.toml" for item in seed + config):
        raise ValueError("profile mode generates settings.toml; remove seeded settings.toml to avoid undeclared overrides")
    cgroup_start = cgroup_envelope()
    renderer_environment = {key: os.environ[key] for key in RENDERER_ENV if key in os.environ}
    if diagnostic:
        renderer_environment.pop("XDG_RUNTIME_DIR", None)
        if runtime := (diagnostic.get("audio_backend") or {}).get("xdg_runtime_dir"):
            renderer_environment["XDG_RUNTIME_DIR"] = runtime["path"]
    experiment = {"frames": args.frames, "clock_ms_since_1900": args.clock, "input": args.input,
                  "resolution": args.resolution, "cpu_affinity": affinity or allowed,
                  "rss_watchdog_limit_mib": args.rss_limit_mib, "seed_data": seed,
                  "max_other_cpu_fraction": args.max_other_cpu_fraction,
                  "guest_trace_interval_frames": args.trace_every,
                  "resource_envelope": cgroup_start["comparable"],
                  "seed_config": config, "profiles": profiles,
                  "cache_policy": "cloned immutable application cache seed" if args.seed_cache else "fresh application cache per trial; host page cache uncontrolled",
                  "seed_cache": cache, "driver_cache": args.driver_cache,
                  "probe_observer": {"batch_telemetry": args.batch_telemetry, "capture": args.capture,
                                     "measure_start": args.measure_start, "measure_end": args.measure_end or args.frames},
                  "renderer_environment": renderer_environment}
    if implementation:
        experiment["implementation_constraints"] = implementation["constraints"]
    manifest = {"schema_version": SCHEMA, "rom": identity(args.rom), "executable": identity(args.executable),
                "library": identity(args.library), "harness": {**identity(Path(__file__)),
                    "implementation_profiles_sha256": digest(Path(profile_rules.__file__))},
                "experiment": experiment, "repeats": args.repeats,
                "host": {"uname": list(platform.uname()), "python": sys.version,
                         "cpuinfo": re.sub(r"^cpu MHz\s*:.*\n", "", Path("/proc/cpuinfo").read_text(), flags=re.MULTILINE),
                         "meminfo": Path("/proc/meminfo").read_text()},
                "source": git_provenance(Path(__file__).resolve().parents[2]),
                "build_metadata": identity(args.build_metadata) if args.build_metadata else None,
                "scene_evidence": identity(args.scene_evidence) if args.scene_evidence else None,
                "scene_review": json.loads(args.scene_evidence.read_text()) if args.scene_evidence else None,
                "cgroup_observations": {"before": cgroup_start},
                "status": "running", "trials": []}
    if implementation:
        manifest["implementation_profile"] = {"declaration": implementation,
            "declaration_sha256": profile_rules.declaration_hash(implementation),
            "source": implementation_source, "allocator_before": allocator_before}
        shutil.copy2(args.implementation_profile, args.output / "implementation-profile.json")
    if diagnostic:
        experiment["diagnostic_profile"] = diagnostic
        manifest["evidence_class"] = diagnostic_rules.NON_TIMING
        manifest["diagnostic_profile"] = {"declaration": diagnostic,
            "declaration_sha256": diagnostic_rules.declaration_hash(diagnostic),
            "source": diagnostic_source, "backend_before": diagnostic_backend}
        manifest["harness"]["diagnostic_profiles_sha256"] = digest(Path(diagnostic_rules.__file__))
        (args.output / "diagnostic-profile.json").write_bytes(diagnostic_data)
    if render:
        experiment["render_profile"] = render
        manifest["render_profile"] = {"declaration": render,
            "declaration_sha256": render_rules.declaration_hash(render), "source": render_source}
        manifest["harness"]["render_profiles_sha256"] = digest(Path(render_rules.__file__))
        if not render_rules.timing_eligible(render):
            manifest["evidence_class"] = diagnostic_rules.NON_TIMING
    write_json(args.output / "manifest.json", manifest)
    manifest["harness"]["batch_metrics_sha256"] = digest(Path(batch_metrics.__file__))
    for index in range(args.repeats):
        trial = args.output / f"trial-{index + 1:02}"
        trial.mkdir()
        for name, source in (("data", args.seed_data), ("config", args.seed_config), ("titles", args.titles)):
            if source:
                shutil.copytree(source, trial / name)
            else:
                (trial / name).mkdir()
        for name in ("home", "cache", "tmp", "xdg-data"):
            if name == "cache" and args.seed_cache:
                shutil.copytree(args.seed_cache, trial / name)
            else:
                (trial / name).mkdir()
        if implementation:
            (trial / "data/settings.toml").write_text(profile_rules.settings(implementation))
        render_text = render_rules.config_settings(render) if render else None
        if render_text is not None:
            with (trial / "config/settings.toml").open("x") as target:
                target.write(render_text)
        env = {**({"PATH": os.environ["PATH"]} if "PATH" in os.environ else {}), **renderer_environment}
        env.update({"HOME": str(trial / "home"), "XDG_CONFIG_HOME": str(trial / "config"),
                    "XDG_CACHE_HOME": str(trial / "cache"), "XDG_DATA_HOME": str(trial / "xdg-data"),
                    "TMPDIR": str(trial / "tmp"), "POCKET_NATIVE_CONFIG": str(trial / "config"),
                    "POCKET_NATIVE_TITLES": str(trial / "titles"), "ZAKURO_DATA": str(trial / "data"),
                    "ZAKURO_RECOMPILED": str(args.library), "ZAKURO_CLOCK": str(args.clock),
                    "ZAKURO_INPUT": args.input, "ZAKURO_RESOLUTION": str(args.resolution),
                    "ZAKURO_FRAMESKIP": "0", "RUST_LOG": "info", "LC_ALL": "C", "TZ": "UTC"})
        command = [str(args.executable), "--probe", str(args.rom), str(args.frames)]
        if args.trace_every is not None:
            env["ZAKURO_PROBE_TRACE_EVERY"] = str(args.trace_every)
        if args.batch_telemetry:
            env["ZAKURO_PROBE_TELEMETRY"] = "1"
        if args.capture == "boundaries":
            env["ZAKURO_PROBE_CAPTURE"] = "boundaries"
        if args.driver_cache != "uncontrolled":
            env.update({"__GL_SHADER_DISK_CACHE": "0" if args.driver_cache == "off" else "1",
                        "MESA_SHADER_CACHE_DISABLE": "true" if args.driver_cache == "off" else "false",
                        "__GL_SHADER_DISK_CACHE_PATH": str(trial / "cache/driver-nvidia"),
                        "MESA_SHADER_CACHE_DIR": str(trial / "cache/driver-mesa")})
        if implementation:
            env.update(profile_rules.environment(implementation))
        if diagnostic:
            env.update(diagnostic_rules.environment(diagnostic))
        if render:
            env.update(render_rules.environment(render, trial))
        result = {"trial": trial.name, "command": command, "environment": env}
        if render:
            result["render_settings"] = identity(trial / "config/settings.toml") if render_text is not None else None
        if implementation:
            result["generated_settings_sha256"] = digest(trial / "data/settings.toml")
        result["resource_envelope"] = cgroup_envelope()["comparable"]
        try:
            if diagnostic:
                result["diagnostic_backend"] = diagnostic_rules.stage_backend(diagnostic, trial)
            preload_path = Path(implementation["knobs"]["allocator"]["path"]) if allocator_before else None
            result["metrics"] = measure(command, trial, env, args.timeout, affinity, args.rss_limit_mib, preload_path)
            result["host_activity"] = result["metrics"].pop("host_activity")
            result["probe"] = probe_result(trial, args.frames)
            if args.batch_telemetry:
                reports = list(trial.glob("probe-*/report.toml"))
                if len(reports) != 1:
                    raise ValueError("expected one probe for batch telemetry")
                timing_path = reports[0].with_name("batch-timing.csv")
                result["batch_telemetry"] = {"source": identity(timing_path),
                    "summary": batch_metrics.read(timing_path, args.frames, args.measure_start, args.measure_end or args.frames)}
            if (args.trace_every is not None and (result["probe"].get("guest_trace") or {}).get("sampling_interval_frames") != args.trace_every):
                result["probe"]["errors"].append("requested guest trace missing or using a different interval")
                result["probe"]["valid"] = False
            stderr_text = (trial / "stderr.log").read_text(errors="replace")
            if (diagnostic or result["probe"].get("diagnostic_artifacts")
                    or any(key in result["probe"].get("report", {}) for key in ("audio", "render_geometry_file"))
                    or "texture cache stats" in stderr_text):
                result["diagnostics"] = diagnostic_rules.collect(trial, result["probe"].get("report", {}),
                    result["probe"].get("screenshots", {}), diagnostic)
                manifest["evidence_class"] = diagnostic_rules.NON_TIMING
                manifest["harness"].setdefault("diagnostic_profiles_sha256", digest(Path(diagnostic_rules.__file__)))
                if reader_source := result["diagnostics"].get("gpu_reader_source"):
                    initial = manifest["harness"].setdefault("diagnostic_gpu_reader_sha256", reader_source["sha256"])
                    if initial != reader_source["sha256"]:
                        result["diagnostics"]["valid"] = False
                        result["diagnostics"]["errors"].append("GPU census reader changed between trials")
            result["renderer_log_lines"] = [line for line in stderr_text.splitlines()
                                            if re.search(r"renderer|vendor|OpenGL|Vulkan|EGL|llvmpipe", line, re.IGNORECASE)][:30]
            result["renderer_identity"] = sorted({match[1].strip() for line in result["renderer_log_lines"]
                                                  if (match := re.search(r"drawing with OpenGL ES on (.+)", line))})
            if render:
                result["render_observation"] = render_rules.observe(render,
                    geometry=(result.get("diagnostics") or {}).get("geometry", {}).get("record"),
                    renderer_log_lines=result["renderer_log_lines"], trial=trial)
                if render_text is not None:
                    result["render_settings_after"] = identity(trial / "config/settings.toml")
            if implementation:
                result["implementation_profile_proof"] = profile_rules.proof(implementation,
                    (trial / "stderr.log").read_text(errors="replace").splitlines(), result["metrics"]["preload_proof"])
                result["generated_settings"] = identity(trial / "data/settings.toml")
            result["resource_envelope_after"] = cgroup_envelope()["comparable"]
            result["valid"] = (result["metrics"]["exit_code"] == 0 and not result["metrics"]["timed_out"]
                               and not result["metrics"]["memory_limit_exceeded"]
                               and result["resource_envelope"].get("known") is True
                               and result["resource_envelope"] == experiment["resource_envelope"]
                               and result["resource_envelope_after"] == result["resource_envelope"]
                               and result["probe"]["valid"])
            if implementation:
                result["valid"] &= result["implementation_profile_proof"]["verified"]
            if render:
                result["valid"] &= result["render_observation"]["admitted"]
                if render_text is not None:
                    result["valid"] &= result["render_settings_after"] == result["render_settings"]
            if "diagnostics" in result:
                result["valid"] &= result["diagnostics"]["valid"]
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            result.update(valid=False, error=str(error))
        write_json(trial / "result.json", result)
        manifest["trials"].append(result)
        write_json(args.output / "manifest.json", manifest)
    changed = [key for key in ("rom", "executable", "library")
               if identity(getattr(args, key)) != manifest[key]]
    if digest(Path(__file__)) != manifest["harness"]["sha256"]:
        changed.append("harness")
    if digest(Path(profile_rules.__file__)) != manifest["harness"]["implementation_profiles_sha256"]:
        changed.append("implementation_profile_support")
    if implementation:
        if identity(args.implementation_profile) != implementation_source:
            changed.append("implementation_profile")
        if allocator_before:
            allocator_after = identity(Path(implementation["knobs"]["allocator"]["path"]))
            manifest["implementation_profile"]["allocator_after"] = allocator_after
            if allocator_after != allocator_before:
                changed.append("implementation_allocator")
    if digest(Path(batch_metrics.__file__)) != manifest["harness"]["batch_metrics_sha256"]:
        changed.append("batch_metrics_support")
    if diagnostic or "diagnostic_profiles_sha256" in manifest["harness"]:
        if digest(Path(diagnostic_rules.__file__)) != manifest["harness"]["diagnostic_profiles_sha256"]:
            changed.append("diagnostic_profile_support")
    if "diagnostic_gpu_reader_sha256" in manifest["harness"]:
        try:
            if digest(Path(__file__).with_name("gpu_census.py")) != manifest["harness"]["diagnostic_gpu_reader_sha256"]:
                changed.append("diagnostic_gpu_reader")
        except OSError:
            changed.append("diagnostic_gpu_reader")
    if diagnostic:
        if identity(diagnostic_path) != diagnostic_source:
            changed.append("diagnostic_profile")
        try:
            manifest["diagnostic_profile"]["backend_after"] = diagnostic_rules.backend_inputs(diagnostic)
            if manifest["diagnostic_profile"]["backend_after"] != diagnostic_backend:
                changed.append("diagnostic_backend")
        except (OSError, ValueError) as error:
            manifest["diagnostic_profile"]["backend_after_error"] = str(error)
            changed.append("diagnostic_backend")
    if render:
        if identity(Path(render_source["path"])) != render_source:
            changed.append("render_profile")
        if digest(Path(render_rules.__file__)) != manifest["harness"]["render_profiles_sha256"]:
            changed.append("render_profile_support")
    for key, initial in (("seed_data", seed), ("seed_config", config), ("seed_cache", cache), ("titles", profiles)):
        if tree_identity(getattr(args, key)) != initial:
            changed.append(key)
    cgroup_end = cgroup_envelope()
    manifest["cgroup_observations"]["after"] = cgroup_end
    if cgroup_start["comparable"] != cgroup_end["comparable"]:
        changed.append("resource_envelope")
    manifest["inputs_changed_during_run"] = changed
    manifest["status"] = "complete" if all(t["valid"] for t in manifest["trials"]) and not changed else "incomplete"
    write_json(args.output / "manifest.json", manifest)
    print(json.dumps({"manifest": str(args.output / "manifest.json"), "status": manifest["status"]}))
    return 0 if manifest["status"] == "complete" else 2


def validate_trial_receipt(manifest: dict, trial: dict) -> list[str]:
    """Recheck captured runner facts; a stored valid=true is never sufficient."""
    errors = []
    if not isinstance(trial, dict):
        return ["invalid trial receipt shape"]
    experiment = manifest.get("experiment", {})
    frames, clock, resolution, script = (experiment.get(key) for key in
                                         ("frames", "clock_ms_since_1900", "resolution", "input"))
    if (type(frames) is not int or not 8 <= frames < 2**64 or type(clock) is not int or not 0 <= clock < 2**64
            or type(resolution) not in (int, float) or not math.isfinite(resolution) or resolution <= 0
            or not isinstance(script, str)):
        return ["missing/invalid fixed experiment workload"]
    try:
        validate_input(script)
        validate_resolution(resolution)
    except ValueError as error:
        errors.append(str(error))
    metrics = trial.get("metrics", {})
    if not isinstance(metrics, dict):
        return ["invalid execution metrics shape"]
    if (type(metrics.get("exit_code")) is not int or metrics["exit_code"] != 0
            or metrics.get("timed_out") is not False or metrics.get("memory_limit_exceeded") is not False):
        errors.append("unsuccessful, timed out, memory-limited or incomplete execution metrics")
    metric_names = ("wall_seconds", "cpu_seconds", "peak_rss_kib", "user_cpu_seconds", "system_cpu_seconds")
    for key in metric_names:
        number = metrics.get(key)
        if (type(number) not in (int, float) or not math.isfinite(number)
                or number < 0 or key in ("wall_seconds", "cpu_seconds", "peak_rss_kib") and number <= 0):
            errors.append(f"invalid execution metric: {key}")
    if not any(error.startswith("invalid execution metric") for error in errors):
        if not math.isclose(metrics["cpu_seconds"], metrics["user_cpu_seconds"] + metrics["system_cpu_seconds"], rel_tol=1e-9, abs_tol=1e-6):
            errors.append("CPU total contradicts user/system counters")
    probe = trial.get("probe", {})
    if not isinstance(probe, dict) or not isinstance(probe.get("report"), dict):
        return errors + ["missing/invalid probe receipt shape"]
    if probe.get("valid") is not True or probe.get("errors") != []:
        errors.append("probe is invalid or reports errors")
    errors.extend(validate_probe_report(probe.get("report"), frames))
    screenshots = probe.get("screenshots")
    if (not isinstance(screenshots, dict) or set(screenshots) != expected_screenshots(frames)
            or any(not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{64}", value) for value in screenshots.values())):
        errors.append("missing/invalid sampled screenshot hash inventory")
    if not isinstance(probe.get("report_sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", probe["report_sha256"]):
        errors.append("missing/invalid raw probe report hash")
    try:
        warnings = probe.get("warnings_errors", {})
        entries = warnings.get("entries")
        if (not isinstance(entries, list) or any(not isinstance(row, dict)
                or set(row) != {"count", "first_frame", "message"}
                or type(row["count"]) is not int or type(row["first_frame"]) is not int
                or not isinstance(row["message"], str) for row in entries)
                or entries != warning_receipt(warnings.get("section"), frames)
                or not isinstance(warnings.get("report_md_sha256"), str)
                or not re.fullmatch(r"[0-9a-f]{64}", warnings["report_md_sha256"])):
            raise ValueError("contradictory warnings/errors receipt")
    except (ValueError, TypeError, AttributeError) as error:
        errors.append(f"invalid warnings/errors receipt: {error}")
    renderer_lines = trial.get("renderer_log_lines", [])
    if not isinstance(renderer_lines, list) or any(not isinstance(line, str) for line in renderer_lines):
        renderer_lines = []
    actual_renderer = sorted({match[1].strip() for line in renderer_lines if isinstance(line, str)
                              and (match := re.search(r"drawing with OpenGL ES on (.+)", line))})
    if not actual_renderer or trial.get("renderer_identity") != actual_renderer:
        errors.append("renderer identity contradicts captured renderer logs")
    expected = {"ZAKURO_CLOCK": str(clock), "ZAKURO_INPUT": script, "ZAKURO_RESOLUTION": str(resolution),
                "ZAKURO_FRAMESKIP": "0", "RUST_LOG": "info", "LC_ALL": "C", "TZ": "UTC",
                "ZAKURO_RECOMPILED": manifest.get("library", {}).get("path")}
    every = experiment.get("guest_trace_interval_frames")
    if every is not None:
        if type(every) is not int or not 1 <= every <= frames or frames // every > 100000:
            errors.append("invalid requested guest trace interval")
        else:
            expected["ZAKURO_PROBE_TRACE_EVERY"] = str(every)
            trace = probe.get("guest_trace") or {}
            if trace.get("sampling_interval_frames") != every or trace.get("checkpoint_count") != frames // every:
                errors.append("guest trace receipt contradicts requested interval/workload")
    elif probe.get("guest_trace") is not None or probe.get("report", {}).get("guest_trace_file"):
        errors.append("undeclared guest trace instrumentation")
    env = trial.get("environment", {})
    observer = experiment.get("probe_observer", {})
    if observer.get("batch_telemetry"):
        expected["ZAKURO_PROBE_TELEMETRY"] = "1"
        timing = trial.get("batch_telemetry", {})
        summary = timing.get("summary", {})
        if (summary.get("rows") != frames or summary.get("schema_version") != 1
                or summary.get("measurement", {}).get("start_batch_inclusive") != observer.get("measure_start")
                or summary.get("measurement", {}).get("end_batch_exclusive") != observer.get("measure_end")
                or not re.fullmatch(r"[0-9a-f]{64}", str(timing.get("source", {}).get("sha256", "")))):
            errors.append("missing/contradictory buffered batch telemetry")
    if observer.get("capture") == "boundaries":
        expected["ZAKURO_PROBE_CAPTURE"] = "boundaries"
    driver_cache = experiment.get("driver_cache", "uncontrolled")
    if driver_cache not in ("uncontrolled", "off", "isolated"):
        errors.append("unknown driver-cache policy")
    elif driver_cache != "uncontrolled":
        expected.update({"__GL_SHADER_DISK_CACHE": "0" if driver_cache == "off" else "1",
                         "MESA_SHADER_CACHE_DISABLE": "true" if driver_cache == "off" else "false",
                         "__GL_SHADER_DISK_CACHE_PATH": str(Path(env.get("XDG_CACHE_HOME", "")) / "driver-nvidia"),
                         "MESA_SHADER_CACHE_DIR": str(Path(env.get("XDG_CACHE_HOME", "")) / "driver-mesa")})
    if not isinstance(env, dict):
        return errors + ["invalid process environment shape"]
    if any(not isinstance(value, str) or env.get(key) != value for key, value in expected.items()):
        errors.append("fixed workload/quality environment contradicts experiment")
    renderer_env = experiment.get("renderer_environment")
    if (not isinstance(renderer_env, dict) or not set(renderer_env) <= set(RENDERER_ENV)
            or any(not isinstance(value, str) for value in renderer_env.values())
            or {key: env[key] for key in RENDERER_ENV if key in env} != renderer_env):
        errors.append("renderer environment contradicts experiment")
    allowed = {"PATH", *RENDERER_ENV, "HOME", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "TMPDIR",
               "POCKET_NATIVE_CONFIG", "POCKET_NATIVE_TITLES", "ZAKURO_DATA", *expected}
    receipt = manifest.get("implementation_profile")
    if receipt is not None:
        try:
            allowed.update(profile_rules.environment(profile_rules.validate(receipt["declaration"])))
        except (ValueError, KeyError, TypeError):
            errors.append("cannot derive declared implementation environment")
    receipt = manifest.get("diagnostic_profile")
    if receipt is not None:
        try:
            declaration = diagnostic_rules.validate(receipt["declaration"])
            diagnostic_env = diagnostic_rules.environment(declaration)
            allowed.update(diagnostic_env)
            if (receipt.get("declaration_sha256") != diagnostic_rules.declaration_hash(declaration)
                    or experiment.get("diagnostic_profile") != declaration
                    or any(env.get(key) != value for key, value in diagnostic_env.items())
                    or set(env).intersection(diagnostic_rules.ENV_KEYS) != set(diagnostic_env)
                    or trial.get("diagnostics", {}).get("valid") is not True
                    or trial.get("diagnostics", {}).get("requested") != declaration
                    or trial.get("diagnostics", {}).get("timing_eligible") is not False):
                errors.append("diagnostic declaration/environment/receipt mismatch")
        except (ValueError, KeyError, TypeError):
            errors.append("cannot derive declared diagnostic environment")
    receipt = manifest.get("render_profile")
    if receipt is not None:
        try:
            if manifest.get("harness", {}).get("render_profiles_sha256") != digest(Path(render_rules.__file__)):
                raise ValueError("render-profile support SHA-256 does not match the loaded adapter")
            declaration = render_rules.validate(receipt["declaration"])
            implementation_receipt = manifest.get("implementation_profile")
            implementation = (profile_rules.validate(implementation_receipt["declaration"])
                              if implementation_receipt else None)
            diagnostic_receipt = manifest.get("diagnostic_profile")
            diagnostic = (diagnostic_rules.validate(diagnostic_receipt["declaration"])
                          if diagnostic_receipt else None)
            render_rules.check_run(declaration, resolution=resolution, frames=frames, diagnostic=diagnostic,
                seeded_config=[item["path"] for item in experiment.get("seed_config", [])],
                implementation_settings=profile_rules.settings(implementation) if implementation else None,
                implementation_environment=profile_rules.environment(implementation) if implementation else None)
            trial_path = Path(env.get("ZAKURO_DATA", "")).parent
            if (not trial_path.is_absolute()
                    or env.get("POCKET_NATIVE_CONFIG") != str(trial_path / "config")):
                raise ValueError("render trial data/config paths are not isolated siblings")
            render_env = render_rules.environment(declaration, trial_path)
            allowed.update(render_env)
            observation = render_rules.observe(declaration,
                geometry=(trial.get("diagnostics") or {}).get("geometry", {}).get("record"),
                renderer_log_lines=renderer_lines, trial=trial_path)
            text = render_rules.config_settings(declaration)
            settings_receipt = trial.get("render_settings")
            settings_ok = settings_receipt is None if text is None else (
                isinstance(settings_receipt, dict)
                and settings_receipt.get("path") == str(trial_path / "config/settings.toml")
                and settings_receipt.get("sha256") == hashlib.sha256(text.encode()).hexdigest()
                and settings_receipt.get("bytes") == len(text.encode())
                and trial.get("render_settings_after") == settings_receipt)
            if (receipt.get("declaration_sha256") != render_rules.declaration_hash(declaration)
                    or experiment.get("render_profile") != declaration
                    or any(env.get(key) != value for key, value in render_env.items())
                    or set(env).intersection(render_rules.ENV_KEYS) != set(render_env)
                    or trial.get("render_observation") != observation or observation["admitted"] is not True
                    or "render_settings" not in trial or not settings_ok):
                errors.append("render declaration/environment/observation/settings receipt mismatch")
        except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
            errors.append(f"cannot derive declared render receipt: {error}")
    elif (set(env).intersection(render_rules.ENV_KEYS) or "render_profile" in experiment
            or "render_observation" in trial or "render_settings" in trial):
        errors.append("undeclared render profile/environment/observation")
    if not isinstance(env, dict) or set(env) - allowed:
        errors.append("undeclared process environment variables")
    return errors


def compare_values(baseline: dict, candidate: dict) -> dict:
    errors, implementation_report = profile_rules.compare(baseline, candidate)
    left_render, right_render = baseline.get("render_profile"), candidate.get("render_profile")
    if any(receipt is not None and not isinstance(receipt, dict) for receipt in (left_render, right_render)):
        errors.append("invalid render profile receipt shape")
    if ((left_render is None) != (right_render is None)
            or isinstance(left_render, dict) and isinstance(right_render, dict)
            and left_render.get("declaration_sha256") != right_render.get("declaration_sha256")):
        errors.append("different render profile declarations; scale/aspect/layout are independent workload axes")
    if left_render is not None or right_render is not None:
        left_support = baseline.get("harness", {}).get("render_profiles_sha256")
        if (not isinstance(left_support, str) or not re.fullmatch(r"[0-9a-f]{64}", left_support)
                or left_support != candidate.get("harness", {}).get("render_profiles_sha256")):
            errors.append("missing or different render-profile support SHA-256")
    for name, value in (("baseline", baseline), ("candidate", candidate)):
        receipt = value.get("render_profile")
        render = receipt.get("declaration") if isinstance(receipt, dict) else None
        if (isinstance(render, dict) and render.get("presentation_capture") is not None
                or any(set(trial.get("environment", {})).intersection(render_rules.ENV_KEYS)
                       for trial in value.get("trials", []))):
            errors.append(f"{name}: {render_rules.NON_TIMING}")
        if reasons := diagnostic_rules.timing_reasons(value):
            errors.append(f"{name}: {diagnostic_rules.NON_TIMING}: {', '.join(reasons)}")
    if any(type(value.get("schema_version")) is not int or value["schema_version"] != SCHEMA for value in (baseline, candidate)):
        errors.append("missing, unsupported or different manifest schema_version")
    harness = baseline.get("harness", {}).get("sha256")
    if not harness or harness != candidate.get("harness", {}).get("sha256"):
        errors.append("missing or different measurement harness SHA-256; regenerate both runs with the same harness")
    for name, value in (("baseline", baseline), ("candidate", candidate)):
        if value.get("status") != "complete" or len(value.get("trials", [])) < 3:
            errors.append(f"{name} requires >= 3 complete, valid trials")
        if any(not isinstance(trial, dict) or trial.get("valid") is not True for trial in value.get("trials", [])):
            errors.append(f"{name} contains invalid trials")
        for trial in value.get("trials", []):
            errors.extend(f"{name} {trial.get('trial', 'trial')}: {error}" for error in validate_trial_receipt(value, trial))
        if value.get("inputs_changed_during_run"):
            errors.append(f"{name} inputs changed during run")
        envelope = value.get("experiment", {}).get("resource_envelope")
        if (not isinstance(envelope, dict) or envelope.get("known") is not True
                or type(envelope.get("schema_version")) is not int or envelope["schema_version"] != 1):
            errors.append(f"{name} has missing/unknown cgroup resource envelope")
        for trial in value.get("trials", []):
            if trial.get("resource_envelope") != envelope or trial.get("resource_envelope_after") != envelope:
                errors.append(f"{name} has missing or changing per-trial cgroup resource envelope")
                break
        budget = value.get("experiment", {}).get("max_other_cpu_fraction")
        if not isinstance(budget, (int, float)) or isinstance(budget, bool) or not math.isfinite(budget) or not 0 <= budget <= 1:
            errors.append(f"{name} has missing/invalid external CPU budget")
            continue
        for trial in value.get("trials", []):
            activity = trial.get("host_activity") or {}
            fraction = activity.get("other_cpu_fraction")
            if (type(activity.get("schema_version")) is not int or activity["schema_version"] != 1 or activity.get("known") is not True
                    or not isinstance(fraction, (int, float)) or isinstance(fraction, bool)
                    or not math.isfinite(fraction) or fraction < 0):
                errors.append(f"{name} has missing/unknown host activity telemetry")
                break
            if fraction > budget:
                errors.append(f"{name} host busy: estimated other CPU fraction {fraction:.4f} exceeds {budget:.4f}")
                break
    governors = [trial.get("host_activity", {}).get("before", {}).get("governors")
                 for value in (baseline, candidate) for trial in value.get("trials", [])]
    if governors and any(value != governors[0] for value in governors):
        errors.append("CPU frequency governors differ across trials/builds")
    for key in ("experiment", "host"):
        # Free RAM fluctuates; compare machine and CPU identity, not /proc/meminfo snapshots.
        left, right = baseline.get(key), candidate.get(key)
        if key == "host":
            left = {k: v for k, v in (left or {}).items() if k != "meminfo"}
            right = {k: v for k, v in (right or {}).items() if k != "meminfo"}
        if left != right:
            errors.append(f"different {key}: comparison is confounded")
    if baseline.get("rom", {}).get("sha256") != candidate.get("rom", {}).get("sha256"):
        errors.append("different ROM content")
    signatures = []
    renderers = []
    for value in (baseline, candidate):
        for trial in value.get("trials", []):
            probe = trial.get("probe", {})
            report = probe.get("report", {})
            signatures.append({"screenshots": probe.get("screenshots"),
                               "top_pictures": report.get("top_pictures"),
                               "bottom_pictures": report.get("bottom_pictures"),
                               "completed_frames": report.get("completed_frames"),
                               "warnings_errors": probe.get("warnings_errors", {}).get("entries"),
                               "interpreted_instructions": report.get("interpreted_instructions"),
                               "recompiled_instructions": report.get("recompiled_instructions")})
            if any(not isinstance(report.get(key), int) or isinstance(report.get(key), bool)
                   or report[key] < 0 for key in ("interpreted_instructions", "recompiled_instructions")):
                errors.append("missing or invalid guest instruction workload counters")
            renderers.append(trial.get("renderer_identity"))
    if not signatures or not signatures[0].get("screenshots") or any(s != signatures[0] for s in signatures):
        errors.append("sampled outputs, picture counts or guest instruction workloads differ across trials/builds")
    if not renderers or not renderers[0] or any(renderer != renderers[0] for renderer in renderers):
        errors.append("missing or different actual GL renderer identity")
    if errors:
        return {"status": "incomparable", "errors": errors, "target_met": False,
                "implementation_profile_comparison": implementation_report}
    metrics = {}
    for name in ("wall_seconds", "cpu_seconds", "peak_rss_kib"):
        left = [trial["metrics"][name] for trial in baseline["trials"]]
        right = [trial["metrics"][name] for trial in candidate["trials"]]
        if any(not math.isfinite(n) or n <= 0 for n in left + right):
            return {"status": "incomparable", "errors": [f"invalid {name}"], "target_met": False,
                    "implementation_profile_comparison": implementation_report}
        metrics[name] = {"baseline_median": statistics.median(left), "candidate_median": statistics.median(right),
                         "candidate_over_baseline": statistics.median(right) / statistics.median(left),
                         "conservative_candidate_over_baseline": max(right) / min(left),
                         "baseline_range": [min(left), max(left)], "candidate_range": [min(right), max(right)]}
    measured = all(metrics[name]["conservative_candidate_over_baseline"] <= 0.5
                   for name in ("wall_seconds", "cpu_seconds", "peak_rss_kib"))
    def reviewed_scene(value: dict) -> bool:
        review = value.get("scene_review") or {}
        return (isinstance(review, dict) and review.get("schema_version") == 1
                and review.get("verified_gameplay") is True
                and bool(review.get("scene")) and bool(review.get("reviewed_by"))
                and review.get("rom_sha256") == value["rom"]["sha256"]
                and review.get("frames") == value["experiment"]["frames"]
                and review.get("screenshots") == signatures[0]["screenshots"])
    reviewed = reviewed_scene(baseline) and reviewed_scene(candidate)
    activity_ranges = {}
    for name, value in (("baseline", baseline), ("candidate", candidate)):
        fractions = [trial["host_activity"]["other_cpu_fraction"] for trial in value["trials"]]
        activity_ranges[name] = [min(fractions), max(fractions)]
    return {"status": "comparable", "errors": [], "metrics": metrics,
            "host_activity_gate": {"max_other_cpu_fraction": baseline["experiment"]["max_other_cpu_fraction"],
                                   "observed_fraction_ranges": activity_ranges},
            "measured_workload_target_met": measured, "scene_review_evidence_supplied": reviewed,
            "implementation_profile_comparison": implementation_report,
            "target_met": measured and reviewed and implementation_report["formal_eligible"],
            "scope": "recorded workload only; sampled pixels are not complete game-state, audio or gameplay equivalence",
            "hardware_simulation": False}


def compare(args: argparse.Namespace) -> int:
    baseline = json.loads((args.baseline / "manifest.json").read_text())
    candidate = json.loads((args.candidate / "manifest.json").read_text())
    result = compare_values(baseline, candidate)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        write_json(args.output, result)
    print(json.dumps(result, indent=2))
    return 0 if result["target_met"] else 2


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    runner = commands.add_parser("run", help="measure one binary/library pair; creates new output directory")
    for name in ("rom", "executable", "library", "output"):
        runner.add_argument(f"--{name}", type=Path, required=True)
    for name in ("seed-data", "seed-config", "titles", "build-metadata", "scene-evidence", "implementation-profile"):
        runner.add_argument(f"--{name}", type=Path)
    runner.add_argument("--diagnostic-profile", type=Path,
                        help="closed JSON observer declaration; every diagnostic replay is NON-TIMING evidence")
    runner.add_argument("--render-profile", type=Path,
                        help="closed scale/aspect/layout declaration; offscreen panel captures are NON-TIMING")
    runner.add_argument("--frames", type=int, default=600)
    runner.add_argument("--repeats", type=int, default=3)
    runner.add_argument("--timeout", type=float, default=600)
    runner.add_argument("--clock", type=int, default=4000000000000)
    runner.add_argument("--input", default="0:A")
    runner.add_argument("--resolution", type=float, default=1.0, help="internal raster request: quarter steps 1x..4x; effective renderer geometry must be checked separately")
    runner.add_argument("--cpu-affinity", default="", help="comma-separated allowed Linux CPU IDs")
    runner.add_argument("--rss-limit-mib", type=int, help="sampled child RSS watchdog; may overshoot; no virtual-address-space limit")
    runner.add_argument("--max-other-cpu-fraction", type=float, default=0.15,
                        help="comparison gate: maximum estimated unrelated CPU / all online host capacity (default 0.15)")
    runner.add_argument("--trace-every", type=int, help="diagnostic guest-state checkpoint interval in completed frames; default off")
    runner.add_argument("--seed-cache", type=Path, help="immutable per-build warmed XDG cache; cloned each trial, never shared writable")
    runner.add_argument("--driver-cache", choices=("uncontrolled", "off", "isolated"), default="uncontrolled")
    runner.add_argument("--capture", choices=("legacy", "boundaries"), default="legacy",
                        help="boundaries retains eight screenshots but omits every-ten-batch flash reads")
    runner.add_argument("--batch-telemetry", action="store_true", help="require optional buffered run_frame wall/CPU/progress observations")
    runner.add_argument("--measure-start", type=int, default=0, help="inclusive zero-based batch boundary; startup still executes")
    runner.add_argument("--measure-end", type=int, help="exclusive measurement boundary; default frames")
    runner.set_defaults(action=run)
    comparer = commands.add_parser("compare", help="compare complete baseline/candidate manifests")
    comparer.add_argument("baseline", type=Path)
    comparer.add_argument("candidate", type=Path)
    comparer.add_argument("--output", type=Path)
    comparer.set_defaults(action=compare)
    args = parser.parse_args()
    try:
        return args.action(args)
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(2, f"eval error: {error}\n")


if __name__ == "__main__":
    sys.exit(main())
