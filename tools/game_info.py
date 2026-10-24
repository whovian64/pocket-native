#!/usr/bin/env python3
"""Prints a decrypted .3ds/.cci image's own title, from the SMDH in its
ExeFS (the "icon" file the HOME Menu shows), and with an output path writes
its 48x48 icon there as a PNG, scaled up by a whole number for a launcher.
Prints nothing and exits 1 when the image has no SMDH.

usage: game_info.py game.3ds [icon.png [scale]]
"""
import struct
import sys
import zlib

MEDIA = 0x200
# the SMDH's titles: sixteen languages of short title, long title and
# publisher, English second; the large icon after the small one
TITLES = 0x8
TITLE_SIZE = 0x200
ENGLISH = 1
LARGE_ICON = 0x24C0


def smdh(path):
    with open(path, "rb") as rom:
        header = rom.read(0x200)
        if header[0x100:0x104] != b"NCSD":
            return None
        first = struct.unpack_from("<I", header, 0x120)[0] * MEDIA
        rom.seek(first)
        ncch = rom.read(0x200)
        if ncch[0x100:0x104] != b"NCCH" or not ncch[0x18F] & 0x4:
            return None
        exefs_offset = struct.unpack_from("<I", ncch, 0x1A0)[0] * MEDIA
        if exefs_offset == 0:
            return None
        rom.seek(first + exefs_offset)
        exefs = rom.read(0x200)
        for entry in range(10):
            name, offset, size = struct.unpack_from("<8sII", exefs, entry * 16)
            if name.rstrip(b"\0") == b"icon":
                rom.seek(first + exefs_offset + 0x200 + offset)
                data = rom.read(size)
                return data if data[:4] == b"SMDH" else None
    return None


def title(data):
    for language in (ENGLISH, 0):
        at = TITLES + language * TITLE_SIZE
        short = data[at : at + 0x80].decode("utf-16-le", "ignore").split("\0")[0].strip()
        if short:
            # the HOME Menu breaks long titles over lines
            return " ".join(short.split())
    return None


def icon(data):
    """the large icon's 48x48 pixels as rows of RGB: RGB565 in 8x8 tiles,
    each in Morton order."""
    pixels = [[(0, 0, 0)] * 48 for _ in range(48)]
    for index in range(48 * 48):
        tile, inside = divmod(index, 64)
        x = (tile % 6) * 8 + sum(((inside >> (2 * bit)) & 1) << bit for bit in range(3))
        y = (tile // 6) * 8 + sum(((inside >> (2 * bit + 1)) & 1) << bit for bit in range(3))
        value = struct.unpack_from("<H", data, LARGE_ICON + index * 2)[0]
        r, g, b = (value >> 11) & 31, (value >> 5) & 63, value & 31
        pixels[y][x] = ((r << 3) | (r >> 2), (g << 2) | (g >> 4), (b << 3) | (b >> 2))
    return pixels


def png(pixels, scale, path):
    rows = b"".join(b"\0" + bytes(channel for pixel in row for _ in range(scale) for channel in pixel) for row in pixels for _ in range(scale))
    size = len(pixels) * scale

    def chunk(kind, body):
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body))

    with open(path, "wb") as out:
        out.write(b"\x89PNG\r\n\x1a\n")
        out.write(chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0)))
        out.write(chunk(b"IDAT", zlib.compress(rows, 9)))
        out.write(chunk(b"IEND", b""))


data = smdh(sys.argv[1])
if data is None or len(data) < LARGE_ICON + 48 * 48 * 2:
    sys.exit(1)
name = title(data)
if name:
    print(name)
if len(sys.argv) > 2:
    png(icon(data), int(sys.argv[3]) if len(sys.argv) > 3 else 1, sys.argv[2])
