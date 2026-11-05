# Ultra Sun prologue workload

`ultra-sun-usa-prologue.input` is an original sequence of buttons and touch
coordinates for a fresh USA Ultra Sun run. It contains no game data.
`ultra-sun-usa-prologue.json` records the exact tested ROM identity, clock,
initial state and observed scene. Other dumps, updates or regions require
independent validation.

The script clears the initial trainer-name input and enters a valid name,
then advances the prologue. A manually inspected host run reached the
bedroom cutscene, with the trainer still lying in bed at frame 17999. It does
not establish player control, overworld exploration or battles;
`verified_gameplay` remains false.

Pass the script to the evaluator with the same fresh seed for every trial:

```sh
python3 tools/perf/evaluate.py run \
  --rom /absolute/path/to/game.3ds \
  --executable /absolute/path/to/pocket-native \
  --library /absolute/path/to/00040000001B5000.so \
  --titles /absolute/path/to/pocket-native/titles \
  --seed-data /absolute/path/to/fresh-font-only-seed \
  --implementation-profile /absolute/path/to/reviewed-serialized-gpu-profile.json \
  --frames 18000 --repeats 3 --clock 4000000000000 \
  --input "$(cat tools/perf/workloads/ultra-sun-usa-prologue.input)" \
  --output build/perf/ultra-sun-prologue
```

The seed contains only `sysdata/shared_font.bin` prepared by the existing
build workflow, with no prior saves, settings or shader cache. Keep it and
all resulting game assets/screenshots outside Git. Identical sampled output
must still pass across repetitions and both builds on the same backend.

The initial asynchronous-GPU evaluation failed strict output comparability:
sampled images and picture counts varied. Subsequent serialized-GPU runs
(`gpu_thread=false`) match sampled BMPs, picture counts and both guest instruction
counters exactly. Three fresh original-runtime/library monolithic trials also
passed the known host-activity gate (external CPU fractions 0.0786..0.0835,
budget 0.10). Their raw receipts remain local and are not included in the PR.
Those diagnostics are distinct from formal paired public-evaluator runs.

Use a reviewed [implementation profile](../IMPLEMENTATION_PROFILES.md) with
`gpu_thread=false` on both sides. Keep its fixed quality constraints identical;
the comparator reports declared optimization differences and retains all exact
work/output/noise gates. The repeatability result does not turn this cutscene
fixture into verified gameplay or provide target-device performance evidence.

## Separate 24,000-frame bedroom fixture

`ultra-sun-usa-bedroom.input` extends navigation through waking and scripted
CLEFT/CDOWN bedroom movement. Its JSON is an **agent visual-review receipt**,
not user/human signoff, battle coverage or full-game correctness. It can be
supplied as scene evidence only when every bound sampled hash and work gate
matches. Use 24,000 frames and the same clock/fresh font-only seed above.
The [final results](../../../docs/ultra-sun-eval-results.md) retain the exact
protocol and rejected timing admission. Genuine same-frame captures stay local;
no game imagery is included.
The 18,000-frame fixture above remains cutscene-only; their scene approvals
must not be interchanged or applied to other titles.
