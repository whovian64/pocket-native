# Pocket Native

Nintendo 3DS games on cheap handhelds and Android, rebuilt for them
instead of emulated.

**[Start here](START_HERE.md)** to play · [Which games work](docs/compatibility.md) ·
[Latest release](https://github.com/whovian64/pocket-native/releases/latest)

[![Discord](https://img.shields.io/badge/Discord-join-5865F2?logo=discord&logoColor=white)](https://discord.gg/fZ7Q9hmWCd)
[![Ko-fi](https://img.shields.io/badge/Ko--fi-support-FF5E5B?logo=ko-fi&logoColor=white)](https://ko-fi.com/whovian64)

## Now on Android

Pocket Native now runs on Android too, on one screen or two, thanks to
[gopnikcosmoslav2077](https://github.com/gopnikcosmoslav2077), who
brought up the Android port almost single-handedly
([#8](https://github.com/whovian64/pocket-native/pull/8)). It's been
tested on Samsung and Google phones, the AYN Thor and Anbernic's RG DS
running Android. Your PC still builds the games and the app plays them.

## How it works

An emulator translates a game's code while you play, and a $40 handheld
doesn't have the power to spare for that on top of a 3DS game. Pocket
Native does the translating once, on your PC. It recompiles each game's
code ahead of time into a native program for the handheld's CPU. What
runs on the handheld is that program, plus the parts of an emulator the
game still needs: the 3DS's system services, its GPU redone in OpenGL
ES, sound, and a frontend made for handhelds.

That's enough even for an R36S, a quad Cortex-A35 with a Mali-G31 and
1 GB of RAM.

You bring your own games: **no game data is included**, and Pocket
Native only works with games dumped from your own console. **No
precompiled shaders are included either**, so each game builds its own
the first time it runs.

Pocket Native is a fork of [Zakuro](https://github.com/fearkov/zakuro),
a 3DS emulator built to run recompiled code, and
[3dsrecomp](https://github.com/fearkov/3dsrecomp), its recompiler, both
by fearkov. On top of them it adds:

- an OpenGL ES 3.1 renderer, tuned for Mali GPUs with no Vulkan driver
- a frontend for handhelds with no desktop: KMS or the framebuffer for
  the screens, evdev for the buttons, ALSA for sound
- an in-game menu for layouts, frame cap, power, controls and per-game
  settings
- per-game profiles in `titles/`, and a probe mode for trying games
- a kit that builds your games on Windows or Linux with one drop on
  `Build a game`, and updates itself
- a lot of performance work for low-powered handhelds and Android

## Games

Pokémon Omega Ruby and Alpha Sapphire are the showcase and play well.
Pokémon X and Y, Sun and Moon, Animal Crossing: New Leaf and Mario Kart
7 are playable, and plenty more get somewhere. It's early days: a game
nobody has tried may not boot at all.

The [compatibility list](docs/compatibility.md) has what's known for
each game. Tried one that isn't on it? Please
[open an issue](https://github.com/whovian64/pocket-native/issues),
working or not.

## Handhelds

| Handheld | System | State |
|---|---|---|
| R36S and its clones | ArkOS, dArkOS, dArkOSRE, ArkOS and dArkOS for clones | The main target, developed and tested on an R36S with ArkOS |
| Powkiddy RX6H | dArkOS for clones | Played by testers. No FN button: L3 does its job |
| Anbernic H700 handhelds: RG35XX and RG40XX families, RG CubeXX | Knulli, muOS | Played by testers |
| Anbernic RG353 and RG503 | ArkOS | Buttons mapped, no tester report yet |
| Anbernic RG52 Mini | dArkOS | Played by testers |
| Anbernic RG351P | ArkOS | Buttons not found yet |
| AYN Thor | Android | Works, both screens |
| Anbernic RG DS | Android | Works, both screens |
| Anbernic RG DS | Anbernic's Linux | Not yet: one screen only, buttons not found |
| Anbernic RG DS | ROCKNIX | Both screens and both touch screens, no tester report yet |
| Android phones | Android 11 or newer, 64-bit | Works, with touch controls or a gamepad |
| Other ROCKNIX handhelds | ROCKNIX | Should start as on the RG DS, untested |
| AmberELEC | | Not yet |

Screens other than the R36S's 640x480 work: the layouts follow the
panel's shape (16:9, square, 1024x768), and **Display > Rotation** turns
the picture for a panel mounted sideways.

### The RG DS under ROCKNIX

The 3DS's top screen goes on the top panel and its bottom screen on the
bottom one, which you can touch. A tap of L2 or FN + L1 swaps them.
**Display > Aspect ratio** shapes the top screen on its panel as it does
on one screen, and **Crop** there fills the top panel with the top
screen's 4:3 middle, its sides cut off. **Display > Stretch** fills a
panel instead of keeping the screen's shape.
Sway, ROCKNIX's desktop, keeps running while you play: the launcher
switches the console away from it, which hands the panels to the game,
and switches back when you quit.

If something comes out wrong, these go in `run.sh` next to the other
`export` lines:

| Setting | Does |
|---|---|
| `ZAKURO_KMS_TOP=DSI-1` | Puts the top screen on the other panel (`DSI-2` by default) |
| `ZAKURO_KMS_SINGLE=1` | Uses one panel only, as on a handheld with one screen |
| `ZAKURO_TOUCH_BOTTOM=...` | Names the bottom panel's touch screen by its place in `/sys` (`fe5e0000.i2c` by default) |
| `ZAKURO_TOUCH_FLIP=x`, `y` or `xy` | Mirrors the touch screens, for touches landing on the wrong side |

The game's log is `log.txt` in its `data` folder: please add it to a
report.

## Controls

| Handheld | 3DS |
|---|---|
| A / B / X / Y | A / B / X / Y |
| L1 / R1 | L / R |
| D-pad | D-pad |
| Left stick | Circle pad, or the D-pad (Controls in the menu) |
| Right stick | Moves the touch cursor |
| R2 | Touches the bottom screen under the cursor |
| R3 | Switches the right stick to the C-stick, with L2 and R2 as ZL and ZR, and back |
| Touch screen | Touches the bottom screen where it is shown (RG DS under ROCKNIX) |
| L2 held + right stick | Tilts the console, for games that use motion |
| Start / Select | Start / Select |

| Hotkey | Does |
|---|---|
| Tap FN, or L3 + R2 | Opens the menu and pauses |
| FN + Start | Quits |
| Tap L2, or FN + L1 | Swaps the screens |
| FN + R1 | Switches between both screens and one big screen |
| FN + Select | Switches the D-pad between D-pad, circle pad and touch cursor |

On a handheld with no FN button, L3 does what FN does.

On Android, the touch controls cover all of this, with a Menu button in
place of FN, and hide while you use a gamepad. **Controls > Edit touch
controls** in the menu moves and resizes them, separately for holding
the device upright and sideways.

## The menu

Every setting has a line of help at the bottom of the menu.

- **Display:** layout (big top, side by side, one screen, original size
  and more), aspect ratio, render resolution, smooth scaling, frame
  interpolation, fps counter, clock and battery, rotation
- **Performance:** frame cap (20, 30 or 60) and power (saver, balanced,
  performance)
- **This game:** options from the game's profile, where it has some
- **Controls:** touch cursor speed, the touch cursor shown or hidden
  (on two screens Auto shows it only while the stick moves it), Nintendo
  or Xbox A/B, left stick as
  circle pad or D-pad, the D-pad's job, tilt directions, and the touch
  controls on Android
- **Game:** resume, advanced settings, reset, quit

**Advanced settings** are saved per game. They switch the engine's speed
hacks and its graphics, CPU and loading options for a game that doesn't
run or look right, and set the language and region the game is told.
**Most compatible** turns off every speed hack at once: if that fixes a
game, turn them back on one at a time to find the one it trips over.
Settings under a "requires restart" heading need **Restart game**.

**Aspect ratio > Crop** shows the top screen's 4:3 middle, its sides cut
off, a little bigger than the whole of it would be: on a 4:3 panel the
top screen fills it.

**Frame interpolation** draws an in-between picture for a game running
below the target, up to 30 or 60 fps. It follows motion in 16 pixel
blocks, so slow scenes come out smooth and fast ones can smear thin
details. It's off by default.

## For developers

| Path | What's in it |
|---|---|
| `engine/` | The Zakuro emulator: CPU, GPU, file systems, system services |
| `engine/gpu/src/raster/gles.rs` | The OpenGL ES renderer |
| `recompiler/` | 3dsrecomp, which turns a game's code into C |
| `frontend/` | The `pocket-native` program: panel, input, sound, menu, probe |
| `titles/<title id>/` | Per-game profiles and hand-written code overrides |
| `scripts/` | The kit's build (`build.sh`, `container/`) and what runs on the handheld (`device/`) |
| `tools/` | Build helpers: title IDs, `.cia` decryption, the system font, licenses |

Pocket Native builds and runs on a Linux PC too, with Mesa's OpenGL ES:

```bash
cargo build --release -p pocket-native
cargo test --workspace --features zakuro-gpu/gles
```

With no panel, `ZAKURO_PANEL_SHOT=file.ppm` draws offscreen and saves
what the panel would show (a `%` in the name saves every frame).
`--probe` runs a game unattended and writes a report.
[docs/games.md](docs/games.md) covers probing a game, writing its
profile and the advanced settings a profile can set.

## Credits and license

See [CREDITS.md](CREDITS.md). Pocket Native is under the MIT license,
like Zakuro, see [LICENSE](LICENSE). The menu font is under the SIL
Open Font License, see [assets/fonts/OFL.txt](assets/fonts/OFL.txt).
Every build also puts `LICENSE`, `CREDITS.md`, `OFL.txt` and
`THIRD_PARTY_LICENSES.txt` in `Pocket Native/pocket-native`.

Pocket Native is not affiliated with or endorsed by Nintendo. Nintendo
3DS and the game names are trademarks of their owners.
