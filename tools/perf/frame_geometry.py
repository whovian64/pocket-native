#!/usr/bin/env python3
"""Strict geometry sidecars for equal-profile captures; no image resampling.

The probe reads scaled screen readbacks before presentation. The current GLES
and software scale getters return 1; other implementations may differ.
Readback dimensions cannot prove internal render scale, aspect or layout.
This module does not retrofit that missing observer: a caller must supply real
internal-surface/presentation observations to claim coverage of those stages.
"""

from __future__ import annotations

import argparse
from fractions import Fraction
import hashlib
import json
import math
from pathlib import Path
import re
import struct


SCREENS = {"top": [400, 240], "bottom": [320, 240]}
SOURCE_REGIONS = {"top": [0, 0, 400, 240], "bottom": [40, 240, 320, 240]}
SHA256 = re.compile(r"[0-9a-f]{64}\Z")


class GeometryError(ValueError):
    """Missing, contradictory or incomparable geometry evidence."""


def _keys(value, expected, name):
    if not isinstance(value, dict) or set(value) != set(expected):
        raise GeometryError(f"{name}: expected exactly {sorted(expected)}")


def _size(value, name):
    if (not isinstance(value, list) or len(value) != 2
            or any(type(n) is not int or n <= 0 for n in value)):
        raise GeometryError(f"{name}: expected two positive integer dimensions")
    return value


def _round(value):
    # All geometry here is nonnegative; Rust f32::round rounds ties away from 0.
    return (value.numerator * 2 + value.denominator) // (2 * value.denominator)


