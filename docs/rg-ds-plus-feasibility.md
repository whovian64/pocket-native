# RG DS Plus: hardware, deployment and evaluation limits

Research checked **2026-11-04**. Repository baseline inspected:
`178f2b0f4f3444ed8d6af49fe5c130fe3094b598`.

This is a preparation and source-review result. No RG DS Plus was measured,
flashed or tested for this report. Ultra Sun performance on that hardware and
the requested 2× speed / 0.5× resource targets remain unproven.

## Establish the exact device

| Property | RG DS Plus | Original RG DS |
|---|---|---|
| CPU | RK3568, four 64-bit Cortex-A55 cores, up to 2.0 GHz | Same advertised CPU |
| GPU | Arm Mali-G52 2EE | Same advertised GPU |
| RAM | **1 GB** | **3 GB**, with 32 GB storage |
| Panels | Two 4.3-inch, 1024×768 panels; lower panel supports touch/stylus | Two 4-inch, 640×480 touch panels |
| Advertised system | Linux on supplied 16 GB TF card | Android 14 / 64-bit Linux |

Sources: manufacturer [RG DS Plus specification][plus] and [RG DS
specification][original]. These are shipping product listings, not merely an
announcement. Neither specification establishes sustained CPU/GPU clocks,
available application RAM, memory bandwidth or a particular installed driver.

**Implication:** the Plus is not a faster CPU upgrade. It has one third of the
original's RAM and 2.56× as many panel pixels. Pixel ratio is arithmetic, not a
prediction of game slowdown: game render resolution and final presentation
resolution are different workloads. Preserve separate profiles for the two
devices.

## Deployment route matrix

| Route | What the primary source establishes | Pocket Native fit and unresolved work |
|---|---|---|
| **ROCKNIX on microSD — recommended first target** | Official 20261101 release explicitly lists both RG DS and RG DS Plus against `ROCKNIX-RK3566.aarch64-20261101-Specific.img.gz`.[rocknix] The image family name does not change the Plus's advertised RK3568 silicon. | Existing Linux/aarch64 build and KMS/GBM dual-panel path. Repository README reports original RG DS play under ROCKNIX; it does **not** establish Plus or Ultra Sun performance. Verify Plus display connectors, touch paths, pad mapping and driver. |
| Stock Linux | Manufacturer supplies Linux on the Plus.[plus] | README says original RG DS stock Linux had one screen and missing buttons. Do not transfer that outcome to the Plus; classify Plus integration as unknown until tested. |
| **GammaOS Nano Core for RG DS Plus** | Device-specific v1.4.4 release exists. Core only, no Google apps; Android can be opened from its menu. Release is password gated for supporters at this check. Maintainer warns that improvements concern NDS, not 3DS.[gamma-release] | Existing Android 11+/arm64 APK and dual-display / GammaOS DualStack paths make this plausible. Verify APK launch, native title loading, both panels, touch, audio and memory pressure on the actual Plus. This is not a measured performance advantage over ROCKNIX. |
| Other Linux distributions | Not established by this bounded audit. | Treat boot, display, driver and controller support as unknown. A kernel/SoC match alone does not prove this board works. |

GammaOS's Plus installation guide says it runs from microSD and requires its
Rockchip SD Tool / documented alternative, rather than a generic image writer.
It recommends an A2 card of at least 32 GB. The guide's Plus section still names
v1.4.3 while the release page offers v1.4.4: select the exact Plus release and
follow that release's instructions, not the original RG DS internal-flash
procedure.[gamma-install] No image was downloaded or written here.

The v1.4.4 maintainer notes describe a power-state fix after use of mainline
Linux, but explicitly say the fix was confirmed on original RG DS and not
separately reproduced on Plus.[gamma-release] Record the OS transition when
comparing routes; boot behavior is not a game-speed result.

## Existing repository integration

These are inspected implementation paths, not tests run on the handheld:

* [`scripts/build.sh`](../scripts/build.sh) and
  [`scripts/container/inside.sh`](../scripts/container/inside.sh) build the Linux
  runtime for `aarch64-unknown-linux-gnu`, generate the game `title.so` with
  `RECOMP_FASTMEM`, and package a launcher for `roms/ports`.
* [`scripts/device/run.sh`](../scripts/device/run.sh) enables KMS, stops
  EmulationStation, switches away from sway's VT when present, and restores
  launcher/power state on exit. Sway remains a resident process; record its
  memory when evaluating whole-system cost.
* [`Kms::open` in display.rs](../engine/gpu/src/raster/gles/display.rs) opens
  `/dev/dri/card0`, dynamically loads libdrm/libgbm, and stacks two equal-size
  panels. It defaults the top panel to `DSI-2`, supports `ZAKURO_KMS_TOP` and
  `ZAKURO_KMS_SINGLE`, and uses preferred panel dimensions. Actual connector
  identities on the Plus are unverified.
* [`scripts/android/build.sh`](../scripts/android/build.sh) builds the APK;
  `--test` makes a separate test package. Its
  [`inside.sh`](../scripts/android/inside.sh) builds
  `aarch64-linux-android`. This is a different runtime artifact from Linux.
  Use the game-import instructions in [`START_HERE.md`](../START_HERE.md).
* [`android_main`](../android/src/lib.rs) recognizes a stacked GammaOS window
  with `height * 2 == width * 3`. A hypothetical 1024×1536 Plus window matches
  that condition; this arithmetic is not proof GammaOS supplies that window.
* The renderer is OpenGL ES 3.1. Current Mesa documentation lists Mali-G52
  GLES 3.1 support.[panfrost] README reports use of Mali's own driver on original
  RG DS ROCKNIX. Capture the actual EGL/GL vendor, renderer, versions and
  library paths before selecting optimizations; do not assume Panfrost is
  active merely because the kernel is mainline.

