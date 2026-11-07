#!/usr/bin/env python3
"""Summarize Linux ZAKURO_HOST_PROFILE samples without treating offsets as VAs."""

from __future__ import annotations

import argparse
import collections
import hashlib
import json
import math
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys

CAPACITY = 1 << 18
HEX = re.compile(r"[0-9a-fA-F]{1,16}")


def sha256(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def parse_sample(line: str) -> tuple[str, str, int] | None:
    """Suffix fields have no spaces; thread names may contain spaces.

    Unquoted paths containing spaces cannot be recovered from this format.
    Refuse lines whose four suffix fields cannot be identified exactly.
    """
    parts = line.strip().rsplit(maxsplit=4)
    if len(parts) == 4:
        parts.insert(0, "(unnamed)")
    if len(parts) != 5:
        return None
    thread, file, offset, caller, caller_offset = parts
    is_mapping = lambda text: text == "?" or text.startswith("/") or bool(re.fullmatch(r"\[[^\s]+\]", text))
    if not (thread and is_mapping(file) and is_mapping(caller)
            and HEX.fullmatch(offset) and HEX.fullmatch(caller_offset)):
        return None
    # A path fragment in the inferred thread is ambiguous, not a verified name.
    if any(token.startswith("/") for token in thread.split()):
        return None
    return thread, file, int(offset, 16)


def load_segments(path: Path) -> list[tuple[int, int, int]]:
    """Validate an ELF64 PT_LOAD table; return (file offset, VA, file size)."""
    size = path.stat().st_size
    with path.open("rb") as source:
        header = source.read(64)
        if len(header) != 64 or header[:4] != b"\x7fELF" or header[4] != 2 or header[5] not in (1, 2):
            raise ValueError("not a supported 64-bit ELF")
        endian = "<" if header[5] == 1 else ">"
        phoff = struct.unpack_from(endian + "Q", header, 32)[0]
        phentsize, phnum = struct.unpack_from(endian + "HH", header, 54)
        if not phnum or phentsize < 56 or phoff + phentsize * phnum > size:
            raise ValueError("invalid or truncated ELF program-header table")
        segments = []
        for index in range(phnum):
            source.seek(phoff + index * phentsize)
            kind, _, offset, address, _, filesz = struct.unpack(endian + "IIQQQQ", source.read(40))
            if kind == 1:
                if offset + filesz > size:
                    raise ValueError("ELF segment exceeds file")
                segments.append((offset, address, filesz))
        if not segments:
            raise ValueError("ELF has no file-backed PT_LOAD segments")
    return segments


def to_address(segments: list[tuple[int, int, int]], offset: int) -> int | None:
    addresses = {offset - start + address for start, address, size in segments if start <= offset < start + size}
    return addresses.pop() if len(addresses) == 1 else None


def symbolize(tool: str, elf: Path, addresses: list[int], timeout: float) -> tuple[dict, list[str]]:
    names, errors = {}, []
    for first in range(0, len(addresses), 128):
        batch = addresses[first:first + 128]
        try:
            result = subprocess.run([tool, "--addresses", "--functions", "--demangle", "--exe", str(elf)],
                                    input="".join(f"0x{address:x}\n" for address in batch),
                                    text=True, capture_output=True, timeout=timeout, check=False)
            lines = result.stdout.splitlines()
            if result.returncode != 0 or len(lines) != len(batch) * 3:
                raise ValueError(f"symbolizer exit {result.returncode}, unexpected output shape")
            decoded = {}
            for index, address in enumerate(batch):
                echoed, function, location = lines[index * 3:index * 3 + 3]
                if int(echoed, 16) != address:
                    raise ValueError("symbolizer returned an address out of order")
                if function and function != "??":
                    decoded[address] = {"function": function, "location": location}
            names.update(decoded)
        except (OSError, ValueError, subprocess.TimeoutExpired) as error:
            errors.append(str(error))
    return names, errors


def summarize(profile: Path, *, overrides: dict[str, Path] | None = None, tool: str | None = None,
              top: int = 20, max_samples: int = CAPACITY, max_files: int = 128,
              max_addresses: int = 50000, max_input_mib: int = 64, timeout: float = 10) -> dict:
    if profile.stat().st_size > max_input_mib * 1024 * 1024:
        raise ValueError("profile exceeds --max-input-mib; increase the deliberate input budget")
    if min(top, max_samples, max_files, max_addresses, max_input_mib) < 1 or not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("all limits must be positive")
    overrides = overrides or {}
    samples = collections.Counter()
    unknown = collections.Counter()
    total, truncated, malformed = 0, False, 0
    with profile.open(errors="replace") as source:
        for index, line in enumerate(source):
            if index >= max_samples:
                truncated = True
                break
            total += 1
            sample = parse_sample(line)
            if sample is None:
                malformed += 1
                unknown["malformed_or_ambiguous_line"] += 1
            else:
                samples[sample] += 1
    symbolizer = {"path": tool, "version": None, "available": False}
    if tool:
        try:
            version = subprocess.run([tool, "--version"], capture_output=True, text=True, timeout=timeout, check=False)
            symbolizer["version"] = version.stdout.splitlines()[0] if version.stdout else ""
            symbolizer["available"] = version.returncode == 0 and symbolizer["version"].startswith("GNU addr2line")
        except (OSError, subprocess.TimeoutExpired) as error:
            symbolizer["error"] = str(error)
    wanted = collections.defaultdict(set)
    file_totals = collections.Counter()
    thread_totals = collections.Counter()
    for (thread, file, offset), count in samples.items():
        wanted[file].add(offset)
        file_totals[file] += count
        thread_totals[thread] += count
    # Resolve the hottest files first when the deliberate symbolization budget is exhausted.
    resolved, failures, files = {}, {}, []
    used_addresses = 0
    for file_index, (file, count) in enumerate(file_totals.most_common()):
        entry = {"recorded_path": file, "samples": count}
        path = overrides.get(file, Path(file))
        reason = None
        if file_index >= max_files:
            reason = "file_budget_exhausted"
        elif not file.startswith("/"):
            reason = "mapping_not_file_backed_or_unknown"
        elif not path.is_file():
            reason = "mapped_elf_unavailable"
        else:
            try:
                segments = load_segments(path)
                # An override must retain file offsets and load addresses, not merely its basename.
                if file in overrides and Path(file).is_file() and load_segments(Path(file)) != segments:
                    raise ValueError("override PT_LOAD layout differs from mapped ELF")
                entry.update({"symbol_elf": str(path.resolve()), "sha256": sha256(path), "segments": segments,
                              "override": file in overrides,
                              "original_layout_checked": file not in overrides or Path(file).is_file()})
                address_map = {offset: to_address(segments, offset) for offset in wanted[file]}
                addresses = sorted({address for address in address_map.values() if address is not None})
                selected = addresses[:max(0, max_addresses - used_addresses)]
                used_addresses += len(selected)
                if symbolizer["available"]:
                    names, errors = symbolize(tool, path, selected, timeout)
                    entry["symbolizer_errors"] = errors
                else:
                    names, errors = {}, []
                selected_set = set(selected)
                for offset, address in address_map.items():
                    place = (file, offset)
                    if address is None:
                        failures[place] = "offset_outside_or_ambiguous_PT_LOAD"
                    elif address not in selected_set:
                        failures[place] = "address_budget_exhausted"
                    elif not symbolizer["available"]:
                        failures[place] = "GNU_addr2line_unavailable"
                    elif address not in names:
                        failures[place] = "symbolizer_failed_or_symbol_unknown" if errors else "symbol_unknown"
                    else:
                        resolved[place] = names[address]
            except (OSError, ValueError) as error:
                reason = "invalid_or_unreadable_ELF"
                entry["error"] = str(error)
        if reason:
            for offset in wanted[file]:
                failures[file, offset] = reason
        files.append(entry)
    functions, by_thread = collections.Counter(), collections.defaultdict(collections.Counter)
    unknown_by_file = collections.Counter()
    locations = {}
    for (thread, file, offset), count in samples.items():
        place = (file, offset)
        symbol = resolved.get(place)
        if symbol:
            label = (file, symbol["function"])
            locations[label] = symbol["location"]
            functions[label] += count
            by_thread[thread][label] += count
        else:
            unknown[failures.get(place, "unresolved")] += count
            unknown_by_file[file] += count

    def rows(counter: collections.Counter, denominator: int) -> list[dict]:
        return [{"file": file, "function": function, "location": locations[file, function], "samples": count,
                 "percent_of_denominator": 100 * count / denominator if denominator else 0}
                for (file, function), count in counter.most_common(top)]

    return {"schema_version": 1, "profile": {"path": str(profile.resolve()), "sha256": sha256(profile)},
            "status": "empty" if not total else "partial" if truncated or unknown else "symbolized",
            "samples_read": total, "parsed_samples": total - malformed,
            "symbolized_samples": sum(functions.values()), "unknown_samples": sum(unknown.values()),
            "unknown_reasons": dict(unknown), "truncated_by_reader": truncated,
            "sampler_capacity_may_have_been_reached": total >= CAPACITY,
            "symbolizer": symbolizer, "unique_addresses_attempted": used_addresses,
            "limits": {"top": top, "max_samples": max_samples, "max_files": max_files,
                       "max_unique_addresses": max_addresses, "max_input_mib": max_input_mib},
            "top_functions": rows(functions, total),
            "top_files": [{"file": file, "samples": count, "unknown_samples": unknown_by_file[file],
                           "percent_of_all_samples": 100 * count / total if total else 0}
                          for file, count in file_totals.most_common(top)],
            "top_threads": [{"thread": thread, "samples": count,
                             "percent_of_all_samples": 100 * count / total if total else 0,
                             "top_functions": rows(by_thread[thread], count)}
                            for thread, count in thread_totals.most_common(top)],
            "files": files, "scope": "sampled instruction pointers, not exact timings or call-graph evidence",
            "limitations": ["ITIMER_PROF signal samples can be biased or dropped across threads",
                            "x86_64 profiler records no caller PC; caller suffix is not unwound",
                            "input format cannot reliably represent paths containing spaces",
                            "ELFs must match the sampled build; current file hashes cannot prove capture-time identity",
                            "an override whose original file is unavailable has unverified original ELF layout"]}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--top", type=int, default=20)
    parser.add_argument("--max-samples", type=int, default=CAPACITY)
    parser.add_argument("--max-files", type=int, default=128)
    parser.add_argument("--max-unique-addresses", type=int, default=50000)
    parser.add_argument("--max-input-mib", type=int, default=64)
    parser.add_argument("--symbolizer-timeout", type=float, default=10)
    parser.add_argument("--addr2line", default=shutil.which("addr2line"))
    parser.add_argument("--elf", action="append", default=[], metavar="RECORDED_PATH=UNSTRIPPED_ELF")
    args = parser.parse_args()
    try:
        overrides = {}
        for entry in args.elf:
            recorded, separator, source = entry.partition("=")
            if not separator or not recorded.startswith("/") or not source or recorded in overrides:
                raise ValueError("--elf requires a unique absolute recorded path and ELF filename")
            overrides[recorded] = Path(source)
        result = summarize(args.profile, overrides=overrides, tool=args.addr2line, top=args.top,
                           max_samples=args.max_samples, max_files=args.max_files,
                           max_addresses=args.max_unique_addresses, max_input_mib=args.max_input_mib,
                           timeout=args.symbolizer_timeout)
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps({"output": str(args.output.resolve()), "status": result["status"],
                          "samples": result["samples_read"], "unknown": result["unknown_samples"]}))
        return 0
    except (OSError, ValueError, struct.error) as error:
        parser.exit(2, f"profile error: {error}\n")


if __name__ == "__main__":
    sys.exit(main())
