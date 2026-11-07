"""Authored geometry metadata fixtures only; no game assets or runtime launches."""

import copy
import hashlib
import importlib.util
from pathlib import Path
import struct
import tempfile
import unittest


SPEC = importlib.util.spec_from_file_location("frame_geometry", Path(__file__).with_name("frame_geometry.py"))
geometry = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(geometry)


def record(scale=1.0, aspect="Original", layout="Original", swapped=False, presentation=False, internal=False):
    profile = {"resolution": scale, "aspect": aspect, "layout": layout,
               "swapped": swapped, "layout_size": [640, 480]}
    expected = geometry.expected_geometry(profile)
    captures = [{"checkpoint": 300, "scope": "source-native-stacked",
                 **copy.deepcopy(expected["source_capture"]),
                 "sha256": hashlib.sha256(b"authored synthetic top green bottom blue").hexdigest()}]
    if presentation:
        captures.append({"checkpoint": 300, "scope": "presentation-layout",
                         **copy.deepcopy(expected["presentation_capture"]),
                         "sha256": hashlib.sha256(b"authored synthetic presentation").hexdigest()})
    return {"schema_version": 1, "profile": profile,
            "internal_screens": expected["nominal_internal_screens"] if internal else None,
            "captures": captures, "trace_sha256": hashlib.sha256(b"synthetic complete trace").hexdigest()}


