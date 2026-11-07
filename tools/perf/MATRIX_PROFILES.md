# Prospective scale, aspect, layout and audio matrix

`evaluate.py run --render-profile FILE` now supports an explicit execution
profile, separately from this prospective matrix. The runtime must supply the
required observer/log evidence; a declaration does not establish capability.
For example, a scale-only profile preserves today's **BigTop/Original**
presentation default:

```json
{"schema_version":1,"id":"native-1.5x","internal_scale":1.5,"aspect":"Original","layout":"BigTop","swapped":false,"stretched":"Off","presentation_capture":null}
```

Supply matching `--resolution 1.5`. Aspect/layout cells require a declared
offscreen PPM panel capture and a geometry-capable runtime. They are always
NON-TIMING: the panel writes every presentation, even when a fixed filename
overwrites the previous image. Capture byte budgets bound retained output at
admission, not cumulative writes or a live filesystem quota. Settings, surface
records and panel bytes are retained separately; the partial geometry check
described below can validate recorded viewports, while presentation-frame linkage
remains unverified and presentation matrix capability is not admitted. The `Original` layout modeled below is a workload change from
the actual BigTop default, not an alias for it.

`matrix_profiles.py` freezes a bounded list of requested comparisons. It does
not launch games, generate runtime settings, collect results or admit timings.
Every output cell has `runnable: false`. A missing capability produces a visible
blocked cell rather than silently dropping a requested configuration.

Use it before allocating a campaign budget:

```sh
python3 tools/perf/matrix_profiles.py \
  --specification build/my-matrix/specification.json \
  --output build/my-matrix/prospective-matrix.json
```

The output must not exist. The output freezes the complete specification,
SHA-256 of canonical JSON, independent cell bindings and a seeded proposed
Original/Current order. A/A repeats and alternating paired processes are listed
separately. Counts exclude per-build cache warmups and all runs remain pending.
There is no game runner, retry loop or relaxed admission mode in this tool.

## Choose the coverage before execution

The three required independently bound titles are `ultra-sun`, `omega-ruby`
and `pokemon-y`. Internal render scale is `1`, `1.5` or `2`. Display aspect is
`Original` or `FourThree`, using the frontend's actual enum names. `FourThree`
changes presentation shape; it does not mean a 4/3 render scale or a crop.

`coverage: "screen"` makes these eight warmed-gameplay profiles per title:

| Internal scale | Aspect | Layout |
| --- | --- | --- |
| 1, 1.5, 2 | Original and FourThree | Original, both screens stacked |
| 1 | Original | TopOnly, one visible screen |
| 1 | Original | SideBySide, both screens across |

Available cold-start and transition scenes initially get only the first native
1x / Original-aspect / Original-layout profile. This screening schedule does
not establish scale/layout or aspect/layout interactions. `coverage: "full"`
explicitly lists all 18 scale/aspect/layout combinations for every available
scene. `max_cells` (1..512) is a hard prospective budget, not a machine
concurrency setting. One warmed scene and one cold scene in each title yields
27 screening cells or 108 full cells. Even three pairs plus three A/A processes
means 243 or 972 planned processes, before warmups: review and allocate the
matrix deliberately instead of launching a Cartesian campaign automatically.

The logical presentation `layout_size: [width, height]` stays fixed independently
of internal scale. Neither changing the host window nor stretching a 400x480
source screenshot proves a higher internal resolution. `TopOnly` describes
visibility; it does not authorize skipping the other screen's guest/GPU work.
Two physical displays or second-window mode need their own future adapter and
profile contract; these three layouts use the existing one-surface frontend
settings.

## Specification contract

Schema version 1 has a closed set of top-level fields:

