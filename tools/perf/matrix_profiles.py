#!/usr/bin/env python3
"""Freeze prospective render/audio cells, never launch games or admit timings.

An adapter must subsequently prove effective settings and retain the evaluator's
work/output/host gates. Capability declarations describe support, not execution.
"""

import argparse
import copy
import hashlib
import json
import math
from pathlib import Path
import random
import re

import implementation_profiles


TITLES = {"ultra-sun", "omega-ruby", "pokemon-y"}
SCALES = (1.0, 1.5, 2.0)
ASPECTS = ("Original", "FourThree")
LAYOUTS = ("Original", "TopOnly", "SideBySide")
PHASES = {"cold-start", "warmed-gameplay", "transition"}
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
SAFE_ID = re.compile(r"[a-z0-9][a-z0-9-]*\Z")


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False)


def digest(value):
    return hashlib.sha256(canonical(value).encode()).hexdigest()


def keys(value, required, where):
    if not isinstance(value, dict) or set(value) != set(required):
        raise ValueError(f"{where}: require exactly {sorted(required)}")


def hashes(value, required, where):
    keys(value, required, where)
    if any(not isinstance(v, str) or not SHA256.fullmatch(v) for v in value.values()):
        raise ValueError(f"{where}: lowercase SHA-256 identities required")


def integer(value, low, high, where):
    if type(value) is not int or not low <= value <= high:
        raise ValueError(f"{where}: integer in [{low}, {high}] required")


def validate_capability(value, runtime_hash):
    keys(value, {"schema_version", "runtime_sha256", "internal_scales", "aspects",
                 "layouts", "device_audio", "effective_settings_reporting",
                 "observed_internal_scale_reporting", "presentation_capture_reporting"}, "capability")
    if type(value["schema_version"]) is not int or value["schema_version"] != 1:
        raise ValueError("unsupported capability schema")
    if value["runtime_sha256"] != runtime_hash:
        raise ValueError("capability belongs to a different runtime")
    for name, allowed in (("internal_scales", SCALES), ("aspects", ASPECTS), ("layouts", LAYOUTS)):
        items = value[name]
        if (not isinstance(items, list) or any(isinstance(v, bool) or v not in allowed for v in items)
                or len(items) != len(set(items))):
            raise ValueError(f"invalid capability {name}")
    for name in ("device_audio", "effective_settings_reporting", "observed_internal_scale_reporting",
                 "presentation_capture_reporting"):
        if type(value[name]) is not bool:
            raise ValueError(f"{name} capability must be boolean")


def render_cells(coverage):
    """Screening measures axes independently; full coverage also tests interactions."""
    if coverage == "full":
        return [(s, a, l) for s in SCALES for a in ASPECTS for l in LAYOUTS]
    if coverage != "screen":
        raise ValueError("coverage must be screen or full")
    return ([(s, a, "Original") for s in SCALES for a in ASPECTS]
            + [(1.0, "Original", l) for l in LAYOUTS[1:]])


def validate_host_policy(policy):
    """Validate frozen policy fields; live affinity and host admission stay with paired.py."""
    proposal = {"max_busy_fraction", "max_other_cpu_fraction", "aa_max_span_fraction"}
    complete = proposal | {"cpus", "siblings", "max_core_fraction", "max_cpu_c",
                           "max_temperature_span_c", "window_seconds", "consecutive_windows", "timeout_seconds"}
    if not isinstance(policy, dict) or set(policy) not in (proposal, complete):
        raise ValueError("host policy requires the complete schema or the three-field blocked proposal")
    caps = {"max_busy_fraction": 0.1, "max_other_cpu_fraction": 0.15, "aa_max_span_fraction": 0.1,
            "max_core_fraction": 0.25, "max_cpu_c": 85, "max_temperature_span_c": 5}
    for name in set(policy) & caps.keys():
        value = policy[name]
        if type(value) not in (int, float) or not math.isfinite(value) or not 0 < value <= caps[name]:
            raise ValueError(f"host policy {name}: invalid or relaxed threshold")
    if set(policy) == proposal:
        return False
    for name in ("cpus", "siblings"):
        items = policy[name]
        if (not isinstance(items, list) or (name == "cpus" and not items)
                or any(type(cpu) is not int or cpu < 0 for cpu in items)
                or len(items) != len(set(items))):
            raise ValueError(f"host policy {name}: invalid CPU list")
    if set(policy["cpus"]) & set(policy["siblings"]):
        raise ValueError("host policy CPUs and siblings must be disjoint")
    for name in ("window_seconds", "timeout_seconds"):
        value = policy[name]
        if type(value) not in (int, float) or not math.isfinite(value):
            raise ValueError(f"host policy {name}: finite number required")
    if not 0 < policy["window_seconds"] <= 10 or not policy["window_seconds"] <= policy["timeout_seconds"] <= 60:
        raise ValueError("host policy preflight must be bounded by 60 seconds")
    integer(policy["consecutive_windows"], 1, 5, "host policy consecutive_windows")
    return True


