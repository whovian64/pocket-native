# Reproducible performance evaluation

The evaluation measures a specific converted game and fixed frame workload on
Linux, preserving captured work and sampled screen output. Report CPU cost,
elapsed time and memory independently; no universal speedup target is required.
RSS excludes GPU allocations, the operating system, battery use and storage.
For paired A/A controls, warmed caches, named gameplay windows and normal async
scheduling, use [the consistent multi-game protocol](consistent-evals.md).

Use your own local ROM. Keep ROMs, converted libraries, save data, screenshots
and trial output outside Git. `build/perf/` is already ignored. The harness
requires Python 3.11+ and Linux; it needs no Python packages or GNU `time`.

## Build and identify the workload

Build the game with the existing conversion workflow in
[games.md](games.md). Preserve a baseline executable and matching `title.so`
before changing the runtime or converter. Release builds with identical build
flags make useful comparisons; debug versus release does not measure an
optimization. Record build commands, revision, Rust/compiler versions, target,
flags and relevant build logs in a local build metadata file.

The normal handheld kit produces **aarch64 Linux** artifacts. A desktop eval
needs both the runtime and converted game library built for the desktop ABI;
an aarch64 `title.so` cannot be loaded by an x86_64 runtime. For an already
decrypted `.3ds` on a Linux development host:

```sh
cargo build --release --locked -p pocket-native -p recomp3ds -j 2
CC=cc RECOMP_CFLAGS=-DRECOMP_FASTMEM \
  target/release/3dsrecomp build /absolute/path/to/game.3ds build/perf/recomp-host
```

The converter names the resulting library `<TITLE_ID>.so`. It may generate
hundreds of megabytes of C and take substantial time to compile. Its compiler
parallelism follows available CPUs; use an explicit CPU affinity if necessary
to bound concurrency. CIA preprocessing uses the existing `tools/cia2cci.py`
workflow. A CIA may require preprocessing before the runtime can load it. Preserve the
original input hash separately from the converted cartridge image hash.

Use an absolute ROM path as the evaluation parameter. The harness streams the
ROM through SHA-256 before trials and verifies it again afterward. It records
executable, converted-library and harness hashes, source checkout provenance,
input script, clock, settings/profile seeds, host identity and trial environment.
Source revision alone does not prove which source produced an existing binary;
`--build-metadata` supplies a separately retained build receipt. System driver
and transitive shared-library binaries are not automatically hashed.
The harness preserves declared renderer environment settings, including
`EGL_PLATFORM`, `LIBGL_ALWAYS_SOFTWARE`, `MESA_LOADER_DRIVER_OVERRIDE` and
`GALLIUM_DRIVER`. For a display-free Mesa run, prefix both commands with
`EGL_PLATFORM=surfaceless`. Info-level logs retain the actual GL renderer name;
comparison rejects a missing or changed renderer identity, including a switch
between a physical GPU and llvmpipe software rendering.

For example, replace these local paths with the actual game/build paths:

```sh
python3 tools/perf/evaluate.py run \
  --rom '/absolute/path/to/Pokemon Ultra Sun.3ds' \
  --executable /absolute/path/to/baseline/pocket-native \
  --library /absolute/path/to/baseline/title.so \
  --titles /absolute/path/to/pocket-native/titles \
  --build-metadata /absolute/path/to/baseline-build.txt \
  --frames 600 --repeats 3 --timeout 600 \
  --clock 4000000000000 --input '0:A,120:START,240:A' \
  --output build/perf/ultra-sun-baseline
```

Then run the same command with the candidate executable/library, candidate build
metadata and a new output directory. Preserve the same ROM, frame count, input,
clock, resolution, title profiles, settings, data seed and resource constraints.
`--titles` copies profiles into each trial; omitting it deliberately gives every
trial an empty profile directory. A copied executable does not automatically
inherit profiles from the source repository.

