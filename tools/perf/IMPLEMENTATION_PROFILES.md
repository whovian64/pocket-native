# Declared implementation profiles

Pass `--implementation-profile profile.json` to `evaluate.py run` on **both** sides of a comparison. The runner accepts a closed list of reviewed implementation choices and generates `data/settings.toml`. Seed data/config directories must not contain `settings.toml` in this mode; all their remaining bytes, title profiles, input, clock, resolution, frameskip, renderer, CPU affinity, cgroup envelope and noise budget still have to match.

An example declaration (replace the review metadata with the actual review):

```json
{
  "schema_version": 1,
  "id": "reviewed-original-monolithic",
  "reviewed_by": "reviewer identity",
  "rationale": "Original rendering configuration under serialized GPU execution",
  "constraints": {
    "gpu_thread": false,
    "generic_shaders": false,
    "generic_lighting": false,
    "stand_ins": false
  },
  "knobs": {
    "shader_linking": "monolithic",
    "background_shaders": true,
    "texture_cache_mib": null,
    "gles_ring_kib": null,
    "rom_cache_mib": null,
    "allocator": {"kind": "system"}
  }
}
```

`constraints` must be identical between runs. Generic shaders, generic lighting and approximate stand-ins must remain false. GPU threading is an explicit fixed determinism constraint, not a permitted comparison difference. The experiment still records resolution and forces frameskip zero.

Reviewed knobs may differ:

| Knob | Accepted values | Effective proof |
|---|---|---|
| `shader_linking` | `monolithic`, `separable` | Runtime switch log; pipeline fallback rejected |
| `background_shaders` | Boolean | Synchronous-compilation switch log when false |
| `texture_cache_mib` | `null` or integer 1..256 | Explicit budget log when requested |
| `gles_ring_kib` | `null` or integer 128..8192 | Initial mutable-ring request log, or explicit persistent-storage ignore log |
| `rom_cache_mib` | `null` or integer 0..1024 | Successful backend initialization log for nonzero budgets |
| `allocator` | Omitted/system or declared GNU preload | File hash before/after, exact environment and observed child mapping |
| `release_shader_compiler` | Omitted or `false` | Compatibility field; `true` and stale release-hint controls/logs are rejected |

`null` retains the selected runtime's default and exports no corresponding override. ROM zero explicitly disables its cache. Ring sizes are initial capacities, can grow on demand, and preserve backend alignment/table minimums. Persistent rings retain their original immutable storage; an ignored cap is reported explicitly. ROM caching uses completed-read, best-effort page advice; a successful configuration log does not establish a hard kernel page-residency limit. A requested nonzero ROM cap on an unsupported backend fails the effective-configuration gate.

The experimental shader-compiler release hint was removed after it showed no admitted benefit. The false-only field preserves existing profile declarations. Historical experimental receipts require their frozen harness; this version does not accept a request for that removed feature.

For jemalloc replace the allocator object with an absolute, canonical library path, its real SHA-256 and explicit configuration:

```json
{
  "kind": "gnu_preload",
  "name": "jemalloc",
  "path": "/usr/lib/libjemalloc.so.2",
  "sha256": "REPLACE_WITH_THE_ACTUAL_64_CHARACTER_LOWERCASE_SHA256",
  "configuration": {
    "MALLOC_CONF": "background_thread:true,narenas:2,dirty_decay_ms:100,muzzy_decay_ms:0,retain:false,thp:never"
  }
}
```

The accepted jemalloc options are `background_thread`, `retain` (true/false), `narenas` (1..256), `dirty_decay_ms` and `muzzy_decay_ms` (-1 or 0..60000), and `thp` (`default`/`never`). Unknown or repeated options are rejected. `thp:never` changes this process's allocator policy; it does not change the host THP setting. Mapping proof demonstrates loading, while allocator internal option state is not independently introspected.

Mimalloc is available only as an explicitly acknowledged **diagnostic-only** preload. Its configuration requires `MIMALLOC_ALLOW_THP` ("0"/"1"/"2"), `MIMALLOC_PURGE_DELAY` (decimal "0".."60000"), and `MIMALLOC_PURGE_DECOMMITS` ("0"/"1"). Its object must also contain `"diagnostic_only": true` and `"acknowledged_limitations": ["calloc overflow can return NULL with errno 0 instead of ENOMEM"]`. The comparator reports that limitation and withholds final `target_met` even if its measured ratios pass. System and jemalloc profiles remain eligible for the formal target.

Each manifest retains the source declaration identity, normalized declaration/hash, generated settings hash, exact environment, runtime log checks, allocator identities/mapping proof and profile-support source hash. The source file and allocator must remain unchanged during the run. The comparator reports all declaration differences and effective configuration summaries. Missing profile metadata/proof, mixed profile-free/profile runs, or differing fixed constraints are incomparable. There is no ignore-fields facility and no arbitrary environment map.

The comparator independently rechecks execution exit/timeout/memory flags, finite metrics and CPU totals, the full probe report shape/completion/stop/skipping facts, image hash inventory, fixed workload and renderer environment, and captured warning/error entries. A stored `valid=true` cannot override contradictory facts. Normal warnings are retained and must match exactly by message, count and first frame; no warning class is silently ignored. Screenshot cadence uses the same policy as `Probe::shot_frames`; the public runner continues to require at least eight frames.

The existing requirements remain: at least three valid repetitions, exact sampled images, exact picture/frame/instruction counts, identical actual renderer and host/resource envelope, known host activity within budget, and conservative candidate/baseline ratios at most 0.5 for wall time, CPU time **and** peak RSS. Final `target_met` also requires correctly bound, reviewed evidence of actual gameplay; the receipt must identify the reviewer. An agent's visual review must be labeled as such. The current 18,000-frame Ultra Sun prologue remains cutscene evidence with `verified_gameplay=false`; declaring a profile does not change that status or simulate ARM hardware.
