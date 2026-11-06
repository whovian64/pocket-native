# Consistent multi-game evaluations

Use this protocol to compare the original runtime with a candidate on the same
saved scene. It separates startup, gameplay, cache effects and asynchronous
scheduling; it does not set a universal speedup target. ROMs, original native
libraries, save seeds, game captures and raw telemetry stay under ignored
`build/` directories. Each title needs its own original/current comparison.

## Prepare the two runtimes

For a runtime-only comparison, both sides must load the **same original native
library** for that title. Recompiling only the candidate game would combine two
independent changes. Record runtime/library/ROM hashes and release build commands,
compiler versions, features and flags. A source revision alone does not bind an
existing executable to those sources.

The original revision predates the observer. Prepare it without importing newer
engine/compiler implementations:

```sh
python3 tools/perf/prepare_baseline.py \
  --revision 178f2b0f4f3444ed8d6af49fe5c130fe3094b598 \
  --output build/original-observer-source
```

This produces a new source archive and `observer-source.json`. Only
`frontend/src/lib.rs` and `frontend/src/probe.rs` receive observer/reporting
changes; engine, compiler and lockfile blobs are checked against the original.
Git archive's attribute-driven Windows batch-file newline conversions are listed
separately. The tool refuses unexpected/duplicate observer anchors and an
existing destination. Do not copy current engine files into the original.

Build both with equivalent release flags, using separate target directories or
forcing/verifying fresh local-crate compilation when sharing a target cache.
For example, run in each source directory:

```sh
cargo build --release --locked --offline -p pocket-native \
  --features zakuro-gpu/gles --target x86_64-unknown-linux-gnu -j 1 \
  --target-dir /absolute/path/to/this-build-target \
  --config 'target.x86_64-unknown-linux-gnu.rustflags=[]'
```

Keep a build receipt with the executable SHA-256, exact source hashes/observer
patch, command, target, toolchain and success status. `observer-source.json`
proves preparation; it does not by itself prove which binary Cargo produced.

## Freeze a campaign

Copy [paired.example.json](../tools/perf/paired.example.json) into an ignored
local directory. Supply your own matching ROMs, original native libraries,
immutable save seeds, runtime/build receipts and CPU topology. The example uses
three existing route scripts but does not distribute the required saves. Its
CPU IDs and temperature limit are examples, not hardware detection or RG DS Plus
simulation. Inspect `thread_siblings_list`, AC state and actual thermal sensors.

```sh
python3 tools/perf/paired.py --plan build/my-eval/plan.json \
  --output build/my-eval/series-01
```

The output must be new. `freeze.json` fixes the plan, identities, random seed and
complete order before measurements. A cooperating file lock prevents overlapping
campaigns using the same lock; it does not reserve the machine against other
software. Preflight measures interval aggregate/core/SMT load, CPU temperature,
AC supply and governor/boost stability, and checks for compiler/game processes.
It requires consecutive quiet windows within a bounded waiting budget.

The campaign performs:

1. One full-work warmup per title/build for each warmed-cache lane. Each seed is
   immutable and cloned into fresh per-trial XDG cache directories. Cache bytes
   belong to their build; they are not shared writable between implementations.
2. At least three A/A processes per title/lane, using the same candidate binary,
   to test exact repeatability and whole-process/window timing spread.
3. At least three paired Original/Current processes per title/lane, alternating
   AB/BA order with a seeded first order. Three pairs support a pilot, not a
   precise small-effect claim. Larger budgets must be chosen before running.

Do not overwrite held campaigns, drop slow trials or retry until favorable.
Freeze any revised policy before a new campaign. Reassessing old data under new
limits is a **retrospective sensitivity analysis**, not a replacement for the
original decision or a prospectively validated gain.
`--diagnostic-on-hold` allows the fixed workload to
continue after a failed quiet/thermal preflight, retaining that failure and
blocking timing admission. A still-active compiler/game remains a hard hold.
Use that flag to investigate behavior, never to obtain accepted benchmark wins.

### Practical desktop limits

These thresholds are evaluation-tool policy introduced in this PR, **not rules
set by the repository owner or universal benchmarking standards**. The revised
example tolerates moderate desktop activity:

