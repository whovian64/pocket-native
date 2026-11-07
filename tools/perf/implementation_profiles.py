"""Closed, reviewed implementation choices; never an arbitrary environment override."""

from __future__ import annotations

import copy
import hashlib
import json
from pathlib import Path
import re

MIMALLOC_LIMITATION = "calloc overflow can return NULL with errno 0 instead of ENOMEM"
CONSTRAINT_KEYS = {"gpu_thread", "generic_shaders", "generic_lighting", "stand_ins"}
KNOB_KEYS = {"shader_linking", "background_shaders", "texture_cache_mib", "gles_ring_kib", "rom_cache_mib"}


def exact_keys(value: dict, keys: set[str], label: str) -> None:
    if not isinstance(value, dict) or set(value) != keys:
        raise ValueError(f"{label} requires exactly {sorted(keys)}")


def bounded(value: object, low: int, high: int, label: str) -> None:
    if value is not None and (type(value) is not int or not low <= value <= high):
        raise ValueError(f"{label} requires null or integer {low}..{high}")


def validate_allocator(value: dict) -> None:
    if value == {"kind": "system"}:
        return
    if not isinstance(value, dict) or value.get("kind") != "gnu_preload" or value.get("name") not in {"jemalloc", "mimalloc"}:
        raise ValueError("allocator must be system or an explicit jemalloc/mimalloc GNU preload")
    keys = {"kind", "name", "path", "sha256", "configuration"}
    if value["name"] == "mimalloc":
        keys |= {"diagnostic_only", "acknowledged_limitations"}
        if value.get("diagnostic_only") is not True or value.get("acknowledged_limitations") != [MIMALLOC_LIMITATION]:
            raise ValueError("mimalloc requires diagnostic_only=true and acknowledgement of its known errno limitation")
    exact_keys(value, keys, "allocator")
    if (not isinstance(value["path"], str) or not Path(value["path"]).is_absolute()
            or any(c in value["path"] for c in (":", "\n", "\r", "\0", " ", "\t"))
            or not isinstance(value["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", value["sha256"])):
        raise ValueError("preload requires a single absolute loader-safe path and lowercase SHA-256")
    config = value["configuration"]
    if value["name"] == "jemalloc":
        exact_keys(config, {"MALLOC_CONF"}, "jemalloc configuration")
        text = config["MALLOC_CONF"]
        if not isinstance(text, str) or not text:
            raise ValueError("jemalloc requires explicit MALLOC_CONF")
        seen = set()
        for item in text.split(","):
            key, separator, setting = item.partition(":")
            if not separator or key in seen:
                raise ValueError("jemalloc options require unique key:value entries")
            seen.add(key)
            if key in {"background_thread", "retain"}:
                valid = setting in {"true", "false"}
            elif key == "thp":
                valid = setting in {"default", "never"}
            elif key == "narenas":
                valid = setting.isdecimal() and 1 <= int(setting) <= 256
            elif key in {"dirty_decay_ms", "muzzy_decay_ms"}:
                valid = setting == "-1" or setting.isdecimal() and 0 <= int(setting) <= 60000
            else:
                valid = False
            if not valid:
                raise ValueError(f"unsupported/invalid reviewed jemalloc option: {key}")
    else:
        exact_keys(config, {"MIMALLOC_ALLOW_THP", "MIMALLOC_PURGE_DELAY", "MIMALLOC_PURGE_DECOMMITS"}, "mimalloc configuration")
        if (config["MIMALLOC_ALLOW_THP"] not in {"0", "1", "2"}
                or config["MIMALLOC_PURGE_DECOMMITS"] not in {"0", "1"}
                or not isinstance(config["MIMALLOC_PURGE_DELAY"], str)
                or not config["MIMALLOC_PURGE_DELAY"].isdecimal()
                or not 0 <= int(config["MIMALLOC_PURGE_DELAY"]) <= 60000):
            raise ValueError("invalid mimalloc configuration")


def validate(value: dict) -> dict:
    exact_keys(value, {"schema_version", "id", "reviewed_by", "rationale", "constraints", "knobs"}, "implementation profile")
    if type(value["schema_version"]) is not int or value["schema_version"] != 1:
        raise ValueError("unsupported implementation profile schema")
    for name in ("id", "reviewed_by", "rationale"):
        if not isinstance(value[name], str) or not value[name].strip():
            raise ValueError(f"profile {name} requires nonempty review metadata")
    constraints = value["constraints"]
    exact_keys(constraints, CONSTRAINT_KEYS, "profile constraints")
    if any(type(v) is not bool for v in constraints.values()):
        raise ValueError("profile constraints must be booleans")
    if any(constraints[name] for name in CONSTRAINT_KEYS - {"gpu_thread"}):
        raise ValueError("generic shaders/lighting and approximate stand-ins must stay disabled")
    knobs = value["knobs"]
    if not isinstance(knobs, dict) or not KNOB_KEYS <= set(knobs) <= KNOB_KEYS | {"allocator", "release_shader_compiler"}:
        raise ValueError("profile requires the closed shader/cache knob set and optional allocator")
    if knobs["shader_linking"] not in {"monolithic", "separable"} or type(knobs["background_shaders"]) is not bool:
        raise ValueError("shader_linking must be monolithic/separable and background_shaders boolean")
    for name, low, high in (("texture_cache_mib", 1, 256), ("gles_ring_kib", 128, 8192), ("rom_cache_mib", 0, 1024)):
        bounded(knobs[name], low, high, name)
    result = copy.deepcopy(value)
    result["knobs"].setdefault("allocator", {"kind": "system"})
    result["knobs"].setdefault("release_shader_compiler", False)
    release = result["knobs"]["release_shader_compiler"]
    if type(release) is not bool or release:
        raise ValueError("release_shader_compiler is a false-only compatibility field; the experimental hint was removed")
    validate_allocator(result["knobs"]["allocator"])
    return result


def declaration_hash(profile: dict) -> str:
    return hashlib.sha256(json.dumps(profile, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def settings(profile: dict) -> str:
    advanced = {**profile["constraints"],
                "separable_shaders": profile["knobs"]["shader_linking"] == "separable",
                "background_shaders": profile["knobs"]["background_shaders"]}
    return "[advanced]\n" + "".join(f"{key} = {str(value).lower()}\n" for key, value in sorted(advanced.items()))


def environment(profile: dict) -> dict[str, str]:
    knobs = profile["knobs"]
    env = {variable: str(knobs[name]) for name, variable in
           (("texture_cache_mib", "ZAKURO_TEXTURE_CACHE_MIB"), ("gles_ring_kib", "ZAKURO_GLES_RING_KIB"),
            ("rom_cache_mib", "ZAKURO_ROM_CACHE_MIB")) if knobs[name] is not None}
    allocator = knobs["allocator"]
    if allocator["kind"] == "gnu_preload":
        env.update({"LD_PRELOAD": allocator["path"], **allocator["configuration"]})
    return env


def proof(profile: dict, logs: list[str], preload: dict | None = None) -> dict:
    """Positive logs required for enabled runtime knobs; retain ignored ring requests."""
    knobs = profile["knobs"]
    selected = [line for line in logs if any(marker in line for marker in
                ("ZAKURO_SYNC_GPU", "ZAKURO_SEPARABLE_SHADERS", "ZAKURO_SYNC_SHADERS",
                 "ZAKURO_GENERIC_SHADERS", "ZAKURO_GENERIC_LIGHTING", "ZAKURO_STAND_INS",
                 "decoded texture cache budget:", "GLES mutable ring initial capacity:",
                 "ZAKURO_GLES_RING_KIB", "ROM cache", "no program pipelines", "shader stages stay linked",
                 "ZAKURO_RELEASE_SHADER_COMPILER", "shader compiler release hint"))]
    contains = lambda marker: any(marker in line for line in selected)
    checks = {
        "gpu_thread": contains("GPU thread off, ZAKURO_SYNC_GPU") == (not profile["constraints"]["gpu_thread"]),
        "shader_linking": contains("ZAKURO_SEPARABLE_SHADERS") == (knobs["shader_linking"] == "separable"),
        "background_shaders": contains("shaders compiled when a draw needs them, ZAKURO_SYNC_SHADERS") == (not knobs["background_shaders"]),
        "no_pipeline_fallback": not contains("no program pipelines") and not contains("shader stages stay linked"),
        "quality": not any(contains(name) for name in ("ZAKURO_GENERIC_SHADERS", "ZAKURO_GENERIC_LIGHTING", "ZAKURO_STAND_INS")),
        "release_shader_compiler_requested": not any("ZAKURO_RELEASE_SHADER_COMPILER" in line or "shader compiler release hint" in line for line in logs),
    }
    if knobs["texture_cache_mib"] is not None:
        checks["texture_cache"] = contains(f"decoded texture cache budget: {knobs['texture_cache_mib']} MiB (ZAKURO_TEXTURE_CACHE_MIB)")
    ring_ignored = contains("ZAKURO_GLES_RING_KIB ignored for persistent mapped rings")
    if knobs["gles_ring_kib"] is not None:
        checks["ring_capacity"] = contains(f"requested cap {knobs['gles_ring_kib']} KiB,") or ring_ignored
    if knobs["rom_cache_mib"] not in (None, 0):
        checks["rom_cache"] = contains(f"ROM cache enabled: budget_mib={knobs['rom_cache_mib']} ")
    allocator = knobs["allocator"]
    if allocator["kind"] == "gnu_preload":
        mapped = preload.get("mapping_lines") if isinstance(preload, dict) else None
        path_observed = (isinstance(mapped, list) and any(isinstance(line, str)
                         and len(fields := line.split(maxsplit=5)) == 6 and fields[5] == allocator["path"] for line in mapped))
        checks["allocator_mapping"] = bool(preload and preload.get("observed") is True
                                           and preload.get("requested_path") == allocator["path"]
                                           and path_observed)
    return {"verified": all(checks.values()), "checks": checks, "logs": selected,
            "ring_request_ignored_for_persistent_storage": ring_ignored, "preload": preload,
            "allocator_configuration_scope": "declared environment and observed mapping; allocator internal option state is not introspected"}


def differences(left: object, right: object, prefix: str = "") -> list[dict]:
    if isinstance(left, dict) and isinstance(right, dict):
        return [row for key in sorted(left.keys() | right.keys())
                for row in differences(left.get(key), right.get(key), f"{prefix}.{key}" if prefix else key)]
    return [] if left == right else [{"field": prefix, "baseline": left, "candidate": right}]


def compare(baseline: dict, candidate: dict) -> tuple[list[str], dict]:
    left, right = baseline.get("implementation_profile"), candidate.get("implementation_profile")
    report = {"mode": "legacy identical experiment", "differences": [], "formal_eligible": True}
    support = baseline.get("harness", {}).get("implementation_profiles_sha256")
    other_support = candidate.get("harness", {}).get("implementation_profiles_sha256")
    support_errors = []
    if left is not None or right is not None or support is not None or other_support is not None:
        if (not isinstance(support, str) or not re.fullmatch(r"[0-9a-f]{64}", support) or support != other_support):
            support_errors.append("missing or different implementation-profile support SHA-256")
    if left is None and right is None:
        if any("implementation_constraints" in value.get("experiment", {}) for value in (baseline, candidate)):
            support_errors.append("implementation constraints require explicit profiles on both sides")
        report["formal_eligible"] = not support_errors
        return support_errors, report
    report["mode"] = "declared implementation profiles"
    if left is None or right is None:
        report["formal_eligible"] = False
        return support_errors + ["profile-free and declared-profile runs cannot be compared"], report
    errors, profiles = support_errors, []
    for name, manifest, receipt in (("baseline", baseline, left), ("candidate", candidate, right)):
        try:
            profile = validate(receipt["declaration"])
            if receipt.get("declaration_sha256") != declaration_hash(profile):
                raise ValueError("declaration hash mismatch")
            source = receipt.get("source", {})
            if (not isinstance(source.get("sha256"), str) or not re.fullmatch(r"[0-9a-f]{64}", source["sha256"])
                    or type(source.get("bytes")) is not int or source["bytes"] < 1 or not source.get("path")):
                raise ValueError("missing profile source identity")
            if manifest.get("experiment", {}).get("implementation_constraints") != profile["constraints"]:
                raise ValueError("fixed profile constraints differ from experiment")
            allocator = profile["knobs"]["allocator"]
            if allocator["kind"] == "gnu_preload":
                before, after = receipt.get("allocator_before", {}), receipt.get("allocator_after", {})
                if (before != after or before.get("sha256") != allocator["sha256"]
                        or before.get("path") != allocator["path"] or type(before.get("bytes")) is not int or before["bytes"] < 1):
                    raise ValueError("allocator before/after identity is missing or differs from declared preload")
            elif receipt.get("allocator_before") is not None or receipt.get("allocator_after") is not None:
                raise ValueError("system allocator profile has a preload receipt")
            expected_settings = hashlib.sha256(settings(profile).encode()).hexdigest()
            for trial in manifest.get("trials", []):
                captured = trial.get("implementation_profile_proof", {})
                logs = captured.get("logs")
                if not isinstance(logs, list) or any(not isinstance(line, str) for line in logs):
                    raise ValueError("missing effective profile logs")
                rebuilt = proof(profile, logs, trial.get("metrics", {}).get("preload_proof"))
                if (not rebuilt["verified"] or captured.get("verified") is not True or captured != rebuilt
                        or any(type(check) is not bool for check in captured.get("checks", {}).values())):
                    raise ValueError("missing or inconsistent effective implementation configuration proof")
                if trial.get("generated_settings_sha256") != expected_settings:
                    raise ValueError("generated settings differ from declared profile")
                env = trial.get("environment", {})
                declared_env = environment(profile)
                variable_set = {"ZAKURO_TEXTURE_CACHE_MIB", "ZAKURO_GLES_RING_KIB", "ZAKURO_ROM_CACHE_MIB",
                                "ZAKURO_RELEASE_SHADER_COMPILER", "ZAKURO_COMPILERS",
                                "LD_PRELOAD", "MALLOC_CONF", "MIMALLOC_ALLOW_THP", "MIMALLOC_PURGE_DELAY", "MIMALLOC_PURGE_DECOMMITS"}
                if {key: env[key] for key in variable_set if key in env} != declared_env:
                    raise ValueError("implementation environment differs from declared profile")
            profiles.append(profile)
            effective = [{"checks": trial["implementation_profile_proof"]["checks"],
                          "ring_request_ignored_for_persistent_storage": trial["implementation_profile_proof"]["ring_request_ignored_for_persistent_storage"]}
                         for trial in manifest.get("trials", [])]
            report[name] = {"id": profile["id"], "reviewed_by": profile["reviewed_by"], "rationale": profile["rationale"],
                            "source": source, "diagnostic_only": allocator.get("diagnostic_only", False),
                            "known_limitations": allocator.get("acknowledged_limitations", []),
                            "effective_configurations": [json.loads(item) for item in sorted({json.dumps(item, sort_keys=True) for item in effective})]}
        except (KeyError, ValueError, TypeError, AttributeError) as error:
            errors.append(f"{name} invalid implementation profile: {error}")
    if len(profiles) == 2:
        report["differences"] = differences(profiles[0], profiles[1])
        if profiles[0]["constraints"] != profiles[1]["constraints"]:
            errors.append("fixed implementation-profile constraints differ")
    report["formal_eligible"] = not errors and not any(report.get(name, {}).get("diagnostic_only") for name in ("baseline", "candidate"))
    return errors, report
