"""Checks the exact receipt/guest-state replay seam used for drift diagnosis."""

import copy
import importlib.util
from pathlib import Path
import unittest

SPEC = importlib.util.spec_from_file_location("check_determinism", Path(__file__).with_name("check_determinism.py"))
determinism = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(determinism)


class DeterminismTests(unittest.TestCase):
    def trace(self):
        return {"schema_version": 1, "sampling_interval_frames": 10, "dropped_checkpoints": 0,
                "checkpoints": [{"completed_frames": frame, "guest_cycles": frame * 100,
                                 "registers": [frame] * 16, "recompiled_instructions": frame * 50}
                                for frame in (10, 20, 30)]}

    def test_first_guest_divergence_has_an_exact_preceding_boundary(self):
        original, candidate = self.trace(), self.trace()
        candidate["checkpoints"][1]["registers"][15] += 4
        result = determinism.check_traces([original, candidate])
        self.assertFalse(result["deterministic"])
        self.assertEqual(result["comparisons"][0]["last_matching_frame"], 10)
        self.assertEqual(result["comparisons"][0]["first_difference"]["reference_frame"], 20)
        self.assertEqual(set(result["comparisons"][0]["first_difference"]["fields"]), {"registers"})

    def test_identical_traces_pass_but_missing_or_dropped_records_do_not(self):
        self.assertTrue(determinism.check_traces([self.trace(), self.trace()])["deterministic"])
        for field, value in (("checkpoints", []), ("dropped_checkpoints", 1), ("schema_version", 2)):
            other = self.trace()
            other[field] = value
            self.assertFalse(determinism.check_traces([self.trace(), other])["deterministic"])

    def test_one_extra_picture_is_detected_even_if_sampled_pixels_match(self):
        trial = {"valid": True, "probe": {"report": {"top_pictures": 20}, "screenshots": {"frame.bmp": "same"}}}
        manifest = {"trials": [trial, copy.deepcopy(trial)]}
        self.assertTrue(determinism.check(manifest)["deterministic"])
        manifest["trials"][1]["probe"]["report"]["top_pictures"] = 21
        result = determinism.check(manifest)
        self.assertFalse(result["deterministic"])
        self.assertEqual(result["comparisons"][0]["differences"]["top_pictures"], {"reference": 20, "actual": 21})


if __name__ == "__main__":
    unittest.main()
