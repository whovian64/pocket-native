# Credits

## Zakuro and 3dsrecomp

Pocket Native is a fork of [Zakuro](https://github.com/fearkov/zakuro), the
3DS emulator, and [3dsrecomp](https://github.com/fearkov/3dsrecomp), its
ahead-of-time recompiler, both by **fearkov** under the MIT license. The
emulator in `engine/` and the recompiler in `recompiler/` are their work.
This fork changed and added to them, but the CPU, the OS services, the file
systems, the PICA200 emulation and the recompiler itself all come from
upstream.

This fork started from the repository that put the two together,
[WINDROID-EMU/3dsrecomp](https://github.com/WINDROID-EMU/3dsrecomp), by
**genrogamer**.

## Android

The Android port started from **gopnikcosmoslav2077**'s work in
[#8](https://github.com/whovian64/pocket-native/pull/8): the renderer drawing
into an Android window and onto a second display, touch, AAudio sound and the
first APK build.

## Contributors

Code from these people is in Pocket Native, through their commits and
pull requests:

- **gopnikcosmoslav2077**: the Android port ([#8](https://github.com/whovian64/pocket-native/pull/8)),
  the profiler, hint and shader diagnostics ([#7](https://github.com/whovian64/pocket-native/pull/7)),
  and faster shader building on first runs ([#42](https://github.com/whovian64/pocket-native/pull/42))
- **AeolusUX**: the RG52 Mini's stick and screen rotation, the RK3566
  right stick and profile names ([#31](https://github.com/whovian64/pocket-native/pull/31)),
  the battery on Knulli, the Pokémon profiles ([#9](https://github.com/whovian64/pocket-native/pull/9))
  and README fixes ([#13](https://github.com/whovian64/pocket-native/pull/13))
- **iArti**: the RG DS under ROCKNIX, both panels, both touch screens and
  its pad ([#17](https://github.com/whovian64/pocket-native/pull/17)),
  every toggle shown in the advanced settings ([#16](https://github.com/whovian64/pocket-native/pull/16))
  and the tables buffer bound from the start ([#28](https://github.com/whovian64/pocket-native/pull/28))
- **Sekyleky**: the New 3DS system modes' memory ([#33](https://github.com/whovian64/pocket-native/pull/33))
  and Stretch filling both displays on two-screen handhelds ([#32](https://github.com/whovian64/pocket-native/pull/32))
- **kxn203**: game profiles ([#3](https://github.com/whovian64/pocket-native/pull/3))
  and the script that rebuilds every game built before ([#34](https://github.com/whovian64/pocket-native/pull/34))
- **Kvn0l**: the Monster Hunter 4 Ultimate profile
- **peuic**: the Miyoo Flip under spruceOS ([#27](https://github.com/whovian64/pocket-native/pull/27))
- **sabogalc**: Z3DS-compressed `.zcci` games ([#29](https://github.com/whovian64/pocket-native/pull/29))
- **trbocode**: the muOS controls fix ([#5](https://github.com/whovian64/pocket-native/pull/5))
- **kadeshar**: the DSP filter fix that keeps PCM sound from going silent ([#37](https://github.com/whovian64/pocket-native/pull/37))
- **ecer96**: culling that leaves a mesh drawn more than once alone,
  which made Super Mario 3D Land's coins and blocks flicker

## Menu font

The menu uses [M PLUS Rounded 1c](https://fonts.google.com/specimen/M+PLUS+Rounded+1c),
Copyright 2016 The Rounded M+ Project Authors, under the SIL Open Font
License 1.1 ([assets/fonts/OFL.txt](assets/fonts/OFL.txt)). It is baked
into `assets/fonts/menu.font` by `tools/bake_font.py`.

## System font

Games that use the 3DS's shared system font get the open-source replacement
made by [citra_system_archives](https://github.com/B3n30/citra_system_archives)
by **B3n30** (GPL-2.0), as kept in [Azahar](https://github.com/azahar-emu/azahar).
It is drawn from [Montserrat](https://github.com/JulietaUla/Montserrat) (SIL
Open Font License 1.1) and [GNU Unifont](https://unifoundry.com/unifont/)
(GPL-2.0-or-later with the GNU font embedding exception). The build
downloads it; it is not part of this repository.

## Rust crates

The program is built from about seventy crates from crates.io, nearly all
under MIT, Apache-2.0 or similar permissive licenses. The AAC decoder,
[Symphonia](https://github.com/pdeljanov/Symphonia), is under MPL-2.0 and
used unmodified. Every build writes the full list with each crate's license
text to `THIRD_PARTY_LICENSES.txt` (`tools/third_party.py`).

## Compatibility notes

Which games need the right eye's picture drawn, and which need the PICA's
own multiplication, come from the compatibility lists of
[Azahar](https://github.com/azahar-emu/azahar). Their profiles in `titles/`
turn the matching switch off.
