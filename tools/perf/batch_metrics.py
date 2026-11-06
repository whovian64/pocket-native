"""Validate buffered run_frame observations; never equate batches with pictures."""

import csv
import math
from pathlib import Path

FIELDS = [
    "frame",
    "wall_ns",
    "process_cpu_ns",
    "batch_wall_ns",
    "batch_process_cpu_ns",
    "guest_cycles",
    "interpreted_instructions",
    "recompiled_instructions",
    "gpu_wait_delta_us",
]


def percentile(values, fraction):
    ordered = sorted(values)
    at = (len(ordered) - 1) * fraction
    lo = math.floor(at)
    hi = math.ceil(at)
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (at - lo)


def read(path: Path, frames: int, start: int, end: int) -> dict:
    if not 0 <= start < end <= frames <= 100_000:
        raise ValueError("invalid batch measurement range or >100000 batches")
    with path.open(newline="") as source:
        reader = csv.DictReader(source)
        if reader.fieldnames != FIELDS:
            raise ValueError("unexpected batch telemetry columns")
        rows = []
        for index, row in enumerate(reader, 1):
            if (
                index > frames
                or set(row) != set(FIELDS)
                or any(
                    not isinstance(v, str) or not v.isascii() or not v.isdecimal()
                    for v in row.values()
                )
            ):
                raise ValueError("invalid/excess batch telemetry row")
            parsed = {key: int(row[key]) for key in FIELDS}
            if parsed["frame"] != index or (
                parsed["wall_ns"] <= 0
                or parsed["batch_wall_ns"] < parsed["wall_ns"]
                or parsed["batch_process_cpu_ns"] < parsed["process_cpu_ns"]
            ):
                raise ValueError("noncontiguous frames or invalid wall duration")
            if rows and any(parsed[k] < rows[-1][k] for k in FIELDS[5:8]):
                raise ValueError("guest progress counters went backward")
            rows.append(parsed)
    if len(rows) != frames:
        raise ValueError("incomplete batch telemetry")

    def window(lo, hi):
        selected = rows[lo:hi]
        wall = [r["batch_wall_ns"] / 1e6 for r in selected]
        before = rows[lo - 1] if lo else None
        work = {k: selected[-1][k] - (before[k] if before else 0) for k in FIELDS[5:8]}
        return {
            "start_batch_inclusive": lo,
            "end_batch_exclusive": hi,
            "batches": hi - lo,
            "wall_seconds": sum(wall) / 1000,
            "process_cpu_seconds": sum(r["batch_process_cpu_ns"] for r in selected)
            / 1e9,
            "run_frame_wall_seconds": sum(r["wall_ns"] for r in selected) / 1e9,
            "run_frame_process_cpu_seconds": sum(r["process_cpu_ns"] for r in selected)
            / 1e9,
            "gpu_wait_seconds": sum(r["gpu_wait_delta_us"] for r in selected) / 1e6,
            "batch_wall_ms": {
                "median": percentile(wall, 0.5),
                "p95": percentile(wall, 0.95),
                "p99": percentile(wall, 0.99),
                "maximum": max(wall),
            },
            "guest_work_delta": work,
        }

    return {
        "schema_version": 1,
        "rows": len(rows),
        "measurement": window(start, end),
        "whole_loop": window(0, frames),
        "scope": "frontend-loop batches include capture/presentation calls, exclude pre-loop load and final report/exit; run_frame is a nested interval; CPU includes all process threads during each interval; GPU wait is the existing host queue wait counter, not GPU duration; batch latency is not unique-picture/display frame time",
    }
