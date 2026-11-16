#!/usr/bin/env python3
"""Bakes the logo for the engine, the kit, the Android app and the README.

    tools/bake_logo.py "Pocket Native-02.png" "Pocket Native-01.png"

The colour logo is the first, the monochrome one the second. Writes:

    assets/logo/mono.alpha      u16 width, u16 height, then alpha bytes, the
                                monochrome logo for the handheld's screens
    assets/logo/colour.rgba     u16 width, u16 height, then RGBA, for the kit
    assets/logo/pocket-native.png
    scripts/android/res/mipmap-*/ic_launcher.png and ic_launcher_foreground.png,
                                the adaptive icon's logo
"""
import struct
import sys
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
MONO_HEIGHT = 112
COLOUR_HEIGHT = 128
ANDROID = {"mdpi": 48, "hdpi": 72, "xhdpi": 96, "xxhdpi": 144, "xxxhdpi": 192}


def sized(image, height):
    return image.resize((round(image.width * height / image.height), height), Image.LANCZOS)


def square(image, side):
    image = image.copy()
    image.thumbnail((side, side), Image.LANCZOS)
    out = Image.new("RGBA", (side, side))
    out.paste(image, ((side - image.width) // 2, (side - image.height) // 2))
    return out


def main(colour_png, mono_png):
    colour = Image.open(colour_png).convert("RGBA")
    mono = Image.open(mono_png).convert("RGBA")
    out = ROOT / "assets" / "logo"
    out.mkdir(parents=True, exist_ok=True)
    alpha = sized(mono, MONO_HEIGHT).getchannel("A")
    (out / "mono.alpha").write_bytes(struct.pack("<HH", *alpha.size) + alpha.tobytes())
    small = sized(colour, COLOUR_HEIGHT)
    (out / "colour.rgba").write_bytes(struct.pack("<HH", *small.size) + small.tobytes())
    sized(colour, 512).save(out / "pocket-native.png", optimize=True)
    for density, side in ANDROID.items():
        folder = ROOT / "scripts" / "android" / "res" / f"mipmap-{density}"
        folder.mkdir(parents=True, exist_ok=True)
        square(colour, side).save(folder / "ic_launcher.png", optimize=True)
        # 108 dp, the logo filling the middle 72 dp a launcher shows
        canvas = Image.new("RGBA", (side * 9 // 4,) * 2)
        logo = square(colour, side * 9 // 4 * 70 // 108)
        canvas.paste(logo, ((canvas.width - logo.width) // 2,) * 2)
        canvas.save(folder / "ic_launcher_foreground.png", optimize=True)


if __name__ == "__main__":
    main(*sys.argv[1:3])
