#!/usr/bin/env python3
"""Compare GLES fragment ISA through a prepared local Mali-G52 no-op compiler path.

No dependency installation, GPU execution, rendered correctness, or target timing.
Exit 0: all pairs have identical ISA/statistics; 1: compiled but different;
2: invalid input, failed compilation, or incomplete backend evidence.
"""

import argparse
import csv
import hashlib
import io
import json
import math
import os
import re
import subprocess
import sys
from pathlib import Path

METRICS = (
    "code_bytes",
    "work_registers",
    "uniform_registers",
    "stack_bytes",
    "spills",
    "fills",
    "loops",
)
SCOPE = {
    "compiler_only": True,
    "gpu_execution": False,
    "target_timing": False,
    "pixel_correctness": False,
    "core_mask": "synthetic drm-shim metadata, not RK3568 topology",
    "no_op_assurance": "conditional on supplied shim built from pinned upstream no-op source; kernel fd checked as /dev/null",
}


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def identity(path: Path, cache: dict | None = None) -> dict:
    resolved = path.resolve(strict=True)
    state = resolved.stat()
    stamp = (
        state.st_dev,
        state.st_ino,
        state.st_size,
        state.st_mtime_ns,
        state.st_ctime_ns,
    )
    key = str(resolved)
    if cache is not None and key in cache and cache[key][0] == stamp:
        return dict(cache[key][1])
    result = {"path": key, "sha256": digest(resolved.read_bytes())}
    after = resolved.stat()
    if (
        after.st_dev,
        after.st_ino,
        after.st_size,
        after.st_mtime_ns,
        after.st_ctime_ns,
    ) != stamp:
        raise ValueError(f"file changed while hashing: {resolved}")
    if cache is not None:
        cache[key] = (stamp, result)
    return result


def load_pairs(path: Path, data: bytes | None = None) -> list[dict]:
    """Read explicit pairs; accept the existing source-dump 'selective' alias."""
    lines = (path.read_bytes() if data is None else data).decode("utf-8").splitlines()
    if not lines:
        raise ValueError("empty pair manifest")
    # Historical fixture dumps escaped tabs in the header only.
    lines[0] = lines[0].replace("\\t", "\t")
    reader = csv.DictReader(io.StringIO("\n".join(lines)), delimiter="\t")
    fields = reader.fieldnames or []
    candidate = "candidate" if "candidate" in fields else "selective"
    if len(fields) != len(set(fields)) or not {"case", "reference", candidate}.issubset(
        fields
    ):
        raise ValueError(
            "pairs require unique case, reference, candidate (or selective) TSV columns"
        )
    pairs, seen = [], set()
    for row in reader:
        if None in row or any(value is None for value in row.values()):
            raise ValueError("pair manifest row has an incorrect number of fields")
        case = row["case"]
        if not case or case in seen or not row["reference"] or not row[candidate]:
            raise ValueError(
                "pair cases must be nonempty and unique, with both source paths"
            )
        seen.add(case)
        pair = {"case": case, "metadata": row}
        for role, column in (("reference", "reference"), ("candidate", candidate)):
            source = (path.parent / row[column]).resolve(strict=True)
            if not source.is_file():
                raise ValueError(f"not a shader file: {source}")
            expected = row.get(f"{column}_sha256")
            if expected and not re.fullmatch(r"[0-9a-f]{64}", expected):
                raise ValueError(f"invalid {column}_sha256 for {case}")
            pair[role] = source
            pair[f"{role}_sha256"] = expected
        pairs.append(pair)
    if not pairs:
        raise ValueError("pair manifest must contain at least one case")
    return pairs


def snapshot(
    path: Path, directory: Path, suffix: str, expected: str | None = None
) -> dict:
    data = path.read_bytes()
    if not data or len(data) > 16 * 1024 * 1024:
        raise ValueError(f"shader must be 1 byte to 16 MiB: {path}")
    if b"\0" in data:
        raise ValueError(f"shader source must not contain NUL bytes: {path}")
    sha256 = digest(data)
    if expected and sha256 != expected:
        raise ValueError(f"source SHA256 mismatch: {path}")
    target = directory / (sha256 + suffix)
    if target.exists():
        if target.read_bytes() != data:
            raise ValueError("snapshot hash collision or modified output")
    else:
        target.write_bytes(data)
    return {
        "original_path": str(path),
        "sha256": sha256,
        "bytes": len(data),
        "path": str(target),
    }


