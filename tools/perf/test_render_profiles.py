"""Render profiles generate only real settings and admit only observed geometry."""

import copy
import json
import math
from pathlib import Path
import tempfile
import tomllib
import unittest

import render_profiles as rules

ADVANCED = ("[advanced]\nbackground_shaders = true\ngeneric_lighting = false\ngeneric_shaders = false\n"
            "gpu_thread = false\nseparable_shaders = false\nstand_ins = false\n")


def declaration(scale=1.0, layout="BigTop", aspect="Original", capture=None, **changes):
    value = {"schema_version": 1, "id": "cell", "internal_scale": scale, "aspect": aspect, "layout": layout,
             "swapped": False, "stretched": "Off", "presentation_capture": capture}
    value.update(changes)
    return value


def capture(retain="last", panel=(640, 480), budget=None):
    file = "panel.ppm" if retain == "last" else "panel-%.ppm"
    return {"panel_size": list(panel), "file": file, "retain": retain,
            "max_bytes": budget if budget is not None else rules.ppm_bytes(panel)}


def presentation(profile, sequence=1, index=None):
    panel = profile["presentation_capture"]["panel_size"]
    numbered = profile["presentation_capture"]["retain"] == "numbered"
    row = {"sequence": sequence, "output": "offscreen", "panel": list(panel), "turned": False,
           "target": list(panel), "layout": rules.layout_size(panel), "layout_set": True, "fit": [1.0, 0.0, 0.0],
           "top_crop": 0.10000000149011612 if profile["aspect"] == "Crop" else 0.0, "second_window": False,
           "panel_shot_status": "written", "panel_shot_retained": numbered, "panel_shot_numbered": numbered}
    if numbered:
        row["panel_shot_index"] = index if index is not None else sequence - 1
    shown = rules.screens_shown(profile)
    for name, visible in zip(("top", "bottom"), shown):
        if visible:
            row[name] = {"area": [0, 0, 400, 240], "viewport": [0, 0, 400, 240]}
    return row


