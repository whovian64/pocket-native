#!/usr/bin/env python3
"""third_party.py <out> [package target]: the licenses of every crate built
into pocket-native for the handheld, or into the package for the target given
as the Android app, and of the fonts, into one file to ship beside it.

Reads `cargo metadata` from the repository and each crate's license files
from where cargo unpacked it.
"""
import json
import pathlib
import subprocess
import sys

TARGET = "aarch64-unknown-linux-gnu"
LICENSE_NAMES = ("license", "licence", "copying", "notice", "copyright")

FONTS = """\
== Fonts ==

The menu font is M PLUS Rounded 1c, Copyright 2016 The Rounded M+ Project
Authors, under the SIL Open Font License 1.1 (OFL.txt beside this file),
baked into the program.

Each game's sysdata/shared_font.bin is the open-source replacement for the
3DS's system font, made at build time from Azahar's
externals/open_source_archives, which B3n30's citra_system_archives (GPL-2.0)
generated from Montserrat (SIL Open Font License 1.1) and GNU Unifont 10.0.05
(GPL-2.0-or-later with the GNU font embedding exception).

A game's sysdata/CFL_Res.placeholder.dat, where there is one, is the
open-source placeholder for the 3DS's Mii parts, taken at build time from the
same archives, which citra_system_archives (GPL-2.0) generated. A
sysdata/CFL_Res.dat came from the player's own console or game, or from
Nintendo's update servers, opened with the player's own keys.
"""


def main():
    out = pathlib.Path(sys.argv[1])
    name, target = sys.argv[2:4] if len(sys.argv) > 3 else ("pocket-native", TARGET)
    metadata = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target],
        check=True, stdout=subprocess.PIPE).stdout.decode("utf-8"))
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    root = next(id for id, package in packages.items() if package["name"] == name)

    # what ends up in the program: normal dependencies, all the way down
    built, waiting = set(), [root]
    while waiting:
        node = nodes[waiting.pop()]
        for dependency in node["deps"]:
            if any(kind["kind"] is None for kind in dependency["dep_kinds"]) and dependency["pkg"] not in built:
                built.add(dependency["pkg"])
                waiting.append(dependency["pkg"])

    members = set(metadata["workspace_members"])
    sections = []
    for id in sorted(built - members, key=lambda id: (packages[id]["name"], packages[id]["version"])):
        package = packages[id]
        folder = pathlib.Path(package["manifest_path"]).parent
        files = sorted(path for path in folder.iterdir() if path.is_file() and path.name.lower().startswith(LICENSE_NAMES))
        texts = "\n\n".join(path.read_text(encoding="utf-8", errors="replace").strip() for path in files)
        heading = f"== {package['name']} {package['version']} ({package.get('license') or 'see below'}) =="
        source = package.get("repository") or ""
        sections.append(f"{heading}\n{source}\n\n{texts or 'No license file in the package, its license is ' + str(package.get('license'))}\n")

    out.write_text(
        "Pocket Native is under the MIT license (LICENSE beside this file).\n"
        "These are the licenses of what it is built from.\n\n" + FONTS + "\n" + "\n".join(sections), encoding="utf-8")
    print(f"{len(sections)} crates")


if __name__ == "__main__":
    main()