| Field | Meaning |
| --- | --- |
| `schema_version` | Integer 1 |
| `comparison_factor` | `runtime` or `native-library` |
| `order_seed` | Nonnegative fixed integer |
| `pairs`, `aa_repeats` | Each 3..30; choose before measurements |
| `coverage`, `max_cells` | `screen` / `full`, explicit cell budget |
| `implementation_profile` | Complete existing reviewed implementation-profile declaration |
| `host_policy` | Complete frozen evaluator policy: `cpus`, `siblings`, `max_busy_fraction`, `max_core_fraction`, `max_cpu_c`, `max_temperature_span_c`, `window_seconds`, `consecutive_windows`, `timeout_seconds`, `max_other_cpu_fraction`, `aa_max_span_fraction`. The existing three-fraction proposal remains blocked. The evaluator separately checks live affinity and admits runs. |
| `layout_size` | Fixed logical presentation surface width and height |
| `renderer_environment` | Explicit shared string environment, for example EGL backend |
| `runtimes` | `original` and `current` objects described below |
| `titles` | Exactly one object per required title |

Each runtime contains `identities` with lowercase SHA-256 values for
`executable`, `build_receipt` and `compiler`, plus a `capability` declaration:

```json
{
  "schema_version": 1,
  "runtime_sha256": "REPLACE_WITH_EXECUTABLE_SHA256",
  "internal_scales": [1.0],
  "aspects": ["Original", "FourThree"],
  "layouts": ["Original", "TopOnly", "SideBySide"],
  "device_audio": false,
  "effective_settings_reporting": false,
  "observed_internal_scale_reporting": false,
  "presentation_capture_reporting": false
}
```

These are explicit support declarations bound to an executable, not evidence
that a game ran. Do not set reporting/audio flags true until the corresponding
adapter exists. A current probe run suppresses device audio and captures native
source pixels before final presentation. DSP mixing, nonempty samples or native
BMP dimensions therefore cannot satisfy these four reporting/audio checks.
Requested resolution alone also cannot prove the observed render-target sizes.

Each title contains `id`, `identities` and `scenes`. Title identities require
SHA-256 values for `rom`, `title_profile`, `original_library`, `current_library`,
`original_codegen` and `current_codegen`. The codegen identity should bind the
compiler command/options and source provenance, not merely a branch name.
Runtime-only comparisons require the same native library on both sides.
Native-library comparisons require identical runtime/build/compiler identities;
their game libraries deliberately differ. This separate planning factor does
not relax the existing public runtime comparator's same-library gate.

Each scene contains exactly these fields:

```json
{
  "id": "route-walk",
  "phase": "warmed-gameplay",
  "available": true,
  "cache": "warm",
  "frames": 6000,
  "measure_start": 1800,
  "identities": {
    "seed_data": "REPLACE_WITH_SEED_INVENTORY_SHA256",
    "input": "REPLACE_WITH_INPUT_SHA256",
    "scene_review": "REPLACE_WITH_SCENE_REVIEW_SHA256",
    "clock": "REPLACE_WITH_FIXED_CLOCK_RECORD_SHA256",
    "original_cache_seed": "REPLACE_WITH_ORIGINAL_CACHE_INVENTORY_SHA256",
    "current_cache_seed": "REPLACE_WITH_CURRENT_CACHE_INVENTORY_SHA256"
  }
}
```

Paths and private data are resolved by the future adapter; this portable plan
uses identities only. Directory identities must bind complete sorted file
inventories; a path string alone is not an identity. The clock record binds
the actual clock value. Cache seeds belong to their respective builds and may
differ. `cold-start` requires fresh application caches and `measure_start: 0`;
it makes no disk-cold claim. `warmed-gameplay` requires a separately prepared
warm application-cache seed. `transition` is another independently validated
fixture, not an automatic rename of the route walk.

Uncaptured scenes stay explicit with `available: false`, `identities: null`,
`frames: null` and `measure_start: null`. Their phase and requested cache policy
remain recorded, but the planner creates no workload or scene evidence for
them. Existing route receipts are native-scale source-image reviews; they do
not automatically approve another scale, final layout, transition or audio.

## Execution adapter and admission remain separate

An adapter must map `settings.frontend_settings.resolution/aspect/layout/swapped`
to the actual runtime, fix the remaining settings, and report settings actually
used. The requested `internal_scale` is independent of `layout_size`. Report
observed internal surfaces, native source captures and final presented captures
under separate scopes; do not use native source BMP sizes as proof of either
the presentation shape or internal resolution.