`--seed-data DIR` starts every trial with an independent copy of a known save
and game settings. `--seed-config DIR` copies global configuration; otherwise
both start empty. Symlinks in seed/profile trees are rejected. Each repetition
gets its own working directory, HOME, temporary directory, application cache,
configuration and `ZAKURO_DATA`. Source seeds are never passed to the runtime.
The output directory must be new and cannot be inside a seed directory.
Application caches are fresh by default; `--seed-cache` explicitly clones a
per-build warmed cache instead. The host's filesystem/page cache is
**not flushed**, and pre-trial ROM hashing itself warms file pages, so these
are not controlled cold-disk measurements. Keep the
machine otherwise idle and alternate baseline/candidate batches to assess drift.

The default input is `0:A`, held for the engine's six-frame scripted interval,
then released. Supply a deliberate scene-navigation script for useful results.
An empty script would enable the probe's automatic button mashing, so the
harness rejects it. The pinned clock is milliseconds since 1900, as expected
by `ZAKURO_CLOCK`. Fixing clock and input does not prove full engine determinism;
the comparison explicitly checks output consistency across repetitions.

## Read results and compare

```sh
python3 tools/perf/evaluate.py compare \
  build/perf/ultra-sun-baseline build/perf/ultra-sun-candidate \
  --output build/perf/ultra-sun-comparison.json
```

Each output contains `manifest.json`, and each `trial-NN/` contains logs,
isolated data, the native probe report/screenshots and `result.json`. The harness
requires the version 1 `report.toml` emitted by the instrumented frontend. Older
Markdown-only probes are recorded as incomplete instead of estimating from
their rounded duration. Failed loads, faults, timeouts, missing reports,
shortened workloads, zero converted instructions, skipped frames, a screen with
no pictures or missing/corrupt sampled BMPs fail the workload gate. An interpreter
fallback cannot count as a successful converted-code evaluation.

Wall duration covers process startup, ROM load, execution, screenshot/report
writing and exit. The precise native probe duration is retained separately.
Linux `wait4` measures CPU time and peak RSS for the exact child process; this
avoids cumulative RSS from earlier repetitions. It measures process memory and
CPU, with Linux's accounting for waited descendants, rather than a simultaneous
sum of every process or GPU allocation.
The evaluator also samples `VmRSS`, `RssAnon`, `RssFile` and `RssShmem` from the
same `/proc/<pid>/status` snapshot at the largest **sampled total RSS**. This is
an approximate observation at a 10 ms cadence, separate from `wait4`'s peak;
it does not sum independent component maxima. Missing samples remain unknown.
Page-fault and block-I/O counters are retained with the child resource usage.
File-backed pages may be reclaimable, and explicit ROM-cache retirement can
trade RSS for rereads/faults. Neither RSS nor these counters establishes total
system RAM, GPU memory or SD-card behavior; see [ROM cache policy](rom-cache-policy.md). A 10 ms process polling interval adds
small overhead to wall measurements; use workloads long enough to amortize it.

Comparison requires at least three valid trials per build, identical experiment
parameters and machine identity, the same ROM hash, and identical sampled BMP
hashes, picture counts and completed frame counts across **all** trials. Exact
manifest schema versions and measurement-harness SHA-256 values must match;
regenerate both measurements after changing harness logic. Missing schema,
harness identity or resource-envelope provenance prevents a passing comparison.
Pixel equality is deliberately strict; a differing pixel is a review condition,
not automatic permission to weaken correctness checks. Medians and observed
ranges are reported. The legacy comparison also retains its conservative half-resource threshold
fields for compatibility, using candidate maximum divided by baseline minimum.
Those fields are not the project objective or the paired runner's admission
criteria, and the range check is not a statistical confidence interval.

`measured_workload_target_met` reports those numerical gates. `target_met` also
requires explicit human scene review evidence. A deterministic black screen,
static menu or compatibility hang is not proof of faster playable Ultra Sun.
Review the sampled screenshots, warning report, frame progression and actual
gameplay before supplying an evidence JSON using `--scene-evidence PATH` to
both measured runs:

