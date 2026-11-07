"""Pure Python input/evidence tests; no GL context or GPU is launched."""

import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "mali_compiler_check", Path(__file__).with_name("mali_compiler_check.py")
)
compiler = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(compiler)


class MaliCompilerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name, data in (
            ("vertex.vert", b"vertex"),
            ("reference.frag", b"reference"),
            ("candidate.frag", b"candidate"),
            ("harness", b"local harness"),
            ("shim.so", b"noop"),
            ("driver.so", b"gallium"),
        ):
            (self.root / name).write_bytes(data)
        (self.root / "harness").chmod(0o700)
        (self.root / "mesa.json").write_text(
            json.dumps({"ICD": {"library_path": "libEGL_mesa.so.0"}})
        )
        self.manifest = self.root / "pairs.tsv"
        self.manifest.write_text(
            "case\treference\tcandidate\nshape-1\treference.frag\tcandidate.frag\n"
        )

    def args(self):
        return SimpleNamespace(
            bundle=None,
            pairs=self.manifest,
            vertex=self.root / "vertex.vert",
            harness=self.root / "harness",
            shim=self.root / "shim.so",
            driver_library=self.root / "driver.so",
            egl_vendor_json=self.root / "mesa.json",
            dri_directory=None,
            output=self.root / "output",
            timeout=1,
        )

    def log(self):
        return f"""DRM_OPEN_INTERCEPTOR={self.root / "shim.so"}
KERNEL_RENDER_FD_TYPE=character
KERNEL_RENDER_FD_MAJOR=1
KERNEL_RENDER_FD_MINOR=3
KERNEL_RENDER_FD_PATH=/dev/null
GL_VENDOR=Mesa
GL_RENDERER=Mali-G52 MC4 (Panfrost)
GL_VERSION=OpenGL ES 3.1 Mesa 26.2.5-test
LOADED_LIBRARY={self.root / "shim.so"}
LOADED_LIBRARY={self.root / "driver.so"}
Model: Mali-G52
Shader type: MESA_SHADER_FRAGMENT
Architecture: Bifrost
Work registers:    12 (38% used at 100% occupancy)
Uniform registers: 0 (0% used)
Code size:         3 bytes
Loops:             0
Spills/fills:      0/0
Stack size:        0 bytes
NOOP_DRAW_GL_ERROR=0x0
NO_GPU_EXECUTION=1
"""

    def fake_backend(self, binary_for_source=None, log=None):
        def invoke(command, *, env, stdout, stderr, timeout, check):
            self.assertEqual(env["PAN_GPU_ID"], "7212")
            self.assertEqual(env["MESA_SHADER_CACHE_DISABLE"], "true")
            self.assertEqual(
                env["__EGL_VENDOR_LIBRARY_FILENAMES"], str(self.root / "mesa.json")
            )
            binary = (
                binary_for_source(Path(command[-1]).read_bytes())
                if binary_for_source
                else b"ISA"
            )
            (Path(env["BIFROST_MESA_DUMP_DIR"]) / "actual.frag.bin").write_bytes(binary)
            stdout.write(self.log() if log is None else log)
            return subprocess.CompletedProcess(command, 0)

        return invoke

    def test_real_bytes_match_despite_different_sources_and_retained_hashes_bind_inputs(
        self,
    ):
        with patch.object(
            compiler.subprocess, "run", side_effect=self.fake_backend()
        ) as process:
            report = compiler.run(self.args())
        self.assertEqual(process.call_count, 2)
        self.assertEqual(report["status"], "identical")
        self.assertEqual(report["summary"]["byte_identical_pairs"], 1)
        pair = report["pairs"][0]
        self.assertNotEqual(
            pair["reference_source_sha256"], pair["candidate_source_sha256"]
        )
        self.assertTrue(pair["byte_identical_isa"])
        self.assertFalse(report["scope"]["gpu_execution"])
        self.assertFalse(report["scope"]["target_timing"])
        self.assertEqual(
            json.loads((self.root / "output/report.json").read_text())["status"],
            "identical",
        )

    def test_equal_counts_do_not_hide_different_isa_bytes(self):
        with patch.object(
            compiler.subprocess,
            "run",
            side_effect=self.fake_backend(
                lambda source: b"ABC" if source == b"reference" else b"XYZ"
            ),
        ):
            report = compiler.run(self.args())
        self.assertEqual(report["status"], "different")
        self.assertFalse(report["pairs"][0]["byte_identical_isa"])
        self.assertTrue(
            all(value == 0 for value in report["pairs"][0]["metric_deltas"].values())
        )

    def test_source_hash_mismatch_reports_failure_before_any_compiler_call(self):
        self.manifest.write_text(
            "case\treference\tcandidate\tcandidate_sha256\n"
            "shape-1\treference.frag\tcandidate.frag\t" + "0" * 64 + "\n"
        )
        with patch.object(compiler.subprocess, "run") as process:
            report = compiler.run(self.args())
        process.assert_not_called()
        self.assertEqual(report["status"], "error")
        self.assertIn("SHA256 mismatch", report["error"])
        self.assertNotIn("summary", report)

    def test_duplicate_case_or_missing_pair_is_rejected(self):
        original = self.manifest.read_text()
        self.manifest.write_text(original + original.splitlines()[1] + "\n")
        with self.assertRaisesRegex(ValueError, "unique"):
            compiler.load_pairs(self.manifest)
        self.manifest.write_text(
            "case\treference\tcandidate\nshape-1\treference.frag\n"
        )
        with self.assertRaisesRegex(ValueError, "number of fields"):
            compiler.load_pairs(self.manifest)

    def test_historical_selective_column_and_escaped_header_are_supported(self):
        self.manifest.write_text(
            "case\\treference\\tselective\nshape-1\treference.frag\tcandidate.frag\n"
        )
        pairs = compiler.load_pairs(self.manifest)
        self.assertEqual(pairs[0]["candidate"], self.root / "candidate.frag")

    def test_missing_backend_stats_cannot_be_reported_as_acceptance(self):
        log = self.log().replace("Spills/fills:      0/0\n", "")
        with patch.object(
            compiler.subprocess, "run", side_effect=self.fake_backend(log=log)
        ):
            report = compiler.run(self.args())
        self.assertEqual(report["status"], "error")
        self.assertIn("spill/fill", report["error"])
        self.assertNotIn("summary", report)

    def test_non_g52_renderer_wrong_library_and_ambiguous_stats_fail_closed(self):
        bad_logs = [
            self.log().replace("Mali-G52 MC4 (Panfrost)", "llvmpipe"),
            self.log().replace(f"LOADED_LIBRARY={self.root / 'driver.so'}\n", ""),
            self.log().replace(
                "Work registers:    12", "Work registers: 12\nWork registers:    12"
            ),
        ]
        for log in bad_logs:
            with self.subTest(log=log), self.assertRaises(ValueError):
                compiler.parse_backend_log(
                    log, self.root / "shim.so", self.root / "driver.so"
                )

    def test_existing_output_is_not_replayed(self):
        args = self.args()
        args.output.mkdir()
        with (
            patch.object(compiler.subprocess, "run") as process,
            self.assertRaisesRegex(ValueError, "fresh"),
        ):
            compiler.run(args)
        process.assert_not_called()

    def test_real_drm_fd_is_rejected_even_with_g52_and_expected_interceptor(self):
        log = self.log().replace(
            "KERNEL_RENDER_FD_MAJOR=1", "KERNEL_RENDER_FD_MAJOR=226"
        )
        log = log.replace(
            "KERNEL_RENDER_FD_PATH=/dev/null",
            "KERNEL_RENDER_FD_PATH=/dev/dri/renderD128",
        )
        with self.assertRaisesRegex(ValueError, "kernel render descriptor"):
            compiler.parse_backend_log(
                log, self.root / "shim.so", self.root / "driver.so"
            )

    def test_kernel_fd_evidence_is_required(self):
        log = self.log().replace("KERNEL_RENDER_FD_MAJOR=1\n", "")
        with self.assertRaisesRegex(ValueError, "kernel descriptor major"):
            compiler.parse_backend_log(
                log, self.root / "shim.so", self.root / "driver.so"
            )

    def test_library_hash_cache_invalidates_when_bytes_change(self):
        path = self.root / "driver.so"
        cache = {}
        with patch.object(compiler, "digest", wraps=compiler.digest) as hashed:
            first = compiler.identity(path, cache)
            self.assertEqual(compiler.identity(path, cache), first)
            self.assertEqual(hashed.call_count, 1)
            path.write_bytes(b"changed-driver")
            self.assertNotEqual(
                compiler.identity(path, cache)["sha256"], first["sha256"]
            )
            self.assertEqual(hashed.call_count, 2)

    def test_access_time_change_during_hash_is_not_content_mutation(self):
        state = {
            "st_dev": 1,
            "st_ino": 2,
            "st_size": 7,
            "st_mtime_ns": 3,
            "st_ctime_ns": 4,
        }
        with patch.object(
            Path,
            "stat",
            side_effect=[
                SimpleNamespace(**state, st_atime=0),
                SimpleNamespace(**state, st_atime=1),
            ],
        ):
            result = compiler.identity(self.root / "driver.so")
        self.assertEqual(result["sha256"], compiler.digest(b"gallium"))

    def test_nul_source_cannot_be_silently_truncated_by_gl(self):
        shader = self.root / "candidate.frag"
        shader.write_bytes(b"valid-prefix\0uncompiled-suffix")
        with self.assertRaisesRegex(ValueError, "NUL"):
            compiler.snapshot(shader, self.root, ".frag")

    def test_wrong_sized_binary_and_timeout_retain_failure_reports(self):
        with patch.object(
            compiler.subprocess,
            "run",
            side_effect=self.fake_backend(lambda _: b"bad-size"),
        ):
            report = compiler.run(self.args())
        self.assertEqual(report["status"], "error")
        self.assertIn("wrong-sized", report["error"])
        args = self.args()
        args.output = self.root / "timeout-output"
        with patch.object(
            compiler.subprocess,
            "run",
            side_effect=subprocess.TimeoutExpired("harness", 1),
        ):
            report = compiler.run(args)
        self.assertEqual(report["status"], "error")
        self.assertTrue((args.output / "report.json").is_file())


if __name__ == "__main__":
    unittest.main()
