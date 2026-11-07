#!/usr/bin/env python3
"""Compare exact outcomes across captured repetitions of one fixed workload."""

import argparse
import json
from pathlib import Path
import sys
import tomllib


def check(manifest: dict) -> dict:
    trials = manifest.get("trials", [])
    errors = []
    if len(trials) < 2:
        errors.append("at least two repetitions required")
    reference = None
    comparisons = []
    for index, trial in enumerate(trials):
        probe = trial.get("probe", {})
        report = probe.get("report", {})
        state = {key: report.get(key) for key in ("requested_frames", "attempted_frames", "completed_frames",
                                                "stop_reason", "skipped_frames", "top_pictures", "bottom_pictures",
                                                "interpreted_instructions", "recompiled_instructions")}
        state["screenshots"] = probe.get("screenshots")
        if not trial.get("valid") or not state["screenshots"]:
            errors.append(f"trial {index + 1} incomplete or without sampled frames")
        if reference is None:
            reference = state
            continue
        differences = {}
        for key, value in state.items():
            if value != reference[key]:
                if key == "screenshots":
                    names = sorted(set(value or {}) | set(reference[key] or {}))
                    differences[key] = [name for name in names
                                        if (value or {}).get(name) != (reference[key] or {}).get(name)]
                else:
                    differences[key] = {"reference": reference[key], "actual": value}
        comparisons.append({"trial": trial.get("trial", index + 1), "differences": differences})
        if differences:
            errors.append(f"trial {index + 1} differs from trial 1")
    return {"deterministic": not errors, "errors": errors, "comparisons": comparisons,
            "scope": "captured fixed-workload outputs; rerun actual workloads after changes"}


def check_traces(traces: list[dict]) -> dict:
    errors, comparisons = [], []
    if len(traces) < 2:
        errors.append("at least two guest traces required")
    reference = traces[0] if traces else {}
    for index, trace in enumerate(traces):
        if (trace.get("schema_version") != 1 or not trace.get("checkpoints")
                or trace.get("dropped_checkpoints") != 0):
            errors.append(f"trace {index + 1} missing, unsupported or truncated")
        if index == 0:
            continue
        if trace.get("sampling_interval_frames") != reference.get("sampling_interval_frames"):
            errors.append(f"trace {index + 1} uses different sampling interval")
        left, right = reference.get("checkpoints", []), trace.get("checkpoints", [])
        last_matching_frame, difference = None, None
        for expected, actual in zip(left, right):
            if expected != actual:
                fields = {key: {"reference": expected.get(key), "actual": actual.get(key)}
                          for key in sorted(set(expected) | set(actual)) if expected.get(key) != actual.get(key)}
                difference = {"reference_frame": expected.get("completed_frames"),
                              "actual_frame": actual.get("completed_frames"), "fields": fields}
                break
            last_matching_frame = expected.get("completed_frames")
        if difference is not None or len(left) != len(right):
            errors.append(f"trace {index + 1} differs from trace 1")
        comparisons.append({"trace": index + 1, "last_matching_frame": last_matching_frame,
                            "first_difference": difference, "checkpoint_counts": [len(left), len(right)]})
    return {"deterministic": not errors, "errors": errors, "comparisons": comparisons,
            "scope": "sampled guest state only; exact pixels and full workload must also match"}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--manifest", type=Path)
    source.add_argument("--trace", type=Path, action="append", help="repeat for two or more guest-trace.toml files")
    args = parser.parse_args()
    try:
        result = (check(json.loads(args.manifest.read_text())) if args.manifest else
                  check_traces([tomllib.loads(path.read_text()) for path in args.trace]))
    except (OSError, ValueError, TypeError) as error:
        parser.exit(2, f"determinism check error: {error}\n")
    print(json.dumps(result, indent=2))
    return 0 if result["deterministic"] else 2


if __name__ == "__main__":
    sys.exit(main())