```json
{
  "schema_version": 1,
  "verified_gameplay": true,
  "scene": "Describe the verified playable scene and input sequence",
  "reviewed_by": "Reviewer identity",
  "rom_sha256": "The exact manifest ROM SHA-256",
  "frames": 600,
  "screenshots": {
    "frame-00074.bmp": "The exact sampled BMP SHA-256"
  }
}
```

The example screenshot object is abbreviated: supply the entire trial's
`probe.screenshots` object. Review evidence must bind the exact ROM, frame count
and all sampled hashes. It is a human attestation, not automated game-state,
audio, save-integrity or full-game correctness proof. First perform a diagnostic
run to obtain and review those hashes, then measure with the review receipt.
If evidence is absent or mismatched, the report can show measured improvements
but cannot claim the gameplay target. `compare` exits 0 only for a passing target;
exit 2 covers incomplete, incomparable or unmet targets. `run` exits 0 only when
all requested trials completed their workload; it does not claim an improvement.

## Diagnose exact workload drift

Check repeated guest outcomes independently of speed and host-activity gates:

```sh
python3 tools/perf/check_determinism.py \
  --manifest build/perf/ultra-sun-baseline/manifest.json
```

This checks exact sampled image hashes, picture counts, frame counts and guest
instruction counters. A one-picture discrepancy fails even when every sampled
image matches. The command exits 2 for drift or incomplete repetitions. Reuse
it against newly captured receipts after a change; old receipts stay immutable.

For a narrower diagnostic, an instrumented runtime supports
`ZAKURO_PROBE_TRACE_EVERY=N`; the evaluator exposes this as `--trace-every N` and
records the interval in the experiment. At completed guest-frame boundaries,
the probe samples guest cycles, all sixteen CPU registers, the current guest
thread and instruction counters without adding GPU waits. It accumulates at
most 100,000 checkpoints and writes `guest-trace.toml` after execution. Dropped
checkpoints and trace-write failures cannot silently count as complete trace
evidence. Compare two traces to locate the first sampled divergence:

```sh
python3 tools/perf/check_determinism.py \
  --trace /absolute/path/to/trial-01/probe-TITLE/guest-trace.toml \
  --trace /absolute/path/to/trial-02/probe-TITLE/guest-trace.toml
```

Trace collection is diagnostic overhead; rerun performance measurements with
it disabled after diagnosis. It does not replace pixel comparisons or a
complete fixed-workload check. Screenshot readbacks themselves wait for GPU
work, so preserve their cadence when comparing a control with the original.

Async GPU completions are observed by guest system calls. Their availability
can depend on host scheduling even with a pinned guest clock and input script.
The existing game setting `[advanced] gpu_thread = false` serializes GPU work;
use an identical frozen seed setting for both original and candidate when
testing that control. Raw `ZAKURO_SYNC_GPU` environment values are overwritten
by the settings export and are insufficient evidence that the control applied.
Keep rendering quality, resolution and frame skipping unchanged. Determinism
under this serialized profile is a result for that profile; default concurrent
operation needs its own verification, and changing profiles is not an engine
optimization or proof of the requested speedup.

## Reject CPU contention before attributing a change

Every trial snapshots aggregate `/proc/stat` counters immediately around the
child measurement, retaining the raw counters, system `USER_HZ`, online CPU
count, load averages, `MemAvailable` and exposed CPU-frequency governors. Busy
CPU time includes user, nice, system, IRQ, softIRQ and steal ticks; idle and
IO-wait ticks are excluded, and guest ticks are not counted twice. Subtracting
the child's measured user+system CPU time estimates CPU consumed elsewhere,
including the evaluator's own small overhead. Negative residuals within the
accounting tick tolerance are clamped to zero rather than called a speedup.

`--max-other-cpu-fraction` sets the comparison budget (default **0.10**) and is
part of the experiment identity. The estimate is normalized by wall seconds
times **all online host CPUs**, so 0.10 permits approximately 1.6 CPU cores of
external work on a 16-CPU machine. Every trial must remain within the same
declared budget. Missing/unreadable telemetry, regressing counters, changed
CPU count/tick rate, a governor change during a trial, or a trial above budget
makes the comparison `incomparable`. A `run` can still be workload-complete;
that status preserves the raw result and does not approve a performance claim.
Older manifests without the telemetry cannot pass a future comparison.

