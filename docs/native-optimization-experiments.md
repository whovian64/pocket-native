# Native execution experiments

Five further approaches were assessed against the runtime retained at
`bba1096`. These are separate experiments from the
[original/current runtime comparison](consistent-eval-results.md). Private
generated game code was used as an oracle and never added to the repository.
None of these five prototypes is enabled in this PR update.

| Approach | Finding | Decision |
| --- | --- | --- |
| Packet predecode | Both whole-byte and header-shape caches preserved the recorded output, but misses dominated. Header-shape hits covered only 3.03% / 8.64% / 7.36% of processed headers in Ultra Sun / Omega Ruby / Y. | Exclude both prototypes. |
| Exact resource-write tracking | Generated direct stores, physical aliases and mutable slices bypass the existing advisory journal. A bounded owned-page model passed its tests, but no current game resource has complete writer coverage. | Preserve safety hashes; no tracking-based fast path. |
| Native copy | Guarded replacements matched full state, budget/resume behavior and all recorded game outputs across three titles. All three timing series remained held. | No compiler transform promoted. |
| Native graphics producer | A float-uniform packet producer was replaced privately in Y and Omega Ruby. All 12 game runs matched, but both timing series remained held. | No producer replacement or renderer bridge promoted. |
| Effect-aware compiler IR | Selected component checks passed, but an accepted authored case adds a first-touch fault after the original compiler removes an overwritten store and its load. | Exclude the prototype; preserve the existing compiler. |

## Packet parsing and write coverage

The header-shape prototype validates every cached raw header and snapshot
length, reads payload values fresh, and executes every original action. Three
6,000-batch diagnostic replays matched work, eight sampled images, warnings and
all 20 guest-trace checkpoints per title. Across 12.42 million snapshots,
11.44 million lookups missed. The instrumented cache-plus-digest path cost
1.695 times the scalar digest path in aggregate. This includes diagnostic
timers, checksums and allocation; it is not a renderer or game-speed benchmark.
Retained record capacities were approximately 0.57–0.97 MiB per title, plus
metadata and allocator overhead; no memory saving was established.

The write-tracking audit checked generated FASTMEM and page-table stores,
interpreter, privileged and service writes, DMA, GPU writes, mutable physical
borrows, aliases and mapping lifetimes. The journal can suppress identical
consecutive range notifications across a cache observation. Three tests bound
to the actual journal demonstrate missing-store, repeated-write and alias
counterexamples. Adding a generation counter to that journal would not make
its current coverage exact. All current texture validation remains intact.

## Native-library comparisons

Native replacements are a different experimental factor from runtime changes:
both sides use the same current runtime, while the original and candidate game
libraries intentionally differ. The public runtime comparator still requires
the same original library on both sides.

The fixed native-copy campaign completed 18 executions, six per title in
control/candidate/candidate/control/control/candidate order. All full work,
eight sampled images, literal warning sections, 20 guest-trace checkpoints,
named-window work and renderer identities matched. Separate component checks
passed 217,056 full-state one-call cases, 2,736 actual resume paths containing
19,104 steps, and 27 unsafe-state guard rejections.

All three copy timing series failed host conditions and/or the four-metric
control A/A gate. Process-CPU / process-wall / window-CPU / window-wall control
spans were 11.45 / 8.03 / 35.73 / 34.59% for Ultra Sun,
10.95 / 11.63 / 6.52 / 6.72% for Omega Ruby, and
19.66 / 18.42 / 29.40 / 28.47% for Y; each must be at most 10%.
Favorable and unfavorable observations were retained. No run was discarded,
replaced or replayed to obtain a better score.

The graphics-producer campaign completed another 12 fixed executions, six
each for Y and Omega Ruby. All 96 sampled images and 240 guest-trace
checkpoints matched, along with work, window work, warnings and renderer.
Control timing spans exceeded 10% in all four metrics for both titles, with
additional host holds. Component proof covered 6,336 full-state cases,
28 guard rejections, 40 actual resume paths containing 3,632 steps and
42 typed packet-byte comparisons. The candidate preserves packet bytes;
it is not a new typed renderer ABI. Ultra Sun uses a different producer
and received no such replacement.

