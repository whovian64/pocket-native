# Trying a game

Pocket Native runs any 3DS game Zakuro can boot, and nothing in it is
specific to one game: a game without a profile simply runs without one.
What's known about each game is on the
[compatibility list](compatibility.md). This page is for trying one that
isn't, and for writing its profile.

## Building it

Build it the same way as any other game, by dropping it on `Build a game`
or with:

```bash
scripts/build.sh path/to/game.3ds
```

If the game has no profile yet, the build prints `(no profile yet)` and the
launcher is named after the title ID.

## Probing it

`--probe` runs a game as fast as it goes with no one holding the handheld.
It presses A now and then and Start once in a while. It writes
`probe-<title id>/report.md` with:

- how many frames it ran and how fast
- how many pictures each screen got, and how often a screen flashed black
  between two that were not
- how busy the game kept the console's CPU
- whether the game stopped, and why
- every warning and error the emulator logged, counted, with the frame each
  first appeared at. An access to unmapped memory names the thread and the
  code address that made it.
- every thread at the end, with its code address and what it waits for,
  which shows where a game that hangs is stuck
- both screens at eight points through the run, as BMPs

On the handheld, from the game's folder
(`Pocket Native/pocket-native/games/<title id>`):

```bash
ZAKURO_KMS=1 ZAKURO_RECOMPILED=title.so ZAKURO_DATA=data ../../pocket-native --probe game.3ds 3600
```

The number is how many frames to run. 3600 is a minute of the console's
time. The program exits with 1 if the game faulted. With
`ZAKURO_PROBE_SPEED=0.6` it skips frames the way a handheld running at 60%
speed would, which shows up frame skip problems on a fast PC.
`ZAKURO_INPUT=frame:BUTTON,...` presses buttons on given frames in place of
the probe's own mashing (`120:A,240:START+A`, each held for six frames), and
`ZAKURO_CLOCK=` a number of milliseconds pins the console's clock and the
random numbers it hands out, so two runs make the same choices.

The warnings and errors are the useful part. They name the services,
commands and GPU features the game needed that the emulator lacks. Please
put the report in an issue when a game does not work.

## Adding a profile

A profile is `titles/<title id>/profile.toml`. The title ID is 16 hex
digits:

```bash
python3 tools/title_id.py path/to/game.3ds
```

A profile gives the game its name and the options the menu lists under
"This game":

```toml
name = "Pokémon Omega Ruby"

[[option]]
name = "Outlines"
help = "The black outlines outdoors. Off saves a quarter of the GPU's work there."
default = true
drops_when_off = ["@depth:8744197d"]

[[option]]
name = "Bottom screen in battle"
help = "How often the battle menu is drawn. 15 fps leaves more time for the battle."
values = ["15 fps", "30 fps"]
default = false
bottom_full_rate_when_on = true
```

An option can:

- `drops_when_off`: leave out draws while the option is off, to save GPU
  time on effects the player can live without. Each rule is the start of a
  draw's signature, or `@depth:<start>` for a screen-wide pass that reads
  the depth buffer. Run with `ZAKURO_DRAW_STATS=1` to log the
  busiest draws' signatures every few seconds.
- `bottom_full_rate_when_on`: draw the bottom screen with every picture,
  rather than every other one while the game is behind.

`values` are the names the menu shows for off and on, "Off" and "On" if
left out. `help` is what the menu says about the option under the list, two
lines at most (`cargo test` checks). The player's choices are saved per
game.

## Advanced settings in a profile

When a game needs one of the engine's speed hacks off, its profile can say so
in an `[advanced]` table, with the names and defaults below. The game
starts with them until the player changes its advanced settings in the
menu.

```toml
[advanced]
skip_bottom = true        # bottom screen skipping
skip_right_eye = true     # the right eye's picture left out, 3D being off
frame_skip = 3            # most frames skipped in a row, 0 to 3
shown_only = false        # frames the frame cap hides left undrawn
fast = true               # speed hacks, which the next ten need
quick_checks = true       # surfaces checked against memory from a few spots
half_precision = true     # fragment colors at half precision
plain_math = true         # fast vertex math
tile_copies = true        # depth tiles on the GPU
gpu_clears = true         # clears on the GPU
gpu_arrays = true         # vertex arrays on the GPU
trust_meshes = true       # trust cached meshes
trust_textures = true     # trust cached textures
small_cache = true        # small texture cache
cull = true               # hide scenery out of view
gpu_drawing = true        # false draws in software, for telling GPU bugs apart
gpu_vertices = true       # vertex shading on the GPU, false for the CPU
gpu_transfers = true      # display transfers on the GPU, false for the CPU
gpu_texture_copies = true # textures the game drew copied on the GPU, false
                          # reads them back from memory
in_place = true           # drawn textures sampled in place
past_drawing = true       # a texture past a drawing repeats its edge
etc1 = true               # compressed textures
etc1a4 = true             # compressed textures with see-through parts, as two textures
generic_shaders = false
generic_lighting = false
draw_ranges = true        # vertex ranges
gpu_thread = true
realtime_gpu = false
recompiled = true         # false runs the game's code in the interpreter
exact_events = false      # exact event timing
prewarm = true            # shader prewarm
read_ahead = true         # ROM read-ahead
background_shaders = true # new shaders compiled in the background
stand_ins = false         # new things drawn plainly while their shaders build,
                          # rather than left out
new3ds = false            # a game with a New 3DS mode run as on a New 3DS
```

Only the ones that differ from these need to be listed. A game's
`data/log.txt` lists every switch it ran with that is off its default,
on lines starting with `switch:`.

## Overrides

`titles/<title id>/overrides/` holds C functions that replace the
recompiled code at an address, for fixes and faster versions of hot
functions. The build passes them to the recompiler. See
[recompiler/docs/overrides.md](../recompiler/docs/overrides.md).

Overrides must be your own code. Never put the game's own code or data in
this repository: no copied instructions, tables, textures or text.
