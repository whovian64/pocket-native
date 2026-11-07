# Current overhead priorities and attribution limits

The resumed study is based on runtime/compiler source at `6bcd630` and retained desktop evidence. The opt-in attribution implementation is commit `e363601`; current-source capture attempts remain held. No new game-level gain is accepted. The prior 170 timing executions and 15 held series remain closed/held.

| Priority | Evidence and plausible benefit | Confidence and next validation |
| --- | --- | --- |
| Warm-start GL stalls | Retained representative batches take 1.4–2.8 s. Loading/hitch benefit could be substantial if the cause persists. Door/battle recurrence is unobserved. | Old stalls observed; current cause/cost unknown. Identify costly calls and owning thread first. |
| Shader compiler resources | Source requests unlimited driver compiler threads. Historical async/serialized anonymous-memory differences around 150–180 MiB change two switches and do not isolate ownership. | Source policy certain; actual driver behavior and removable cost unknown. Attribute first, then compare one hint with external workers/quality/cache fixed. |
| Texture fingerprints | Retained interval streams hash about 0.29–2.10 GB of logical input. This is not measured DRAM traffic or exclusive CPU time. | Work population observed; current cost unknown. Prefer bit-identical implementation work if current attribution supports it. |
| ROM prefetch and GPU transport | Plausible shared-runtime costs; no current exclusive attribution or positive retirement churn. | Preserve default-off retirement and ordering; count actual work before redesign. |
| Lookup and compiler ABI | Old libraries predate floating-point changes and old lookup profiles predate the runtime lookup fold. | Current cost and handheld behavior unknown. Rejected pointer hoist/restrict/GPR/effect-IR ideas remain closed. |

For every row, current critical-path fraction is unknown; a numeric end-to-end Amdahl ceiling would be invented. Desktop GCC 16/NVIDIA behavior does not establish GCC 8/A55/Mali or physical RG DS Plus performance.

The diagnostics are off by default. The profile companion keeps numeric TIDs/raw PCs, explicit incomplete slots/overflow, and bounded monotonic markers. Unknown handler tails and exact sample/frame membership remain unknown. Shader spans are inclusive/nested/concurrent; the first 256 events may exclude later work, and snapshots never prove worker finality. Instrumentation can perturb scheduling. These are attribution tools, not timing acceptance.

Validation: integrated frontend profiler 10 passed, full frontend 28 passed, public GPU suite with diagnostics 173 passed/7 ignored. The normal release workspace build passed and 430 tests passed/8 ignored. The initial command could not locate rustdoc after these tests completed; only the remaining doctest phase was run with the matching toolchain binary, and it passed. Android ARM64 normal release compile/link also passed; no device was run. A separate private fixture investigation demonstrated reuse of a deleted GL program name by a sharing compiler context; the fixture now settles its startup jobs before raw-name assertions, retaining every lifetime assertion and separate in-flight coverage. This demonstrates an observation race, not the cause of the historical failure or a production lifetime fix. Observer finality remains independent.

The evaluation matrix still requires three games, 1×/1.5×/2×, requested and observed layouts/aspects including 4:3, audio-on correctness, and scene transitions. These axes are unobserved until individually recorded. The public probe suppresses audio delivery. Host profiles and null-backend checks cannot prove audible playback, presentation/frame causality, scanout, or physical-device results.


The fresh default Omega Ruby library was built with the current header, production FASTMEM flags, no compiler prototype, and the title lz.c override. Its 179,917 entries/145 modules match the prior inventory. A current diagnostic release runtime also built. These builds establish source/toolchain provenance, not game correctness or speed.

Both attempted captures remain held. The first guardian used path/hash-only pins where its stat check required enriched identities. The second native process exited with SIGXFSZ under the private guardian's inherited 64 MiB file limit before producing a probe report. Guest physical memory uses a 135/263 MiB shared memfd; sparse CPU fixtures reproduce the limit failure, though the exact native syscall was not traced. The corrected future driver passes 36 CPU fixtures with a separately pinned finite file/memfd limit. No further title process was launched in this diagnostic round. No complete current profile or revised numerical game-speed ceiling was obtained.

The first resource-policy prototype requests exactly 1–4 driver compiler threads under the diagnostic feature while retaining the original unlimited production request and external worker selector. It passed feature-off compilation and 15 pure tests, including four new selector cases. It remains private, unapplied and **held**: the requested hint does not prove driver compliance, lower RAM, faster readiness, unchanged pictures/audio, or gameplay improvement. The next evidence step remains a valid current-source attribution capture; a benefit claim requires the corresponding equal-work controls and matrix gates.

Optimization has resumed. The existing rejected/held candidate ledger is preserved; no new game-level gain is accepted. No merge is requested.