def single(log: str, pattern: str, name: str) -> str:
    values = re.findall(pattern, log, re.MULTILINE)
    if len(values) != 1:
        raise ValueError(f"missing or ambiguous {name}")
    return values[0]


def parse_backend_log(log: str, shim: Path, driver: Path) -> dict:
    kernel_fd = {
        "type": single(log, r"^KERNEL_RENDER_FD_TYPE=(.+)$", "kernel descriptor type"),
        "major": int(
            single(log, r"^KERNEL_RENDER_FD_MAJOR=(\d+)$", "kernel descriptor major")
        ),
        "minor": int(
            single(log, r"^KERNEL_RENDER_FD_MINOR=(\d+)$", "kernel descriptor minor")
        ),
        "path": single(log, r"^KERNEL_RENDER_FD_PATH=(.+)$", "kernel descriptor path"),
    }
    if kernel_fd != {"type": "character", "major": 1, "minor": 3, "path": "/dev/null"}:
        raise ValueError(
            "kernel render descriptor is not upstream no-op shim /dev/null (1:3)"
        )
    vendor = single(log, r"^GL_VENDOR=(.+)$", "GL vendor")
    renderer = single(log, r"^GL_RENDERER=(.+)$", "GL renderer")
    version = single(log, r"^GL_VERSION=(.+)$", "GL version")
    if (
        vendor != "Mesa"
        or not re.fullmatch(r"Mali-G52(?: MC\d+)? \(Panfrost\)", renderer)
        or not re.fullmatch(r"OpenGL ES 3\.[1-9] Mesa .+", version)
    ):
        raise ValueError("active context is not Mesa GLES 3.1+ Panfrost Mali-G52")
    interceptor = Path(
        single(log, r"^DRM_OPEN_INTERCEPTOR=(.+)$", "DRM open interceptor")
    ).resolve()
    libraries = sorted(
        {
            str(Path(p).resolve())
            for p in re.findall(r"^LOADED_LIBRARY=(.+)$", log, re.MULTILINE)
        }
    )
    if (
        interceptor != shim.resolve()
        or str(shim.resolve()) not in libraries
        or str(driver.resolve()) not in libraries
    ):
        raise ValueError(
            "expected no-op shim/driver library is not the active loaded path"
        )
    if any("nvidia" in path.lower() for path in libraries):
        raise ValueError(
            "unexpected NVIDIA library; select a Mesa-only EGL vendor file"
        )
    if (
        single(log, r"^NOOP_DRAW_GL_ERROR=(.+)$", "no-op draw status") != "0x0"
        or single(log, r"^NO_GPU_EXECUTION=(.+)$", "compiler-only marker") != "1"
    ):
        raise ValueError("no-op compiler draw did not complete")
    blocks = re.findall(
        r"Model: ([^\n]+)\nShader type: MESA_SHADER_FRAGMENT\n(.*?)(?=NOOP_DRAW|\Z)",
        log,
        re.DOTALL,
    )
    if len(blocks) != 1 or blocks[0][0] != "Mali-G52":
        raise ValueError("missing or ambiguous Mali-G52 fragment backend statistics")
    block = blocks[0][1]
    if single(block, r"^Architecture:\s*(\S+)$", "ISA architecture") != "Bifrost":
        raise ValueError("fragment backend is not Bifrost")
    stats = {}
    for key, label in (
        ("work_registers", "Work registers"),
        ("uniform_registers", "Uniform registers"),
        ("code_bytes", "Code size"),
        ("stack_bytes", "Stack size"),
        ("loops", "Loops"),
    ):
        stats[key] = int(single(block, rf"^{label}:\s+(\d+).*?$", label))
    spills = single(block, r"^Spills/fills:\s+(\d+/\d+)$", "spill/fill statistics")
    stats["spills"], stats["fills"] = map(int, spills.split("/"))
    return {
        "kernel_render_fd": kernel_fd,
        "vendor": vendor,
        "renderer": renderer,
        "version": version,
        "libraries": libraries,
        **stats,
    }


