#!/usr/bin/env python3
"""symbolize_profile.py <profile> <apk> <symbolizer> <name>=<unstripped elf>...

Turns the samples ZAKURO_HOST_PROFILE wrote on Android into the functions
they fell in, busiest first, overall and for each thread. The app's
libraries sit uncompressed inside its APK, so a sample in base.apk is placed
in the library whose bytes hold it, then at that library's address, which
llvm-symbolizer names from the build's unstripped copy. The recompiled
game's functions are named after the 3DS address they start at.
"""

import collections
import re
import struct
import subprocess
import sys
import zipfile


def load_segments(path):
    """the PT_LOAD segments of a 64-bit ELF, (file offset, address, size)."""
    with open(path, "rb") as f:
        data = f.read(64)
        phoff = struct.unpack_from("<Q", data, 0x20)[0]
        phentsize, phnum = struct.unpack_from("<HH", data, 0x36)
        f.seek(phoff)
        table = f.read(phentsize * phnum)
    segments = []
    for i in range(phnum):
        kind, _, offset, vaddr, _, filesz = struct.unpack_from("<IIQQQQ", table, i * phentsize)
        if kind == 1:
            segments.append((offset, vaddr, filesz))
    return segments


def to_address(segments, offset):
    for start, vaddr, size in segments:
        if start <= offset < start + size:
            return offset - start + vaddr
    return None


def entries(apk):
    """where each stored library's bytes start in the APK, and how many."""
    out = {}
    with open(apk, "rb") as raw, zipfile.ZipFile(apk) as z:
        for info in z.infolist():
            if not info.filename.endswith(".so"):
                continue
            raw.seek(info.header_offset)
            header = raw.read(30)
            name_len, extra_len = struct.unpack_from("<HH", header, 26)
            start = info.header_offset + 30 + name_len + extra_len
            out[info.filename.rsplit("/", 1)[-1]] = (start, info.file_size)
    return out


def main():
    profile, apk, symbolizer = sys.argv[1:4]
    elves = dict(arg.split("=", 1) for arg in sys.argv[4:])
    stored = entries(apk)
    segments = {name: load_segments(path) for name, path in elves.items()}

    samples = collections.Counter()
    for line in open(profile, errors="replace"):
        # a thread's name can have spaces in it, the file is where the
        # first word that is a path or ? comes before a hex offset
        parts = line.split()
        for i in range(1, len(parts) - 1):
            if parts[i][:1] in "/?[" and re.fullmatch(r"[0-9a-f]+", parts[i + 1]):
                thread, path, offset = " ".join(parts[:i]), parts[i], int(parts[i + 1], 16)
                break
        else:
            continue
        place = (path.rsplit("/", 1)[-1], offset)
        if path.endswith(".apk"):
            for name, (start, size) in stored.items():
                if start <= offset < start + size:
                    place = (name, offset - start)
                    break
        samples[(thread, place)] += 1

    # every distinct place in a library we have, at its address
    wanted = collections.defaultdict(set)
    for (_, (name, offset)) in samples:
        if name in segments:
            address = to_address(segments[name], offset)
            if address is not None:
                wanted[name].add(address)
    names = {}
    for name, addresses in wanted.items():
        ordered = sorted(addresses)
        text = "\n".join(hex(a) for a in ordered) + "\n"
        result = subprocess.run(
            [symbolizer, "--obj=" + elves[name], "--functions=linkage", "--no-inlines", "--output-style=GNU"],
            input=text, capture_output=True, text=True,
        ).stdout.split("\n")
        # two lines for each address, the function then the file and line
        for i, address in enumerate(ordered):
            function = result[2 * i] if 2 * i < len(result) else "??"
            names[(name, address)] = function if function and function != "??" else "?"

    def label(place):
        name, offset = place
        if name in segments:
            address = to_address(segments[name], offset)
            if address is not None:
                return f"{name} {names.get((name, address), '?')}"
        return f"{name} +{offset:x}"

    total = sum(samples.values())
    overall = collections.Counter()
    threads = collections.defaultdict(collections.Counter)
    for (thread, place), count in samples.items():
        where = label(place)
        overall[where] += count
        threads[thread][where] += count

    print(f"{total} samples\n")
    print("busiest overall")
    for where, count in overall.most_common(40):
        print(f"  {100 * count / total:5.1f}%  {where}")
    by_thread = sorted(threads.items(), key=lambda item: -sum(item[1].values()))
    for thread, counter in by_thread[:4]:
        own = sum(counter.values())
        print(f"\nthread {thread}, {100 * own / total:.1f}% of the samples")
        for where, count in counter.most_common(25):
            print(f"  {100 * count / own:5.1f}%  {where}")


if __name__ == "__main__":
    main()
