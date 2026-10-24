# Pocket Native

If you have not used it before, [start here](START_HERE.md). The short
version: there is no app to install and no game included, a PC rebuilds
each game first (drop it on `Build a game`).

Nintendo 3DS games, recompiled, running natively on cheap
Linux handhelds. Made for the R36S and its Mali-G31 GPU under ArkOS, and for
the Anbernic RG DS under Android, see [below](#android-the-anbernic-rg-ds).

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
- a build that turns your own game dump into a folder for the SD card:
  drop the game on `Build a game`, on Windows or Linux
- an updater that brings the kit up to the latest release and builds
  your games again with it

The Vulkan renderer of upstream Zakuro is not part of this fork.

## Android: the Anbernic RG DS

This branch also runs on the Anbernic RG DS under GammaOS Next (Android 14),
built from the same engine as the ArkOS build. Each game becomes an app of
its own, named and iconed from the game:

- both screens: the top on the top panel, the bottom on the second display
  with touch going straight to the 3DS, or both on one when GammaOS's
  DualStack joins the panels
- each screen can fill its panel or keep the 3DS's shape
- sound through AAudio that stays in sync with the picture
- the game on the SD card, and the app movable to an SD card adopted as
  internal storage
- shaders kept between runs, so effects stutter only the first time

A PC builds the apps in a container and puts them on the handheld over adb,
with USB debugging on:

```bash
scripts/android/build.sh path/to/game.3ds --install
scripts/android/install.sh "fire emblem" --log    # its log, and hints for the next build
```

Games are named by any part of their name, no title ids. See
[docs/android.md](docs/android.md) for the setup, the switches a game takes
from `data/env.txt`, and the tools for finding what holds a game back
(`profile.sh`, `analyze.sh`).

Tested on the RG DS: Pokémon Alpha Sapphire and X, Fire Emblem Awakening
(30 fps in battle, cutscenes still below full speed) and Monster Hunter 4
Ultimate (27 fps in the village with Exact event timing on, alongside its
profile's Real-time GPU).

## Games

Tested and tuned on the R36S:

- Pokémon Omega Ruby and Alpha Sapphire
- Animal Crossing: New Leaf (US version)
- Mario Kart 7 (US version)

Other games may or may not boot. See [docs/games.md](docs/games.md) for
trying a game and adding a profile for it.

## What you need

- An R36S (or a close clone) with ArkOS. dArkOSRE works too. Other
  systems, dArkOS included, are untested.
- A PC for the build: Linux with [podman](https://podman.io) or Docker, or
  Windows with [Docker Desktop](https://www.docker.com/products/docker-desktop/)
  or [Podman Desktop](https://podman-desktop.io).
- Your own dump of the game: a decrypted `.3ds`/`.cci`, or a `.cia` along
  with your console's `aes_keys.txt`.

No game data is included or downloaded. Pocket Native only works with games
you dump from your own console.

## Building

Get the kit from the [latest release](https://github.com/whovian64/pocket-native/releases/latest)
(**Source code**), or clone the repository. Then drop your games on
`Build a game` in the kit's folder, or open it and pick them:
`Build a game.bat` on Windows, `Build a game.desktop` on Linux. A `.cia`
uses the `aes_keys.txt` next to it or in the kit's folder, and asks for
it otherwise.

From a terminal:

```bash
scripts/build.sh path/to/game.3ds
scripts/build.sh path/to/game.cia --keys path/to/aes_keys.txt
```

On Windows, `scripts\build.ps1` takes the same from PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build.ps1 path\to\game.3ds
powershell -ExecutionPolicy Bypass -File scripts\build.ps1 path\to\game.cia -Keys path\to\aes_keys.txt
```

Several games can be built at once.

Docker Desktop has to be running.

The first build sets up the build container, which downloads a few hundred
MB (Ubuntu packages, Rust, crates) and takes a while. Each game's code is
recompiled to C and compiled for the handheld, which takes several minutes
for a big game. Later builds reuse what they can.

The result is a `Pocket Native` folder in `build/` (or wherever `--out` or
`-Out` points), with the game in it. Building more games into the same folder adds
them next to each other, one launcher each.

## Updating

A build ends by saying when a newer release is out. `Update Pocket Native`
in the kit's folder, or `scripts/update.sh` (`scripts\update.ps1` on
Windows), downloads it into the kit, moves the kit's old files to `.old`,
and builds every game in `build/` again from the copies the last build
left there. A git clone is updated with `git pull` instead. `--out`/`-Out`
points it at another output folder. The menu shows the version running,
next to "Pocket Native" at the top.

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
| L2 held + right stick | Tilts the console, for games that use motion |
| Start / Select | Start / Select |

| Hotkey | Does |
|---|---|
| Tap FN, or L3 + R2 | Opens the menu and pauses the game |
| FN + Start | Quits |
| Tap L2, or FN + L1 | Swaps the screens |
| FN + R1 | Switches between both screens and one big screen |
| FN + Select | Switches the D-pad between D-pad, circle pad and touch cursor, for handhelds short of sticks |

On a handheld with no FN, L3 does what FN does in these.

## The menu

- **Display:** screen layout, smooth scaling, frame interpolation, fps and
  temperature in the bottom right corner, battery and a 12-hour clock in
  the bottom left, and rotation for a screen that shows the picture the
  wrong way round
- **Performance:** frame cap (20, 30 or 60) and power (saver, balanced,
  performance)
- **This game:** options from the game's profile
- **Controls:** touch cursor speed, Nintendo or Xbox button layout, left
  stick as circle pad or D-pad, D-pad as itself, circle pad or touch
  cursor, tilt reversed left and right or up and down
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
look right, and set the console language and region the game is told. **Most compatible** turns off every speed hack a game could
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
