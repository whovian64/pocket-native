# Source contracts for finish and capture-cap adapters

These helpers consume source contracts. Synthetic tests are contract checks,
not native finish/cap validation, acoustic evidence, or performance evidence.
The runtime sources are owned and integrated separately. No compiler, draw,
fog, packet production candidate or engine ABI change belongs to this adapter.

The audio input is `f9b566479b49268a18f9c77a90df6a39955128af`, based on
`aa0bddb5e80fc93536aa882eeb126540897626a3`. The cap input is
`08ba8b5cda4ab21fcdd6914173bc26fbdf70590d`, based on private diagnostic
`d09aa3fde15b8ef1046a68fab48e97e289a66e1a`. They are separate source proposals;
neither source commit identifies a combined validated native binary.

## Audio

Top-level report schema remains 1. Historical audio schema 1 remains readable:
`drained` retains its historical delivery meaning, and backend finish is
explicitly `unknown_historical_schema1`. No old receipt is rewritten.

Audio schema 2 replaces `drained` with `channel_finished`, `backend_finish`,
`backend_finish_operation` and an optional exact signed i32
`backend_finish_code`. Only returned `succeeded`/`failed` statuses have a code;
zero means succeeded and every nonzero value means failed. An observed drain
requires an open ALSA backend and operation `snd_pcm_drain`. Unavailable and
not-attempted statuses use operation `none`; in-progress has no return code.
Channel completion, full backend writes and backend finish are distinct.

An optional diagnostic declaration `"audio_completion": "backend"` requests
the stronger rule without changing historical declarations/hashes. It requires
audio on. `collect()` also applies the stronger rule to any newly observed
audio schema 2. Admission requires full-write/frame gates, a finished channel
and observed successful ALSA return zero. Schema 1, AAudio not-attempted,
failure, unavailable and unfinished/unobserved returns cannot satisfy it.

Device frames and mixed/output frames have distinct rate domains. No equality
between them is inferred. Optional ratio/queue summaries remain observations;
Linux underruns remain unmeasured. Log placeholders never establish underruns
zero, physical device identity, acoustic completion or human sound quality.

## Geometry and capture cap

Geometry sidecar schema 1 accepts historical fields and retained-file index
meaning. It rejects budget fields and indices on unsuccessful/unnumbered shots.
Sidecar schema 2 implements the cap contract's changed attempt-index semantics,
including failed/unnumbered attempts, exact budget statuses and canonical u64
decimal-string limit/charged counters. The original cap source serialized
version 1 pending root's decision; the integrated runtime must bump the sidecar
to 2. Top-level report and diagnostic/render declaration versions stay 1.

`presentation_capture.max_bytes` still bounds retained files at admission.
Optional positive u64 `max_io_bytes` declares cumulative logical whole-file
reservations on the owning GL thread. It requires numbered capture and adds
only `ZAKURO_PANEL_SHOT_MAX_IO_BYTES` to the controlled child environment.
Historical profiles gain no field or implicit cap, and their declaration hashes
remain unchanged. No-profile/default runtime inputs remain unchanged.

Capped observations require schema 2, observed ready state, exact requested
limit, charged <= limit, numbered written/retained indices, complete exact P6
files and a bounded complete stderr snapshot free of capture error messages.
Missing, malformed, absent, invalid, exhausted, write/size failure and old-runtime
ignored environment all hold admission. Existing files cannot override failure
records or error logs. Captured stderr and observer sources are bound on reopen;
a stored valid flag alone cannot satisfy the new rules.

For the selected single-thread fixed-size control, each sampled successful
attempt's charged count must equal `(attempt_index + 1) * exact_P6_bytes`.
Retained inventory must start at zero without gaps and every file must have an
independent sampled success; the last observed charge must match that inventory.
Sparse snapshots are held without inventing unseen attempts. These checks
report `consistent_observed_numbered_attempts_only` when satisfied.

Even that result does not prove the GL thread's lifetime, final unseen work,
every presentation or presentation/probe-frame ties. `cap_trial_coverage` stays
unknown and **capped trial admission remains held**: these schemas contain no
independent coverage attestation. Root must supply/review such evidence and its
binding before extending admission. Full matrix cap and process quota remain
false. The cap measures neither physical IO nor memory/storage consumption.

All captures remain NON-TIMING. The existing mathematical geometry capability
is still only exact-model conformance for independently bound numbered observed
presentations, with unknown probe-frame ties and no full-matrix/scanout claim.
The compiled geometry model pin is retained. Adapter/diagnostic support is also
hashed from the source actually compiled, with disk-change and reopen guards.

## Deferred verification

After root source review and release of its fixed16 compile census lane, root
can run the three focused unittest modules followed by the established broad
Python suite. No tests, syntax/import execution, builds, games or profiling were
run in this source-only adapter lane. The proposed single bounded control and
exact implementation handoff live in
`build/continuous-r1o/finish-cap-adapters/` in the implementation checkout.
