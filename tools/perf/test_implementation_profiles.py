"""Profile declarations cannot weaken work, quality, provenance or noise gates."""

import copy
import hashlib
import unittest

from test_evaluate import evaluate, fixture, scale_metrics

rules = evaluate.profile_rules


def declaration() -> dict:
    return {"schema_version": 1, "id": "reviewed-baseline", "reviewed_by": "synthetic reviewer",
            "rationale": "test declared optimization boundaries", "constraints": {"gpu_thread": False,
            "generic_shaders": False, "generic_lighting": False, "stand_ins": False},
            "knobs": {"shader_linking": "monolithic", "background_shaders": True,
                      "texture_cache_mib": None, "gles_ring_kib": None, "rom_cache_mib": None}}


def attach(manifest: dict, declared: dict) -> dict:
    profile = rules.validate(declared)
    allocator = profile["knobs"]["allocator"]
    receipt = {"declaration": profile, "declaration_sha256": rules.declaration_hash(profile),
               "source": {"path": "/synthetic/profile.json", "sha256": "a" * 64, "bytes": 100},
               "allocator_before": None}
    manifest["implementation_profile"] = receipt
    manifest["harness"]["implementation_profiles_sha256"] = "b" * 64
    manifest["experiment"]["implementation_constraints"] = copy.deepcopy(profile["constraints"])
    logs = []
    if not profile["constraints"]["gpu_thread"]:
        logs.append("switch: GPU thread off, ZAKURO_SYNC_GPU")
    knobs = profile["knobs"]
    if knobs["shader_linking"] == "separable":
        logs.append("switch: shader stages built apart and put together when drawn, ZAKURO_SEPARABLE_SHADERS")
    if not knobs["background_shaders"]:
        logs.append("switch: shaders compiled when a draw needs them, ZAKURO_SYNC_SHADERS")
    if knobs["texture_cache_mib"] is not None:
        logs.append(f"decoded texture cache budget: {knobs['texture_cache_mib']} MiB (ZAKURO_TEXTURE_CACHE_MIB)")
    if knobs["gles_ring_kib"] is not None:
        logs.append(f"GLES mutable ring initial capacity: 128 KiB (original 8192 KiB, requested cap {knobs['gles_ring_kib']} KiB, target 0x8892); grows on demand")
    if knobs["rom_cache_mib"]:
        logs.append(f"ROM cache enabled: budget_mib={knobs['rom_cache_mib']} chunk_mib=1 host_page_bytes=4096 eviction_scope=old_tracked_chunks code_copy_retirement=true best_effort=true")
    preload = None
    if allocator["kind"] == "gnu_preload":
        identity = {"path": allocator["path"], "sha256": allocator["sha256"], "bytes": 1000}
        receipt.update(allocator_before=identity, allocator_after=copy.deepcopy(identity))
        preload = {"requested_path": allocator["path"], "observed": True,
                   "mapping_lines": [f"1000-2000 r-xp 00000000 00:00 1 {allocator['path']}"]}
    for trial in manifest["trials"]:
        trial["metrics"]["preload_proof"] = copy.deepcopy(preload)
        trial["implementation_profile_proof"] = rules.proof(profile, logs, preload)
        trial["environment"].update(rules.environment(profile))
        trial["generated_settings_sha256"] = hashlib.sha256(rules.settings(profile).encode()).hexdigest()
    return manifest


