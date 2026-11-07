"""Closed, opt-in diagnostic replay contract; never throughput evidence.

Python 3.11+, standard library only. Declarations do not establish acoustic
quality, physical device identity, scanout, or alignment between observers.
"""

from __future__ import annotations

import hashlib
import importlib.util
import json
import math
from pathlib import Path
import re
import stat
import tomllib

U64_MAX = 2**64 - 1
NON_TIMING = "NON-TIMING diagnostic evidence; ineligible for throughput comparison"
ENV_KEYS = {"ZAKURO_PROBE_AUDIO", "ZAKURO_PROBE_GEOMETRY", "ZAKURO_PACKET_CENSUS_FROM",
            "ZAKURO_TEXTURE_CACHE_STATS", "ZAKURO_TEXTURE_CACHE_TIMING"}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def keys(value, required, optional=()):
    require(type(value) is dict and set(required) <= value.keys() <= set(required) | set(optional),
            "diagnostic object has missing or unknown fields")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate diagnostic field: {key}")
        result[key] = value
    return result


def snapshot(path: Path, limit=2 * 1024 * 1024):
    require(not path.is_symlink() and path.is_file(), f"diagnostic input must be a regular non-symlink file: {path}")
    with path.open("rb") as source:
        data = source.read(limit + 1)
    require(len(data) <= limit, f"diagnostic input exceeds byte budget: {path.name}")
    return data, {"path": str(path.resolve()), "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}


def validate(value):
    keys(value, ("schema_version",), ("audio", "geometry", "packet_census", "packet_window_start",
                                     "texture_stats", "texture_timing", "audio_backend", "audio_completion"))
    require(type(value["schema_version"]) is int and value["schema_version"] == 1,
            "unsupported diagnostic profile schema_version")
    result = {"schema_version": 1, "audio": value.get("audio", "off"),
              **{name: value.get(name, False) for name in
                 ("geometry", "packet_census", "texture_stats", "texture_timing")}}
    require(result["audio"] in ("off", "paced", "unpaced"), "audio must be off, paced or unpaced")
    for name in ("geometry", "packet_census", "texture_stats", "texture_timing"):
        require(type(result[name]) is bool, f"{name} must be boolean")
    require(not result["texture_timing"] or result["texture_stats"], "texture_timing requires texture_stats")
    if "packet_window_start" in value:
        start = value["packet_window_start"]
        require(result["packet_census"] and type(start) is int and 0 <= start <= U64_MAX,
                "packet_window_start requires packet_census and a u64 batch index")
        result["packet_window_start"] = start
    backend = value.get("audio_backend")
    if "audio_completion" in value:
        require(result["audio"] != "off" and value["audio_completion"] == "backend",
                "audio_completion requires audio on and the backend completion rule")
        result["audio_completion"] = "backend"
    if result["audio"] == "off":
        require("audio_backend" not in value, "audio off cannot declare an unused audio_backend")
    else:
        keys(backend, ("kind", "asoundrc"), ("xdg_runtime_dir",))
        require(backend["kind"] in ("alsa_null", "real_device"), "unknown audio backend kind")
        config = backend["asoundrc"]
        keys(config, ("path", "sha256"))
        require(type(config["path"]) is str and Path(config["path"]).is_absolute(), "asoundrc path must be absolute")
        require(type(config["sha256"]) is str and re.fullmatch(r"[0-9a-f]{64}", config["sha256"]),
                "asoundrc requires a lowercase SHA-256")
        if "xdg_runtime_dir" in backend:
            runtime = backend["xdg_runtime_dir"]
            keys(runtime, ("path", "uid", "device", "inode"))
            require(backend["kind"] == "real_device", "runtime directory is only allowed for a requested real device")
            require(type(runtime["path"]) is str and Path(runtime["path"]).is_absolute(), "runtime directory path must be absolute")
            require(all(type(runtime[name]) is int and runtime[name] >= 0 for name in ("uid", "device", "inode")),
                    "runtime directory identity requires nonnegative uid/device/inode")
        result["audio_backend"] = {"kind": backend["kind"], "asoundrc": dict(config),
            **({"xdg_runtime_dir": dict(backend["xdg_runtime_dir"])} if "xdg_runtime_dir" in backend else {})}
    return result


def declaration_hash(profile):
    return hashlib.sha256(json.dumps(profile, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def load(path):
    data, source = snapshot(path)
    return validate(json.loads(data, object_pairs_hook=unique_object)), source, data


def backend_inputs(profile):
    backend = profile.get("audio_backend")
    if backend is None:
        return None
    _, config = snapshot(Path(backend["asoundrc"]["path"]))
    require(config["sha256"] == backend["asoundrc"]["sha256"], "asoundrc differs from declared SHA-256")
    result = {"declared_kind": backend["kind"], "asoundrc_source": config,
              "device_identity": "unknown; declaration is an experiment label", "runtime_directory": None}
    if runtime := backend.get("xdg_runtime_dir"):
        path = Path(runtime["path"])
        observed = path.stat()
        actual = {"path": str(path), "uid": observed.st_uid, "device": observed.st_dev, "inode": observed.st_ino}
        require(not path.is_symlink() and stat.S_ISDIR(observed.st_mode) and actual == runtime,
                "runtime directory differs from declared identity")
        result["runtime_directory"] = actual
    return result


def environment(profile):
    result = {"ZAKURO_PROBE_AUDIO": profile["audio"]}
    for field, name in (("geometry", "ZAKURO_PROBE_GEOMETRY"), ("texture_stats", "ZAKURO_TEXTURE_CACHE_STATS"),
                        ("texture_timing", "ZAKURO_TEXTURE_CACHE_TIMING")):
        if profile[field]:
            result[name] = "1"
    if "packet_window_start" in profile:
        result["ZAKURO_PACKET_CENSUS_FROM"] = str(profile["packet_window_start"])
    return result


def stage_backend(profile, trial):
    receipt = backend_inputs(profile)
    if receipt is None:
        return None
    data, identity = snapshot(Path(profile["audio_backend"]["asoundrc"]["path"]))
    require(identity == receipt["asoundrc_source"], "asoundrc changed during preparation")
    destination = trial / "home/.asoundrc"
    with destination.open("xb") as output:
        output.write(data)
    _, clone = snapshot(destination)
    require(clone["sha256"] == identity["sha256"], "trial asoundrc clone mismatch")
    return {**receipt, "asoundrc_clone": clone}


def audio_admission(row, requested, completion_required=False):
    require(type(row) is dict, "audio observation must be a table")
    fields = ("mixed_frames", "received_frames", "discarded_frames", "output_frames", "written_frames",
              "device_frames_offered", "device_frames_accepted", "device_writes", "partial_writes", "failed_writes",
              "write_errors", "write_recoveries")
    version = row.get("schema_version")
    require(type(version) is int and version in (1, 2), "unknown audio schema")
    finish_fields = ("drained",) if version == 1 else ("channel_finished", "backend_finish", "backend_finish_operation")
    keys(row, ("schema_version", "requested", "pacing", "mixing_requested", "backend", "delivery", *finish_fields,
               "underruns_measured", *fields),
         ("device_rate", "stretch_ratio_min", "stretch_ratio_max", "queued_seconds_mean", "queued_seconds_max",
          "underruns", "paced_batches", "late_batches", *(("backend_finish_code",) if version == 2 else ())))
    require(row["requested"] == requested
            and requested in ("paced", "unpaced") and row["pacing"] == ("frontend" if requested == "paced" else "unpaced")
            and row["mixing_requested"] in ("async", "sync"), "audio schema or requested mode/mixing mismatch")
    require(all(type(row[name]) is int and 0 <= row[name] <= U64_MAX for name in fields), "invalid audio frame/write counters")
    finished = row["drained" if version == 1 else "channel_finished"]
    require(type(finished) is bool and type(row["underruns_measured"]) is bool, "invalid audio booleans")
    require(type(row["backend"]) is str and row["backend"] and row["delivery"] in ("device", "incomplete", "mixing_only"),
            "invalid audio backend/delivery")
    for name in ("stretch_ratio_min", "stretch_ratio_max", "queued_seconds_mean", "queued_seconds_max"):
        if name in row:
            require(type(row[name]) in (int, float) and math.isfinite(row[name]) and row[name] >= 0, "invalid audio ratio/queue observation")
    require(row["underruns_measured"] == ("underruns" in row), "audio underrun availability contradicts count")
    if "underruns" in row:
        require(type(row["underruns"]) is int and row["underruns"] >= 0, "invalid measured underrun count")
    if "device_rate" in row:
        require(type(row["device_rate"]) is int and 0 < row["device_rate"] < 2**32, "invalid audio device rate")
    for name in ("paced_batches", "late_batches"):
        require((name in row) == (requested == "paced"), "audio pacing counters contradict mode")
        if name in row:
            require(type(row[name]) is int and 0 <= row[name] <= U64_MAX, "invalid paced batch count")
    require(row.get("late_batches", 0) <= row.get("paced_batches", 0), "late batches exceed paced batches")
    require(row["device_frames_accepted"] <= row["device_frames_offered"]
            and row["partial_writes"] + row["failed_writes"] <= row["device_writes"]
            and row["written_frames"] <= row["output_frames"]
            and row["discarded_frames"] <= row["received_frames"] <= row["mixed_frames"], "contradictory audio counters")
    for low, high in (("stretch_ratio_min", "stretch_ratio_max"), ("queued_seconds_mean", "queued_seconds_max")):
        if low in row and high in row:
            require(row[low] <= row[high], "contradictory audio summary observations")
    finish = "unknown_historical_schema1"
    if version == 2:
        require(row["backend"] in ("alsa", "aaudio", "pending", "unavailable"),
                "unknown schema-2 audio backend")
        finish, operation, code = row["backend_finish"], row["backend_finish_operation"], row.get("backend_finish_code")
        require(finish in ("unavailable", "not_attempted", "in_progress", "succeeded", "failed"),
                "invalid backend finish status")
        returned = finish in ("succeeded", "failed")
        require(("backend_finish_code" in row) == returned, "backend finish code availability contradicts status")
        require(operation == ("snd_pcm_drain" if finish in ("in_progress", "succeeded", "failed") else "none"),
                "backend finish operation contradicts status")
        if returned:
            require(type(code) is int and -(2**31) <= code < 2**31
                    and (code == 0) == (finish == "succeeded"), "backend finish code contradicts status")
        require((row["backend"] == "unavailable") == (finish == "unavailable"), "backend availability contradicts finish")
        if operation == "snd_pcm_drain":
            require(row["backend"] == "alsa" and "device_rate" in row, "drain observation requires an open ALSA device")
        require(finish != "in_progress" or not finished, "channel finished before backend return")
        require(row["backend"] != "pending" or (finish == "not_attempted" and not finished), "pending device contradicts finish")
        require(row["backend"] != "alsa" or not row["underruns_measured"], "ALSA underruns are unmeasured in this source")
        accepted = "device_rate" in row and row["device_frames_accepted"] > 0
        expected_delivery = ("mixing_only" if not accepted else
                             "incomplete" if row["partial_writes"] + row["failed_writes"] else "device")
        require(row["delivery"] == expected_delivery, "delivery contradicts backend write counters")
        if row["backend"] in ("pending", "unavailable"):
            require("device_rate" not in row and all(row[name] == 0 for name in fields if name not in ("mixed_frames",)),
                    "unopened backend contradicts write/frame domains")
    whole = (row["delivery"] == "device" and row["backend"] not in ("unavailable", "pending")
             and "device_rate" in row and finished and row["written_frames"] > 0
             and row["received_frames"] == row["mixed_frames"] and row["device_writes"] > 0
             and row["device_frames_accepted"] == row["device_frames_offered"] > 0
             and row["partial_writes"] == row["failed_writes"] == 0
             and row["written_frames"] == row["output_frames"])
    completion = whole and version == 2 and finish == "succeeded"
    return {"full_write_admitted": whole, "backend_completion_admitted": completion,
            "backend_finish": finish, "completion_required": completion_required,
            "admitted": completion if completion_required else whole, "report": row,
            "sound_correctness": "delivery counters only; catch-up discards retained",
            "acoustic_quality": "unknown_not_measured", "physical_device_identity": "unknown_not_measured"}


def canonical_u64(value):
    require(type(value) is str and re.fullmatch(r"0|[1-9][0-9]{0,19}", value) is not None,
            "budget counter must be a canonical u64 decimal string")
    number = int(value)
    require(number <= U64_MAX, "budget counter exceeds u64")
    return number


def presentation_record(row, schema_version=1):
    budget_fields = ("panel_budget_status", "panel_budget_limit_bytes", "panel_budget_charged_bytes")
    keys(row, ("sequence", "output", "panel", "turned", "target", "layout", "layout_set", "fit", "top_crop",
               "second_window", "panel_shot_status", "panel_shot_retained"),
         ("panel_shot_index", "panel_shot_numbered", "top", "bottom", *(budget_fields if schema_version == 2 else ())))
    require(type(row["sequence"]) is int and 0 <= row["sequence"] <= U64_MAX
            and type(row["output"]) is str and row["panel_shot_status"] in ("disabled", "written", "write_failed"),
            "invalid geometry presentation identity/status")
    require(all(type(row[name]) is bool for name in ("turned", "layout_set", "second_window", "panel_shot_retained")),
            "invalid geometry presentation flags")
    for field in ("panel", "target", "layout"):
        require(type(row[field]) is list and len(row[field]) == 2
                and all(type(n) is int and -(2**31) <= n < 2**31 for n in row[field]), "invalid presentation dimensions")
    require(type(row["fit"]) is list and len(row["fit"]) == 3
            and all(type(n) in (int, float) and math.isfinite(n) for n in [*row["fit"], row["top_crop"]]),
            "invalid presentation fit/crop")
    if "panel_shot_index" in row:
        require(type(row["panel_shot_index"]) is int and 0 <= row["panel_shot_index"] < 2**32, "invalid panel shot index")
    if "panel_shot_numbered" in row:
        require(type(row["panel_shot_numbered"]) is bool, "invalid panel shot numbering flag")
    if schema_version == 1 and "panel_shot_index" in row:
        require(row["panel_shot_status"] == "written" and row.get("panel_shot_numbered") is True
                and row["panel_shot_retained"], "v1 index only identifies a retained numbered written file")
    if schema_version == 2:
        attempted = row["panel_shot_status"] != "disabled"
        require(all((field in row) == attempted for field in
                    ("panel_shot_index", "panel_shot_numbered", "panel_budget_status")),
                "v2 attempt identity/budget availability contradicts shot status")
        require(row["panel_shot_retained"] == (attempted and row["panel_shot_numbered"]
                                              and row["panel_shot_status"] == "written"),
                "retained flag contradicts numbered written attempt")
        status = row.get("panel_budget_status")
        require(not attempted or status in ("absent", "invalid", "ready", "exhausted", "write_failed", "size_failed"),
                "invalid panel budget status")
        counted = status in ("ready", "exhausted", "write_failed", "size_failed")
        require(all((field in row) == counted for field in budget_fields[1:]), "panel budget counter availability mismatch")
        if counted:
            limit, charged = (canonical_u64(row[field]) for field in budget_fields[1:])
            require(limit > 0 and charged <= limit, "panel budget charged/limit contradiction")
        if status == "ready":
            require(row["panel_shot_status"] == "written" and charged > 0, "ready budget contradicts written reservation")
        if status in ("invalid", "exhausted", "write_failed", "size_failed"):
            require(row["panel_shot_status"] == "write_failed", "failed budget contradicts shot status")
    for name in ("top", "bottom"):
        if name not in row:
            continue
        screen = row[name]
        keys(screen, ("area",), ("viewport", "clip", "surface", "pixels"))
        for field, values in screen.items():
            size = 2 if field == "pixels" else 4
            lower, upper = (0, 2**32) if field == "pixels" else (-(2**31), 2**31)
            require(type(values) is list and len(values) == size
                    and all(type(n) is int and lower <= n < upper for n in values), "invalid presented screen vector")


def geometry_record(data, screenshots):
    row = tomllib.loads(data.decode())
    keys(row, ("schema_version", "unobserved", "renderer", "requested", "shots"), ("final_presentation",))
    require(type(row["schema_version"]) is int and row["schema_version"] in (1, 2), "unknown render geometry schema")
    require(type(row["unobserved"]) is list and all(type(item) is str for item in row["unobserved"]), "invalid geometry unknowns")
    keys(row["renderer"], ("kind", "readback_scale"), ("name", "effective_quarters"))
    require(row["renderer"]["kind"] in ("gles", "gles-panel", "software")
            and type(row["renderer"]["readback_scale"]) is int and row["renderer"]["readback_scale"] > 0,
            "invalid geometry renderer")
    if "name" in row["renderer"]:
        require(type(row["renderer"]["name"]) is str, "invalid renderer name")
    if "effective_quarters" in row["renderer"]:
        require(type(row["renderer"]["effective_quarters"]) is int and row["renderer"]["effective_quarters"] > 0,
                "invalid renderer scale")
    keys(row["requested"], ("resolution", "layout", "aspect", "swapped", "stretched", "stacked", "second_window"))
    requested = row["requested"]
    require(type(requested["resolution"]) in (int, float) and math.isfinite(requested["resolution"])
            and requested["resolution"] > 0
            and all(type(requested[name]) is str for name in ("layout", "aspect", "stretched"))
            and all(type(requested[name]) is bool for name in ("swapped", "stacked", "second_window")),
            "invalid requested geometry")
    require(type(row["shots"]) is list and len(row["shots"]) == len(screenshots), "geometry screenshot inventory mismatch")
    files = []
    for shot in row["shots"]:
        keys(shot, ("frame", "file", "top", "bottom"), ("presentation",))
        require(type(shot["frame"]) is int and 0 <= shot["frame"] <= U64_MAX and type(shot["file"]) is str
                and shot["file"] == f"frame-{shot['frame']:05}.bmp", "invalid geometry shot frame/file binding")
        files.append(shot["file"])
        if "presentation" in shot:
            presentation_record(shot["presentation"], row["schema_version"])
        for name in ("top", "bottom"):
            screen = shot[name]
            keys(screen, ("readback",), ("gpu_surface",))
            require(type(screen["readback"]) is list and len(screen["readback"]) == 2
                    and all(type(n) is int and 0 < n < 2**32 for n in screen["readback"]), "invalid geometry readback dimensions")
            if "gpu_surface" in screen:
                surface = screen["gpu_surface"]
                keys(surface, ("console", "drawn", "rows", "columns"))
                require(all(type(surface[field]) is list and len(surface[field]) == 2
                            and all(type(n) is int and 0 <= n < 2**32 for n in surface[field])
                            for field in ("console", "drawn", "rows"))
                        and type(surface["columns"]) is int and 0 <= surface["columns"] < 2**32,
                        "invalid observed GPU surface")
    require(len(set(files)) == len(files) and set(files) == set(screenshots), "geometry shots do not bind probe screenshots")
    if "final_presentation" in row:
        presentation_record(row["final_presentation"], row["schema_version"])
    return {"record": row, "validation_scope": f"v{row['schema_version']} schema and screenshot/readback bindings; presentation is not scanout proof",
            "presentation_pixels": "unknown_not_captured"}


def gpu_reader():
    path = Path(__file__).with_name("gpu_census.py")
    spec = importlib.util.spec_from_file_location("diagnostic_gpu_census", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def collect(trial, report, screenshots, profile=None):
    """Retain observed inputs, including undeclared compile-time instrumentation."""
    reports = list(trial.glob("probe-*/report.toml"))
    packet_paths = list(trial.glob("probe-*/packet-census.toml"))
    geometry_paths = list(trial.glob("probe-*/render-geometry.toml"))
    log_data, log_source = snapshot(trial / "stderr.log", 64 * 1024 * 1024)
    texture_seen = b"texture cache stats" in log_data
    observed = bool(packet_paths or geometry_paths or "audio" in report or texture_seen)
    if profile is None and not observed:
        return None
    result = {"evidence_class": NON_TIMING, "timing_eligible": False, "valid": True, "errors": [],
              "requested": profile, "packet": {"status": "unknown_not_observed"},
              "texture": {"status": "unknown_not_observed"}, "audio": {"status": "off_or_unobserved"},
              "geometry": {"status": "unknown_not_observed"}}
    try:
        require(len(reports) == 1, "diagnostics require exactly one probe report")
        _, result["report_source"] = snapshot(reports[0])
        if "audio" in report:
            require(type(report["audio"]) is dict, "audio observation must be a table")
            mode = profile["audio"] if profile else report["audio"].get("requested")
            completion_required = bool(profile and profile.get("audio_completion") == "backend") or report["audio"].get("schema_version") == 2
            result["audio"] = audio_admission(report["audio"], mode, completion_required)
            require(result["audio"]["admitted"], "audio did not satisfy requested delivery/backend-completion admission")
        elif profile and profile["audio"] != "off":
            raise ValueError("requested audio observer missing")
        if geometry_paths or (profile and profile["geometry"]):
            require(len(geometry_paths) == 1 and report.get("render_geometry_file") == "render-geometry.toml", "requested/observed geometry file missing or ambiguous")
            data, source = snapshot(geometry_paths[0])
            result["geometry"] = {"source": source, **geometry_record(data, screenshots)}
        if packet_paths or (profile and profile["packet_census"]) or texture_seen or (profile and profile["texture_stats"]):
            reader = gpu_reader()
            _, result["gpu_reader_source"] = snapshot(Path(reader.__file__))
            if packet_paths or (profile and profile["packet_census"]):
                require(len(packet_paths) == 1, "requested packet-census feature artifact missing or ambiguous")
                data, source = snapshot(packet_paths[0], reader.MAX_PACKET_BYTES)
                result["packet"] = {"source": source, "instrumentation": "compile-time; cannot disable with profile",
                                    "requested": bool(profile and profile["packet_census"]), **reader.parse_packet(data)}
                if profile:
                    require(result["packet"]["window_coverage"]["start_batch"] == profile.get("packet_window_start"), "packet window contradicts declaration")
            if texture_seen or (profile and profile["texture_stats"]):
                result["texture"] = {"source": log_source, **reader.parse_texture_log(log_data)}
                if result["texture"]["line_count"]:
                    require(profile is None or result["texture"]["timed_input"] == profile["texture_timing"], "texture timing mode contradicts declaration")
                # Zero rows are explicitly unknown: process::exit omits the Drop tail.
    except (OSError, ValueError, TypeError, KeyError, UnicodeError) as error:
        result["valid"] = False
        result["errors"].append(str(error))
    return result


def timing_reasons(manifest):
    reasons = []
    if manifest.get("evidence_class") == NON_TIMING:
        reasons.append("non-timing evidence class")
    if "diagnostic_profile" in manifest or "diagnostic_profile" in manifest.get("experiment", {}):
        reasons.append("declared diagnostic replay")
    for trial in manifest.get("trials", []):
        if "diagnostics" in trial:
            reasons.append("observed diagnostic receipt/artifact")
        env = trial.get("environment", {})
        if any(key in env for key in ENV_KEYS):
            reasons.append("diagnostic process environment")
        report = trial.get("probe", {}).get("report", {})
        if "audio" in report or "render_geometry_file" in report or trial.get("probe", {}).get("diagnostic_artifacts"):
            reasons.append("diagnostic probe output")
    return sorted(set(reasons))