class FrameGeometryTests(unittest.TestCase):
    def test_actual_authored_bmp_bytes_are_bound_without_resampling(self):
        width, height = 4, 2
        pixels = bytes([0, 0, 255] * width + [0, 255, 0] * width)
        data = struct.pack("<2sI4xIIiiHHI20x", b"BM", 54 + len(pixels), 54, 40,
                           width, height, 1, 24, 0) + pixels
        capture = {"size": [width, height], "sha256": hashlib.sha256(data).hexdigest()}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "synthetic.bmp"
            path.write_bytes(data)
            self.assertEqual(geometry.verify_bmp_capture(capture, path), capture)
            with self.assertRaisesRegex(geometry.GeometryError, "byte budget"):
                geometry.verify_bmp_capture(capture, path, max_bytes=54)
            capture["size"] = [height, width]
            with self.assertRaisesRegex(geometry.GeometryError, "metadata"):
                geometry.verify_bmp_capture(capture, path)
            capture["size"] = [width, height]
            capture["sha256"] = "0" * 64
            with self.assertRaisesRegex(geometry.GeometryError, "metadata"):
                geometry.verify_bmp_capture(capture, path)
            path.write_bytes(data[:-1])
            with self.assertRaises(geometry.GeometryError):
                geometry.verify_bmp_capture(capture, path)

    def test_fractional_scale_and_aspect_are_independent(self):
        for aspect in ("Original", "FourThree"):
            for scale in (1, 1.5, 2):
                with self.subTest(scale=scale, aspect=aspect):
                    fixture = record(scale, aspect, internal=True)
                    expected = geometry.expected_geometry(fixture["profile"])
                    self.assertEqual(expected["nominal_internal_screens"]["top"], [int(400 * scale), int(240 * scale)])
                    self.assertEqual(expected["nominal_internal_screens"]["bottom"], [int(320 * scale), int(240 * scale)])
                    self.assertEqual(fixture["captures"][0]["size"], [400, 480])
                    self.assertTrue(geometry.validate_record(fixture)["valid"])

    def test_source_capture_cannot_certify_presentation_or_internal_scale(self):
        fixture = record(1.5, "FourThree", "SideBySide")
        result = geometry.validate_record(fixture)
        self.assertTrue(result["source_capture_declared"])
        self.assertFalse(result["presentation_capture_declared"])
        self.assertFalse(result["internal_screen_dimensions_declared"])
        with self.assertRaisesRegex(geometry.GeometryError, "presentation observer missing"):
            geometry.validate_record(fixture, require_presentation=True)
        with self.assertRaisesRegex(geometry.GeometryError, "internal-surface observer missing"):
            geometry.validate_record(fixture, require_internal=True)

    def test_layout_reference_rectangles_and_screen_roles(self):
        cases = [
            ("Original", False, {"top": [120, 0, 400, 240], "bottom": [160, 240, 320, 240]}),
            ("Original", True, {"top": [120, 240, 400, 240], "bottom": [160, 0, 320, 240]}),
            ("TopOnly", False, {"top": [0, 48, 640, 384], "bottom": None}),
            ("TopOnly", True, {"top": None, "bottom": [0, 0, 640, 480]}),
            ("SideBySide", False, {"top": [0, 133, 356, 213], "bottom": [356, 133, 284, 213]}),
            ("SideBySide", True, {"top": [284, 133, 356, 213], "bottom": [0, 133, 284, 213]}),
        ]
        for layout, swapped, regions in cases:
            with self.subTest(layout=layout, swapped=swapped):
                expected = geometry.expected_geometry(record(layout=layout, swapped=swapped)["profile"])
                self.assertEqual(expected["presentation_capture"]["regions"], regions)

    def test_four_three_reshapes_top_only_without_changing_source_dimensions(self):
        fixture = record(aspect="FourThree", presentation=True)
        self.assertEqual(fixture["captures"][1]["regions"],
                         {"top": [160, 0, 320, 240], "bottom": [160, 240, 320, 240]})
        self.assertEqual(fixture["captures"][0]["regions"]["top"], [0, 0, 400, 240])

    def test_wrong_dimensions_never_resize_to_pass(self):
        fixture = record(1.5)
        fixture["captures"][0]["size"] = [600, 720]
        with self.assertRaises(geometry.GeometryError):
            geometry.validate_record(fixture)

    def test_swapped_screen_labels_and_layout_mismatch_fail(self):
        fixture = record(presentation=True)
        regions = fixture["captures"][1]["regions"]
        regions["top"], regions["bottom"] = regions["bottom"], regions["top"]
        with self.assertRaisesRegex(geometry.GeometryError, "named screen regions differ"):
            geometry.validate_record(fixture)
        fixture = record(presentation=True)
        fixture["profile"]["layout"] = "SideBySide"
        with self.assertRaises(geometry.GeometryError):
            geometry.validate_record(fixture)

    def test_comparison_reads_bytes_and_retains_unverified_provenance(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            left, right = root / "left", root / "right"
            left.mkdir(); right.mkdir()
            fixture = record()
            capture = fixture["captures"][0]
            width, height = capture["size"]
            row = (width * 3 + 3) // 4 * 4
            # Bottom-up BGR rows, black centered margins as in probe.rs.
            top = bytes([0, 255, 0]) * 400
            bottom = bytes(40 * 3) + bytes([255, 0, 0]) * 320 + bytes(40 * 3)
            pixels = bottom * 240 + top * 240
            bmp = struct.pack("<2sI4xIIiiHHI20x", b"BM", 54 + row * height, 54, 40,
                              width, height, 1, 24, 0) + pixels
            trace = b"synthetic trace bytes"
            capture["sha256"] = hashlib.sha256(bmp).hexdigest()
            fixture["trace_sha256"] = hashlib.sha256(trace).hexdigest()
            for directory in (left, right):
                (directory / "source-native-stacked-300.bmp").write_bytes(bmp)
                (directory / "guest-trace.toml").write_bytes(trace)
            result = geometry.compare_records(fixture, copy.deepcopy(fixture), baseline_dir=left, candidate_dir=right)
            self.assertTrue(result["byte_equal"])
            self.assertFalse(result["provenance_verified"])
            self.assertFalse(result["trace_checkpoint_link_verified"])
            path = right / "source-native-stacked-300.bmp"
            path.write_bytes(bmp[:-1] + bytes([bmp[-1] ^ 1]))
            with self.assertRaises(geometry.GeometryError):
                geometry.compare_records(fixture, fixture, baseline_dir=left, candidate_dir=right)
            path.write_bytes(bmp)
            (right / "guest-trace.toml").write_bytes(b"changed")
            with self.assertRaises(geometry.GeometryError):
                geometry.compare_records(fixture, fixture, baseline_dir=left, candidate_dir=right)
            (right / "guest-trace.toml").write_bytes(trace)
            path.unlink()
            with self.assertRaises(OSError):
                geometry.compare_records(fixture, fixture, baseline_dir=left, candidate_dir=right)

    def test_scaled_readback_geometry_is_independent_of_requested_scale(self):
        fixture = record(scale=1)
        fixture["captures"][0]["size"] = [800, 960]
        fixture["captures"][0]["regions"] = {"top": [0, 0, 800, 480], "bottom": [80, 480, 640, 480]}
        self.assertTrue(geometry.validate_record(fixture)["valid"])
        width, height = 800, 960
        # Producer's 2x shape: bottom-up, centered bottom, BGR rows.
        top = bytes([0, 255, 0]) * width
        bottom = bytes(80 * 3) + bytes([255, 0, 0]) * 640 + bytes(80 * 3)
        pixels = bottom * 480 + top * 480
        bmp = struct.pack("<2sI4xIIiiHHI20x", b"BM", 54 + len(pixels), 54, 40,
                          width, height, 1, 24, 0) + pixels
        fixture["captures"][0]["sha256"] = hashlib.sha256(bmp).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "frame.bmp"
            path.write_bytes(bmp)
            self.assertEqual(geometry.verify_bmp_capture(fixture["captures"][0], path)["size"], [800, 960])
        fixture["captures"][0]["regions"]["bottom"][0] = 40
        with self.assertRaises(geometry.GeometryError):
            geometry.validate_record(fixture)

    def test_missing_or_duplicate_capture_coverage_fails(self):
        fixture = record()
        fixture["captures"].append(copy.deepcopy(fixture["captures"][0]))
        with self.assertRaisesRegex(geometry.GeometryError, "duplicate"):
            geometry.validate_record(fixture)

    def test_observed_internal_dimensions_must_match_profile(self):
        fixture = record(1.5, internal=True)
        fixture["internal_screens"]["top"] = [400, 240]
        with self.assertRaisesRegex(geometry.GeometryError, "observed internal screen dimensions"):
            geometry.validate_record(fixture)

    def test_strict_schema_does_not_accept_unknown_modes_or_boolean_dimensions(self):
        for key, value in (("resolution", 1.4), ("resolution", True), ("resolution", float("nan")),
                           ("aspect", "Stretch"), ("layout", "BigTop"), ("swapped", 1),
                           ("layout_size", [True, 480]), ("unmodeled_touch_controls", True)):
            fixture = record()
            fixture["profile"][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(geometry.GeometryError):
                geometry.validate_record(fixture)
        fixture = record()
        fixture["captures"][0]["regions"]["top"][2] = 400.0
        with self.assertRaises(geometry.GeometryError):
            geometry.validate_record(fixture)



if __name__ == "__main__":
    unittest.main()