The native comparisons use the frozen current executable SHA-256
`939d92257b0e521bbd7737faef3bdf68ff055a5b92520022e5d3ab97bd6bd2f8`,
native 1× quality, serialized GPU, fixed per-title save/input/clock,
6,000 batches and the `[1800, 6000)` window. They enable identical trace,
batch-telemetry and boundary-capture observers and disable driver disk caches.
Their trace-enabled timings must not be pooled with the earlier timing
campaign. The [revised desktop policy](consistent-evals.md#practical-desktop-limits)
was frozen prospectively: 10% aggregate preflight CPU, 15% unrelated CPU during
execution, 25% selected-core/SMT activity, 10% A/A span and a 5°C preflight
temperature change; the local ceiling remains 97°C.

## Bounded compiler regions

The private IR experiment keeps register values in temporaries across short
copy/store regions in the main ARM executable and materializes architectural
registers at the region boundary. It retains explicit memory ordering, the
function-entry FASTMEM view, every label, budget charge and continuation.
Paging, 32-bit hosts and Context/view overlap keep the original lowering.
Floating point, flags, conditional instructions, PC operands, callbacks and
mode changes are outside the pass.
Relocated CRO modules retain their original lowering. The private source
scanner inventories their declarations but does not parse their relative
budget expressions; this is a coverage limit, not evidence of zero eligible
CRO code. The prototype integration was restricted to the main executable.

Prototype admission required at least 16 memory operations and 24 fewer static Context
access mentions, with at most 24 instructions per region and four regions per
function. Loaded values had to feed a direct store consuming their full
width; dead/cancelled loads and load-derived arithmetic or addresses declined.
Negative tests showed why: forcing a dead load could allocate an additional
zero page compared with the compiled original despite equal final registers
and bytes. The prototype rejected those initial shapes. Explicit value uses
retained admitted loads; the later dead-store case below exposed their limit.

The final emitter passed eight unit tests and 942 complete Context/backing
comparisons, including 191 matching actual runtime zero-page faults per
implementation on a 4 KiB-page Linux host. The prototype integration passed
50 release library tests, its
integration test and its doctest with all features; the new execution fixture
checks paging/FASTMEM and flag-local combinations, cross-page accesses,
budget/resume, Context overlap and a callback changing the Context's view
field. ARM64 Android Rust checking and generated-C cross-compilation passed;
no Android execution is implied.

Three selected Ultra Sun copy regions had lower complete-wrapper times in a
seven-pair synthetic fixed-point microbenchmark. This is a component result,
not game timing or a population-wide speed claim. Across those wrappers,
original machine code was 700 bytes and guarded output was 1,521 bytes: the
fallback and guard cost matter even though the unchecked cores shrink.
All three complete native libraries built and passed exports, ABI and immutable
input checks. Controls reproduced the original libraries byte for byte. The
prototype changed 662 main-ARM regions across 124 translation units; library
files grew by 86,016 / 49,152 / 45,056 bytes in Ultra Sun / Omega Ruby / Y.
These are file sizes, not resident-memory measurements.

The final compiler campaign completed 18 fixed executions, six per title.
All 144 sampled images, 360 trace checkpoints, full/window work, warnings and
renderer identities matched; immutable inputs were unchanged. Every title's
timing series remained held by host conditions, with an additional control
A/A failure in Omega Ruby's route window:

| Title | Control A/A span: process CPU / wall / window CPU / wall | Timing decision |
| --- | --- | --- |
| Ultra Sun | 0.76 / 0.70 / 0.54 / 0.65% | Host/thermal holds. |
| Omega Ruby | 4.04 / 2.98 / 16.82 / 16.24% | Host holds and window A/A failure. |
| Pokémon Y | 3.69 / 3.42 / 6.89 / 6.23% | Host/thermal holds. |

All four A/A spans must be at most 10%; passing those alone does not clear
the host checks. No trial was dropped, replaced or retried. CPU/wall/RSS,
simultaneous anonymous/file/shared RSS, faults and I/O observations remain
local; no game-speed or memory reduction is inferred from held timings.

A later source check found an admission gap: a loaded value can feed a
full-width store that a later store overwrites. A bounded authored test
confirmed it with the frozen emitter and the runtime's actual owned-View fault
handler. The emitter accepted all 20 instructions, including its source-size
gate. With lowerer-equivalent C chunks and the original GCC flags, the
original caused zero first-touch faults and the prototype caused one, while
complete Context and mapped backing bytes matched. The original compiler
removed the dead store and load; the prototype explicitly retained the load.

This is a reproduced resource-effect difference in an authored case, not an
observed visual defect in a game. The selected positive component checks and
matching sampled game outputs do not establish generic first-touch
equivalence. The prototype and its codegen integration were removed from the
proposed diff; their source snapshots and all positive/negative evidence are
retained privately. The existing compiler remains unchanged by this round.

## Scope

These experiments cover x86-64 Linux/NVIDIA GLES, startup and short outdoor
walks with long idle periods in Ultra Sun Route 1, Omega Ruby Route 101 and
Pokémon Y Route 1. They do not establish battle/interior/transition coverage,
full-game equivalence, power savings or RG DS Plus/Mali/Android performance.
Private source snapshots, rejected patches, raw runs and failure receipts are
retained locally. No game assets, generated game code/libraries, saves or
private captures are included in the proposed changes.