Every planned cell requests audio device playback, normal speed and zero
frameskip. It therefore needs a paced audio-capable runner with a bounded
completed-work observer. Do not send audio from the existing uncapped probe
and call that normal playback. Require device-open success, submitted/played
frames, queue/discard/underrun accounting with backend limits stated, and
separate audio verification. A device that cannot report underruns must say
they were not measured.

Compare Original/Current only within the same cell. Retain equal guest progress,
exact sampled images/traces at matching scopes, matching quality, all failed
trials and the [existing host/noise policy](../../docs/consistent-evals.md).
Lower scale, a different aspect or hidden screen is a workload change, not
evidence of an optimization. No held observation becomes a gain merely because
its profile is in this matrix. Desktop results and modeled handheld constraints
remain distinct from RG DS Plus hardware evidence.


`frame_geometry.py` metadata validation reports declarations, not observations.
For byte comparison, pass `--baseline-dir` and `--candidate-dir`; each directory
must contain `guest-trace.toml` and `<scope>-<checkpoint>.bmp` for every capture.
Complete BMP and trace bytes are hashed and checked, but stage, screen roles and
trace-to-checkpoint linkage remain unverified. Probe readback is before
presentation. Current software and GLES readback scale getters return 1; the
integer-scaled geometry model is a producer-contract model, not a claim that
current GLES supplies a higher-scale image or evidence of internal allocation.

## Bounded native 1x presentation control

The separate `presentation_geometry.py` model checks observed single landscape
panel areas and target-pixel viewports for BigTop, Original, TopOnly and
SideBySide with Original or FourThree aspect, including swapped screen roles.
It uses frontend f32 arithmetic and GLES endpoint rounding. A result of
`observed_numbered_presentations_only` means the parsed settings, observed
geometry and indexed retained PPMs agree for the presentations actually
recorded; it does not bind a presentation to the probe's requested frame.
Unobserved presentations, physical scanout, rotation, second windows, portrait
touch controls, screen content and padded surface sampling remain unknown.
Unsupported or contradictory geometry is not admitted as this partial capability.
`matrix_presentation_capability_admitted` remains false. The existing render
`admitted` field retains its scale/settings/panel-byte scope; inspect the
separate geometry result rather than treating it as full matrix admission.
Render receipts pin the geometry source bytes actually compiled by the adapter.
The evaluator checks that identity before and after execution and when reopening
receipts; historical render receipts without this support identity are not
accepted by the new validator. A model file changed during a run invalidates it.

Root-run plan, pending a geometry-enabled runtime and exact existing scene and
binary receipts: start with one native 1x BigTop/Original, unswapped, 640x480
numbered PPM control, 8 frames, one repeat, geometry diagnostic, NON-TIMING.
The panel is 921,615 bytes per presentation; preflight for eight is 7,372,920
bytes retained. The capture's `max_bytes` can be set to that amount, but this
is an admission limit, not a live write quota. Startup and presentation rate
may exceed eight writes, and the harness does not enforce cumulative I/O. Do
not launch until root has a separately enforced cumulative cap (for example a
bounded filesystem or an appropriate runtime write cap) and has budgeted all
other artifacts. Abort on a cap or resource breach; do not call an estimated
8-frame write count an enforced limit. No runtime execution is authorized by
this plan itself.

The control declaration is:

```json
{"schema_version":1,"id":"native1-bigtop-control","internal_scale":1,"aspect":"Original","layout":"BigTop","swapped":false,"stretched":"Off","presentation_capture":{"panel_size":[640,480],"file":"panel-%.ppm","retain":"numbered","max_bytes":7372920}}
```

Use `{"schema_version":1,"geometry":true}` for the diagnostic profile and
`--resolution 1 --frames 8 --repeats 1`, along with root's exact bound runtime,
library, scene, seed and output arguments. Check retained receipts, parsed
settings, surface scale, each recorded index and the separate geometry result.
Only after the control is accepted, run at most one cell at a time with the
same budget and bindings: BigTop/FourThree, Original/Original,
TopOnly/Original, SideBySide/Original, then any explicitly needed swap. Each
cell is dependent on the previous evidence and is not timing evidence. Leave
unrun cells pending; no 24-cell 6000-frame panel campaign is implied.