This is an estimate and a contamination gate, not proof of complete isolation.
It counts external activity even outside the game's CPU affinity, but does not
locate it on particular cores. Activity below the whole-host threshold can
still collide with a game's busiest core. Lower the budget deliberately for a
quieter experiment, record it for both builds, and repeat alternating batches
after unrelated jobs finish. Do not change the budget retroactively to approve
a previously busy run. Load averages include runnable and blocked work and
are context, rather than a substitute for CPU-counter deltas.

The snapshots do not rule out frequency or thermal changes between samples,
shared-cache/memory-bandwidth contention, external GPU load, IO pressure or
physical-memory pressure. The current process RSS remains a Linux accounting
measurement, not an exact memory requirement under an isolated handheld OS.
Retain contaminated runs as exploratory evidence; a slower run on a busier host
does not establish a candidate regression.

## Constrained-host pressure proxy

```sh
python3 tools/perf/evaluate.py run \
  --rom '/absolute/path/to/Pokemon Ultra Sun.3ds' \
  --executable /absolute/path/to/pocket-native \
  --library /absolute/path/to/title.so \
  --frames 600 --repeats 3 --cpu-affinity '0,1,2,3' \
  --rss-limit-mib 768 --output build/perf/ultra-sun-pressure
```

Choose four CPU IDs actually allowed on the host (`os.sched_getaffinity(0)`).
The sample 768 MiB ceiling is an experiment parameter, not a measured budget
available to applications on any handheld. The RSS watchdog samples the child
roughly every 10 ms and kills its process group if the observed RSS exceeds the
limit. It can overshoot and does not cap descendants' combined memory, VRAM,
file cache or system RAM. Its peak-RSS measurement still comes from `wait4`.
Do not use `RLIMIT_AS` to imitate RAM: fastmem reserves a large virtual address
range that need not consume corresponding physical RAM. For stronger isolation,
use a separately configured Linux cgroup with `MemoryMax` and no swap, retaining
its configuration and counters alongside the run.

The evaluator reads its unified cgroup membership and walks every ancestor up
to `/sys/fs/cgroup`. The game child inherits that membership. Before and after
each trial it records a comparable resource envelope: CPU quota/period and
burst, scheduling weight/idle/utilization clamps, memory maximum/high/swap and
protection, effective CPU/NUMA sets, process limits and available IO constraints.
It retains source hierarchy paths separately for diagnosis. Transient systemd
unit names and current usage counters are excluded from comparison; normalized
unique constraints are included. Thus two independently created pressure units
with equivalent constraints can compare, while a 768 MiB/400% pressure run and
an unrestricted host run cannot, even if their CPU affinity matches.

Normal kernel root semantics and a controller explicitly not delegated to a
child are recorded as inherited/unlimited rather than mistaken for a new local
limit. Missing or unreadable files for an enabled controller make the envelope
unknown. Unknown, changed or unequal envelopes fail the comparison. These
snapshots describe limits, not sibling contention or a continuous audit of
administrator changes during execution. The normalized envelope deliberately
does not model the number of sibling groups sharing each scheduler weight.

Restricting an x86 host to four cores and a memory ceiling is **not ARM hardware
simulation**. It does not reproduce Cortex-A55 IPC, Mali drivers/GPU, shared
memory bandwidth, Android services, dual-display composition or thermal/power
limits. `ZAKURO_PROBE_SPEED` simulates the engine's frame-skip pressure only and
is intentionally excluded from this no-skipping evaluation. Linux desktop
measurements cannot establish performance of an Android APK. Native device
measurements on the actual hardware and OS are the final evidence for that
deployment. Keep pressure-proxy and native-device results separately labelled.

## Find actionable CPU hotspots

Capture a **separate diagnostic run** with the engine's sampling profiler;
profiler overhead must stay outside the performance comparison. Keep the same
clock, scene input, data seed, converted library and renderer as the measured
workload, but use a fresh diagnostic data/config/cache directory and set:

