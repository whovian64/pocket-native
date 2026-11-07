#!/usr/bin/env python3
"""Closed render-profile adapter: requested settings apart from what was observed.

Python 3.11+, standard library only. A profile generates the frontend's
configuration settings.toml and, for an optional offscreen panel capture, the
GL presentation environment. It never launches games, builds or times anything.

Values come from the frontend and GLES sources (unchanged at 31b12dc, f50025a
and 781341a): Settings lives in POCKET_NATIVE_CONFIG/settings.toml with
`#[serde(default)]` and no deny_unknown_fields, so a misspelt key or variant is
silently a default and the generator emits only the exact enum names below.
ZAKURO_RESOLUTION overrides the settings file's resolution, and GLES draws
`round(scale * 4).clamp(4, 16)` quarters. Without ZAKURO_KMS or an app window,
which the evaluator never provides, a panel exists only when ZAKURO_PANEL_SHOT
asks for the offscreen one (ZAKURO_PANEL_SIZE=WxH, else 640x480); otherwise
nothing is presented, so aspect and layout cannot be exercised or observed.

Three things stay separate: the requested profile, the runtime's own record of
what it parsed and drew (ZAKURO_PROBE_GEOMETRY), and scanout, which nothing
here observes. An offscreen PPM is a GL readback, never a scanned-out frame.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import math
import os
import stat
from pathlib import Path
import re
import struct

LAYOUTS = ("BigTop", "TopOnly", "SideBySide", "Original", "BigBeside")
ASPECTS = ("Original", "Integer", "FourThree", "SixteenTen", "SixteenNine", "Stretch", "Crop")
STRETCHES = ("Off", "Top", "Bottom", "Both")
# Settings::default(): what a trial without a configuration settings.toml gets.
DEFAULT_PRESENTATION = {"layout": "BigTop", "aspect": "Original", "swapped": False, "stretched": "Off"}
PROFILE_KEYS = {"schema_version", "id", "internal_scale", "aspect", "layout", "swapped", "stretched",
                "presentation_capture"}
CAPTURE_KEYS = {"panel_size", "file", "retain", "max_bytes"}
ENV_KEYS = {"ZAKURO_PANEL_SHOT", "ZAKURO_PANEL_SIZE"}
SAFE_ID = re.compile(r"[a-z0-9][a-z0-9.-]{0,79}\Z")
SHOT_FILE = {"last": re.compile(r"[a-z][a-z0-9-]{0,62}\.ppm\Z"),
             "numbered": re.compile(r"[a-z][a-z0-9-]{0,62}%\.ppm\Z")}
# Bounds retained artifacts at admission, not total overwrite I/O or an
# enforced live filesystem quota (the process may write before rejection).
MAX_CAPTURE_BYTES = 8 * 1024**3
NON_TIMING = "NON-TIMING presentation capture; every presentation is read back and written to disk"
SCANOUT_UNOBSERVED = ("physical scanout: offscreen panel images are GL readbacks; no KMS flip or display timing is observed",)
RENDERER_LINE = re.compile(r"drawing on (.+) at ([0-9]+(?:\.[0-9]+)?)x(?:, vertices shaded on the CPU,)? through OpenGL ES\Z")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def exact(value, keys, label):
    require(type(value) is dict and set(value) == set(keys), f"{label} requires exactly {sorted(keys)}")


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate render-profile field: {key}")
        result[key] = value
    return result


def f32(value):
    """The value as Rust stores an f32, for comparing recorded floats exactly."""
    try:
        return struct.unpack("<f", struct.pack("<f", value))[0]
    except (OverflowError, struct.error):
        return math.nan


def ppm_bytes(panel):
    width, height = panel
    return len(f"P6\n{width} {height}\n255\n") + width * height * 3


def layout_size(panel):
    """display.rs layout_for: the logical layout canvas a panel this size gets."""
    width, height = panel
    if width * 3 >= height * 4:
        return [(480 * width + height // 2) // height, 480]
    return [640, (640 * height + width // 2) // width]


def validate(value):
    exact(value, PROFILE_KEYS, "render profile")
    require(type(value["schema_version"]) is int and value["schema_version"] == 1,
            "unsupported render profile schema_version")
    require(type(value["id"]) is str and SAFE_ID.fullmatch(value["id"]) is not None,
            "render profile id must be a short lowercase path-safe name")
    scale = value["internal_scale"]
    # GLES rounds and clamps; any other request would label work it never drew.
    require(type(scale) in (int, float) and math.isfinite(scale) and 1 <= scale <= 4
            and float(scale * 4).is_integer(), "internal_scale must be a whole quarter step from 1 through 4")
    for name, variants in (("layout", LAYOUTS), ("aspect", ASPECTS), ("stretched", STRETCHES)):
        require(type(value[name]) is str and value[name] in variants, f"{name} must be one of the frontend's {variants}")
    require(type(value["swapped"]) is bool, "swapped must be boolean")
    # Stretch only reaches a second window or stacked panels, which no evaluator trial has.
    require(value["stretched"] == "Off", "stretched only affects a second window or stacked panels; it must be Off")
    result = copy.deepcopy(value)
    result["internal_scale"] = float(scale)
    capture = value["presentation_capture"]
    if capture is None:
        require(presentation_is_default(result),
                "aspect, layout and swap have no panel without presentation_capture; declare one or keep the defaults")
        return result
    exact(capture, CAPTURE_KEYS, "presentation_capture")
    panel = capture["panel_size"]
    require(type(panel) is list and len(panel) == 2 and all(type(n) is int and 64 <= n <= 8192 for n in panel),
            "panel_size must be two integers from 64 through 8192")
    require(type(capture["retain"]) is str and capture["retain"] in SHOT_FILE, "retain must be last or numbered")
    require(type(capture["file"]) is str and SHOT_FILE[capture["retain"]].fullmatch(capture["file"]) is not None,
            "capture file must be a bare lowercase .ppm name, with one % before .ppm exactly when numbered")
    budget = capture["max_bytes"]
    require(type(budget) is int and ppm_bytes(panel) <= budget <= MAX_CAPTURE_BYTES,
            "max_bytes must hold at least one panel image and at most 8 GiB")
    return result


def presentation_is_default(profile):
    return all(profile[name] == value for name, value in DEFAULT_PRESENTATION.items())


def declaration_hash(profile):
    return hashlib.sha256(json.dumps(profile, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def load(path, limit=64 * 1024):
    path = Path(path)
    require(not path.is_symlink() and path.is_file(), f"render profile must be a regular non-symlink file: {path}")
    with path.open("rb") as source:
        data = source.read(limit + 1)
    require(len(data) <= limit, "render profile exceeds byte budget")
    profile = validate(json.loads(data, object_pairs_hook=unique_object))
    return profile, {"path": str(path.resolve()), "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}


def config_settings(profile):
    """The trial's POCKET_NATIVE_CONFIG/settings.toml, or None for today's absent file.

    Default presentation writes nothing, so its trials stay byte-identical to a
    run without a render profile; ZAKURO_RESOLUTION already carries the scale.
    """
    if presentation_is_default(profile):
        return None
    fields = {"aspect": f'"{profile["aspect"]}"', "layout": f'"{profile["layout"]}"',
              "resolution": repr(profile["internal_scale"]), "stretched": f'"{profile["stretched"]}"',
              "swapped": str(profile["swapped"]).lower()}
    return "".join(f"{key} = {text}\n" for key, text in sorted(fields.items()))


def environment(profile, trial):
    """Presentation variables for a trial directory; empty without a capture."""
    capture = profile["presentation_capture"]
    if capture is None:
        return {}
    trial = Path(trial)
    require(trial.is_absolute() and ".." not in trial.parts, "trial directory must be absolute and normalized")
    require(trial.resolve() == trial, "trial directory must not traverse symlinks")
    width, height = capture["panel_size"]
    return {"ZAKURO_PANEL_SHOT": str(trial / capture["file"]), "ZAKURO_PANEL_SIZE": f"{width}x{height}"}


def timing_eligible(profile):
    return profile["presentation_capture"] is None


def check_run(profile, *, resolution, frames, implementation_environment=None, diagnostic=None,
              seeded_config=(), implementation_settings=None):
    """Reject a trial binding that would contradict or hide the requested profile.

    `diagnostic` is a validated diagnostic profile; `seeded_config` lists the
    relative paths of the seed configuration tree.
    """
    require(type(resolution) in (int, float) and float(resolution) == profile["internal_scale"],
            "--resolution sets ZAKURO_RESOLUTION, which overrides the settings file; it must equal internal_scale")
    require(type(frames) is int and frames >= 8, "frames must be an integer of at least 8")
    # Even the default writes nothing only because Settings::default() then applies.
    require(all(Path(item).name != "settings.toml" for item in seeded_config),
            "a seeded configuration settings.toml would override the render profile's presentation")
    if implementation_settings is not None:
        require(implementation_settings.startswith("[advanced]\n") and "\n[" not in implementation_settings[1:]
                and not any(line.split(" = ")[0] in PROFILE_KEYS | {"resolution"}
                            for line in implementation_settings.splitlines()[1:]),
                "implementation settings may only carry the data directory's [advanced] switches")
    render_env = set(environment(profile, Path("/trial")))
    require(not render_env & set(implementation_environment or {}),
            "render presentation environment conflicts with the implementation profile")
    capture = profile["presentation_capture"]
    if capture is None:
        return
    require(diagnostic is not None and diagnostic.get("geometry") is True,
            "presentation capture requires the ZAKURO_PROBE_GEOMETRY diagnostic observer")
    if capture["retain"] == "numbered":
        # Preflight planning estimate, not an in-process disk quota. Runtime
        # presentation frequency still needs live validation; observe() checks
        # the complete retained file set independently after the run.
        require(frames * ppm_bytes(capture["panel_size"]) <= capture["max_bytes"],
                "numbered panel images for every frame would exceed max_bytes")


def renderer_scales(lines):
    """Scales the loader logged, `drawing on <GPU> at <scale>x through OpenGL ES`."""
    return sorted({float(match[2]) for line in lines if isinstance(line, str)
                   and (match := RENDERER_LINE.search(line.rstrip()))})


def verify_ppm(path, panel):
    """Bind an offscreen panel image: binary P6, the declared size, exact bytes."""
    path = Path(path)
    require(not path.is_symlink() and path.is_file(), f"panel image must be a regular non-symlink file: {path.name}")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, "rb") as stream:
        require(stat.S_ISREG(os.fstat(stream.fileno()).st_mode), "panel image must be a regular file")
        data = stream.read(ppm_bytes(panel) + 1)
    width, height = panel
    header = f"P6\n{width} {height}\n255\n".encode()
    require(data.startswith(header) and len(data) == ppm_bytes(panel),
            "panel image is not a complete P6 image of the declared panel size")
    return {"file": path.name, "size": [width, height], "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


def screens_shown(profile):
    """Which of [top, bottom] a single offscreen panel shows for this layout."""
    if profile["layout"] != "TopOnly":
        return [True, True]
    return [not profile["swapped"], profile["swapped"]]


def presentation_errors(profile, row):
    capture = profile["presentation_capture"]
    panel = capture["panel_size"]
    errors = []
    expected = {"output": "offscreen", "panel": panel, "target": panel, "layout": layout_size(panel),
                "layout_set": True, "turned": False, "second_window": False,
                "panel_shot_status": "written", "panel_shot_numbered": capture["retain"] == "numbered",
                "panel_shot_retained": capture["retain"] == "numbered"}
    for name, value in expected.items():
        if row.get(name) != value:
            errors.append(f"presentation {name}={row.get(name)!r}, expected {value!r}")
    crop = row.get("top_crop")
    # toml may write the f32 widened or shortest; compare both as f32.
    if type(crop) not in (int, float) or f32(crop) != f32(0.1 if profile["aspect"] == "Crop" else 0.0):
        errors.append(f"presentation top_crop={crop!r} contradicts aspect {profile['aspect']}")
    shown = [name in row for name in ("top", "bottom")]
    if shown != screens_shown(profile):
        errors.append(f"presented screens {shown}, expected {screens_shown(profile)} for the layout")
    return errors


def observe(profile, *, geometry=None, renderer_log_lines=(), trial=None):
    """Split the requested profile from runtime observations, unknowns explicit.

    `geometry` is the record diagnostic_profiles.geometry_record returned (or
    None when the observer was off); `trial` holds the panel images. Geometry
    may retain an earlier presentation. File names and bytes bind artifacts,
    never a screenshot to an exact presentation frame or physical scanout.
    """
    checks, errors, observed = {}, [], {}
    record = geometry or {}
    requested_by_runtime = record.get("requested")
    if requested_by_runtime is None:
        checks["runtime_requested"] = None
    else:
        expected = {"layout": profile["layout"], "aspect": profile["aspect"], "swapped": profile["swapped"],
                    "stretched": profile["stretched"], "stacked": False, "second_window": False}
        drift = sorted(name for name, value in expected.items() if requested_by_runtime.get(name) != value)
        resolution = requested_by_runtime.get("resolution")
        if type(resolution) not in (int, float) or f32(resolution) != profile["internal_scale"]:
            drift.append("resolution")
        checks["runtime_requested"] = not drift
        errors += [f"runtime parsed {name}={requested_by_runtime.get(name)!r}" for name in drift]
    quarters = round(profile["internal_scale"] * 4)
    logged = renderer_scales(renderer_log_lines)
    observed["renderer_log_scales"] = logged
    checks["renderer_scale_log"] = None if not logged else logged == [profile["internal_scale"]]
    renderer = record.get("renderer")
    observed["renderer"] = renderer
    if renderer is None or "effective_quarters" not in renderer:
        checks["renderer_quarters"] = None
    else:
        checks["renderer_quarters"] = renderer.get("kind") == "gles" and renderer["effective_quarters"] == quarters
    surfaces = [shot[name]["gpu_surface"] for shot in record.get("shots", [])
                for name in ("top", "bottom") if "gpu_surface" in shot.get(name, {})]
    pairs = sorted({(tuple(s["console"]), tuple(s["drawn"])) for s in surfaces})
    observed["surfaces"] = {"count": len(surfaces), "console_drawn": [[list(c), list(d)] for c, d in pairs]}
    # gles.rs up(): a surface made after set_scale is console * quarters / 4.
    checks["surface_scale"] = None if not surfaces else all(
        list(drawn) == [n * quarters // 4 for n in console] for console, drawn in pairs)
    for name in ("renderer_quarters", "renderer_scale_log", "surface_scale"):
        if checks[name] is False:
            errors.append(f"{name} contradicts internal_scale {profile['internal_scale']:g}")
    presentations = [shot["presentation"] for shot in record.get("shots", []) if "presentation" in shot]
    if "final_presentation" in record:
        presentations.append(record["final_presentation"])
    observed["presentations"] = len(presentations)
    observed["presentation_records"] = presentations
    observed["presentation_geometry_conformance"] = "unknown_not_mathematically_verified"
    capture = profile["presentation_capture"]
    images = []
    if capture is None:
        # No panel is expected; one appearing means the environment was not the evaluator's.
        checks["presentation"] = None if geometry is None else not presentations
        if presentations:
            errors.append("a presentation was recorded although no panel was requested")
    elif not presentations:
        checks["presentation"] = False
        errors.append("presentation capture requested but no presentation was observed")
    else:
        problems = sorted({error for row in presentations for error in presentation_errors(profile, row)})
        checks["presentation"] = not problems
        errors += problems
        try:
            require(trial is not None, "panel images need the trial directory")
            if capture["retain"] == "last":
                images.append(verify_ppm(Path(trial) / capture["file"], capture["panel_size"]))
            else:
                indices = [row.get("panel_shot_index") for row in presentations]
                require(indices and all(type(index) is int and 0 <= index < 2**32 for index in indices),
                        "numbered capture requires a u32 index on every observed presentation")
                prefix, suffix = capture["file"].split("%")
                pattern = re.compile(re.escape(prefix) + r"([0-9]{5,10})" + re.escape(suffix) + r"\Z")
                paths = {}
                # Snapshots are sparse. Account for all retained files, including
                # presentations absent from the geometry snapshots.
                for path in Path(trial).iterdir():
                    if match := pattern.fullmatch(path.name):
                        index = int(match[1])
                        require(index < 2**32 and path.name == capture["file"].replace("%", f"{index:05}"),
                                "noncanonical panel image index")
                        paths[index] = path
                        require(len(paths) * ppm_bytes(capture["panel_size"]) <= capture["max_bytes"],
                                "retained panel images exceed max_bytes")
                require(set(indices) <= set(paths), "numbered capture retained no indexed panel image")
                for index, path in sorted(paths.items()):
                    images.append({"index": index, **verify_ppm(path, capture["panel_size"])})
            checks["panel_images"] = True
        except (OSError, ValueError, TypeError, KeyError) as error:
            checks["panel_images"] = False
            errors.append(str(error))
    observed["panel_images"] = images
    scale_proven = checks["renderer_quarters"] is True or checks["renderer_scale_log"] is True
    required = ["presentation", "panel_images"] if capture is not None else []
    # A written settings file counts only once the runtime reports parsing it.
    if config_settings(profile) is not None:
        required.append("runtime_requested")
    admitted = (scale_proven and not any(value is False for value in checks.values())
                and all(checks.get(name) is True for name in required))
    return {"requested": {"declaration": profile, "declaration_sha256": declaration_hash(profile),
                          "config_settings": config_settings(profile),
                          "environment_keys": sorted(environment(profile, Path("/trial")))},
            "runtime_requested": requested_by_runtime, "observed": observed,
            "unobserved": sorted({*record.get("unobserved", []), *SCANOUT_UNOBSERVED}),
            "scanout": "unknown_not_observed", "checks": checks, "errors": errors, "admitted": admitted,
            "admission_scope": "scale evidence and settings/panel-byte consistency only; layout areas and viewports are retained but mathematical agreement with requested aspect/layout is unverified; latest presentation may precede screenshot",
            "matrix_presentation_capability_admitted": False,
            "timing_eligible": timing_eligible(profile) and geometry is None,
            "evidence_class": None if timing_eligible(profile) else NON_TIMING}


def frame_geometry_profile(profile):
    """frame_geometry.expected_geometry input when that model covers the profile, else None."""
    capture = profile["presentation_capture"]
    if (capture is None or profile["layout"] not in ("Original", "TopOnly", "SideBySide")
            or profile["aspect"] not in ("Original", "FourThree")):
        return None
    return {"resolution": profile["internal_scale"], "aspect": profile["aspect"], "layout": profile["layout"],
            "swapped": profile["swapped"], "layout_size": layout_size(capture["panel_size"])}


TITLES = ("ultra-sun", "omega-ruby", "pokemon-y")
SCREEN_SCALES = (1.0, 1.5, 2.0)
SCREEN_PRESENTATIONS = (("BigTop", "Original"), ("BigTop", "FourThree"), ("Original", "Original"),
                        ("TopOnly", "Original"), ("SideBySide", "Original"))


def profile_for(scale, layout, aspect, capture):
    suffix = "-panel" if capture else ""
    return validate({"schema_version": 1, "id": f"{scale:g}x-{layout.lower()}-{aspect.lower()}{suffix}",
                     "internal_scale": scale, "aspect": aspect, "layout": layout, "swapped": False,
                     "stretched": "Off", "presentation_capture": capture})


def screening_plan():
    """Non-timing screening: scale cells at default presentation, then aspect and
    layout cells one axis at a time at 1x on a declared 640x480 offscreen panel."""
    capture = {"panel_size": [640, 480], "file": "panel.ppm", "retain": "last", "max_bytes": ppm_bytes([640, 480])}
    per_title = ([("scale", profile_for(scale, "BigTop", "Original", None)) for scale in SCREEN_SCALES]
                 + [("presentation", profile_for(1.0, layout, aspect, copy.deepcopy(capture)))
                    for layout, aspect in SCREEN_PRESENTATIONS])
    cells = [{"id": f"{title}-{profile['id']}", "title": title, "axis": axis, "profile": profile,
              "profile_sha256": declaration_hash(profile), "resolution": profile["internal_scale"],
              "diagnostic_profile": {"schema_version": 1, "geometry": True},
              "evidence_class": NON_TIMING if profile["presentation_capture"] else "NON-TIMING diagnostic geometry observer",
              # Each presentation overwrites the image: frames bound the disk writes.
              "panel_write_bytes_per_presentation": ppm_bytes(capture["panel_size"]) if profile["presentation_capture"] else 0,
              "status": "pending-observation"} for title in TITLES for axis, profile in per_title]
    return {"schema_version": 1, "kind": "render-profile-screening-plan", "timing_admitted": False,
            "cells": cells, "limits": [
                "Planning only: nothing ran, and no cell proves a matrix capability until observe() admits it.",
                "Scale cells keep the default presentation, so no settings.toml is written for them.",
                "Presentation cells exist only with the offscreen panel capture; every presentation is read back and written.",
                "Aspect and layout are varied one at a time at 1x; their interactions with scale are not covered.",
                "Offscreen panel images are GL readbacks; physical scanout stays unobserved."]}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    check = commands.add_parser("validate", help="validate a profile and print its generated inputs")
    check.add_argument("profile", type=Path)
    plan = commands.add_parser("plan", help="freeze the non-timing screening plan")
    plan.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "validate":
            profile, source = load(args.profile)
            print(json.dumps({"profile": profile, "source": source, "declaration_sha256": declaration_hash(profile),
                              "config_settings": config_settings(profile), "timing_eligible": timing_eligible(profile),
                              "environment_keys": sorted(environment(profile, Path("/trial")))}, indent=2))
        else:
            # Refuse overwrite: an earlier frozen plan stays inspectable.
            with args.output.open("x") as out:
                out.write(json.dumps(screening_plan(), indent=2, sort_keys=True, allow_nan=False) + "\n")
    except (OSError, ValueError) as error:
        parser.exit(1, f"render profile failed: {error}\n")


if __name__ == "__main__":
    main()