def compare_pair(case: dict, reference: dict, candidate: dict) -> dict:
    """Use actual retained binary bytes, independently of names or hashes."""
    left, right = Path(reference["isa"]["path"]), Path(candidate["isa"]["path"])
    deltas = {key: candidate[key] - reference[key] for key in METRICS}
    same_bytes = left.read_bytes() == right.read_bytes()
    return {
        "case": case["case"],
        "metadata": case["metadata"],
        "reference_source_sha256": reference["source"]["sha256"],
        "candidate_source_sha256": candidate["source"]["sha256"],
        "reference_isa": reference["isa"],
        "candidate_isa": candidate["isa"],
        "byte_identical_isa": same_bytes,
        "metric_deltas": deltas,
        "equivalent_compiled_pair": same_bytes
        and all(value == 0 for value in deltas.values()),
    }


def summarize(records: list[dict], pairs: list[dict]) -> dict:
    return {
        "schema_version": 1,
        "scope": dict(SCOPE),
        "compiled_fragments": len(records),
        "pair_count": len(pairs),
        "all_backend_accepted": bool(records)
        and all(record.get("backend_validated") for record in records),
        "byte_identical_pairs": sum(p["byte_identical_isa"] for p in pairs),
        "all_pairs_identical": bool(pairs)
        and all(p["equivalent_compiled_pair"] for p in pairs),
    }


