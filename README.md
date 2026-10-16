# Pocket Native

If you have not used it before, [start here](START_HERE.md). The short
version: there is no app to install and no game included, a PC rebuilds
each game first, and only Omega Ruby and Alpha Sapphire are tested.

Nintendo 3DS games, recompiled ahead of time, running natively on cheap
Linux handhelds. Made for the R36S and its Mali-G31 GPU under ArkOS.

Pocket Native is a fork of [Zakuro](https://github.com/fearkov/zakuro) and
[3dsrecomp](https://github.com/fearkov/3dsrecomp) by fearkov. Zakuro is a
3DS emulator that runs a game's code recompiled to C instead of through a
JIT, and it does the heavy lifting here. This fork adds:

- an OpenGL ES 3.1 renderer for the handheld's GPU, tuned for Mali (the
  R36S has no Vulkan driver)
- a frontend for handhelds without a desktop: the screens go to the panel
  through KMS, buttons come from evdev, sound goes out through ALSA
- an in-game menu for layout, frame rate, power, controls and per-game
  settings, with a line of help for every setting
- per-game profiles in `titles/`, and a probe mode for trying new games
- a one-command build that turns your own game dump into a folder for the
  SD card

The Android app and the Vulkan renderer of upstream Zakuro are not part of
this fork.

## Games

Only Pokémon Omega Ruby and Alpha Sapphire have been tested and tuned.
Other games may or may not boot. See [docs/games.md](docs/games.md) for
trying a game and adding a profile for it.

## What you need

- An R36S (or a close clone) with ArkOS. Other systems, dArkOS and
  dArkOSRE included, are untested.
- A PC for the build: Linux with [podman](https://podman.io) or Docker, or
  Windows with [Docker Desktop](https://www.docker.com/products/docker-desktop/)
  or [Podman Desktop](https://podman-desktop.io).
- Your own dump of the game: a decrypted `.3ds`/`.cci`, or a `.cia` along
  with your console's `aes_keys.txt`.

No game data is included or downloaded. Pocket Native only works with games
you dump from your own console.

## Building

```bash
scripts/build.sh path/to/game.3ds
```

With a `.cia`:

```bash
scripts/build.sh path/to/game.cia --keys path/to/aes_keys.txt
```

On Windows, from the repository's folder in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build.ps1 path\to\game.3ds
powershell -ExecutionPolicy Bypass -File scripts\build.ps1 path\to\game.cia -Keys path\to\aes_keys.txt
```

Docker Desktop has to be running. The Windows build keeps its scratch space
in a Docker volume, `pocket-native-work`; `docker volume rm
pocket-native-work` frees it.

The first build sets up the build container, which downloads a few hundred
MB (Ubuntu packages, Rust, crates) and takes a while. Each game's code is
recompiled to C and compiled for the handheld, which takes several minutes
for a big game. Later builds reuse what they can.

The result is a `Pocket Native` folder in `build/` (or wherever `--out` or
`-Out` points), with the game in it. Building more games into the same folder adds
them next to each other, one launcher each.

## Installing

1. Copy the `Pocket Native` folder into `ports` on the card's game
   partition (`roms/ports`, or `roms2/ports` with two cards).
2. Restart EmulationStation. Each game shows up under Ports.

To start games from EmulationStation's 3DS list instead, set the 3DS
system's `<command>` in `/etc/emulationstation/es_systems.cfg` to run
`3ds.sh`, for example:

```xml
<command>"/roms2/ports/Pocket Native/pocket-native/3ds.sh" %ROM%</command>
```

`3ds.sh` reads the title ID from the ROM it is given and starts that game's
build, so each game still has to be built first.

Saves, the game's own settings and its log go in
`Pocket Native/pocket-native/games/<title id>/data`, everything else in
`Pocket Native/pocket-native/config`.

## Controls

| Handheld | 3DS |
|---|---|
| A / B / X / Y | A / B / X / Y (Xbox layout in the menu) |
| L1 / R1 | L / R |
| D-pad | D-pad |
| Left stick | Circle pad (or the D-pad, in the menu) |
| Right stick | Moves the touch cursor |
| R2 or R3 | Touches the screen under the cursor |
| Start / Select | Start / Select |

| Hotkey | Does |
|---|---|
| Tap FN | Opens the menu and pauses the game |
| FN + Start | Quits |
| L2, or FN + L1 | Swaps the screens |
| FN + R1 | Switches between both screens and one big screen |

## The menu

- **Display:** screen layout, smooth scaling, frame interpolation, fps and
  temperature in the bottom right corner, battery and a 12-hour clock in
  the bottom left
- **Performance:** frame cap (20, 30 or 60) and power (saver, balanced,
  performance)
- **This game:** options from the game's profile
- **Controls:** touch cursor speed, Nintendo or Xbox button layout, left
  stick as circle pad or D-pad
- **Game:** resume, advanced settings, reset, quit

The selected setting's help shows at the bottom of the menu.

**Frame interpolation** adds an in-between frame for games running below
the target, up to 30 or 60 fps. Up to 60 doubles a 30 fps game; up to 30
doubles a 15 fps one and costs half the GPU time. It tracks motion in 16
pixel blocks and blends where it can't, so slow scenes come out smooth and
fast ones can smear thin details. It pauses while the game is skipping
frames, and it is off by default.

**Advanced settings** are saved per game. They switch the engine's speed
hacks and graphics, CPU and loading options for a game that doesn't run or
look right. **Most compatible** turns off every speed hack a game could
trip over in one go; if that fixes it, turn them back on one at a time to
find the culprit. Settings under a "requires restart" heading take effect
after **Restart game**, and **Reset to defaults** goes back to what the
game's profile asks for. [docs/games.md](docs/games.md) lists them all.

## Layout

| Path | What is in it |
|---|---|
| `engine/` | The Zakuro emulator: CPU, GPU, file systems, OS services |
| `engine/gpu/src/raster/gles.rs` | The OpenGL ES renderer |
| `recompiler/` | 3dsrecomp, which turns a game's code into C |
| `frontend/` | The `pocket-native` program: panel, input, sound, menu, probe |
| `titles/<title id>/` | Per-game profiles and hand-written code overrides |
| `scripts/` | The build (`build.sh`, `container/`) and what runs on the handheld (`device/`) |
| `tools/` | Build helpers: title IDs, `.cia` decryption, the system font, third-party licenses |
| `assets/fonts/` | The menu font |

## Development

Pocket Native builds and runs on a Linux PC too, with Mesa's OpenGL ES:

```bash
cargo build --release -p pocket-native
cargo test --workspace --features zakuro-gpu/gles
```

Without a panel, `ZAKURO_PANEL_SHOT=file.ppm` draws offscreen and saves
what the panel would show (a `%` in the name numbers every frame instead).
`--probe` runs a game without input and writes a report, see
[docs/games.md](docs/games.md).

## Credits and license

See [CREDITS.md](CREDITS.md). Pocket Native is under the MIT license, like
Zakuro, see [LICENSE](LICENSE). The menu font is under the SIL Open Font
License, see [assets/fonts/OFL.txt](assets/fonts/OFL.txt). Every build also
puts `LICENSE`, `CREDITS.md`, `OFL.txt` and `THIRD_PARTY_LICENSES.txt` (the
licenses of the Rust crates built into the program and of the fonts) in
`Pocket Native/pocket-native`.

Pocket Native is not affiliated with or endorsed by Nintendo. Nintendo 3DS
and the game names are trademarks of their owners.