```sh
TMPDIR=/absolute/path/to/diagnostic/tmp \
ZAKURO_HOST_PROFILE=0,600 \
ZAKURO_CLOCK=4000000000000 \
ZAKURO_INPUT='0:A,120:START,240:A' \
ZAKURO_RECOMPILED=/absolute/path/to/title.so \
ZAKURO_DATA=/absolute/path/to/diagnostic/data \
POCKET_NATIVE_CONFIG=/absolute/path/to/diagnostic/config \
EGL_PLATFORM=surfaceless \
/absolute/path/to/pocket-native --probe '/absolute/path/to/game.3ds' 600

python3 tools/perf/summarize_profile.py \
  --profile /absolute/path/to/diagnostic/tmp/zakuro-host-profile.txt \
  --output build/perf/ultra-sun-profile-summary.json --top 20
```

Create the directories and copy any required seed data first. Preserve the
binary/library hashes and original sampler file alongside the diagnostic run.
The engine writes a sample's thread name, mapped ELF path and **file offset**;
this offset is not generally a virtual address. The Linux summarizer validates
the ELF64 program-header table, maps the offset through `PT_LOAD`, then asks GNU
`addr2line` for the function and source location. GNU Binutils `addr2line` is an
external requirement for symbol resolution; absent symbols/tool/ELFs remain
explicitly unknown. It does not invoke a compiler or execute the sampled ELF.

The JSON reports top functions, threads and mapped files, counts/percentages,
unknown reasons, ELF hashes, tool version and active limits. Unknown and malformed
samples remain in the overall percentage denominator. Defaults bound input to
64 MiB, samples to the engine's 262,144-entry capacity, ELF files to 128 and
symbolized unique addresses to 50,000; use the corresponding `--max-*` arguments
to deliberately change those budgets. Truncation and possible sampler-capacity
exhaustion are labelled. Exit 0 means the summary was written; check its `status`
and unknown counts before using its rankings. An empty profile is `empty`, not
proof that a workload consumed no CPU.

The current profiler's text format cannot reliably represent paths containing
spaces, and its upstream `/proc/self/maps` parser can truncate such paths.
Capture from build paths without spaces. Ordinary spaces in thread names are
supported; ambiguous lines are counted as unknown. For an exact archived
unstripped build, map the recorded absolute path explicitly:

```sh
python3 tools/perf/summarize_profile.py \
  --profile /absolute/path/to/zakuro-host-profile.txt \
  --elf /recorded/path/title.so=/archive/exact-build/title.so \
  --output build/perf/archived-profile-summary.json
```

When the original ELF is still present, override `PT_LOAD` layouts must match.
When it is absent, original layout is marked unverified; the analyst must bind
the archived file to the captured build. Current ELF hashes alone cannot prove
capture-time identity. Unsupported/invalid ELF files, out-of-segment offsets,
bad symbolizer output and stripped/unknown functions remain unknown.

Use the busiest resolved functions to select a focused optimization, then
rerun the unchanged evaluation and correctness gates. Samples are approximate
instruction-pointer observations: timer delivery can bias thread attribution,
waiting/GPU time is not fully explained, and percentages are not exact function
durations. The x86_64 sampler writes zero for caller PCs; this summary therefore
makes no call-graph or stack-unwind claim. A dominant GPU-driver or allocation
function is a lead for investigation, not proof that changing it will deliver
the target speedup.

## Harness verification

```sh
python3 -m unittest discover -s tools/perf -p 'test_*.py' -v
```

These tests use synthetic executable/report fixtures, not copyrighted game
assets. They check isolation, source-seed preservation, output-directory reuse,
timeout/RSS failure handling, incomplete/fallback rejection, strict comparison
and scene-review binding, cgroup ancestor inheritance, transient-path
normalization, pressure/host mismatch, missing-limit rejection and CPU telemetry
parsing/deltas/contention gates. Passing them
establishes harness behavior, not game
compatibility or a handheld speedup.
