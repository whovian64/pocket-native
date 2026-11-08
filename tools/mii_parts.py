#!/usr/bin/env python3
"""The console's Mii parts (CFL_Res.dat) out of a file: the file itself, the
RomFS of the console's Mii data as Citra and Azahar keep it, or Azahar's
open-source placeholder as a C header.

usage: mii_parts.py <in> <out>, exiting 1 with nothing written when the file
has none
"""
import re
import struct
import sys


def mii_parts(data):
    """whether data is Mii parts: their version, then the twenty parts'
    offsets in order, the first just past them, inside the file"""
    if len(data) < 0x54:
        return False
    words = struct.unpack_from("<21I", data)
    return words[:2] == (0x05090014, 0x54) and list(words[1:]) == sorted(words[1:]) and words[20] < len(data)


def romfs_files(data):
    """the files of a level 3 RomFS"""
    if len(data) < 0x28 or struct.unpack_from("<I", data)[0] != 0x28:
        return
    files_at, files_size, data_at = struct.unpack_from("<3I", data, 0x1C)
    at = 0
    while at + 0x20 <= files_size:
        offset, size = struct.unpack_from("<2Q", data, files_at + at + 8)
        name_size = struct.unpack_from("<I", data, files_at + at + 0x1C)[0]
        yield data[data_at + offset:data_at + offset + size]
        at += 0x20 + (name_size + 3 & ~3)


def main():
    source, out = sys.argv[1:3]
    data = open(source, "rb").read()
    if source.endswith(".h"):
        data = bytes(int(byte, 16) for byte in re.findall(rb"0x([0-9a-fA-F]{2})", data))
    for found in [data, *romfs_files(data)]:
        if mii_parts(found):
            open(out, "wb").write(found)
            return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
