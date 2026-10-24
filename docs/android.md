# Android: the Anbernic RG DS under GammaOS Next

Work in progress. The goal is Pocket Native on the RG DS (RK3568, four
Cortex-A55 cores, Mali-G52, two 640x480 touch screens) under GammaOS Next
(Android 14), with the 3DS's top screen on the top panel and its bottom
screen, touch included, on the bottom one.

## What carries over and what does not

The engine, the recompiler, the OpenGL ES renderer, the menu and the
per-game profiles are plain Rust and C and build for Android as they are.
Android has `libEGL.so` and `libGLESv2.so`, which the renderer already falls
back to.

Everything that talks to the handheld is Linux-only and needs an Android
counterpart:

| ArkOS | Android |
|---|---|
| KMS/GBM or `/dev/fb0` for the panel | an `ANativeWindow` per display, from an activity |
| evdev for buttons and sticks | Android key and motion events |
| the right stick and R2 for touch | real touch on the bottom display |
| ALSA | AAudio |
| `power.sh` writing sysfs | GammaOS's performance profiles, or `su` with Magisk |
| `title.so` built by gcc against glibc | `title.so` built by the NDK's clang against bionic |
| `title.so` loaded from the card | packaged in the APK's `lib/arm64-v8a/`, since Android 10 and later will not load code from an app's data folder |

## Phases

1. **Headless bring-up (done).** Build `pocket-native` and a game's
   `title.so` for Android, push them with adb and run `--probe` from the
   shell. Drawing goes to an offscreen EGL surface on the handheld's own
   Mali driver, so this tests bionic, fastmem, the recompiled code and the
   renderer before any app exists.
2. **One-screen APK (done).** A NativeActivity through the
   `android-activity` crate (`frontend/src/android.rs`), an `Output::Window`
   next to `Output::Kms` and `Output::Fbdev` in
   `engine/gpu/src/raster/gles.rs`, gamepad and touch from Android events
   into the same pad evdev fills, AAudio in `frontend/src/sound.rs`. The
   existing one-panel layouts work as they are.
3. **Two screens (this branch).** A `Presentation` on the second display
   (`scripts/android/java/.../SecondScreen.java`) hands its window to the
   GPU thread, which draws one screen there in a pass of its own; the top screen fills the top panel
   (400x240 at 1.6 times, 640x384) and the bottom screen the bottom panel at
   exactly twice its size, with touch going straight to the 3DS.
4. **Polish.** The build makes APKs, the menu opens with L3+R2 (a short
   press of the RG button belongs to the system), power through GammaOS's
   profiles.

## Phase 2: the app