def build_matrix(spec):
    keys(spec, {"schema_version", "comparison_factor", "order_seed", "pairs", "aa_repeats",
                "max_cells", "coverage", "implementation_profile", "host_policy",
                "layout_size", "renderer_environment", "runtimes", "titles"}, "matrix specification")
    if type(spec["schema_version"]) is not int or spec["schema_version"] != 1:
        raise ValueError("unsupported matrix schema")
    if spec["comparison_factor"] not in {"runtime", "native-library"}:
        raise ValueError("comparison_factor must be runtime or native-library")
    integer(spec["order_seed"], 0, 2**63 - 1, "order_seed")
    for name in ("pairs", "aa_repeats"):
        integer(spec[name], 3, 30, name)
    integer(spec["max_cells"], 1, 512, "max_cells")
    if not isinstance(spec["layout_size"], list) or len(spec["layout_size"]) != 2:
        raise ValueError("layout_size must be a fixed presentation width/height")
    for dimension in spec["layout_size"]:
        integer(dimension, 1, 16384, "layout_size dimension")
    if (not isinstance(spec["renderer_environment"], dict) or not spec["renderer_environment"]
            or any(not isinstance(k, str) or not isinstance(v, str)
                   for k, v in spec["renderer_environment"].items())):
        raise ValueError("explicit shared renderer environment required")
    profile = implementation_profiles.validate(spec["implementation_profile"])
    complete_policy = validate_host_policy(spec["host_policy"])
    keys(spec["runtimes"], {"original", "current"}, "runtimes")
    for variant, runtime in spec["runtimes"].items():
        keys(runtime, {"identities", "capability"}, f"{variant} runtime")
        hashes(runtime["identities"], {"executable", "build_receipt", "compiler"}, "runtime identities")
        validate_capability(runtime["capability"], runtime["identities"]["executable"])
    if (spec["comparison_factor"] == "native-library"
            and spec["runtimes"]["original"]["identities"] != spec["runtimes"]["current"]["identities"]):
        raise ValueError("native-library factor requires the same runtime/build/compiler")
    titles = spec["titles"]
    if not isinstance(titles, list) or {t.get("id") for t in titles} != TITLES or len(titles) != 3:
        raise ValueError("exactly the three independently bound titles required")
    cells, unavailable = [], []
    choices = render_cells(spec["coverage"])
    for title in titles:
        keys(title, {"id", "identities", "scenes"}, "title")
        hashes(title["identities"], {"rom", "title_profile", "original_library", "current_library",
                                       "original_codegen", "current_codegen"}, "title identities")
        if (spec["comparison_factor"] == "runtime"
                and title["identities"]["original_library"] != title["identities"]["current_library"]):
            raise ValueError("runtime factor requires the same native library on both sides")
        if not isinstance(title["scenes"], list) or not title["scenes"]:
            raise ValueError("explicit per-title scene availability required")
        seen = set()
        for scene in title["scenes"]:
            keys(scene, {"id", "phase", "available", "cache", "frames", "measure_start", "identities"}, "scene")
            if (not isinstance(scene["id"], str) or not SAFE_ID.fullmatch(scene["id"])
                    or scene["id"] in seen or scene["phase"] not in PHASES or type(scene["available"]) is not bool):
                raise ValueError("unique path-safe scene id, phase and explicit availability required")
            seen.add(scene["id"])
            if scene["cache"] not in {"fresh", "warm"}:
                raise ValueError("scene cache must be fresh or warm (neither is disk-cold)")
            if not scene["available"]:
                if scene["identities"] is not None or scene["frames"] is not None or scene["measure_start"] is not None:
                    raise ValueError("unavailable scene must not claim workload bindings")
                unavailable.append({"title": title["id"], "scene": scene["id"], "phase": scene["phase"]})
                continue
            hashes(scene["identities"], {"seed_data", "input", "scene_review", "clock",
                                         "original_cache_seed", "current_cache_seed"}, "scene identities")
            integer(scene["frames"], 8, 100000, "scene frames")
            integer(scene["measure_start"], 0, scene["frames"] - 1, "scene measure_start")
            if scene["phase"] == "cold-start" and (scene["cache"] != "fresh" or scene["measure_start"] != 0):
                raise ValueError("cold-start requires fresh application cache and startup measurement")
            if scene["phase"] == "warmed-gameplay" and scene["cache"] != "warm":
                raise ValueError("warmed gameplay requires a separately bound warm cache seed")
            # Screen startup/transition first at native quality; full opts into
            # their interactions too. No workload is manufactured for a phase.
            rendered = choices if scene["phase"] == "warmed-gameplay" or spec["coverage"] == "full" else choices[:1]
            for scale, aspect, layout in rendered:
                settings = {"internal_scale": scale, "aspect": aspect, "layout": layout,
                            "swapped": False, "stretched": "Off", "audio": "device-on",
                            "speed": 1.0, "frameskip": 0, "implementation_profile": profile,
                            "frontend_settings": {"resolution": scale, "aspect": aspect,
                                                  "layout": layout, "swapped": False},
                            "layout_size": spec["layout_size"]}
                blocked = [] if complete_policy else ["complete frozen evaluator policy missing"]
                for variant, runtime in spec["runtimes"].items():
                    cap = runtime["capability"]
                    for field, requested in (("internal_scales", scale), ("aspects", aspect), ("layouts", layout)):
                        if requested not in cap[field]:
                            blocked.append(f"{variant}: unsupported {field}={requested}")
                    for flag in ("device_audio", "effective_settings_reporting", "observed_internal_scale_reporting",
                                 "presentation_capture_reporting"):
                        if not cap[flag]:
                            blocked.append(f"{variant}: missing {flag}")
                binding = {"title": {"id": title["id"], "identities": title["identities"]},
                           "scene": scene, "runtimes": spec["runtimes"],
                           "settings": settings, "host_policy": spec["host_policy"],
                           "renderer_environment": spec["renderer_environment"],
                           "comparison_factor": spec["comparison_factor"]}
                cell_id = f"{title['id']}-{scene['id']}-{scale:g}x-{aspect.lower()}-{layout.lower()}"
                cells.append({"id": cell_id, "binding_sha256": digest(binding),
                              "binding": copy.deepcopy(binding), "blocked_reasons": blocked,
                              "status": "blocked" if blocked else "needs-execution-adapter",
                              "runnable": False})
                if len(cells) > spec["max_cells"]:
                    raise ValueError("matrix exceeds explicit max_cells; choose a smaller prospective matrix")
    rng = random.Random(spec["order_seed"])
    rng.shuffle(cells)
    for cell in cells:
        first = rng.choice(("original", "current"))
        other = "current" if first == "original" else "original"
        pairs = [[first, other] if i % 2 == 0 else [other, first] for i in range(spec["pairs"])]
        cell["proposed_order"] = {"aa": ["current"] * spec["aa_repeats"], "pairs": pairs}
    return {"schema_version": 1, "kind": "prospective-render-audio-matrix",
            "specification_sha256": digest(spec), "specification": copy.deepcopy(spec),
            "cells": cells, "unavailable_scenes": unavailable,
            "planned_processes_excluding_warmups": len(cells) * (spec["aa_repeats"] + 2 * spec["pairs"]),
            "timing_admitted": False,
            "limits": ["Planning only; no execution adapter or performance result.",
                       "Capabilities are declarations; effective setting/audio proof is still required.",
                       "Compare within a cell; changed scale/aspect/layout is a separate workload.",
                       "Screen coverage does not cover all scale/layout interactions.",
                       "Retain equal-work/output, same-DSO runtime, host/noise gates and failed trials."]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--specification", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = build_matrix(json.loads(args.specification.read_text()))
    # Refuse overwrite: an earlier prospective freeze remains inspectable.
    with args.output.open("x") as out:
        out.write(json.dumps(result, sort_keys=True, indent=2, allow_nan=False) + "\n")


if __name__ == "__main__":
    main()