def geometry(profile, quarters=None, presentations=(), **requested):
    quarters = quarters or round(profile["internal_scale"] * 4)
    surface = {"console": [240, 400], "drawn": [240 * quarters // 4, 400 * quarters // 4],
               "rows": [0, 400], "columns": 240}
    shots = [{"frame": 100 * i, "file": f"frame-{100 * i:05}.bmp",
              "top": {"readback": [400, 240], "gpu_surface": surface}, "bottom": {"readback": [320, 240]}}
             for i in range(2)]
    for shot, row in zip(shots, presentations):
        shot["presentation"] = row
    record = {"schema_version": 1, "unobserved": ["second window viewports"],
              "renderer": {"kind": "gles", "name": "synthetic", "effective_quarters": quarters, "readback_scale": 1},
              "requested": {"resolution": profile["internal_scale"], "layout": profile["layout"],
                            "aspect": profile["aspect"], "swapped": profile["swapped"],
                            "stretched": profile["stretched"], "stacked": False, "second_window": False, **requested},
              "shots": shots}
    if presentations:
        record["final_presentation"] = presentations[-1]
    return record


def write_ppm(path, panel):
    width, height = panel
    path.write_bytes(f"P6\n{width} {height}\n255\n".encode() + bytes(width * height * 3))


class CappedCaptureContractTests(unittest.TestCase):
    def profile(self, count=2):
        size = rules.ppm_bytes([64, 64])
        return rules.validate(declaration(capture={**capture("numbered", panel=(64, 64), budget=count * size),
                                                  "max_io_bytes": count * size}))

    def record(self, profile, count=2):
        size = rules.ppm_bytes([64, 64])
        rows = [presentation(profile, i+1, i) for i in range(count)]
        for i, row in enumerate(rows):
            row.update(panel_budget_status="ready", panel_budget_limit_bytes=str(profile["presentation_capture"]["max_io_bytes"]),
                       panel_budget_charged_bytes=str((i+1)*size))
        record = geometry(profile, presentations=rows)
        record["schema_version"] = 2
        return record

    def files(self, directory, count=2):
        root = Path(directory)
        (root / "stderr.log").write_text("INFO synthetic contract; no runtime validation\n")
        for index in range(count):
            write_ppm(root / f"panel-{index:05}.ppm", [64, 64])

    def test_optional_cap_preserves_historical_declarations_and_defaults(self):
        old = rules.validate(declaration(capture=capture("numbered", panel=(64, 64))))
        old_hash = rules.declaration_hash(old)
        self.assertNotIn("max_io_bytes", old["presentation_capture"])
        self.assertNotIn("ZAKURO_PANEL_SHOT_MAX_IO_BYTES", rules.environment(old, Path("/trial")))
        self.assertEqual(rules.declaration_hash(rules.validate(old)), old_hash)
        capped = self.profile()
        self.assertEqual(rules.environment(capped, Path("/trial"))["ZAKURO_PANEL_SHOT_MAX_IO_BYTES"],
                         str(capped["presentation_capture"]["max_io_bytes"]))
        self.assertEqual(rules.environment(rules.validate(declaration()), Path("/trial")), {})
        for value in (None, True, 0, -1, 1.0, "12301", 2**64):
            changed = copy.deepcopy(capped)
            changed["presentation_capture"]["max_io_bytes"] = value
            with self.subTest(value=value), self.assertRaises(ValueError):
                rules.validate(changed)
        rules.validate({**capped, "presentation_capture": {**capped["presentation_capture"], "max_io_bytes": 2**64-1}})

    def test_exact_limit_success_is_only_observed_attempt_consistency(self):
        profile = self.profile()
        with tempfile.TemporaryDirectory() as directory:
            self.files(directory)
            record = self.record(profile)
            before = copy.deepcopy(record)
            result = rules.observe(profile, geometry=record, trial=directory)
            self.assertTrue(result["checks"]["capture_budget"], result["errors"])
            self.assertIsNone(result["checks"]["cap_trial_coverage"])
            self.assertFalse(result["admitted"])
            budget = result["observed"]["capture_budget"]
            self.assertEqual(budget["charged_bytes"], budget["limit_bytes"])
            self.assertEqual(budget["observed_attempts"], 2)
            self.assertEqual(budget["thread_lifetime_coverage"], "unknown_not_observed")
            self.assertEqual(result["observed"]["presentation_frame_binding"], "unknown_observer_may_precede_probe_frame")
            self.assertFalse(result["full_matrix_cap_admitted"])
            self.assertFalse(result["process_quota_admitted"])
            self.assertFalse(result["timing_eligible"])
            self.assertEqual(record, before)

    def test_old_runtime_ignoring_environment_is_held(self):
        profile = self.profile()
        with tempfile.TemporaryDirectory() as directory:
            self.files(directory)
            record = geometry(profile, presentations=[presentation(profile, 1, 0), presentation(profile, 2, 1)])
            result = rules.observe(profile, geometry=record, trial=directory)
            self.assertFalse(result["admitted"])
            self.assertFalse(result["checks"]["capture_budget"])

    def test_missing_invalid_failed_and_exhausted_budget_never_admit(self):
        profile = self.profile()
        with tempfile.TemporaryDirectory() as directory:
            self.files(directory)
            for status in (None, "absent", "invalid", "unknown", "exhausted", "write_failed", "size_failed"):
                record = self.record(profile)
                for row in [shot["presentation"] for shot in record["shots"]] + [record["final_presentation"]]:
                    row["panel_budget_status"] = status
                with self.subTest(status=status):
                    result = rules.observe(profile, geometry=record, trial=directory)
                    self.assertFalse(result["admitted"])

    def test_failed_index_with_existing_valid_stale_file_is_held(self):
        profile = self.profile()
        with tempfile.TemporaryDirectory() as directory:
            self.files(directory)
            record = self.record(profile)
            record["final_presentation"].update(panel_shot_status="write_failed", panel_shot_retained=False,
                                               panel_budget_status="write_failed")
            result = rules.observe(profile, geometry=record, trial=directory)
            self.assertTrue(result["checks"]["panel_images"])
            self.assertFalse(result["admitted"])
            self.assertFalse(result["checks"]["presentation"])

    def test_capture_error_log_is_bound_and_holds_success_shaped_snapshots(self):
        profile = self.profile()
        with tempfile.TemporaryDirectory() as directory:
            self.files(directory)
            log = Path(directory, "stderr.log")
            log.write_text("ERROR panel capture failed: panel capture I/O budget exhausted\n")
            result = rules.observe(profile, geometry=self.record(profile), trial=directory)
            self.assertFalse(result["admitted"])
            self.assertFalse(result["checks"]["capture_log"])
            self.assertEqual(result["observed"]["capture_log"]["source"]["bytes"], log.stat().st_size)
            log.unlink()
            result = rules.observe(profile, geometry=self.record(profile), trial=directory)
            self.assertFalse(result["admitted"])
            self.assertIn("capped capture stderr log missing", result["errors"])

    def test_sparse_snapshots_and_counter_inventory_contradictions_are_held(self):
        profile = self.profile()
        with tempfile.TemporaryDirectory() as directory:
            self.files(directory)
            for change in ("sparse", "charge", "limit", "decimal", "overflow", "index", "partial"):
                record = self.record(profile)
                row = record["shots"][0]["presentation"]
                if change == "sparse":
                    del record["shots"][0]["presentation"]
                elif change == "charge": row["panel_budget_charged_bytes"] = "1"
                elif change == "limit": row["panel_budget_limit_bytes"] = str(10**6)
                elif change == "decimal": row["panel_budget_charged_bytes"] = "012301"
                elif change == "overflow": row["panel_budget_limit_bytes"] = str(2**64)
                elif change == "index": row["panel_shot_index"] = 5
                elif change == "partial": Path(directory, "panel-00000.ppm").write_bytes(b"P6\n64 64\n255\n")
                with self.subTest(change=change):
                    self.assertFalse(rules.observe(profile, geometry=record, trial=directory)["admitted"])


class ValidationTest(unittest.TestCase):
    def test_default_presentation_changes_nothing_but_the_requested_scale(self):
        for scale in (1, 1.5, 2.0):
            profile = rules.validate(declaration(scale))
            self.assertIs(type(profile["internal_scale"]), float)
            self.assertIsNone(rules.config_settings(profile))
            self.assertEqual(rules.environment(profile, Path("/abs/trial")), {})
            self.assertTrue(rules.timing_eligible(profile))

    def test_validation_does_not_mutate_or_alias_the_declaration(self):
        value = declaration(1.0, "SideBySide", capture=capture())
        before = copy.deepcopy(value)
        profile = rules.validate(value)
        profile["presentation_capture"]["panel_size"][0] = 1
        self.assertEqual(value, before)

    def test_unknown_or_missing_fields_are_rejected(self):
        for value in ({**declaration(), "smooth": True}, {**declaration(), "frame_skip": 0},
                      {key: v for key, v in declaration().items() if key != "presentation_capture"},
                      declaration(capture={**capture(), "path": "x"}), declaration(capture={"panel_size": [640, 480]}),
                      declaration(schema_version=2), declaration(schema_version=True), declaration(id="../up"),
                      declaration(id="Upper")):
            with self.subTest(value=value), self.assertRaises(ValueError):
                rules.validate(value)

    def test_non_finite_or_non_effective_scales_are_rejected(self):
        for scale in (True, math.nan, math.inf, -math.inf, 0.75, 1.1, 1.3, 4.25, 0, "1.5", None, [1.5]):
            with self.subTest(scale=scale), self.assertRaises(ValueError):
                rules.validate(declaration(scale))
        for scale in (1, 1.25, 3.5, 4):
            self.assertEqual(rules.validate(declaration(scale))["internal_scale"], float(scale))

    def test_only_the_frontend_enum_spellings_are_accepted(self):
        for layout in rules.LAYOUTS:
            rules.validate(declaration(layout=layout, capture=capture()))
        for aspect in rules.ASPECTS:
            rules.validate(declaration(aspect=aspect, capture=capture()))
        for field, bad in (("layout", "bigtop"), ("layout", "Big top"), ("layout", "Stacked"), ("aspect", "4:3"),
                           ("aspect", "FourByThree"), ("aspect", ["Original"]), ("layout", 1), ("swapped", 0),
                           ("stretched", "None")):
            with self.subTest(field=field, bad=bad), self.assertRaises(ValueError):
                rules.validate(declaration(capture=capture(), **{field: bad}))

    def test_ineffective_presentation_requests_are_rejected(self):
        for changes in ({"stretched": "Both"}, {"stretched": "Top"}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                rules.validate(declaration(capture=capture(), **changes))
        for changes in ({"layout": "SideBySide"}, {"aspect": "FourThree"}, {"swapped": True}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                rules.validate(declaration(**changes))

    def test_unsafe_or_inconsistent_capture_paths_are_rejected(self):
        for retain, file in (("last", "../panel.ppm"), ("last", "/tmp/panel.ppm"), ("last", "shots/panel.ppm"),
                             ("last", "panel.PPM"), ("last", "panel%.ppm"), ("last", ".panel.ppm"),
                             ("last", "panel.ppm\n"), ("numbered", "panel.ppm"), ("numbered", "panel-%%.ppm"),
                             ("numbered", "%panel.ppm"), ("numbered", "panel-%.ppm/x"), ("last", 7)):
            value = capture(retain)
            value["file"] = file
            with self.subTest(retain=retain, file=file), self.assertRaises(ValueError):
                rules.validate(declaration(capture=value))
        for retain in (["last"], None, "all"):
            with self.subTest(retain=retain), self.assertRaises(ValueError):
                rules.validate(declaration(capture={**capture(), "retain": retain}))

    def test_panel_size_and_byte_budget_are_bounded(self):
        for panel in ([63, 480], [640, 8193], [640.0, 480], [640], [True, 480]):
            with self.subTest(panel=panel), self.assertRaises(ValueError):
                rules.validate(declaration(capture={**capture(), "panel_size": panel}))
        for budget in (rules.ppm_bytes([640, 480]) - 1, rules.MAX_CAPTURE_BYTES + 1, 1.5e9, True):
            with self.subTest(budget=budget), self.assertRaises(ValueError):
                rules.validate(declaration(capture={**capture(), "max_bytes": budget}))

    def test_load_rejects_duplicate_fields_and_symlinks(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory, "profile.json")
            path.write_text('{"schema_version": 1, "schema_version": 1}')
            with self.assertRaisesRegex(ValueError, "duplicate"):
                rules.load(path)
            path.write_text(json.dumps(declaration(1.5)))
            profile, source = rules.load(path)
            self.assertEqual(profile["internal_scale"], 1.5)
            self.assertEqual(source["bytes"], path.stat().st_size)
            link = Path(directory, "link.json")
            link.symlink_to(path)
            with self.assertRaises(ValueError):
                rules.load(link)


class GenerationTest(unittest.TestCase):
    def test_settings_use_the_frontend_root_keys_and_variant_names(self):
        profile = rules.validate(declaration(1.5, "SideBySide", "FourThree", capture()))
        text = rules.config_settings(profile)
        self.assertEqual(text, 'aspect = "FourThree"\nlayout = "SideBySide"\nresolution = 1.5\n'
                               'stretched = "Off"\nswapped = false\n')
        self.assertEqual(tomllib.loads(text), {"aspect": "FourThree", "layout": "SideBySide", "resolution": 1.5,
                                               "stretched": "Off", "swapped": False})
        self.assertNotIn("[advanced]", text)

    def test_capture_environment_stays_inside_an_absolute_trial(self):
        profile = rules.validate(declaration(layout="TopOnly", capture=capture("numbered", (1920, 1080), 10**9)))
        env = rules.environment(profile, Path("/runs/trial-01"))
        self.assertEqual(env, {"ZAKURO_PANEL_SHOT": "/runs/trial-01/panel-%.ppm", "ZAKURO_PANEL_SIZE": "1920x1080"})
        self.assertEqual(set(env), rules.ENV_KEYS)
        self.assertFalse(rules.timing_eligible(profile))
        for trial in ("relative/trial", "/runs/../etc"):
            with self.subTest(trial=trial), self.assertRaises(ValueError):
                rules.environment(profile, Path(trial))

    def test_layout_size_matches_display_layout_for(self):
        for panel, expected in (([640, 480], [640, 480]), ([1920, 1080], [853, 480]), ([640, 640], [640, 640]),
                                ([480, 640], [640, 853]), ([800, 480], [800, 480])):
            self.assertEqual(rules.layout_size(panel), expected)

    def test_frame_geometry_profile_only_for_modeled_presentations(self):
        self.assertIsNone(rules.frame_geometry_profile(rules.validate(declaration())))
        self.assertIsNone(rules.frame_geometry_profile(rules.validate(declaration(capture=capture()))))
        self.assertIsNone(rules.frame_geometry_profile(rules.validate(declaration(aspect="Crop", capture=capture()))))
        modeled = rules.frame_geometry_profile(rules.validate(declaration(2, "SideBySide", "FourThree", capture(panel=(1920, 1080)))))
        self.assertEqual(modeled, {"resolution": 2.0, "aspect": "FourThree", "layout": "SideBySide",
                                   "swapped": False, "layout_size": [853, 480]})


class RunBindingTest(unittest.TestCase):
    def test_resolution_argument_must_equal_the_profile(self):
        profile = rules.validate(declaration(1.5))
        rules.check_run(profile, resolution=1.5, frames=6000)
        for resolution in (1.0, 2.0, "1.5", None):
            with self.subTest(resolution=resolution), self.assertRaises(ValueError):
                rules.check_run(profile, resolution=resolution, frames=6000)

    def test_seeded_configuration_settings_are_rejected_even_for_the_default(self):
        for profile in (rules.validate(declaration()), rules.validate(declaration(layout="TopOnly", capture=capture()))):
            with self.subTest(profile=profile["layout"]), self.assertRaises(ValueError):
                rules.check_run(profile, resolution=1.0, frames=6000, seeded_config=["pocket-native/settings.toml"],
                                diagnostic={"geometry": True})
        rules.check_run(rules.validate(declaration()), resolution=1.0, frames=6000, seeded_config=["touch.toml"])

    def test_implementation_profiles_cannot_share_render_settings_or_environment(self):
        profile = rules.validate(declaration(layout="TopOnly", capture=capture()))
        rules.check_run(profile, resolution=1.0, frames=6000, diagnostic={"geometry": True},
                        implementation_settings=ADVANCED, implementation_environment={"ZAKURO_TEXTURE_CACHE_MIB": "64"})
        for settings in ("layout = \"TopOnly\"\n", ADVANCED + "layout = \"TopOnly\"\n", ADVANCED + "[screen]\n"):
            with self.subTest(settings=settings), self.assertRaises(ValueError):
                rules.check_run(profile, resolution=1.0, frames=6000, diagnostic={"geometry": True},
                                implementation_settings=settings)
        with self.assertRaises(ValueError):
            rules.check_run(profile, resolution=1.0, frames=6000, diagnostic={"geometry": True},
                            implementation_environment={"ZAKURO_PANEL_SIZE": "640x480"})

    def test_capture_requires_the_geometry_observer_and_a_frame_budget(self):
        last = rules.validate(declaration(layout="TopOnly", capture=capture()))
        for diagnostic in (None, {"geometry": False}, {"geometry": 1}):
            with self.subTest(diagnostic=diagnostic), self.assertRaises(ValueError):
                rules.check_run(last, resolution=1.0, frames=6000, diagnostic=diagnostic)
        rules.check_run(last, resolution=1.0, frames=6000, diagnostic={"geometry": True})
        image = rules.ppm_bytes([640, 480])
        numbered = rules.validate(declaration(layout="TopOnly", capture=capture("numbered", budget=100 * image)))
        rules.check_run(numbered, resolution=1.0, frames=100, diagnostic={"geometry": True})
        with self.assertRaises(ValueError):
            rules.check_run(numbered, resolution=1.0, frames=101, diagnostic={"geometry": True})


class ObservationTest(unittest.TestCase):
    def test_renderer_log_scales_follow_rust_float_display(self):
        lines = ["[2026-11-06T00:00:00Z INFO  zakuro_core::loader] drawing on NVIDIA GeForce RTX at 1.5x through OpenGL ES",
                 "INFO drawing on Mali-G31 at 2x, vertices shaded on the CPU, through OpenGL ES  ",
                 "INFO drawing with OpenGL ES on NVIDIA at 1.5x", None]
        self.assertEqual(rules.renderer_scales(lines), [1.5, 2.0])
        self.assertEqual(rules.renderer_scales(["drawing on X at 1x through OpenGL ES"]), [1.0])

    def test_scale_cell_is_admitted_only_with_matching_observed_quarters_and_surfaces(self):
        profile = rules.validate(declaration(1.5))
        result = rules.observe(profile, geometry=geometry(profile))
        self.assertTrue(result["admitted"], result["errors"])
        self.assertEqual(result["checks"], {"runtime_requested": True, "renderer_scale_log": None,
                                            "renderer_quarters": True, "surface_scale": True, "presentation": True})
        self.assertEqual(result["observed"]["surfaces"]["console_drawn"], [[[240, 400], [360, 600]]])
        self.assertEqual(result["scanout"], "unknown_not_observed")
        self.assertIn(rules.SCANOUT_UNOBSERVED[0], result["unobserved"])
        self.assertIn("second window viewports", result["unobserved"])
        self.assertFalse(result["timing_eligible"])
        self.assertIsNone(result["requested"]["config_settings"])
        clamped = rules.observe(profile, geometry=geometry(profile, quarters=4))
        self.assertFalse(clamped["admitted"])
        self.assertFalse(clamped["checks"]["renderer_quarters"])

    def test_surface_drawn_at_another_scale_fails(self):
        profile = rules.validate(declaration(2.0))
        record = geometry(profile)
        record["shots"][0]["top"]["gpu_surface"] = {"console": [240, 400], "drawn": [240, 400], "rows": [0, 400], "columns": 240}
        result = rules.observe(profile, geometry=record)
        self.assertFalse(result["checks"]["surface_scale"])
        self.assertFalse(result["admitted"])

    def test_without_the_observer_only_the_logged_scale_can_prove_the_request(self):
        profile = rules.validate(declaration(2.0))
        proven = rules.observe(profile, renderer_log_lines=["INFO drawing on GPU at 2x through OpenGL ES"])
        self.assertTrue(proven["admitted"])
        self.assertTrue(proven["timing_eligible"])
        self.assertIsNone(proven["checks"]["presentation"])
        self.assertFalse(rules.observe(profile)["admitted"])
        wrong = rules.observe(profile, renderer_log_lines=["INFO drawing on GPU at 1x through OpenGL ES"])
        self.assertFalse(wrong["admitted"])

    def test_runtime_must_report_parsing_a_written_settings_file(self):
        profile = rules.validate(declaration(layout="SideBySide", aspect="FourThree", capture=capture()))
        with tempfile.TemporaryDirectory() as directory:
            write_ppm(Path(directory, "panel.ppm"), [640, 480])
            rows = [presentation(profile, 1), presentation(profile, 2)]
            fallback = rules.observe(profile, geometry=geometry(profile, presentations=rows, layout="BigTop",
                                                                aspect="Original"), trial=directory)
            self.assertFalse(fallback["checks"]["runtime_requested"])
            self.assertFalse(fallback["admitted"])
            record = geometry(profile, presentations=rows)
            del record["requested"]
            self.assertFalse(rules.observe(profile, geometry=record, trial=directory)["admitted"])
            admitted = rules.observe(profile, geometry=geometry(profile, presentations=rows), trial=directory)
            self.assertTrue(admitted["admitted"], admitted["errors"])
            self.assertEqual(admitted["evidence_class"], rules.NON_TIMING)
            self.assertFalse(admitted["timing_eligible"])

    def test_presentation_must_match_the_declared_offscreen_panel(self):
        profile = rules.validate(declaration(layout="TopOnly", capture=capture()))
        with tempfile.TemporaryDirectory() as directory:
            write_ppm(Path(directory, "panel.ppm"), [640, 480])
            good = presentation(profile)
            self.assertTrue(rules.observe(profile, geometry=geometry(profile, presentations=[good]), trial=directory)["admitted"])
            for change in ({"output": "kms"}, {"panel": [800, 480]}, {"layout": [853, 480]}, {"layout_set": False},
                           {"turned": True}, {"second_window": True}, {"panel_shot_status": "write_failed"},
                           {"top_crop": 0.1}, {"bottom": {"area": [0, 0, 1, 1]}}):
                row = {**good, **change}
                with self.subTest(change=change):
                    result = rules.observe(profile, geometry=geometry(profile, presentations=[row]), trial=directory)
                    self.assertFalse(result["checks"]["presentation"])
                    self.assertFalse(result["admitted"])
            missing = rules.observe(profile, geometry=geometry(profile), trial=directory)
            self.assertFalse(missing["checks"]["presentation"])

    def test_crop_accepts_widened_or_shortest_f32_and_swapped_top_only_shows_the_bottom(self):
        crop = rules.validate(declaration(aspect="Crop", capture=capture()))
        swapped = rules.validate(declaration(layout="TopOnly", swapped=True, capture=capture()))
        self.assertEqual(rules.screens_shown(swapped), [False, True])
        with tempfile.TemporaryDirectory() as directory:
            write_ppm(Path(directory, "panel.ppm"), [640, 480])
            for value in (0.1, 0.10000000149011612):
                row = {**presentation(crop), "top_crop": value}
                self.assertTrue(rules.observe(crop, geometry=geometry(crop, presentations=[row]), trial=directory)["admitted"])
            self.assertTrue(rules.observe(swapped, geometry=geometry(swapped, presentations=[presentation(swapped)]),
                                          trial=directory)["admitted"])

    def test_panel_images_are_bound_by_size_bytes_and_index(self):
        profile = rules.validate(declaration(layout="SideBySide", capture=capture("numbered", budget=10**8)))
        rows = [presentation(profile, 1, 0), presentation(profile, 2, 1)]
        with tempfile.TemporaryDirectory() as directory:
            write_ppm(Path(directory, "panel-00000.ppm"), [640, 480])
            missing = rules.observe(profile, geometry=geometry(profile, presentations=rows), trial=directory)
            self.assertFalse(missing["checks"]["panel_images"])
            write_ppm(Path(directory, "panel-00001.ppm"), [320, 240])
            wrong = rules.observe(profile, geometry=geometry(profile, presentations=rows), trial=directory)
            self.assertFalse(wrong["admitted"])
            write_ppm(Path(directory, "panel-00001.ppm"), [640, 480])
            bound = rules.observe(profile, geometry=geometry(profile, presentations=rows), trial=directory)
            self.assertTrue(bound["admitted"], bound["errors"])
            self.assertEqual([image["index"] for image in bound["observed"]["panel_images"]], [0, 1])
            Path(directory, "panel-00001.ppm").write_bytes(Path(directory, "panel-00001.ppm").read_bytes() + b"\0")
            self.assertFalse(rules.observe(profile, geometry=geometry(profile, presentations=rows), trial=directory)["admitted"])

    def test_numbered_budget_counts_files_missing_from_sparse_geometry(self):
        budget = rules.ppm_bytes([64, 64])
        profile = rules.validate(declaration(capture=capture("numbered", panel=(64, 64), budget=budget)))
        with tempfile.TemporaryDirectory() as directory:
            write_ppm(Path(directory, "panel-00000.ppm"), [64, 64])
            rows = [presentation(profile, 1, 0)]
            self.assertTrue(rules.observe(profile, geometry=geometry(profile, presentations=rows), trial=directory)["admitted"])
            write_ppm(Path(directory, "panel-00001.ppm"), [64, 64])
            result = rules.observe(profile, geometry=geometry(profile, presentations=rows), trial=directory)
            self.assertFalse(result["admitted"])
            self.assertTrue(any("max_bytes" in error for error in result["errors"]))

    def test_numbered_indices_cannot_escape_or_alias_a_capture(self):
        profile = rules.validate(declaration(capture=capture("numbered", panel=(64, 64), budget=10**6)))
        with tempfile.TemporaryDirectory() as directory:
            write_ppm(Path(directory, "panel-00000.ppm"), [64, 64])
            for index in (None, True, -1, 2**32, "../../outside"):
                row = presentation(profile)
                row["panel_shot_index"] = index
                with self.subTest(index=index):
                    self.assertFalse(rules.observe(profile, geometry=geometry(profile, presentations=[row]), trial=directory)["admitted"])
            target = Path(directory, "target.ppm")
            write_ppm(target, [64, 64])
            Path(directory, "panel-00001.ppm").symlink_to(target)
            self.assertFalse(rules.observe(profile, geometry=geometry(profile, presentations=[presentation(profile, index=0)]),
                                          trial=directory)["admitted"])

    def test_an_unrequested_panel_is_reported_not_ignored(self):
        profile = rules.validate(declaration())
        stray = rules.validate(declaration(capture=capture()))
        result = rules.observe(profile, geometry=geometry(profile, presentations=[presentation(stray)]))
        self.assertFalse(result["checks"]["presentation"])
        self.assertFalse(result["admitted"])


class PlanTest(unittest.TestCase):
    def test_screening_plan_is_small_independent_and_non_timing(self):
        plan = rules.screening_plan()
        cells = plan["cells"]
        self.assertFalse(plan["timing_admitted"])
        self.assertEqual(len(cells), 24)
        self.assertEqual(len({cell["id"] for cell in cells}), 24)
        self.assertEqual({cell["title"] for cell in cells}, set(rules.TITLES))
        for cell in cells:
            profile = rules.validate(cell["profile"])
            self.assertEqual(cell["profile_sha256"], rules.declaration_hash(profile))
            self.assertEqual(cell["resolution"], profile["internal_scale"])
            self.assertEqual(cell["diagnostic_profile"], {"schema_version": 1, "geometry": True})
            self.assertEqual(cell["status"], "pending-observation")
            rules.check_run(profile, resolution=cell["resolution"], frames=6000, diagnostic=cell["diagnostic_profile"])
            if cell["axis"] == "scale":
                self.assertTrue(rules.presentation_is_default(profile))
                self.assertIsNone(profile["presentation_capture"])
            else:
                self.assertEqual(profile["internal_scale"], 1.0)
                self.assertEqual(profile["presentation_capture"]["retain"], "last")
                self.assertEqual(cell["panel_write_bytes_per_presentation"], rules.ppm_bytes([640, 480]))
        for title in rules.TITLES:
            own = [cell["profile"] for cell in cells if cell["title"] == title]
            self.assertEqual(sorted(p["internal_scale"] for p in own if p["presentation_capture"] is None), [1.0, 1.5, 2.0])
            varied = {(p["layout"], p["aspect"]) for p in own if p["presentation_capture"]}
            self.assertEqual(varied, set(rules.SCREEN_PRESENTATIONS))
            # One axis at a time from the BigTop/Original control.
            for layout, aspect in varied:
                self.assertTrue(layout == "BigTop" or aspect == "Original")


if __name__ == "__main__":
    unittest.main()
