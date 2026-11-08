#!/usr/bin/env python3
"""The console's Mii parts (CFL_Res.dat) from Nintendo's update servers, the
way a 3DS gets its system data: the Mii data title's ticket, metadata and
content, the title key opened with the common key from the player's
aes_keys.txt.

usage: mii_download.py <aes_keys.txt> <out>, exiting 1 with nothing written
when the keys don't have the common key or the download fails
"""
import io
import struct
import sys
import urllib.request

from Cryptodome.Cipher import AES

import cia2cci
import mii_parts

TITLE = 0x0004009B00010202
CDN = f"http://nus.cdn.c.shop.nintendowifi.net/ccs/download/{TITLE:016x}/"


def get(name):
    with urllib.request.urlopen(CDN + name, timeout=60) as response:
        return response.read()


def body(signed):
    """where a ticket's or TMD's body starts, past its signature"""
    kind = struct.unpack_from(">I", signed)[0]
    return {0x10000: 0x240, 0x10001: 0x140, 0x10002: 0x80, 0x10003: 0x240, 0x10004: 0x140, 0x10005: 0x80}[kind]


def main():
    keys = cia2cci.load_keys(sys.argv[1])
    out = sys.argv[2]
    try:
        ticket, tmd = get("cetk"), get("tmd")
    except OSError as error:
        print(f"== Nintendo's servers can't be reached for the Mii parts, {error}")
        return 1
    at = body(ticket)
    common = f"common{ticket[at + 0xB1]}"
    if not all(name in keys for name in ("slot0x3dkeyx", "generator", common)):
        print("== the aes_keys.txt has no common key, no Mii parts from Nintendo's servers")
        return 1
    common_key = cia2cci.normal_key(keys, 0x3D, keys[common])
    title_key = AES.new(common_key, AES.MODE_CBC, ticket[at + 0x9C:at + 0xA4] + bytes(8)).decrypt(ticket[at + 0x7F:at + 0x8F])

    at = body(tmd)
    for record in range(struct.unpack_from(">H", tmd, at + 0x9E)[0]):
        content_id, index, kind, size = struct.unpack_from(">IHHQ", tmd, at + 0x9C4 + record * 0x30)
        content = get(f"{content_id:08x}")[:size]
        if kind & 1:
            content = AES.new(title_key, AES.MODE_CBC, struct.pack(">H", index) + bytes(14)).decrypt(content)
        plain = io.BytesIO()
        cia2cci.decrypt_ncch(io.BytesIO(content), plain, 0, 0, len(content), keys)
        ncch = plain.getvalue()
        romfs = struct.unpack_from("<I", ncch, 0x1B0)[0] * cia2cci.MU
        # the RomFS's level 3, past the IVFC header and master hash, on a
        # block of its own
        master, block = struct.unpack_from("<I", ncch, romfs + 0x08)[0], 1 << struct.unpack_from("<I", ncch, romfs + 0x4C)[0]
        level3 = ncch[romfs + (0x60 + master + block - 1) // block * block:]
        for found in mii_parts.romfs_files(level3):
            if mii_parts.mii_parts(found):
                open(out, "wb").write(found)
                print("== the console's Mii parts from Nintendo's servers, for Mii heads (experimental)")
                return 0
    return 1


if __name__ == "__main__":
    sys.exit(main())