class ProfileTests(unittest.TestCase):
    def test_support_module_identity_is_fixed_even_without_declared_profiles(self):
        a, b = fixture(), fixture()
        a["harness"]["implementation_profiles_sha256"] = "a" * 64
        b["harness"]["implementation_profiles_sha256"] = "b" * 64
        self.assertEqual(evaluate.compare_values(a, b)["status"], "incomparable")
        b["harness"]["implementation_profiles_sha256"] = "a" * 64
        self.assertEqual(evaluate.compare_values(a, b)["status"], "comparable")

    def test_closed_schema_rejects_quality_unknown_fields_types_and_unbounded_caps(self):
        for mutation in ("quality", "unknown", "review", "bool-cap", "texture", "ring", "rom", "environment", "shader"):
            with self.subTest(mutation=mutation):
                p = declaration()
                if mutation == "quality":
                    p["constraints"]["stand_ins"] = True
                elif mutation == "unknown":
                    p["knobs"]["ignored_fields"] = ["resolution"]
                elif mutation == "review":
                    p["reviewed_by"] = ""
                elif mutation == "bool-cap":
                    p["knobs"]["rom_cache_mib"] = False
                elif mutation == "texture":
                    p["knobs"]["texture_cache_mib"] = 257
                elif mutation == "ring":
                    p["knobs"]["gles_ring_kib"] = 127
                elif mutation == "rom":
                    p["knobs"]["rom_cache_mib"] = 1025
                elif mutation == "environment":
                    p["environment"] = {"ZAKURO_FRAMESKIP": "10"}
                else:
                    p["knobs"]["shader_linking"] = "generic"
                with self.assertRaises(ValueError):
                    rules.validate(p)

    def test_reviewed_knobs_may_differ_and_every_difference_is_reported(self):
        left, right = declaration(), declaration()
        right["id"] = "reviewed-candidate"
        right["knobs"].update(shader_linking="separable", background_shaders=False,
                              texture_cache_mib=16, gles_ring_kib=128, rom_cache_mib=16)
        a, b = attach(fixture(), left), attach(fixture(), right)
        result = evaluate.compare_values(a, b)
        self.assertEqual(result["status"], "comparable", result["errors"])
        fields = {row["field"] for row in result["implementation_profile_comparison"]["differences"]}
        self.assertEqual(fields, {"id", *("knobs." + key for key in right["knobs"])})

    def test_profile_mismatch_tampering_or_missing_effective_proof_is_rejected(self):
        for mutation in ("one-sided", "constraints", "hash", "support", "proof", "settings", "env", "source"):
            with self.subTest(mutation=mutation):
                a, b = attach(fixture(), declaration()), attach(fixture(), declaration())
                if mutation == "one-sided":
                    a = fixture()
                elif mutation == "constraints":
                    p = declaration(); p["constraints"]["gpu_thread"] = True
                    b = attach(fixture(), p)
                elif mutation == "hash":
                    b["implementation_profile"]["declaration"]["knobs"]["background_shaders"] = False
                elif mutation == "support":
                    b["harness"]["implementation_profiles_sha256"] = "c" * 64
                elif mutation == "proof":
                    b["trials"][0]["implementation_profile_proof"]["logs"] = []
                elif mutation == "settings":
                    b["trials"][0]["generated_settings_sha256"] = "bad"
                elif mutation == "env":
                    b["trials"][0]["environment"]["ZAKURO_ROM_CACHE_MIB"] = "1024"
                else:
                    del b["implementation_profile"]["source"]
                result = evaluate.compare_values(a, b)
                self.assertEqual(result["status"], "incomparable", result)
                self.assertFalse(result["target_met"])

    def test_declared_profiles_preserve_all_exact_and_noise_gates(self):
        for mutation in ("pixels", "instructions", "pictures", "frames", "noise", "renderer", "seed", "resolution",
                         "timeout", "exit", "skips", "quality-env", "extra-quality-env"):
            with self.subTest(mutation=mutation):
                a, b = attach(fixture(), declaration()), attach(fixture(), declaration())
                trial = b["trials"][0]
                if mutation == "pixels":
                    trial["probe"]["screenshots"]["frame-00000.bmp"] = "changed"
                elif mutation == "instructions":
                    trial["probe"]["report"]["recompiled_instructions"] += 1
                elif mutation == "pictures":
                    trial["probe"]["report"]["top_pictures"] += 1
                elif mutation == "frames":
                    trial["probe"]["report"]["completed_frames"] -= 1
                elif mutation == "noise":
                    trial["host_activity"]["other_cpu_fraction"] = .11
                elif mutation == "renderer":
                    trial["renderer_identity"] = ["other"]
                elif mutation == "timeout":
                    trial["metrics"]["timed_out"] = True
                elif mutation == "exit":
                    trial["metrics"]["exit_code"] = 1
                elif mutation == "skips":
                    trial["probe"]["report"]["skipped_frames"] = 1
                elif mutation == "quality-env":
                    trial["environment"]["ZAKURO_FRAMESKIP"] = "1"
                elif mutation == "extra-quality-env":
                    trial["environment"]["ZAKURO_GENERIC_SHADERS"] = "1"
                else:
                    b["experiment"][mutation] = "changed"
                self.assertEqual(evaluate.compare_values(a, b)["status"], "incomparable")

    def test_allocator_hash_mapping_config_and_known_diagnostic_limitations(self):
        p = declaration()
        p["knobs"]["allocator"] = {"kind": "gnu_preload", "name": "jemalloc", "path": "/synthetic/libjemalloc.so",
                                   "sha256": "d" * 64, "configuration": {"MALLOC_CONF": "narenas:2,retain:false,thp:never"}}
        a, b = attach(fixture(), declaration()), attach(fixture(), p)
        for trial in b["trials"]:
            scale_metrics(trial, .49)
        self.assertTrue(evaluate.compare_values(a, b)["target_met"])
        broken = copy.deepcopy(b)
        broken["implementation_profile"]["allocator_after"]["sha256"] = "e" * 64
        self.assertEqual(evaluate.compare_values(a, broken)["status"], "incomparable")
        broken = copy.deepcopy(b)
        broken["trials"][0]["metrics"]["preload_proof"]["observed"] = False
        self.assertEqual(evaluate.compare_values(a, broken)["status"], "incomparable")
        allocator = p["knobs"]["allocator"]
        allocator.update(name="mimalloc", diagnostic_only=True, acknowledged_limitations=[rules.MIMALLOC_LIMITATION],
                         configuration={"MIMALLOC_ALLOW_THP": "0", "MIMALLOC_PURGE_DELAY": "0", "MIMALLOC_PURGE_DECOMMITS": "1"})
        b = attach(fixture(), p)
        for trial in b["trials"]:
            scale_metrics(trial, .49)
        result = evaluate.compare_values(a, b)
        self.assertTrue(result["measured_workload_target_met"])
        self.assertFalse(result["target_met"])
        self.assertEqual(result["implementation_profile_comparison"]["candidate"]["known_limitations"], [rules.MIMALLOC_LIMITATION])
        del allocator["acknowledged_limitations"]
        with self.assertRaises(ValueError):
            rules.validate(p)

    def test_jemalloc_config_is_closed_and_thp_policy_is_explicit(self):
        for config in ("thp:always", "narenas:0", "unknown:true", "thp:never,thp:default", "retain:yes"):
            with self.subTest(config=config):
                p = declaration()
                p["knobs"]["allocator"] = {"kind": "gnu_preload", "name": "jemalloc", "path": "/synthetic/libjemalloc.so",
                    "sha256": "d" * 64, "configuration": {"MALLOC_CONF": config}}
                with self.assertRaises(ValueError):
                    rules.validate(p)
        p["knobs"]["allocator"]["configuration"]["MALLOC_CONF"] = "thp:default"
        self.assertEqual(rules.environment(rules.validate(p))["MALLOC_CONF"], "thp:default")

    def test_removed_compiler_hint_rejects_true_and_retains_false_compatibility(self):
        default = rules.validate(declaration())
        explicit = declaration()
        explicit["knobs"]["release_shader_compiler"] = False
        self.assertEqual(default, rules.validate(explicit))
        self.assertEqual(rules.declaration_hash(default), rules.declaration_hash(rules.validate(explicit)))
        self.assertNotIn("ZAKURO_RELEASE_SHADER_COMPILER", rules.environment(default))
        self.assertNotIn("ZAKURO_COMPILERS", rules.environment(default))
        for background in (True, False):
            for value in (True, 0, 1, "false"):
                with self.subTest(background=background, value=value):
                    p = declaration()
                    p["knobs"].update(background_shaders=background, release_shader_compiler=value)
                    with self.assertRaisesRegex(ValueError, "false-only"):
                        rules.validate(p)
        a, b = attach(fixture(), declaration()), attach(fixture(), explicit)
        self.assertEqual(evaluate.compare_values(a, b)["status"], "comparable")
        for line in ("switch: release shader compiler scratch, ZAKURO_RELEASE_SHADER_COMPILER",
                     "shader compiler release hint 1", "ZAKURO_RELEASE_SHADER_COMPILER ignored: unsupported"):
            with self.subTest(stale_log=line):
                stale = copy.deepcopy(b)
                proof = stale["trials"][0]["implementation_profile_proof"]
                proof["logs"].append(line)
                self.assertEqual(evaluate.compare_values(a, stale)["status"], "incomparable")
        for name in ("ZAKURO_RELEASE_SHADER_COMPILER", "ZAKURO_COMPILERS"):
            stale = copy.deepcopy(b)
            stale["trials"][0]["environment"][name] = "1"
            self.assertEqual(evaluate.compare_values(a, stale)["status"], "incomparable")


if __name__ == "__main__":
    unittest.main()