| Check | Earlier campaign | Revised example |
|---|---:|---:|
| Aggregate CPU activity before launch | 3% | 10% |
| Estimated unrelated CPU during a run | 10% | 15% |
| Selected-core/SMT activity | 10% | 25% |
| Identical-build A/A timing span / mean | 5% | 10% |
| Preflight temperature change | 3°C | 5°C |

Aggregate CPU fractions use total online host capacity; core fractions use each
watched logical CPU. The core budget covers selected CPUs/siblings before launch
and siblings during execution, not the game's own selected-core utilization.
The A/A budget covers both CPU and wall time for the process and named window.
It is a repeatability screen, not a confidence interval or a guarantee that a
smaller effect is measurable. Keep paired ranges and uncertainty visible.

The temperature ceiling stays host-specific and unchanged: 85°C in the portable
example, 97°C in the earlier local campaign. Unknown telemetry, active competing
builds/games, changing power/governor state, unequal work/output and invalid
receipts still hold a result. The separate 3% during-run diagnostic is retained
for context but is not required by the revised preflight. Explicit finite
acceptance fractions are configurable; the old hard-coded 10% validator ceiling
has been removed. Choose limits before measuring, rather than to obtain a pass.

## Separate the lanes and measurement windows

The supplied [serialized profile](../tools/perf/profiles/common-serialized.json)
uses exact-quality monolithic shaders, synchronous compilation/GPU processing,
no stand-ins/generic shaders/lighting, a system allocator and default cache
policies. The new ROM cache stays off. Resolution is native 1× and frame skipping
is zero. Existing title-specific profiles/options remain part of the frozen
input. This provides equal-work evidence, not a shipped-default performance
claim.

The [async profile](../tools/perf/profiles/common-async.json) enables GPU and
background-shader threads with the same exact-quality defaults. It exercises
normal asynchronous scheduling separately. The runner deliberately classifies
it as diagnostic; guest-state/work divergence or a queue change cannot be
credited as less CPU work. Probe runs are uncapped and suppress device audio;
they do not measure paced interactive FPS, audio continuity or KMS display
latency. Those still require a separate interactive/device test.

Whole-process CPU/wall/RSS measurements include startup, loading, compilation,
screenshots, reporting and exit. `--batch-telemetry --measure-start 1800`
additionally selects the zero-based batch interval `[1800, frames)` without
skipping its prefix. `batch-timing.csv` is buffered in memory and written only
after the workload. It contains full frontend-batch wall/process-CPU time,
nested `run_frame` time, per-batch host GPU-queue wait deltas, and guest work counters.
The wait observer explicitly accounts for the status logger's periodic counter
reset; its regression test binds that reset contract, including filtered logging.
No per-batch picture getter or new GPU wait is introduced by this observer.
The measurement protocol enforces `RUST_LOG=info`; arbitrary per-target logging
filters are outside the supported GPU-wait accounting protocol.

`batch_metrics.py` rejects missing/duplicate/backward/noninteger/incomplete
records and reports median/p95/p99 batch durations, window totals and boundary
work. **A batch is not a unique game picture or a physical display refresh.**
The nested intervals overlap; do not add `run_frame` and full-batch costs.
GPU wait covers the existing host queue-wait counter, not GPU execution
duration or every driver/presentation stall. Other waits remain in full-batch
wall time. Process CPU includes
all process threads during the sampled interval.

`--capture boundaries` retains the eight existing screenshots while omitting the
legacy every-ten-batch black-flash reads. Those legacy reads can synchronize GPU
work and perturb asynchronous execution. It is a different observer protocol;
regenerate both sides rather than comparing it to old legacy-mode timings.
Use `--trace-every 300` in separate correctness runs; the private original
observer now supports the same read-only trace format as the candidate.

The route windows cover short walks and long idle periods after loading.
Battle-entry, battle and transition workloads require independently captured
state/input and visual review. The tooling supports new named fixtures, but
these route results do not certify those scenes.

## Cache, host and admission records

- `--seed-cache DIR` clones an immutable application-cache seed. The paired
  analyzer checks it against that build's warmup inventory and explicitly
  normalizes only this per-build cache inventory for cross-build comparison.
  Raw receipts remain unchanged. A warmed cache does not remove process startup.