def run(args: argparse.Namespace) -> dict:
    hash_cache = {}
    output = args.output.resolve()
    if output.exists():
        raise ValueError(f"output must be a fresh directory: {output}")
    manifest = (args.bundle / "sources.tsv") if args.bundle else args.pairs
    vertex = (args.bundle / "synthetic-fixture.vert") if args.bundle else args.vertex
    if vertex is None:
        raise ValueError("--vertex is required with --pairs")
    manifest = manifest.resolve(strict=True)
    manifest_bytes = manifest.read_bytes()
    pairs = load_pairs(manifest, manifest_bytes)
    tools = {
        key: identity(getattr(args, key), hash_cache)
        for key in ("harness", "shim", "driver_library", "egl_vendor_json")
    }
    if not os.access(tools["harness"]["path"], os.X_OK):
        raise ValueError(
            "harness must be an executable compiled from mali_compile_pair.c"
        )
    icd = json.loads(Path(tools["egl_vendor_json"]["path"]).read_text())
    entry = icd.get("ICD") if isinstance(icd, dict) else None
    library = entry.get("library_path") if isinstance(entry, dict) else None
    if not isinstance(library, str) or "mesa" not in library.lower():
        raise ValueError("--egl-vendor-json must explicitly name the Mesa EGL library")
    if args.dri_directory and not args.dri_directory.is_dir():
        raise ValueError("--dri-directory must exist")
    output.mkdir(parents=True, exist_ok=False)
    inputs = output / "inputs"
    inputs.mkdir()
    records, comparisons = [], []
    report = {
        "schema_version": 1,
        "scope": dict(SCOPE),
        "status": "incomplete",
        "tools": tools,
        "orchestrator": identity(Path(__file__)),
        "harness_source_reference": identity(
            Path(__file__).with_name("mali_compile_pair.c")
        ),
        "records": records,
        "pairs": comparisons,
    }
    try:
        (inputs / "pairs.tsv").write_bytes(manifest_bytes)
        report["input_manifest"] = {
            "original_path": str(manifest.resolve()),
            "sha256": digest(manifest_bytes),
        }
        vertex_input = snapshot(vertex.resolve(strict=True), inputs, ".vert")
        fragments, bound = {}, []
        for pair in pairs:
            binding = {"case": pair["case"], "metadata": pair["metadata"]}
            for role in ("reference", "candidate"):
                source = snapshot(pair[role], inputs, ".frag", pair[f"{role}_sha256"])
                fragments[source["sha256"]] = source
                binding[role] = source["sha256"]
            bound.append(binding)
        report["vertex"] = vertex_input
        report["bindings"] = bound
        by_sha = {}
        for sha256, source in sorted(fragments.items()):
            directory = output / sha256
            for name in ("home", "cache", "data", "tmp", "isa"):
                (directory / name).mkdir(parents=True)
            env = {
                "PATH": os.defpath,
                "LC_ALL": "C",
                "HOME": str(directory / "home"),
                "XDG_CACHE_HOME": str(directory / "cache"),
                "XDG_DATA_HOME": str(directory / "data"),
                "TMPDIR": str(directory / "tmp"),
                "LD_PRELOAD": tools["shim"]["path"],
                "LD_LIBRARY_PATH": str(Path(tools["driver_library"]["path"]).parent),
                "MALI_EXPECTED_SHIM": tools["shim"]["path"],
                "MALI_EXPECTED_DRIVER": tools["driver_library"]["path"],
                "PAN_GPU_ID": "7212",
                "DRM_SHIM_DEBUG": "1",
                "MESA_LOADER_DRIVER_OVERRIDE": "panfrost",
                "MESA_SHADER_CACHE_DISABLE": "true",
                "BIFROST_MESA_DEBUG": "statsfull,shaders",
                "BIFROST_MESA_DUMP_DIR": str(directory / "isa"),
                "__EGL_VENDOR_LIBRARY_FILENAMES": tools["egl_vendor_json"]["path"],
            }
            if args.dri_directory:
                env["LIBGL_DRIVERS_PATH"] = str(args.dri_directory.resolve())
            command = [tools["harness"]["path"], vertex_input["path"], source["path"]]
            record = {
                "source": source,
                "argv": command,
                "environment": env,
                "log_path": str(directory / "compiler.log"),
            }
            records.append(record)
            with (directory / "compiler.log").open("w") as log:
                process = subprocess.run(
                    command,
                    check=False,
                    env=env,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    timeout=args.timeout,
                )
            record["exit_code"] = process.returncode
            if process.returncode:
                raise ValueError(
                    f"compiler process failed ({process.returncode}): {directory / 'compiler.log'}"
                )
            record.update(
                parse_backend_log(
                    (directory / "compiler.log").read_text(),
                    args.shim,
                    args.driver_library,
                )
            )
            binaries = list((directory / "isa").glob("*.frag.bin"))
            if (
                len(binaries) != 1
                or binaries[0].stat().st_size != record["code_bytes"]
                or not record["code_bytes"]
            ):
                raise ValueError(
                    f"missing, ambiguous, or wrong-sized fragment ISA: {directory}"
                )
            record["isa"] = identity(binaries[0])
            record["loaded_libraries"] = [
                identity(Path(path), hash_cache) for path in record["libraries"]
            ]
            for key in ("harness", "shim", "driver_library", "egl_vendor_json"):
                if identity(Path(tools[key]["path"]), hash_cache) != tools[key]:
                    raise ValueError(
                        f"tool/library changed during compiler check: {key}"
                    )
            record["backend_validated"] = True
            by_sha[sha256] = record
        comparisons.extend(
            compare_pair(case, by_sha[case["reference"]], by_sha[case["candidate"]])
            for case in bound
        )
        report["summary"] = summarize(records, comparisons)
        report["status"] = (
            "identical" if report["summary"]["all_pairs_identical"] else "different"
        )
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        report["status"] = "error"
        report["error"] = str(error)
    (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument(
        "--bundle",
        type=Path,
        help="existing source-dump directory (sources.tsv + synthetic-fixture.vert)",
    )
    source.add_argument(
        "--pairs",
        type=Path,
        help="TSV: case, reference, candidate; source paths relative to TSV",
    )
    parser.add_argument(
        "--vertex",
        type=Path,
        help="vertex source for --pairs; declare whether synthetic in your evidence",
    )
    for name in ("harness", "shim", "driver-library", "egl-vendor-json", "output"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument(
        "--dri-directory", type=Path, help="optional explicit Mesa DRI loader directory"
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=15,
        help="seconds per compiler process, default 15",
    )
    args = parser.parse_args()
    if (
        not math.isfinite(args.timeout)
        or args.timeout <= 0
        or (args.bundle and args.vertex)
    ):
        parser.error("timeout must be positive; --vertex applies only to --pairs")
    try:
        report = run(args)
    except (OSError, ValueError, TypeError) as error:
        parser.exit(2, f"Mali compiler check error: {error}\n")
    print(
        json.dumps(
            {
                "status": report["status"],
                "report": str(args.output.resolve() / "report.json"),
                "summary": report.get("summary"),
                "error": report.get("error"),
            },
            indent=2,
        )
    )
    return 2 if report["status"] == "error" else int(report["status"] != "identical")


if __name__ == "__main__":
    sys.exit(main())