def expected_geometry(profile):
    """Model a limited, explicitly supported single-window layout-space profile.

    layout_size is the frontend's logical layout canvas, not physical panel
    pixels. Touch controls, second windows, stacked panels and integer/stretch/
    crop modes need another model; unknown fields/modes fail rather than guess.
    Nominal screen dimensions are NOT a claim that every guest surface has that
    size (games can render to smaller, padded or offscreen surfaces).
    """
    _keys(profile, {"resolution", "aspect", "layout", "swapped", "layout_size"}, "profile")
    resolution = profile["resolution"]
    if (type(resolution) not in (int, float) or not math.isfinite(resolution)
            or not 1 <= resolution <= 4 or resolution * 4 != int(resolution * 4)):
        raise GeometryError("resolution: expected a supported quarter-step scale from 1 to 4")
    if profile["aspect"] not in ("Original", "FourThree"):
        raise GeometryError("aspect: only Original and FourThree are modeled")
    if profile["layout"] not in ("Original", "TopOnly", "SideBySide"):
        raise GeometryError("layout: only Original, TopOnly and SideBySide are modeled")
    if type(profile["swapped"]) is not bool:
        raise GeometryError("swapped: expected boolean")
    width, height = _size(profile["layout_size"], "layout_size")
    # Use rational sizes to avoid Python's ties-to-even integer rounding.
    sizes = [[400 if profile["aspect"] == "Original" else 320, 240], [320, 240]]
    order = ["bottom", "top"] if profile["swapped"] else ["top", "bottom"]
    first, second = [sizes[0 if name == "top" else 1] for name in order]
    layout = profile["layout"]
    if layout == "Original":
        scale = min(Fraction(width, max(first[0], second[0])), Fraction(height, 480))
    elif layout == "TopOnly":
        scale = min(Fraction(width, first[0]), Fraction(height, 240))
    else:
        scale = min(Fraction(width, first[0] + second[0]), Fraction(height, 240))
    a, b = [_round(n * scale) for n in first]
    c, d = [_round(n * scale) for n in second]
    if layout == "Original":
        y = (height - b - d) // 2
        regions = [[(width - a) // 2, y, a, b], [(width - c) // 2, y + b, c, d]]
    elif layout == "TopOnly":
        regions = [[(width - a) // 2, (height - b) // 2, a, b], None]
    else:
        x = (width - a - c) // 2
        regions = [[x, (height - b) // 2, a, b], [x + a, (height - d) // 2, c, d]]
    quarters = int(resolution * 4)
    return {
        "nominal_internal_screens": {
            name: [n * quarters // 4 for n in size] for name, size in SCREENS.items()
        },
        "source_capture": {"size": [400, 480], "regions": {name: list(rect) for name, rect in SOURCE_REGIONS.items()}},
        "presentation_capture": {"size": [width, height], "regions": dict(zip(order, regions))},
    }


def _digest(value, name):
    if not isinstance(value, str) or SHA256.fullmatch(value) is None:
        raise GeometryError(f"{name}: expected lowercase SHA-256")


def verify_bmp_capture(capture, path, *, max_bytes=64 * 1024 * 1024):
    """Bind declared dimensions/digest to a complete, unmodified probe-style BMP.

    Only the current uncompressed 24-bit bottom-up BMP format is admitted.
    A matching file cannot prove that its screen-role/observer labels are true;
    those still need a source-bound capture hook. No decode/resize is performed.
    """
    path = Path(path)
    if path.stat().st_size > max_bytes:
        raise GeometryError("BMP exceeds capture byte budget")
    with path.open("rb") as stream:
        data = stream.read(max_bytes + 1)
    if len(data) > max_bytes or len(data) < 54 or data[:2] != b"BM":
        raise GeometryError("invalid or oversized BMP header")
    size, offset, dib_size = struct.unpack_from("<I4xII", data, 2)
    width, height, planes, bpp, compression = struct.unpack_from("<iiHHI", data, 18)
    if (size != len(data) or offset != 54 or dib_size != 40 or width <= 0 or height <= 0
            or planes != 1 or bpp != 24 or compression != 0):
        raise GeometryError("BMP is not a complete supported 24-bit bottom-up probe capture")
    row = (width * 3 + 3) // 4 * 4
    if 54 + row * height != len(data):
        raise GeometryError("BMP dimensions do not cover exact pixel bytes")
    if capture["size"] != [width, height] or capture["sha256"] != hashlib.sha256(data).hexdigest():
        raise GeometryError("BMP dimensions or exact digest differ from capture metadata")
    return {"size": [width, height], "sha256": hashlib.sha256(data).hexdigest()}


def validate_record(record, *, require_presentation=False, require_internal=False):
    """Validate an observer sidecar, keeping unobserved stages explicitly unknown.

    Captures must be hashed from their actual unmodified bytes. Geometry must
    come from the capture operation, never copied from this expected model.
    This function checks the metadata contract, not its observer provenance.
    trace_sha256 refers to the complete separately validated guest trace.
    """
    _keys(record, {"schema_version", "profile", "internal_screens", "captures", "trace_sha256"}, "record")
    if type(record["schema_version"]) is not int or record["schema_version"] != 1:
        raise GeometryError("schema_version: expected 1")
    expected = expected_geometry(record["profile"])
    _digest(record["trace_sha256"], "trace_sha256")
    internal = record["internal_screens"]
    if internal is not None:
        _keys(internal, SCREENS, "internal_screens")
        for name, size in internal.items():
            _size(size, f"internal_screens.{name}")
        if internal != expected["nominal_internal_screens"]:
            raise GeometryError("observed internal screen dimensions differ from nominal profile")
    elif require_internal:
        raise GeometryError("internal-surface observer missing; native readback cannot prove render scale")
    captures = record["captures"]
    if not isinstance(captures, list) or not captures:
        raise GeometryError("captures: expected nonempty list")
    seen = set()
    scopes = set()
    for capture in captures:
        _keys(capture, {"checkpoint", "scope", "size", "regions", "sha256"}, "capture")
        if type(capture["checkpoint"]) is not int or capture["checkpoint"] < 0:
            raise GeometryError("checkpoint: expected nonnegative integer")
        scope = capture["scope"]
        if scope not in ("source-native-stacked", "presentation-layout"):
            raise GeometryError("scope: unknown capture stage")
        key = (capture["checkpoint"], scope)
        if key in seen:
            raise GeometryError("duplicate checkpoint/scope capture")
        seen.add(key)
        scopes.add(scope)
        _size(capture["size"], "capture.size")
        _digest(capture["sha256"], "capture.sha256")
        _keys(capture["regions"], SCREENS, "capture.regions")
        for rect in capture["regions"].values():
            if rect is not None and (not isinstance(rect, list) or len(rect) != 4
                    or any(type(n) is not int for n in rect)
                    or min(rect[:2]) < 0 or min(rect[2:]) <= 0):
                raise GeometryError("capture.regions: expected integer x/y/width/height or null")
        if scope == "source-native-stacked":
            width, height = capture["size"]
            if width % 400 or height != width * 480 // 400:
                raise GeometryError("source readback dimensions do not match integer-scale probe contract")
            scale = width // 400
            modeled = {"size": [400 * scale, 480 * scale],
                       "regions": {name: [n * scale for n in rect] for name, rect in SOURCE_REGIONS.items()}}
        else:
            modeled = expected["presentation_capture"]
        if capture["size"] != modeled["size"] or capture["regions"] != modeled["regions"]:
            raise GeometryError(f"{scope}: dimensions or named screen regions differ; no resizing permitted")
    if require_presentation and "presentation-layout" not in scopes:
        raise GeometryError("presentation observer missing; probe BMP cannot prove aspect/layout/swap")
    return {
        "valid": True,
        "source_capture_declared": "source-native-stacked" in scopes,
        "presentation_capture_declared": "presentation-layout" in scopes,
        "internal_screen_dimensions_declared": internal is not None,
        "limits": ["metadata validation does not establish observer provenance, equal guest work, timing or audio correctness"],
    }


def compare_records(baseline, candidate, *, baseline_dir, candidate_dir, **requirements):
    """Exact same-profile comparison; never equate different scales/aspects."""
    left = validate_record(baseline, **requirements)
    right = validate_record(candidate, **requirements)
    for record, directory in ((baseline, Path(baseline_dir)), (candidate, Path(candidate_dir))):
        trace = directory / "guest-trace.toml"
        with trace.open("rb") as stream:
            data = stream.read(64 * 1024 * 1024 + 1)
        if len(data) > 64 * 1024 * 1024 or hashlib.sha256(data).hexdigest() != record["trace_sha256"]:
            raise GeometryError("trace bytes do not match declaration or exceed budget")
        for capture in record["captures"]:
            verify_bmp_capture(capture, directory / f'{capture["scope"]}-{capture["checkpoint"]}.bmp')
    if baseline["profile"] != candidate["profile"]:
        raise GeometryError("profile drift: compare equal scale/aspect/layout/settings only")
    if baseline["internal_screens"] != candidate["internal_screens"]:
        raise GeometryError("internal screen observation drift")
    if baseline["trace_sha256"] != candidate["trace_sha256"]:
        raise GeometryError("guest trace bytes differ")
    ordered = lambda record: sorted(record["captures"], key=lambda c: (c["checkpoint"], c["scope"]))
    if ordered(baseline) != ordered(candidate):
        raise GeometryError("capture coverage, geometry or exact bytes differ")
    return {"byte_equal": True, "coverage": left, "candidate_coverage": right, "provenance_verified": False, "trace_checkpoint_link_verified": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("candidate", type=Path, nargs="?")
    parser.add_argument("--baseline-dir", type=Path)
    parser.add_argument("--candidate-dir", type=Path)
    parser.add_argument("--require-presentation", action="store_true")
    parser.add_argument("--require-internal", action="store_true")
    args = parser.parse_args()
    requirements = {"require_presentation": args.require_presentation, "require_internal": args.require_internal}
    try:
        if args.candidate and (args.baseline_dir is None or args.candidate_dir is None):
            raise GeometryError("comparison requires both capture directories")
        baseline = json.loads(args.baseline.read_text())
        result = (compare_records(baseline, json.loads(args.candidate.read_text()), baseline_dir=args.baseline_dir, candidate_dir=args.candidate_dir, **requirements)
                  if args.candidate else validate_record(baseline, **requirements))
    except (GeometryError, OSError, ValueError) as error:
        parser.exit(1, f"geometry validation failed: {error}\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