The build makes an app for each game, packaged with the SDK's build tools
(no Gradle): `libpocket_native.so`, the frontend as a shared library whose
`android_main` the NativeActivity calls, and the game's code as
`libtitle.so` beside it. Each game's app has its own package,
`io.github.pocketnative.t<title id>`, so several can be installed at once.
The app takes its name from the game's profile, or where there is none, the
title the game gives itself, and its launcher icon from the game's own
48x48 icon (`tools/game_info.py` reads both from the ROM's SMDH).

```bash
scripts/android/build.sh path/to/game.cia --install   # builds it and puts it on the handheld
scripts/android/install.sh                            # installs again; lists the games when there are several
scripts/android/install.sh "alpha sapphire"           # a game by any part of its name
scripts/android/install.sh "alpha sapphire" --log     # pulls its log
```

`install.sh --log` also brings back where the game ran code the recompiler
missed, which the engine notes in `libtitle.hints` beside the game; the next
`build.sh` of that game recompiles it ("of the places Zakuro interpreted, N
become ways into functions and M start new ones"). A game busy on its CPU
gets faster from one round of play, --log and build.

Switches a shell would set go in the app's `data/env.txt`, one `NAME=value`
a line, read when the app starts, and the log says which it took, for
example `ZAKURO_GLES_TIMES=1` and `ZAKURO_DRAW_STATS=1` for where the GPU's
time goes, or `ZAKURO_HOST_PROFILE=1800,600` for where the CPU's goes.

A game is named by any part of its name, in any case, accents or not
("pokemon x" finds Pokémon X), or by its title id; with none named,
`install.sh` takes the only game built, or lists them to pick one by number.

The game goes on the SD card, in the app's own folder there,
`/storage/<card>/Android/data/<package>/files/game.3ds`, which the app reads
with no storage permission; with no card in, or with `install.sh --internal`,
it goes in the app's files folder on the built-in storage instead. That
folder, `/sdcard/Android/data/<package>/files`, holds the system font and the
profiles, with `data/` (saves, the log, the shaders it has built) and
`config/` next to them, all small. Uninstalling the app deletes both
folders, saves included. The app's libraries stay inside its APK, stored
uncompressed so that Android loads them from there rather than unpacking a
second copy.

The apps are signed with a key the first build makes, kept in
`build/.work-android/pocket-native.keystore`. Keep it: an app signed with
another key will not install over the old one.

How it behaves for now:

- the top screen fills the top display and the bottom screen the bottom
  display, at twice its size, with its touches going to the 3DS's; the
  screens keep their shape, with bars where they do not fill a display,
  unless Display, Fill the display stretches the top one, the bottom one or
  both, on whichever display each is; Swap
  screens in the menu (or a tap of L2) trades them. A file named
  `one-screen` in the files folder keeps both on the top display, in the
  menu's layouts, where touching the bottom screen touches the 3DS's
- the bottom display's window never takes the buttons from the game, so a
  touch there does not cost the pad its focus
- with the game's app on GammaOS's DualStack list (Settings, the per-app
  Dual-Stack toggles), GammaOS gives it one window over both panels,
  640x960, which it splits between them itself: the app sees the tall
  window, opens no second window of its own, and puts a screen on each
  half, one surface and one swap a frame rather than two, the menu on the
  top panel. Swap screens and Fill the display work as they do otherwise
- the menu opens with L3 and R2 together; FN is the RG button, which the
  system may keep for itself
- A, B, X and Y are what Android calls them; if they come out swapped, the
  menu's Controls has Nintendo and Xbox layouts
- leaving the app (home, back, the RG button's frontend) saves the settings
  and ends the game, as quitting from the menu does; save in the game first

## Phase 1: running a probe

What changed for Android so far:

- fastmem (`engine/core/src/memory/`) builds on Android too. It was behind
  `cfg(target_os = "linux")`, which is false on Android, and would have
  quietly turned itself off.
- the host profiler (`frontend/src/profiler.rs`) names `ITIMER_PROF`
  itself, which the libc crate leaves out on Android.
- `scripts/android/`: a build container with the NDK, the build, and a
  probe over adb. CI cross-builds `pocket-native` for
  `aarch64-linux-android` on every push.

On a Linux PC with podman or Docker, and adb (Android platform-tools):

```bash
scripts/android/build.sh path/to/game.3ds
scripts/android/probe.sh                         # 3600 frames, the game picked as install.sh does
scripts/android/probe.sh "alpha sapphire" 600     # a quicker look
```

The first build makes the container, about 1 GB more than the ArkOS one
because of the NDK. The build prints the title id. The probe pushes
`build/Pocket Native Android/pocket-native` to `/data/local/tmp/pocket-native`
on the handheld (later pushes skip what has not changed), runs the game
there and pulls the report into `build/probe-<title id>-android`, with
`probe-log.txt`, the screenshots and `device.txt` saying what drew them.

Turn on USB debugging in GammaOS first: Settings, About, tap the build
number seven times, then Developer options, USB debugging.

What to look for in the report:

- `probe-log.txt` should log `Pocket Native` and the title, then a
  `speed` line every 300 frames. A load error naming `libEGL` or
  `OpenGL ES 3.1` is the driver; a crash at once with no `speed` line is
  most likely the title's library or fastmem.
- the screenshots show whether the renderer draws right on the Mali-G52.
- the speeds are as fast as it goes with nothing waiting for the panel, so
  above 100% means there is room left.

## First results

Pokémon Alpha Sapphire on an RG DS under GammaOS Next 1.4 (Android 14,
Mali-G52 r25p0): the recompiled code loads and the game boots and draws with
no errors. The first runs of a scene go at 18 to 84% while the Mali driver
compiles its shaders; once the shader manifest has them, the title screen and
intro run at 200 to 300% of the console's speed.