## What can be established without the device

| Method | Useful evidence | Limits |
|---|---|---|
| Native host build, fixed input, real GLES, same converted library | Regressions, sampled output, CPU time, process RSS | Desktop GPU/CPU costs do not predict Plus FPS or battery use |
| Four distinct host cores, CPU quota, cgroup memory ceiling and swap disabled | Completion and failures under an explicit pressure envelope | A quota does not emulate Cortex-A55; the ceiling is not measured free handheld RAM |
| AArch64 Linux/Android cross-build | Target ABI and build compatibility | No execution, graphics, touch or panel proof |
| QEMU user-mode selected-function checks | Bounded Arm functional behavior | No A55 timing, caches, power, display or Mali execution |
| Cortex-A55 LLVM-MCA scheduling model | Selected instruction scheduling estimates | No game speed or memory-system calibration |
| Panfrost DRM shim with `PAN_GPU_ID=7212` | Real Mali-G52 shader compilation/ISA inspection | Stubbed kernel interface; no GPU execution, output pixels or FPS |

One GB is total system RAM. The OS, two panel buffers and shared GPU allocations
also consume memory. Report process RSS, anonymous/file/shared observations and
cgroup counters separately. Sparse fast-memory mappings need a large virtual
address range; an address-space limit is not a resident-memory limit.

The reusable [evaluation guide](performance-eval.md) describes fixed-work host
runs and [pressure.sh](../tools/perf/pressure.sh) supplies a temporary cgroup.
For example, run a prepared evaluation command with:

```sh
bash tools/perf/pressure.sh --memory-mib 768 --cpu-quota 400 \
  --cpus 0,1,2,3 --timeout 900 --report-dir build/perf/pressure-counters \
  -- python3 tools/perf/evaluate.py run ...
```

Choose four available **distinct physical cores** on your host. The 768 MiB
ceiling is an arbitrary stress setting. Preserve the exact command, all trials,
actual driver identity, cgroup counters and failures. Compare timings only under
the same envelope. Missing counters remain unknown; OOM is not a fast run.

### Shader compiler checks

[tools/perf/mali_compiler_check.py](../tools/perf/mali_compiler_check.py) and its
C harness accept declared shader pairs, preserve source/ISA hashes and compare
real compiler resource statistics. See `--help` and the Mesa [DRM-shim guide][drm-shim]
for the explicitly supplied shim, driver and EGL paths. No driver is installed
by the Python tool. A shim may identify a synthetic MC4 core mask: that is not
the RK3568's topology or a throughput calibration. Keep proprietary Arm Mali,
Panfrost and the host's NVIDIA driver as separate profiles.

### Mesa 26.2.6 finding

Mesa 26.2.6 [fixes a CPU-side wrapper leak][mesa-fix] in Gallium Panfrost's
internal precompiled-kernel cache at screen teardown. The affected implementation
includes Mali-G52/Bifrost v7; the identified cache-population route serves
indirect compute dispatch. The renderer currently uses direct compute dispatch.
An isolated application of the official patch to 26.2.5 source matched the
26.2.6 file exactly. This establishes source applicability, with no demonstrated
game-memory or FPS benefit. It does not alter proprietary NVIDIA or Arm drivers.
No system driver was upgraded and no handheld runtime was available.

## Device validation still required

For each OS, capture its exact release, EGL/GL identity, CPU governor/clocks,
resolution, panel connectors, touch/controller mapping and audio configuration.
Then compare the same game build, save, scene and quality with cold application
caches and declared page-cache policy. Include first visits and revisited scenes,
frame-time distributions, shader rebuilds/fallbacks and long-session memory/I/O.
A working boot or compiler check is insufficient for a battery or gameplay claim.
Keep periodic program retirement disabled until its revisit trade-off is measured.

No hardware was flashed or measured during this work. The source-linked routes
are deployment candidates; a faithful RK3568/Mali-G52 timing simulation and
RG DS Plus performance remain unestablished.

## Sources

Product/OS sources were checked November 4, 2026; Mesa fix was checked November 5.

[mesa-fix]: https://gitlab.freedesktop.org/mesa/mesa/-/commit/7898743c038215e41d3598c411e7bf9a57927756
[plus]: https://anbernic.com/products/rg-ds-plus
[original]: https://anbernic.com/products/rgds
[rocknix]: https://releases.rocknix.org/?release_id=430633367
[gamma-release]: https://github.com/TheGammaSqueeze/GammaOSNext/releases/tag/v1.4.4-ANBERNICRGDSPLUS
[gamma-install]: https://github.com/TheGammaSqueeze/GammaOSNext/wiki/GammaOS-Next-Installation#anbernic-rg-ds-plus
[panfrost]: https://docs.mesa3d.org/drivers/panfrost.html
[drm-shim]: https://docs.mesa3d.org/drivers/panfrost/drm-shim.html
[pan-bench]: https://docs.mesa3d.org/drivers/panfrost/benchmarking.html

* [Manufacturer: RG DS Plus][plus]
* [Manufacturer: original RG DS][original]
* [ROCKNIX 20261101 release and exact device/image mapping][rocknix]
* [GammaOS Nano v1.4.4: Plus release, caveats and assets][gamma-release]
* [GammaOS installation: Plus-specific microSD route][gamma-install]
* [Mesa: Panfrost API/hardware support][panfrost]
* [Mesa: Mali-G52 compiler testing through drm-shim][drm-shim]
* [Mesa: reproducible Panfrost benchmarking][pan-bench]
