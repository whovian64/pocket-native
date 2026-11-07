# Optimization closeout — 2026-11-07

This closes the selected work in PR48 before a user-requested pause. No accepted
game-level speedup is claimed. The closed 170-execution timing record and 15 held
series remain unchanged; no host, equality or timing gate was relaxed.

## Published tooling

The update after `37a87f4` changes performance-tool Python and documentation.
Runtime, engine, recompiler, Cargo and Android sources are unchanged. The
resident-buffer lifetime and constant-bit correctness fixes already published
at `37a87f4` remain in place; PR50 audio pacing is preserved.

- Model observed offscreen layout areas/viewports for supported layouts,
  aspects and swapped roles. Report conformance only for individually bound
  numbered presentations; probe-frame association and full matrix capability
  remain unknown/false.
- Bind geometry, render and diagnostic validation to the literal helper bytes
  compiled by the evaluator; detect source drift and reopen raw observer inputs
  before applying new finish/cap admission rules.
- Read audio schema 2 without confusing channel completion, full writes and
  an observed backend return. Match the runtime's lowercase `alsa`/`aaudio`
  tokens; historical schema 1 does not gain backend-completion proof.
- Read cumulative logical capture reservations and unsuccessful attempt
  indices from geometry schema 2. Missing or failed budgets cannot be overridden
  by retained images. Complete capture/thread-lifetime coverage is still required;
  a logical reservation cap is not a process filesystem quota or physical I/O.

All **197 Python performance-tool tests passed**. This includes the lowercase
backend regression failing before its fix and passing afterward. The earlier
exact-runtime Linux/GLES/Android checks and three route-equality replays at
`37a87f4` remain evidence for that unchanged runtime source, not new executions
of this tooling update. Android runtime and handheld hardware remain untested.

## Private diagnostic evidence

The private diagnostic runtime and prototype runner changes are excluded from
this PR update. Their source/binary pins, failures and raw artifacts remain local.

A separate ordinary-async, unpaced ALSA-null run emitted a raw report for all
6,000 batches. It records 5,534,208 device frames offered and accepted, full
output writes, no partial/failed writes, and `snd_pcm_drain` returning zero.
The original campaign remains **held**: a guardian `/proc` exit race terminated
the evaluator before its final native exit/result receipt was published.
Offline revalidation accepts the unchanged raw audio row under the corrected
helper; it does not reconstruct the missing campaign receipt. ALSA-null is
backend-call evidence, not audible or hardware playback. A preceding attempt
held by a filesystem device-identity check is also preserved.

The corrected private mapping guard passed its real read-only-mmap regression
and replacement/device/namespace controls. Its selected fake-process lifecycle
and private audio-contract suite passed **20 tests**, including a reproduction
of the old exit race and rejection of missing/false/nonzero completion evidence.
No further game execution was performed to validate those runner fixes.

The private final-attempt observer type-checked; its focused ledger/publication
tests and real worker/no-hardware transport checks passed. The broader private
GPU suite returned **181 passed, 1 failed, 7 explicitly ignored**. The failure
was an existing separable-stage retirement assertion; the same binary passed
that test alone and its six-test related subset. This does not clear the full
failure. The independent private core/frontend suites passed **106/41 tests**.
The private observer remains held, with coverage explicitly unknown. These
results do not alter the earlier public-runtime validation.

One further, deliberately isolated real-capture test passed on an NVIDIA
GeForce RTX 4060 Laptop GPU. Two distinct 64×64 PPM captures to the same path
consumed exactly 24,602 logical reserved bytes; a third attempted capture was
rejected and the previous file remained byte-identical. The stop ledger matched
the three attempts and two writes. This verifies that fixture's overwrite cap
and readback path, not final worker coverage, physical I/O, probe-frame identity
or a title/layout matrix. It does not clear the private full-suite failure.

## Candidate dispositions

| Work | Disposition at pause |
|---|---|
| Presentation model and finish/cap adapters | Published tooling; partial observations only, no matrix or playback claim |
| Draw-overhead, shader-packet and fog-burst prototypes | Functionally validated earlier; gain unestablished, excluded and held |
| VFP pointer hoist `c3b7fbd` | Rejected by the frozen static rule: x86 text grew |
| Single-precision normalization `cac664c` | Semantic checks and tiny compiler probes passed; full census and workload value population unresolved, excluded/held |
| Normalization population collector | Eight GCC/Clang host configurations passed helper tests; no title value population collected and no gain inferred |
| Vector-length check caching | Parked; prepared census unrun |
| Flag-locals N2 | Parked; default production builds unaffected; earlier stall attribution withdrawn |
| Audio-only native campaign | Held despite useful raw backend-return evidence; missing final native/evaluator completion cannot be inferred |
| Private final-attempt runtime and capture matrix | Held; incomplete worker/finality/frame coverage and private full-suite failure |

## Resume boundary

No new profile/DSO build or optimization round is part of closeout. The provenance
audit found retained production-flag profiles use older generated code; its
earlier broad flag-locals/stall conclusion was corrected. A newly prepared
current-source profile plan has not run. It is a useful next evidence step,
with exact source, compiler, override, scene and DSO bindings required before
using it to choose a production target. Desktop/NDK assembly is not evidence
for the handheld's distinct shipping compiler.

Optimization pauses after the final PR update. No merge is requested. ROMs,
generated game libraries/source, saves, captures, private paths, environment
dumps and raw profiling/core data are not published.
