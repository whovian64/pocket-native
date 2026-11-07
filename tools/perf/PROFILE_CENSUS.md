# CPU and resource census

`profile_census.py` provides a capture **plan** and an offline reader. It never
starts a game, invokes `perf`, changes permissions, or admits a performance
comparison. Use it alongside the existing evaluator, not as a replacement for
equal-work, image/trace, audio, or host-noise checks.

Generate a private capture plan with the exact approved replay command:

```sh
python tools/perf/profile_census.py --output build/census/plan.json plan \
  --prefix build/census/cpu -- path/to/pocket-native --probe path/to/game.3ds
```

The output contains argument arrays, not shell fragments. Do not substitute the
example for a validated title-specific replay. The planned `perf record` uses
199 Hz userspace CPU-clock sampling and a 16 KiB DWARF stack snapshot. Capture
only after the CPU/GPU lane owner grants the slot. Record the installed `perf`
version, command, environment, permission errors, lost samples and capture exit.
Check the host's permitted profiling setup without changing
`perf_event_paranoid` or elevating privileges automatically. A plan proves no
current capability or permission.

Run the exported `perf script` argument array and redirect its stdout privately.
The reader expects fields `comm,tid,time,event,ip,sym,dso`, nanosecond timestamps,
and the default indented call-chain rows. It fails the complete-parse indicator
on unrecognized nonempty rows; inspect them rather than discarding them.

```sh
python tools/perf/profile_census.py --output build/census/summary.json read \
  --perf-script build/census/cpu.script --log build/census/runtime.log \
  --manifest build/census/eval/manifest.json --guest-dso /exact/mapped/title.so
```

Use an exact recorded DSO path, including an alternate build path when present.
`--host-profile` instead reads the existing `ZAKURO_HOST_PROFILE` text format and
reuses `summarize_profile.py`'s ambiguity checks. It cannot resolve function
categories from raw offsets or recover caller stacks. Use the existing
symbolizer for function-level IP analysis; a resolved inline chain is still
not a runtime caller stack. The ARM link-register suffix is also not an
unwound stack.

The output distinguishes these observations:

| Observation | Interpretation and limit |
| --- | --- |
| CPU sample categories | Exclusive leaf samples with a complete parsed-sample denominator per event. Guest native, PICA, GLES submission, driver, core, audio, movement and unknown stay separate. They measure sampled CPU occupancy, not avoidable cost. |
| Memory primitive callers | First supplied resolved caller of sampled memcpy/memmove/memcmp/memset. Caller coverage remains explicit; a stack may still be truncated or wrong. |
| Driver CPU | Userspace GL driver samples. These do not measure GPU execution duration or transfer to another GPU vendor. |
| GPU queue wait | Existing `waiting on gpu` log observations, or evaluator batch `gpu_wait_seconds`. CPU-clock sampling omits off-CPU waits. Log intervals have unknown wall durations; their percentages are not averaged. |
| Texture hashes | Complete 5,000-lookup log intervals, integer KiB lower/upper byte bounds, omitted final tail. No per-reason, per-mesh, command-hash or upload-byte claim. |
| Audio | A device-open failure is detected. Absence of that warning is not proof of audible PCM, AAC accuracy, pitch or continuity. |
| Manifest context | Existing binary/source/settings/work/trace receipt is retained. Supplying it does not prove the profile came from that process or measurement window. |

Byte counts need dedicated counters for the *reason* a texture/mesh was
revalidated, GL calls and upload populations. Leave these values unknown until
instrumentation has an explicit complete-window receipt. Never infer avoidable
cost by summing unrelated sample shares, maxima, or counter populations.

Capture each title and scale/aspect/layout independently, preserve the original
quality/settings and audio path, and separate cold startup from warmed gameplay.
Compare equal guest work and image/trace state with an uninstrumented control.
Sampling overhead makes diagnostic captures ineligible as normal timing runs.
Measure async and serialized profiles separately: work on a different host
thread may still sit on the critical path. Android/A55 models are not physical
handheld evidence.

Private output can contain local paths, symbols and game identities. Keep it in
ignored build storage; publish only reviewed, portable summaries.

Synthetic parser checks, which launch no games or profiling processes:

```sh
PYTHONDONTWRITEBYTECODE=1 python -m unittest discover -s tools/perf \
  -p test_profile_census.py -v
```


Each read input is opened once and read into a bounded snapshot. Its reported
byte count and SHA-256 identify exactly the bytes parsed, even if the original
path is later replaced or changes. Oversized inputs and lines fail closed.
This identity does not bind a live process or prove capture completeness.
