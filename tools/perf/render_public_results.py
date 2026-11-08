#!/usr/bin/env python3
"""Render the pinned public evidence snapshot; no games or benchmarks run.

Requires Pillow. Run from any directory with the repository's Python toolchain.
This renders existing observations, not inferred execution-time reductions.
"""

import json
import subprocess
from functools import lru_cache
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


ROOT = Path(__file__).resolve().parents[2]
ASSETS = ROOT / "docs" / "assets"
DATA = ASSETS / "optimization-results-2026-11-08.json"
INK = "#142536"
MUTED = "#516274"
BG = "#ffffff"
LINE = "#d7e0e9"
CONTROL = "#8091a5"
PROTOTYPE = "#2672b8"


@lru_cache(maxsize=2)
def font_path(bold):
    # Resolve the installed sans face rather than assume a distribution's paths.
    family = "sans-serif:style=Bold" if bold else "sans-serif"
    path = subprocess.run(
        ["fc-match", "-f", "%{file}", family],
        check=True, capture_output=True, text=True, timeout=5,
    ).stdout.strip()
    if not Path(path).is_file():
        raise ValueError("fontconfig did not return an installed font")
    return path


@lru_cache(maxsize=32)
def font(size, bold=False):
    return ImageFont.truetype(font_path(bold), size)


def text(draw, xy, value, size=25, fill=INK, bold=False, anchor=None):
    draw.text(xy, value, font=font(size, bold), fill=fill, anchor=anchor)


def write(image, name):
    image.save(ASSETS / name, optimize=True)


def code_shape(data):
    image = Image.new("RGB", (1600, 940), BG)
    draw = ImageDraw.Draw(image)
    text(draw, (72, 48), "Dispatch candidate: code-generation metrics", 44, bold=True)
    text(draw, (72, 112), "x86 linked code • current C1 prototype • R34–R35", 27, MUTED)
    for x, color, label in [(72, CONTROL, "Current control"), (390, PROTOTYPE, "Private prototype")]:
        draw.rounded_rectangle((x, 174, x + 28, 202), radius=5, fill=color)
        text(draw, (x + 42, 170), label, 25)

    charts = [
        (95, 740, "Hit-path stack operations", "operations", 8, [0, 2, 4, 6, 8], data["dispatch"]["hit_path_stack_operations"]),
        (890, 1535, "Selected dispatch code", "bytes", 1000, [0, 250, 500, 750, 1000], data["dispatch"]["selected_dispatch_text"]),
    ]
    for left, right, title, unit, maximum, ticks, values in charts:
        text(draw, (left, 250), title, 30, bold=True)
        text(draw, (left, 295), unit, 23, MUTED)
        top, bottom = 380, 700
        for tick in ticks:
            x = left + (right - left) * tick / maximum
            draw.line((x, top - 30, x, bottom), fill=LINE, width=2)
            text(draw, (x, bottom + 13), str(tick), 21, MUTED, anchor="mt")
        for y, key, color in [(400, "control", CONTROL), (565, "prototype", PROTOTYPE)]:
            value = values[key]
            end = left + (right - left) * value / maximum
            draw.rounded_rectangle((left, y, end, y + 77), radius=8, fill=color)
            # Value labels stay outside the bar and away from the panel edge.
            text(draw, (end + 14, y + 35), str(value), 29, bold=True, anchor="lm")
        if unit == "bytes":
            text(draw, (left, 770), "Callback + run loop + miss helper", 22, MUTED)

    draw.line((72, 827, 1528, 827), fill=LINE, width=2)
    text(draw, (72, 852), "Code shape only. Gameplay speed and resource savings are unmeasured.", 27, bold=True)
    text(draw, (72, 897), "Misses add a helper call. No default promotion. Source: 6d11a58 • overhead-study • R34–R35.", 21, MUTED)
    write(image, "dispatch-code-shape-2026-11-08.png")


def evidence(data):
    image = Image.new("RGB", (1800, 1010), BG)
    draw = ImageDraw.Draw(image)
    text(draw, (70, 46), "Optimization evidence snapshot", 47, bold=True)
    text(draw, (70, 115), "No accepted new game-level speedup or resource saving", 30, bold=True)
    text(draw, (70, 169), "Independent scopes • evidence retained through 6d11a58 • 8 November 2026", 25, MUTED)

    x0, label_width, cell_width = 70, 360, 325
    header_y, row_y, row_height = 260, 365, 117
    headers = ["Source / CPU\nchecks", "Serialized\nsampled replay", "Normal async /\nquality", "Admitted\ngame benefit"]
    for i, header in enumerate(headers):
        x = x0 + label_width + i * cell_width + cell_width / 2
        for j, line in enumerate(header.split("\n")):
            text(draw, (x, header_y + j * 36), line, 25, bold=True, anchor="mt")
    palette = {
        "PASS": ("#e1f1e9", "#17563b"),
        "HELD": ("#fff0dc", "#855013"),
        "UNPROVEN": ("#e9edf2", "#3f4e5e"),
        "NOT RUN": ("#f0f3f6", "#546575"),
        "PILOT ONLY": ("#e8eff8", "#245080"),
        "COMPONENT": ("#e8eff8", "#245080"),
        "PARTIAL": ("#fff0dc", "#855013"),
        "AUDIT ONLY": ("#e8eff8", "#245080"),
    }
    for row, item in enumerate(data["evidence"]):
        y = row_y + row * row_height
        text(draw, (x0, y + 42), item["name"], 28, bold=True, anchor="lm")
        for column, value in enumerate(item["cells"]):
            left = x0 + label_width + column * cell_width + 10
            bg, fg = palette[value]
            draw.rounded_rectangle((left, y + 3, left + cell_width - 20, y + 85), radius=10, fill=bg)
            text(draw, (left + (cell_width - 20) / 2, y + 43), value, 25, fg, True, "mm")

    draw.line((70, 864, 1730, 864), fill=LINE, width=2)
    text(draw, (70, 892), "PASS covers only its named sampled scope; it does not imply full-game correctness.", 26, bold=True)
    text(draw, (70, 938), "Historical async failures remain held. Timing, full visual/audio quality and physical handheld proof remain incomplete.", 23, MUTED)
    write(image, "optimization-evidence-2026-11-08.png")


def main():
    data = json.loads(DATA.read_text())
    if data["accepted_new_game_level_benefit"] or data["dispatch"]["performance_claim"]:
        raise ValueError("This snapshot must not be rendered as a performance claim")
    code_shape(data)
    evidence(data)
    print("Rendered two public evidence figures from " + DATA.name)


if __name__ == "__main__":
    main()