- `--driver-cache off` explicitly disables NVIDIA/Mesa disk shader caches while
  retaining the application's own program-binary cache. `isolated` gives each
  trial private driver-cache paths; `uncontrolled` retains legacy behavior.
  Source/file hashing warms OS pages. None of these modes is a disk-cold test.
- One-second host observations retain CPU/core activity, sensor names,
  governor/boost, instantaneous frequency reports, pressure and AC state. When
  available, a separate `nvidia-smi` process records GPU state/clocks/power.
  These are not effective CPU-clock residency or integrated energy measurements.
  Monitoring itself has a cost; its CPU appears as external activity.
- Affinity limits migration but does not isolate SMT siblings, shared caches,
  power budgets or desktop GPU work. No global ASLR/SMT/boost changes, realtime
  priority or user-process termination is performed.
- Each trial retains process peak RSS plus one simultaneous sampled
  RssAnon/RssFile/RssShmem snapshot, page faults and block I/O. These exclude GPU
  and total system memory; do not sum independent component maxima.

`analysis.json` keeps all process pairs, ranges, A/A spreads, source references
and failed gates. A zero campaign exit code means all planned processes
completed with unchanged inputs; it does not mean timings were admitted. Even
a complete diagnostic campaign can have every series held. Five or more pairs additionally receive a deterministic,
exploratory bootstrap interval resampling **process pairs**, not correlated
frames. A narrow interval cannot repair unequal work, quality or host conditions.
The declared external-host budget (15% in the revised example) and stricter 3%
diagnostic measure fractions of total online CPU capacity, not percentage timing
error. `admitted` requires complete
budget, unchanged inputs, exact captured work/output/warnings/renderer,
validated observer data, matching window work and eligible A/A/host observations.
A held result remains a diagnostic observation.

## Source precedents

These mechanisms informed the protocol; they are not evidence of Pocket gains:

- Azahar: [separate speed/frame counters](https://github.com/azahar-emu/azahar/blob/1dfc9ed1b31a937fcd468c8fd5dc8344088b2eaf/src/core/perf_stats.cpp#L25-L180),
  [movie clock/revision](https://github.com/azahar-emu/azahar/blob/1dfc9ed1b31a937fcd468c8fd5dc8344088b2eaf/src/core/movie.cpp#L597-L620).
- Dolphin: [performance metrics](https://github.com/dolphin-emu/dolphin/blob/4a70679d0adcd17220568f5c18d2d4005eeea0a2/Source/Core/VideoCommon/PerformanceMetrics.cpp#L41-L112),
  [FIFO renderer replay](https://github.com/dolphin-emu/dolphin/blob/4a70679d0adcd17220568f5c18d2d4005eeea0a2/Source/Core/Core/FifoPlayer/FifoPlayer.cpp#L212-L306).
- PPSSPP: [input/filesystem replay](https://github.com/hrydgard/ppsspp/blob/7a88c1521ef1a6589a5bdf6898750b9e35929677/Core/Replay.h#L24-L91),
  [repeated headless benchmark](https://github.com/hrydgard/ppsspp/blob/7a88c1521ef1a6589a5bdf6898750b9e35929677/headless/Headless.cpp#L866-L908).
- PCSX2: [bounded GS replay](https://github.com/PCSX2/pcsx2/blob/993f6fc3b18e3feb0151ea1fa16b8a0783fe6fca/pcsx2/GSDumpReplayer.cpp#L265-L405).
- Driver disk caches: [NVIDIA OpenGL environment variables](https://download.nvidia.com/XFree86/Linux-x86_64/550.144.03/README/openglenvvariables.html)
  and [Mesa environment variables](https://docs.mesa3d.org/envvars.html). These
  control disk caches, not every internal/Android blob cache.
- Google Benchmark: [noise sources, including boost with performance governor](https://github.com/google/benchmark/blob/main/docs/reducing_variance.md),
  [interleaved repetitions](https://github.com/google/benchmark/blob/main/docs/random_interleaving.md).
- LLVM: [low noise is necessary but does not eliminate bias](https://llvm.org/docs/Benchmarking.html).

No maintained official standardized Pokémon benchmark suite was found in the
searched official sources. Use named, reviewable scenes rather than treating
uncontrolled videos or a menu-FPS overlay as reference scores.
