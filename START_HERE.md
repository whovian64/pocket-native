# Start here

Pocket Native doesn't come with games, and it doesn't play your game
files directly. Your PC rebuilds each of your 3DS games into a program
made for your device first. Then:

- on a **Linux handheld** (R36S and friends) you copy what the PC made
  onto the SD card
- on **Android** you install the app and add what the PC made to it

Essentially: get the kit, drop your games on `Build a game`, then copy
the `Pocket Native` folder it makes to your handheld's card or add it in
the Android app.

Before you start, check [which games work](docs/compatibility.md) and
[which devices](README.md#handhelds).

## What you need

- **A device:**
  - a Linux handheld from the [list](README.md#handhelds), or
  - an Android phone or handheld with Android 11 or newer, 64-bit (almost
    any from the last few years)
- **A PC** with Windows or Linux. It does the heavy work, your device
  never builds anything.
  - Windows: Docker Desktop or Podman Desktop. If neither is
    installed, the build offers to install Docker Desktop for you.
  - Linux: podman or Docker.
  - Free space: a few GB for the build container, and two to three
    times each game's size for each game you build.
- **Your own games**, dumped from your own console: a decrypted
  `.3ds`/`.cci`, or a `.cia` along with your console's `aes_keys.txt`.
  Any region works.

## 1. Get the kit

Open the [latest release](https://github.com/whovian64/pocket-native/releases/latest),
download **Source code (zip)** under Assets, and unzip it somewhere.
The folder you get is the kit: everything below happens in it. You only
do this once, later versions come in through the updater.

## 2. Build your games

This step is the same for every device.

**Windows:** start Docker Desktop (or Podman Desktop) and let it finish
starting. Then drag your games onto `Build a game.bat`, or double-click
it and pick them.

**Linux:** drag your games onto `Build a game`, or open it and pick
them.

Several games can go at once. A `.cia` needs your console's
`aes_keys.txt`: put it next to the game or in the kit's folder and the
build finds it, otherwise it asks.

A window shows the build as it goes and stays open at the end so you
can read how it went. The very first build sets up the build container,
which downloads a few hundred MB and takes a while. After that, each
game takes a few minutes, most of it recompiling the game's code.

When it's done, the kit's `build` folder holds a `Pocket Native` folder
with your games in it. Building more games later adds them to the same
folder.

Prefer a terminal? In the kit's folder:

```bash
scripts/build.sh path/to/game.3ds path/to/other.cia --keys path/to/aes_keys.txt
```

or `scripts\build.ps1` in PowerShell on Windows (`-Keys` for the keys).

## 3. Put the games on your device

### On a Linux handheld

Put the handheld's SD card in your PC and copy the **whole**
`Pocket Native` folder from `build` into the folder your system keeps
ports in:

| System | Where it goes |
|---|---|
| ArkOS, dArkOS, dArkOSRE | `roms/ports`, or `roms2/ports` with two cards |
| Knulli | `roms/ports` on the SHARE partition |
| muOS | `ROMS/Ports` |
| ROCKNIX | `roms/ports` on the GAMES partition (`/storage/roms/ports`) |

On ArkOS it ends up like this:

```
roms/ports/Pocket Native/Pokémon Omega Ruby.sh
roms/ports/Pocket Native/pocket-native/
```

The `.sh` files are the launchers, one per game, and `pocket-native`
holds the program and the games. They only work together, so copy the
folder as it is: don't rename anything or move files between the two.

Put the card back, restart the handheld's frontend (EmulationStation on
ArkOS), and your games are under Ports.

### On Android

1. **Install the app.** Download the `.apk` under Assets on the
   [latest release](https://github.com/whovian64/pocket-native/releases/latest)
   onto the device and open it. The first time, Android asks you to
   allow installing apps from your browser or file manager.
2. **Copy your games over.** Copy the `Pocket Native` folder from the
   kit's `build` folder onto the device, for example into `Download`
   over USB.
3. **Add them in the app.** Tap **Add game**, pick **A folder the kit
   built**, choose that `Pocket Native` folder, tap **Use this folder**
   and **Allow**. The app adds every game it finds in there, so one go
   does them all.

The app keeps its own copy of each game, so you can delete the one in
`Download` afterwards.

Uninstalling the app deletes the games and saves it holds, Android
removes an app's folder along with it. To update, install a newer
`.apk` over the old one instead.

## 4. Play

**The first start of each game is slow.** It builds the GPU programs
the game needs, which can take a few minutes with a black screen or a
stuttering title screen. Let it run. The next start reuses them, and
any that were still missing get built as you play.

### On a Linux handheld

The buttons you need right away:

- **A tap of FN** opens the menu and pauses the game. No FN button? Press
  **L3 and R2** together.
- **FN + Start** quits (**L3 + Start** without FN).
- The **right stick** moves a cursor over the bottom screen, **R2**
  touches it.

The full list of controls is in the [README](README.md#controls).

### On Android

Tap a game to play it. Touch controls cover everything, and a gamepad
works too: the touch controls hide while you use one and come back when
you touch the screen.

- The on-screen **Menu** button, or **Back**, opens the menu.
- **Controls > Edit touch controls** in the menu lets you drag the
  buttons and sticks where you want them and make them bigger or
  smaller, separately for holding the device upright and sideways.
- **Device**, at the top right of the app, sets the orientation, touch
  controls, aspect ratio, resolution and smooth scaling for every game,
  and on a handheld with two screens (AYN Thor, RG DS) whether the
  bottom screen goes on the second one.
- Long-press a game to put it on your home screen, with launchers that
  take shortcuts.

### The menu

The menu has a line of help for each setting at the bottom. The ones
worth knowing first:

- **Performance > Frame cap:** 30 fps by default, which is what most
  3DS games run at. 60 is there for the games made for 60.
- **Display > Layout:** both screens, one big screen, side by side and
  more. FN + R1 switches between both screens and one big one in game
  on a Linux handheld.
- **This game:** options for that game, where it has any.
- **Advanced settings > Most compatible:** for a game that crashes or
  looks wrong, then **Restart game**. If that fixes it, the setting
  that broke it is one of the ones it turned off.

## Updating

When a new version is out, the build tells you when it finishes. Open
`Update Pocket Native` in the kit's folder (`Update Pocket Native.bat`
on Windows): it downloads the new version into the kit, builds every
game in `build` again from the copies the last build kept, so you
don't need your game files for it, and keeps the old files in `.old`.

Then:

- **Linux handheld:** copy the `Pocket Native` folder onto the card
  again, over the old one.
- **Android:** install the new `.apk` from the release, then add the
  rebuilt `Pocket Native` folder in the app again. A game added again
  keeps its saves and settings.

The menu shows which version you're running, at the top.

## If something goes wrong

**Windows says it protected your PC, or asks whether to run the file.**
It does that for anything from the internet. Click **More info**, then
**Run anyway**.

**Nothing happens when you open `Build a game` on Linux.** The file
manager doesn't run launchers it doesn't trust yet. Allow it when it
asks, or in the file's properties, or run `scripts/build.sh` from a
terminal.

**The Windows build can't reach Docker.** Docker Desktop isn't running.
Start it, wait until it says it's running, and build again.

**The build says the game is encrypted.** Decrypt the dump first, or
build it from the `.cia` with your `aes_keys.txt`.

**The update can't reach GitHub.** Check the PC's connection and try
again, or get the new kit by hand as in step 1.

**The game isn't under Ports.** The folder is in the wrong place, or
the frontend hasn't been restarted. The whole `Pocket Native` folder
goes inside the ports folder. If it still doesn't show, your system may
only list launchers sitting directly in the ports folder: move the
`.sh` files and the `pocket-native` folder out of `Pocket Native`,
side by side into the ports folder itself.

**The game is listed but won't start.** The folder arrived in pieces.
Delete it from the card and copy the whole folder again.

**The Android app says there's no game there.** Pick the `Pocket
Native` folder the kit made, the one with `pocket-native` inside.
Games built with a kit from 0.3.0 or before don't load on Android:
update the kit and build them again.

**A black screen for minutes on the first start.** Usually the GPU
programs being built, see step 4. If it never comes up, look in the
game's `log.txt`. On a Linux handheld, a line with
`could not initialize EGL` means the GPU didn't start for Pocket
Native, please report it.

**A game crashes, freezes or looks wrong.** Try **Advanced settings >
Most compatible** and **Restart game** first. The
[compatibility list](docs/compatibility.md) says what's known for each
game.

## Reporting a problem

Open an [issue](https://github.com/whovian64/pocket-native/issues) and
say:

- the device and the system on it (ArkOS, Knulli, muOS, Android ...)
- the game, and which region
- the Pocket Native version, from the top of the menu
- what happened, and where in the game

and attach the game's `log.txt`:

- **Linux handheld:** `Pocket Native/pocket-native/games/<title id>/data/log.txt`
  on the card
- **Android:** `Android/data/io.github.pocketnative/files/games/<title id>/data/log.txt`.
  Plug the device into a PC to get at it, many file managers can't
  open `Android/data`.

Copy it off right after the problem, the next start of that game
replaces it.

A game that works is worth an issue too, it goes on the
[compatibility list](docs/compatibility.md).
