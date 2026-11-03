# Game compatibility

What's known about each game so far. It's early days, so most entries
come from a short run on a PC or from one tester's report, and a game
that isn't listed hasn't been tried by anyone yet.

Tried a game? Please [open an issue](https://github.com/whovian64/pocket-native/issues)
whether it works or not, with your handheld, its system, the game's
region and the Pocket Native version from the top of the menu.

## How to read it

| Status | Means |
|---|---|
| **Plays well** | Plays like on the console, at or close to full speed |
| **Playable** | You can play it, with slowdowns or small glitches |
| **Slow** | Gets into the game, but too slow to enjoy yet |
| **Starts** | Gets through its intro or menus, the game itself not tried yet |
| **Broken** | Crashes or black screens before the game |

Where each was tried:

- **R36S**: played or measured on an R36S with ArkOS.
- **R36S sim**: speed measured on a PC held to the R36S's CPU and GPU.
- **PC**: the current version ran the game for about a minute and a
  half of game time on a PC, pressing buttons, with no errors. That
  says the game boots and draws right, not how fast a handheld runs it.
- **Reported**: a tester's report, with their handheld and version.
  Many of these are from 0.2 and earlier, and plenty has been fixed
  since.

The first start of every game is slow while it builds its shaders. That
isn't counted against any game here.

Several games below crashed in the Mali driver's own shader compiler on
an RX6H. From 0.5.0 such a crash names the shader in the game's
`log.txt`, and the next start leaves it out and gets past it.

## Games

| Game | Status | Tried on | Notes |
|---|---|---|---|
| Andro Dunos II | Starts | Reported: H700 (muOS), 0.4.6 | Full speed for the half minute its log covers, European version |
| Animal Crossing: New Leaf | Playable | R36S, PC, reported: RG DS (Android), 0.4.6 | Tuned on the US version. A steady 30 fps on the RG DS, where its own bottom screen used to show the game's bottom screen at half the rate, fixed now. The first visit to town takes a few minutes building shaders. One report of things no longer drawing after a long session on a Mali-G31 handheld is being looked into |
| Azure Striker Gunvolt: Striker Pack | Playable | Reported: RX6H, 0.2 | Frame skipping in the game select menu, fine in game |
| Ben 10: Omniverse 2 | Slow | Reported: RG DS (Android), 0.4.6 | Doesn't run well, textures flicker all the time |
| Bravely Default | Starts | Reported: RX6H, 0.2 | Sometimes crashed at the title on 0.2.0, got past the opening on a later version |
| Donkey Kong Country Returns 3D | Starts | Reported: RX6H, 0.4.6, RG CubeXX, 0.4.7 | Keeps up full speed by skipping pictures, about 20 a second on the RG CubeXX. Crashed once in the Mali driver's shader compiler on the RX6H |
| Dragon Ball Z: Extreme Butoden | Playable | Reported: H700 (muOS), 0.4.7 | 95% speed on average in a tester's log |
| Fire Emblem: Awakening | Starts | PC | Reaches the avatar maker. If it stutters, try **Frame skip** off in the advanced settings |
| Fire Emblem Fates | Slow | Reported: Logitech G Cloud (Android), 0.4.0, 0.4.6 | US Special Edition. Plays without crashing, battles and cutscenes at 10 to 15 fps. From chapter 2 most character sprites on the map show as grey boxes, still on 0.4.6. Didn't start with **Most compatible** on |
| Hyrule Warriors Legends | Starts | PC | The opening plays, but it keeps the 3DS's CPU three quarters busy, so expect it to be slow on a handheld |
| Kid Icarus: Uprising | Starts | PC | Plays into the first chapter. Text in menus and dialogue showed as bars up to 0.4.7, fixed in 0.5.0, which also draws its effects made with the geometry shader's fixed primitive mode, left out before. Crashed a few seconds in on 0.4.5 |
| Kingdom Hearts 3D: Dream Drop Distance | Slow | PC, R36S sim, reported: H700 (muOS), 0.4.7 | The opening plays into Destiny Islands. About 60% speed in the R36S sim, three quarters on average on an H700, dropping to a fifth in places |
| Kirby Battle Royale | Playable | PC, R36S sim | Full speed at 30 fps in the R36S sim, not played on a handheld yet |
| Kirby: Planet Robobot | Starts | PC | Gets into the first levels. Since 0.4.3 it no longer asks you to accept the Service User Agreement |
| Kirby: Triple Deluxe | Starts | PC | Gets into the first stage. Since 0.4.3 it no longer asks you to accept the Service User Agreement |
| Kirby's Extra Epic Yarn | Slow | Reported: R36S clone, 0.4.7 | Cutscenes up to 30 fps, levels around 12. Some backgrounds glitch or go black while the camera moves. Since 0.4.7 a transformation no longer drops it to 1 fps |
| The Legend of Zelda: A Link Between Worlds | Playable | PC, reported: RX6H, 0.2, RG DS (Android), 0.4.3, H700 (muOS), 0.4.7 | About 40 fps on the RG DS, with glitches during transitions. Good speed on an H700 with **Frame skip** at 2, where grass popped in and out: most likely the culling bug 0.5.0 fixes for Super Mario 3D Land. Some glitches in effects reported. Some R36S players find 0.4.3 slower than 0.4.0 here: if it is for you, try **Compressed alpha textures** off in the advanced settings |
| The Legend of Zelda: Ocarina of Time 3D | Playable | PC, R36S sim | Full speed in Link's house in the R36S sim, about half in the Kokiri Forest fly-in. Stutter reported on 0.2 |
| Luigi's Mansion | Starts | PC, R36S sim | The opening plays into the mansion. About 20 fps in the R36S sim, with dips. Both screens were black on 0.1 |
| Mario & Luigi: Dream Team | Broken | Reported: 0.2 | Black screen |
| Mario Kart 7 | Playable | R36S, PC | Tuned on the US version. Races run well once a track's shaders are built, the first race on a track stutters. Some testers report it slower than that. Since 0.4.3 a first start goes straight to the menus, where it used to stop at "There are currently no Mii characters" |
| Metroid Prime: Federation Force | Slow | PC, R36S sim | The opening plays into the training. About 6 fps in the R36S sim, its GPU flat out |
| Metroid: Samus Returns | Starts | R36S, PC | The intro plays into the game. The black frames between its intro pictures are the game's own fades. About 60% of full speed on the R36S on 0.4.3, against about a third on 0.4.0 |
| My Hero Academia: Battle for All | Starts | PC, R36S sim | Japanese version. The opening plays into the first tutorial. About 15 fps in the R36S sim |
| Monster Hunter 4 Ultimate | Slow | PC, reported: RX6H, 0.2 | Starts into Character Creation without errors on the PC. 6 fps on the defaults on 0.2. Its profile now turns on **Real-time GPU**, which a tester found faster. 30 to 98% speed on an RX6H on 0.4.6, which crashed in the Mali driver's shader compiler |
| New Super Mario Bros. 2 | Playable | PC, reported: 0.2.5, RX6H, 0.4.6 | Close to fully playable on 0.2.5 (European version). The Special Edition crashed on 0.4.0 when turning SpotPass off, fixed in 0.4.3. Close to full speed on an RX6H on 0.4.6, which crashed in the Mali driver's shader compiler after a few minutes |
| Nintendogs + Cats | Starts | Reported: RG DS (Android), 0.4.3 | The intro runs at about 2 fps |
| One Piece: Romance Dawn | Playable | Reported: RG DS (Android), 0.4.3 | Graphical glitches in some areas |
| One Piece: Unlimited Cruise SP | Playable | Reported: RG DS (Android), 0.4.3 | Characters flicker now and then. 30 to 50 fps depending on the area, sometimes under 20 |
| Paper Mario: Sticker Star | Starts | PC | The opening plays, but it keeps the 3DS's CPU all but fully busy, so expect it to be slow on a handheld |
| Pokémon Omega Ruby and Alpha Sapphire | Plays well | R36S, PC, reported: RG40XXV, RX6H, RG DS (Android), RG DS (ROCKNIX), 0.4.9 | Close to 30 fps in towns, routes and battles on the R36S. On the RG DS, 0.4.7 stutters less than 0.4.6 and runs cooler, and Alpha Sapphire averages 98% speed under ROCKNIX. Options for the outlines, battle shading and the battle menu under **This game** |
| Pokémon Sun and Moon | Playable | R36S, R36S sim, PC, reported: R36S, 0.4.7 | Moon: 25 to 29 fps on Route 1 and in Iki Town in the R36S sim. The first start takes a long time. Keep **Generic lighting** off, as it is by default: on, Moon runs at a fifth of its speed on the R36S. Sun gets through its intro to the name entry on the PC and should match Moon, not played yet |
| Pokémon Ultra Sun and Ultra Moon | Starts | PC | Gets through the intro to the name entry. The same engine as Sun and Moon, not played yet |
| Pokémon X and Y | Playable | R36S, PC, reported: RG DS (Android) | 20 to 30 fps in towns on the R36S. The intro runs heavy, it settles once you're in the game |
| Project X Zone | Playable | Reported: H700 (muOS), 0.4.7 | Close to full speed with **Frame skip** at 3. Text in the intro and dialogue glitched up to 0.4.7, fixed in 0.5.0 |
| Project X Zone 2 | Playable | Reported: H700 (muOS), 0.4.7 | Close to the console, with dips in demanding scenes that **Frame skip** at 2 smooths out |
| Rayman Origins | Playable | Reported: R36S clone, 0.2, RG DS (Android), 0.4.6 | Plays well. The intro stutters, and the picture holds for a second when changing characters or skins |
| Resident Evil: Revelations | Starts | PC, reported: RX6H, 0.4.6, 0.4.9 | The opening runs at full speed, then the RX6H's Mali driver crashed in its shader compiler within a minute, every time, and at the start on 0.4.9. From 0.5.0 the next start should get past that. On the PC it plays into the first episode |
| Shantae and the Pirate's Curse | Playable | Reported: H700 (muOS), 0.4.7 | Full speed with **Frame skip** on |
| Shin Megami Tensei IV | Playable | Reported: RX6H, 0.2 | Slows down at times |
| Shin Megami Tensei: Strange Journey Redux | Playable | Reported: RX6H, 0.2 | Slows down at times |
| Sonic Generations | Slow | Reported: R36S clone, 0.2 | Very slow |
| Sonic Lost World | Playable | Reported: R36S | A steady frame rate once its shaders are built, tried through Windy Hill's tutorial and first act |
| Star Fox 64 3D | Starts | PC, reported: 0.4.7 | Building it made 22 GB of C on 0.4.7, and it stopped at "Initializing game data". The next version builds it at a normal size and gets to the title and the training demo |
| Super Mario 3D Land | Playable | Reported: several, 0.2, R36S, 0.4.0, 0.4.3, H700 (muOS), 0.4.7 | About full speed on an H700 on 0.4.7. Coins, blocks and enemies flickered when one of a kind was out of view, fixed in 0.5.0. On the R36S, 0.4.3 froze about 20 seconds in when the GPU driver failed to build a shader, fixed in 0.4.5, and some spots maxed out its GPU on 0.4.0. Sometimes crashed at the start before 0.2.5 |
| Super Smash Bros. for Nintendo 3DS | Playable | R36S, PC, reported: RG DS (Android), 0.4.3, R36S clone, RX6H, 0.4.6, H700 (muOS), 0.4.7 | Menus are fine. 25 to 30 fps in matches on an R36S clone on 0.4.6, about three quarters speed on our R36S on a busy stage. Frequent slowdowns on the RG DS on 0.4.3. Crashed once in the Mali driver's shader compiler on an RX6H. 96% speed on average on an H700 on 0.4.7, but crashed about three minutes in: 0.5.0 answers the system calls Smash got nothing back from (the console's country, its save's secure value), likely the cause |
| Xenoblade Chronicles 3D | Starts | PC | A New 3DS title. Gets into the opening battle. Its top screen was tinted red and its bottom screen wrong up to 0.4.7, fixed in 0.5.0 |
| Yoshi's New Island | Playable | Reported: 0.2 | Hold L2 and use the right stick for the parts you tilt the console in |

## Games with a profile but no report

These have a profile in `titles/` with their name, and some with
settings from other emulators' experience, but nobody has reported on
them yet: Luigi's Mansion 2, Mario &
Luigi: Paper Jam, Mario & Luigi: Superstar Saga + Bowser's Minions,
Mario & Luigi: Bowser's Inside Story + Bowser Jr.'s Journey, Minecraft:
New Nintendo 3DS Edition.
