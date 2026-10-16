# Start here

Pocket Native does not work like an emulator. There is no app to install
on the handheld and no game included. A PC rebuilds one of your own game
dumps at a time into a folder for the SD card, and that is the part most
people miss.

Only Pokémon Omega Ruby and Alpha Sapphire have been tested and tuned.
Other games may or may not boot.

## What you need

- An R36S (or a close clone) with ArkOS. Other systems, dArkOS and
  dArkOSRE included, are untested.
- A PC for the build: Linux with podman or Docker, or Windows with
  Docker Desktop or Podman Desktop. On Windows it has to be running
  before you start.
- Your own dump of the game: a decrypted `.3ds`/`.cci`, or a `.cia`
  along with your console's `aes_keys.txt`.

## Building

From the repository's folder:

```bash
scripts/build.sh path/to/game.3ds
```

With a `.cia`:

```bash
scripts/build.sh path/to/game.cia --keys path/to/aes_keys.txt
```

On Windows, in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build.ps1 path\to\game.3ds
powershell -ExecutionPolicy Bypass -File scripts\build.ps1 path\to\game.cia -Keys path\to\aes_keys.txt
```

The first build sets up the build container, which downloads a few
hundred MB and takes a while. Each game's code is recompiled, which
takes several minutes for a big game. Later builds reuse what they can.

The result is a `Pocket Native` folder in `build/`. Building more games
into the same folder adds them next to each other, one launcher each. A
game with no profile yet builds with `(no profile yet)` after its name
and is named after its title ID.

## Installing

The build leaves a folder called `Pocket Native` in `build/`. Inside
it are two parts, and they have to stay together:

- one `.sh` file per game, named after the game. That file is the
  launcher, and it is what EmulationStation lists.
- a folder called `pocket-native`, with the program and the games in
  it. The launcher starts the copy of `pocket-native` beside it, so
  neither part works on its own.

1. Copy the whole `Pocket Native` folder, as it is, into `ports` on
   the card's game partition: `roms/ports`, or `roms2/ports` with two
   cards. The result should look like this:

   ```
   roms/ports/Pocket Native/Pokémon Omega Ruby.sh
   roms/ports/Pocket Native/pocket-native/
   ```

   Do not copy the `build` folder itself, do not copy just one part,
   and do not rename anything or move files between the two parts.
2. Put the card back in the handheld and restart EmulationStation.
3. Each game shows up under Ports, named after the game, one entry
   per `.sh` file.

Building another game later adds its `.sh` file and its game files to
the same output folder. Copy the folder to the card again and let it
overwrite and merge, so the new game arrives whole.

If a game is missing from Ports, count the `.sh` files where you
copied the folder: there should be one per game, sitting directly in
`Pocket Native`, not inside `pocket-native`. If a game is listed but
does not start, the folder arrived in pieces: delete it from the card
and copy the whole folder again.

A tap of FN opens the menu in game and pauses it, FN + Start quits.
Saves and the game's log go in
`Pocket Native/pocket-native/games/<title id>/data`.

## If it goes wrong

**The build says the image is encrypted.** The dump has to be decrypted
first, or built from the `.cia` with `--keys` instead.

**The Windows build cannot reach Docker.** Docker Desktop is not
running. Start it, let it finish starting, and run the build again.

**The game is not under Ports.** The folder is in the wrong place, or
EmulationStation has not been restarted. The whole `Pocket Native`
folder goes inside `ports`, not just the files in it. If it still does
not show up, your system may only list launchers sitting directly in
`ports`: move the `.sh` files and the `pocket-native` folder out of
`Pocket Native` into `ports` itself, side by side.

**A black screen with choppy sound, and FN pauses but shows no menu.**
The handheld's GPU did not start, so the game is drawing in software,
far too slowly to show anything. No setting fixes that. Older builds
did this on dArkOS and dArkOSRE, so build the game again with the
current version. If it still happens, the game's `log.txt` has a line
with `could not initialize EGL`, please post it in an issue.

**A game other than Omega Ruby or Alpha Sapphire does not start, or
runs badly.** That is expected for now. Its `log.txt`, in the game's
`data` folder above, says what the emulator ran into.
