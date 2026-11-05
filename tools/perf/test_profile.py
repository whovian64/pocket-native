"""Synthetic profiler parsing/ELF mapping checks; no game assets or compilation."""

import importlib.util
from pathlib import Path
import struct
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("summarize_profile", Path(__file__).with_name("summarize_profile.py"))
profile = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(profile)


def elf_fixture(path: Path) -> None:
    data = bytearray(0x180)
    data[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<HH", data, 54, 56, 1)
    struct.pack_into("<IIQQQQ", data, 64, 1, 5, 0x100, 0x400000, 0, 0x40)
    path.write_bytes(data)


class ProfileTests(unittest.TestCase):
    def test_thread_spaces_and_ambiguous_paths(self):
        self.assertEqual(profile.parse_sample("GPU worker 2 /tmp/title.so 1a ? 0\n"),
                         ("GPU worker 2", "/tmp/title.so", 0x1A))
        self.assertEqual(profile.parse_sample("/tmp/title.so 1a ? 0"),
                         ("(unnamed)", "/tmp/title.so", 0x1A))
        self.assertIsNone(profile.parse_sample("GPU /tmp/my game/title.so 1a ? 0"))
        self.assertIsNone(profile.parse_sample("GPU /tmp/title.so NOPE ? 0"))
        self.assertIsNone(profile.parse_sample("GPU /tmp/title.so 1a /caller with spaces.so 2a"))

    def test_file_offset_maps_to_virtual_address_and_invalid_elf_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "fixture.so"
            elf_fixture(path)
            segments = profile.load_segments(path)
            self.assertEqual(profile.to_address(segments, 0x110), 0x400010)
            self.assertEqual(profile.to_address(segments, 0x100), 0x400000)
            self.assertIsNone(profile.to_address(segments, 0x140))
            self.assertIsNone(profile.to_address(segments + [(0x100, 0x800000, 0x40)], 0x110))
            path.write_bytes(path.read_bytes()[:80])
            with self.assertRaises(ValueError):
                profile.load_segments(path)

    def test_symbols_and_unknown_denominators_are_honest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            elf = root / "fixture.so"
            elf_fixture(elf)
            samples = root / "samples.txt"
            samples.write_text(f"GPU worker {elf} 110 ? 0\nGPU worker {elf} 110 ? 0\nCPU ? 0 ? 0\nambiguous input\n")

            def symbolizer(command, **kwargs):
                if "--version" in command:
                    return subprocess.CompletedProcess(command, 0, "GNU addr2line (fixture) 1\n", "")
                # This also checks that file offset 0x110 is not passed as a VA.
                self.assertEqual(kwargs["input"], "0x400010\n")
                return subprocess.CompletedProcess(command, 0, "0x400010\nhot_function\nfixture.c:42\n", "")

            with patch.object(profile.subprocess, "run", side_effect=symbolizer):
                result = profile.summarize(samples, tool="fake-addr2line")
            self.assertEqual(result["samples_read"], 4)
            self.assertEqual(result["symbolized_samples"], 2)
            self.assertEqual(result["unknown_samples"], 2)
            self.assertEqual(result["top_functions"][0]["function"], "hot_function")
            self.assertEqual(result["top_functions"][0]["percent_of_denominator"], 50)
            self.assertEqual(result["top_threads"][0]["thread"], "GPU worker")

    def test_budget_truncation_and_missing_symbolizer(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            elf_fixture(root / "fixture.so")
            samples = root / "samples.txt"
            samples.write_text(f"CPU {root / 'fixture.so'} 110 ? 0\n" * 3)
            result = profile.summarize(samples, tool=None, max_samples=2)
            self.assertTrue(result["truncated_by_reader"])
            self.assertEqual(result["unknown_reasons"], {"GNU_addr2line_unavailable": 2})
            self.assertEqual(result["symbolized_samples"], 0)

    def test_addr2line_out_of_order_output_is_not_assigned(self):
        wrong = subprocess.CompletedProcess([], 0, "0xBAD\nwrong_function\nfixture.c:1\n", "")
        with patch.object(profile.subprocess, "run", return_value=wrong):
            names, errors = profile.symbolize("fake", Path("/unused"), [0x400010], 1)
        self.assertFalse(names)
        self.assertTrue(errors)


if __name__ == "__main__":
    unittest.main()
