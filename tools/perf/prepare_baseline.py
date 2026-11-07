#!/usr/bin/env python3
"""Prepare an original-revision source archive with ONLY frontend observers.

The output must be new. This never edits the checkout, engine, compiler or lock.
Building is separate so callers can freeze and inspect the exact patch first.
"""

import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_REVISION = "178f2b0f4f3444ed8d6af49fe5c130fe3094b598"


def instrument_frontend(source):
    if "probe.begin_batch()" in source or "probe.checkpoint(&system" in source:
        raise ValueError(
            "revision already has observers; use an uninstrumented original"
        )
    replacements = [
        (
            "        let began = Instant::now();",
            "        let began = Instant::now();\n        if let Some(probe) = probe.as_mut() { probe.begin_batch(); }",
        ),
        (
            "        match system.run_frame() {",
            "        if let Some(probe) = probe.as_mut() { probe.begin_frame(); }\n        match system.run_frame() {",
        ),
        (
            "        if let Some(request) = system.keyboard_request() {",
            "        if let Some(probe) = probe.as_mut() { probe.checkpoint(&system, frame_number); }\n        if let Some(request) = system.keyboard_request() {",
        ),
        (
            "        if let Some(limit) = slow {",
            "        if let Some(probe) = probe.as_mut() { probe.finish_batch(); }\n        if let Some(limit) = slow {",
        ),
    ]
    replacements.append(
        (
            "            let (read_us, read_bytes) = zakuro_core::services::fs::rom_reads();",
            "            let (read_us, read_bytes) = zakuro_core::services::fs::rom_reads();\n"
            "            if log::log_enabled!(log::Level::Info) {\n"
            "                if let Some(probe) = probe.as_mut() { probe.before_wait_reset(); }\n"
            "            }",
        )
    )
    for old, new in replacements:
        if source.count(old) != 1:
            raise ValueError(
                "original frontend changed; inspect observer insertion anchor: "
                + old.strip()
            )
        source = source.replace(old, new, 1)
    return source


def prepare(revision, output):
    if not hasattr(tarfile, "data_filter"):
        raise ValueError(
            "source preparation requires Python's tarfile data filter (Python 3.12+, or patched 3.11)"
        )
    commit = subprocess.check_output(
        ["git", "rev-parse", revision + "^{commit}"], cwd=ROOT, text=True
    ).strip()
    archive = subprocess.check_output(["git", "archive", commit], cwd=ROOT)
    output.mkdir(parents=True, exist_ok=False)
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        tar.extractall(output, filter="data")
    before = {
        p.relative_to(output).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in output.rglob("*")
        if p.is_file()
    }
    lib = output / "frontend/src/lib.rs"
    lib.write_text(instrument_frontend(lib.read_text()))
    probe = ROOT / "frontend/src/probe.rs"
    (output / "frontend/src/probe.rs").write_bytes(probe.read_bytes())
    tracked = subprocess.check_output(
        ["git", "ls-tree", "-r", commit], cwd=ROOT, text=True
    ).splitlines()
    changed = []
    archive_export_differences = []
    hashes = {}
    for line in tracked:
        metadata, name = line.split("\t", 1)
        if metadata.split()[1] != "blob":
            raise ValueError("archive with submodules unsupported")
        content = (output / name).read_bytes()
        git_blob = hashlib.sha1(
            b"blob " + str(len(content)).encode() + b"\0" + content
        ).hexdigest()
        if hashlib.sha256(content).hexdigest() != before[name]:
            changed.append(name)
        elif git_blob != metadata.split()[2]:
            archive_export_differences.append(name)
            if name.startswith(("engine/", "recompiler/")) or name in (
                "Cargo.toml",
                "Cargo.lock",
            ):
                raise ValueError("build input differs from original Git blob: " + name)
        hashes[name] = hashlib.sha256(content).hexdigest()
    if sorted(changed) != ["frontend/src/lib.rs", "frontend/src/probe.rs"]:
        raise ValueError("unexpected original source changes: " + repr(changed))
    receipt = {
        "schema_version": 1,
        "original_revision": commit,
        "changed_files": changed,
        "archive_export_differences": archive_export_differences,
        "current_probe_sha256": hashlib.sha256(probe.read_bytes()).hexdigest(),
        "source_sha256": hashes,
        "scope": "only frontend observers differ from the Git archive; engine/compiler/Cargo.lock verified as exact Git blobs; attribute-driven archive export differences listed separately",
    }
    (output / "observer-source.json").write_text(json.dumps(receipt, indent=2) + "\n")
    return receipt


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--revision", default=DEFAULT_REVISION)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    try:
        result = prepare(args.revision, args.output.resolve())
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        p.exit(2, str(error) + "\n")
    print(
        json.dumps(
            {k: result[k] for k in ("original_revision", "changed_files", "scope")}
        )
    )


if __name__ == "__main__":
    main()
