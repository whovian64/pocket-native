#!/usr/bin/env python3
"""Fixed-budget Linux A/A and paired A/B campaign. Private inputs stay local.

A JSON plan binds every executable, DSO, workload, profile, cache and host gate.
No global machine tuning, user-process termination, retries or ROM publication.
"""

import argparse
import copy
import fcntl
import json
import math
import os
from pathlib import Path
import random
import statistics
import subprocess
import sys

import check_determinism
import evaluate
import host_controls

ROOT = Path(__file__).resolve().parents[2]


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def path(value):
    p = Path(value)
    return p.resolve() if p.is_absolute() else (ROOT / p).resolve()


def schedule(plan):
    """Balanced within title/lane, seeded first order; no adaptive trial count."""
    rng = random.Random(plan["order_seed"])
    result = []
    for lane in plan["lanes"]:
        if lane["cache"] == "warm":
            for title in plan["titles"]:
                for variant in ("original", "current"):
                    result.append(
                        {
                            "title": title["id"],
                            "lane": lane["id"],
                            "kind": "warmup",
                            "variant": variant,
                            "pair": 0,
                        }
                    )
    for lane in plan["lanes"]:
        for title in plan["titles"]:
            for index in range(plan["aa_repeats"]):
                result.append(
                    {
                        "title": title["id"],
                        "lane": lane["id"],
                        "kind": "AA",
                        "variant": "current",
                        "pair": index + 1,
                    }
                )
    first = {
        (title["id"], lane["id"]): rng.randrange(2)
        for title in plan["titles"]
        for lane in plan["lanes"]
    }
    for pair in range(1, plan["pairs"] + 1):
        for lane in plan["lanes"]:
            for title in plan["titles"]:
                variants = ["original", "current"]
                if (first[title["id"], lane["id"]] + pair) % 2:
                    variants.reverse()
                for variant in variants:
                    result.append(
                        {
                            "title": title["id"],
                            "lane": lane["id"],
                            "kind": "AB",
                            "variant": variant,
                            "pair": pair,
                        }
                    )
    for index, row in enumerate(result, 1):
        row["name"] = (
            f"{index:03}-{row['title']}-{row['lane']}-{row['kind'].lower()}{row['pair']}-{row['variant']}"
        )
    return result


def validate(plan):
    if (
        plan.get("schema_version") != 1
        or not 3 <= plan["pairs"] <= 30
        or not 3 <= plan["aa_repeats"] <= 30
    ):
        raise ValueError("schema 1, 3..30 pairs and 3..30 A/A observations required")
    if (
        not isinstance(plan["order_seed"], int)
        or not plan["titles"]
        or not plan["lanes"]
    ):
        raise ValueError("fixed order seed, titles and lanes required")
    for group in ("titles", "lanes"):
        names = [x["id"] for x in plan[group]]
        if len(set(names)) != len(names) or any(
            not isinstance(n, str)
            or not n
            or any(c not in "abcdefghijklmnopqrstuvwxyz0123456789-" for c in n)
            for n in names
        ):
            raise ValueError("unique lowercase path-safe ids required")
    for t in plan["titles"]:
        if not 0 <= t["measure_start"] < t["frames"] <= 100000:
            raise ValueError("invalid workload measurement interval")
        for key in ("rom", "library", "input", "scene", "seed_data"):
            path(t[key]).resolve(strict=True)
    for variant in ("original", "current"):
        for key in ("executable", "build_metadata"):
            path(plan[variant][key]).resolve(strict=True)
    for lane in plan["lanes"]:
        if lane["cache"] not in ("fresh", "warm"):
            raise ValueError("cache must be fresh or warm")
        profile = evaluate.profile_rules.validate(
            json.loads(path(lane["profile"]).read_text())
        )
        if (
            lane["role"] not in ("equal-work", "async-diagnostic")
            or lane["role"] == "equal-work"
            and profile["constraints"]["gpu_thread"]
        ):
            raise ValueError("equal-work lane must serialize the GPU")
    host = plan["host_policy"]
    if not host["cpus"] or any(
        cpu not in os.sched_getaffinity(0) for cpu in host["cpus"]
    ):
        raise ValueError("requested affinity unavailable")
    for key in ("max_busy_fraction", "max_core_fraction"):
        if (
            not isinstance(host[key], (int, float))
            or not math.isfinite(host[key])
            or not 0 <= host[key] <= 1
        ):
            raise ValueError("invalid host fraction")
    if (
        not 0 < host["window_seconds"] <= 10
        or not host["window_seconds"] <= host["timeout_seconds"] <= 60
        or not 1 <= host["consecutive_windows"] <= 5
    ):
        raise ValueError("preflight must be bounded by 60 seconds")
    if (
        not 0 < plan["max_other_cpu_fraction"] <= 0.10
        or not 0 < plan["aa_max_span_fraction"] <= 0.10
    ):
        raise ValueError("host/AA acceptance cannot exceed 10%")
    if not 0 < host["max_temperature_span_c"] <= 20 or not 0 < host["max_cpu_c"] <= 110:
        raise ValueError("invalid temperature policy")


def aggregate(manifests):
    first = copy.deepcopy(manifests[0])
    first["trials"] = [trial for m in manifests for trial in m["trials"]]
    first["repeats"] = len(first["trials"])
    return first


def validate_output(plan, output):
    protected = [path(plan["titles_directory"])]
    protected += [path(t["seed_data"]) for t in plan["titles"]]
    if any(output.is_relative_to(directory) for directory in protected):
        raise ValueError(
            "campaign output cannot be inside immutable seed/profile directories"
        )


def effect(original, current):
    ratios = [b / a for a, b in zip(original, current)]
    result = {
        "original": original,
        "current": current,
        "paired_ratios": ratios,
        "original_median": statistics.median(original),
        "current_median": statistics.median(current),
        "paired_ratio_geomean": math.exp(statistics.mean(map(math.log, ratios))),
        "paired_ratio_range": [min(ratios), max(ratios)],
    }
    # Resample independent PROCESS PAIRS, never individual correlated frames.
    if len(ratios) >= 5:
        rng = random.Random(0)
        logs = list(map(math.log, ratios))
        means = sorted(
            math.exp(statistics.mean(rng.choices(logs, k=len(logs))))
            for _ in range(5000)
        )
        result["exploratory_pair_bootstrap_95pct"] = [means[124], means[4874]]
    else:
        result["confidence_interval"] = None
        result["confidence_note"] = "fewer than five process pairs; no precision claim"
    return result


def analyze(output, plan, rows, input_changes=()):
    results = {}
    raw_errors = []
    for row in rows:
        if (
            row.get("manifest")
            and evaluate.digest(path(row["manifest"])) != row["manifest_sha256"]
        ):
            raw_errors.append("raw manifest changed: " + row["name"])
    for title in plan["titles"]:
        for lane in plan["lanes"]:
            aa = [
                r
                for r in rows
                if r["title"] == title["id"]
                and r["kind"] == "AA"
                and r["lane"] == lane["id"]
                and r.get("manifest")
            ]
            aa_manifests = [json.loads(path(r["manifest"]).read_text()) for r in aa]
            aa_valid = len(aa) == plan["aa_repeats"] and all(
                m["status"] == "complete" for m in aa_manifests
            )
            aa_spans = {}
            if aa_valid:
                aa_valid = check_determinism.check(aggregate(aa_manifests))[
                    "deterministic"
                ]
                aa_signatures = [
                    (
                        m["trials"][0]["probe"]["warnings_errors"]["entries"],
                        m["trials"][0]["renderer_identity"],
                    )
                    for m in aa_manifests
                ]
                aa_valid &= all(s == aa_signatures[0] for s in aa_signatures)
                aa_valid &= not any(
                    evaluate.validate_trial_receipt(m, t)
                    for m in aa_manifests
                    for t in m["trials"]
                )
                for metric in ("wall_seconds", "cpu_seconds"):
                    values = [m["trials"][0]["metrics"][metric] for m in aa_manifests]
                    aa_spans[metric] = (max(values) - min(values)) / statistics.mean(
                        values
                    )
                aa_valid &= all(
                    x <= plan["aa_max_span_fraction"] for x in aa_spans.values()
                )
                for metric in ("wall_seconds", "process_cpu_seconds"):
                    values = [
                        m["trials"][0]["batch_telemetry"]["summary"]["measurement"][
                            metric
                        ]
                        for m in aa_manifests
                    ]
                    aa_spans["measurement_" + metric] = (
                        max(values) - min(values)
                    ) / statistics.mean(values)
                aa_valid &= all(
                    x <= plan["aa_max_span_fraction"] for x in aa_spans.values()
                )
            selected = [
                r
                for r in rows
                if r["title"] == title["id"]
                and r["lane"] == lane["id"]
                and r["kind"] == "AB"
            ]
            groups = {
                v: [r for r in selected if r["variant"] == v and r.get("manifest")]
                for v in ("original", "current")
            }
            item = {
                "AA_pass": bool(aa_valid),
                "AA_span_over_mean": aa_spans,
                "status": "held",
                "errors": list(raw_errors)
                + (["campaign input changed"] if input_changes else []),
                "raw_runs": selected,
            }
            results[title["id"] + "/" + lane["id"]] = item
            if any(len(g) != plan["pairs"] for g in groups.values()):
                item["errors"].append("incomplete fixed repetition budget")
                continue
            manifests = {
                v: [json.loads(path(r["manifest"]).read_text()) for r in g]
                for v, g in groups.items()
            }
            composites = {v: aggregate(m) for v, m in manifests.items()}
            # Warm seed bytes legitimately differ by executable. Keep raw
            # receipts intact and expose this analytical normalization. Each
            # seed is generated by that variant's declared full-work warmup.
            if lane["cache"] == "warm":
                warmups = [
                    r
                    for r in rows
                    if r["title"] == title["id"]
                    and r["lane"] == lane["id"]
                    and r["kind"] == "warmup"
                ]
                if len(warmups) != 2 or any(
                    r.get("status") != "complete" for r in warmups
                ):
                    item["errors"].append("missing per-build cache preparation")
                for v in composites:
                    prepared = next(
                        (
                            r.get("prepared_cache_inventory")
                            for r in warmups
                            if r["variant"] == v
                        ),
                        None,
                    )
                    if prepared is None or any(
                        m["experiment"].get("seed_cache") != prepared
                        for m in manifests[v]
                    ):
                        item["errors"].append(
                            "cache seed differs from that build's prepared inventory"
                        )
                for v in composites:
                    composites[v]["experiment"].pop("seed_cache", None)
                item["analytical_normalization"] = (
                    "only per-build warmed cache inventory excluded from experiment equality; raw inventories/warmup manifests retained"
                )
            comparison = evaluate.compare_values(
                composites["original"], composites["current"]
            )
            item["comparison"] = comparison
            item["errors"] += comparison["errors"]
            if not aa_valid:
                item["errors"].append(
                    "A/A missing, unequal work or above declared spread budget"
                )
            if any(not r.get("preflight_passed") for r in selected + aa):
                item["errors"].append(
                    "quiet/thermal preflight failed; diagnostic observations only"
                )
            if any(r.get("host_observation_errors") for r in selected + aa):
                item["errors"].append(
                    "during-run thermal/power/governor/SMT observation failed"
                )
            if any(
                not r.get("preflight_passed") or r.get("host_observation_errors")
                for r in aa
            ):
                item["errors"].append("A/A host observations ineligible")
            if any(
                t.get("host_activity", {}).get("other_cpu_fraction", 1)
                > plan["max_other_cpu_fraction"]
                for m in aa_manifests
                for t in m["trials"]
            ):
                item["errors"].append("A/A host activity exceeds budget")
            for v in groups:
                for m in manifests[v]:
                    if any(evaluate.validate_trial_receipt(m, t) for t in m["trials"]):
                        item["errors"].append("invalid raw trial receipt")
            if lane["role"] == "async-diagnostic":
                item["errors"].append(
                    "asynchronous diagnostic lane; no equal-work speedup claim"
                )
            item["status"] = "admitted" if not item["errors"] else "held"
            item["strict_3pct_pass"] = all(
                t["host_activity"].get("other_cpu_fraction", 1) <= 0.03
                for m in sum(manifests.values(), [])
                for t in m["trials"]
            )
            item["observations"] = {}
            for metric in ("wall_seconds", "cpu_seconds", "peak_rss_kib"):
                values = {
                    v: [m["trials"][0]["metrics"][metric] for m in manifests[v]]
                    for v in manifests
                }
                item["observations"][metric] = effect(
                    values["original"], values["current"]
                )
            for metric in ("wall_seconds", "process_cpu_seconds"):
                values = {
                    v: [
                        m["trials"][0]["batch_telemetry"]["summary"]["measurement"][
                            metric
                        ]
                        for m in manifests[v]
                    ]
                    for v in manifests
                }
                item["observations"]["measurement_" + metric] = effect(
                    values["original"], values["current"]
                )
            work = [
                m["trials"][0]["batch_telemetry"]["summary"]["measurement"][
                    "guest_work_delta"
                ]
                for m in sum(manifests.values(), [])
            ]
            if any(w != work[0] for w in work):
                item["errors"].append("measurement-window guest work differs")
                item["status"] = "held"
    result = {
        "schema_version": 1,
        "series": results,
        "scope": "all trials retained; held observations are not accepted gains; process RSS excludes GPU memory/power; phase timings are run_frame only",
    }
    write(output / "analysis.json", result)
    return result


def run(args):
    plan = json.loads(args.plan.read_text())
    validate(plan)
    output = args.output.resolve()
    validate_output(plan, output)
    output.mkdir(parents=True, exist_ok=False)
    lock_path = path(plan["lock_file"])
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        jobs = schedule(plan)
        inputs = [
            args.plan,
            *[
                Path(__file__).with_name(n)
                for n in (
                    "paired.py",
                    "evaluate.py",
                    "host_controls.py",
                    "batch_metrics.py",
                    "implementation_profiles.py",
                    "check_determinism.py",
                )
            ],
        ]
        for v in ("original", "current"):
            inputs += [path(plan[v][key]) for key in ("executable", "build_metadata")]
        for t in plan["titles"]:
            inputs += [path(t[key]) for key in ("rom", "library", "input", "scene")]
        inputs += [path(lane["profile"]) for lane in plan["lanes"]]
        freeze = {
            "plan": plan,
            "schedule": jobs,
            "diagnostic_on_hold": args.diagnostic_on_hold,
            "sources": [evaluate.identity(p) for p in inputs],
            "topology": host_controls.topology(plan["host_policy"]["cpus"]),
            "note": "file hashing warms OS pages; not disk-cold; all settings fixed before measurements",
        }
        write(output / "freeze.json", freeze)
        rows = []
        cache_seeds = {}
        cache_identities = {}
        for row in jobs:
            t = next(t for t in plan["titles"] if t["id"] == row["title"])
            lane = next(l for l in plan["lanes"] if l["id"] == row["lane"])
            variant = plan[row["variant"]]
            destination = output / row["name"]
            preflight = host_controls.preflight(plan["host_policy"])
            write(output / (row["name"] + "-preflight.json"), preflight)
            receipt = {**row, "preflight_passed": preflight["passed"]}
            last_attempt = preflight["attempts"][-1] if preflight["attempts"] else None
            resource_busy = last_attempt and (
                last_attempt["before"].get("active_jobs")
                or last_attempt["after"].get("active_jobs")
            )
            if not preflight["passed"] and (
                not args.diagnostic_on_hold or resource_busy
            ):
                rows.append({**receipt, "status": "held-preflight"})
                write(output / "progress.json", rows)
                break
            command = [
                sys.executable,
                str(Path(evaluate.__file__)),
                "run",
                "--rom",
                str(path(t["rom"])),
                "--library",
                str(path(t["library"])),
                "--executable",
                str(path(variant["executable"])),
                "--build-metadata",
                str(path(variant["build_metadata"])),
                "--output",
                str(destination),
                "--seed-data",
                str(path(t["seed_data"])),
                "--titles",
                str(path(plan["titles_directory"])),
                "--implementation-profile",
                str(path(lane["profile"])),
                "--scene-evidence",
                str(path(t["scene"])),
                "--frames",
                str(t["frames"]),
                "--repeats",
                "1",
                "--timeout",
                str(plan["timeout_seconds"]),
                "--clock",
                str(plan["clock"]),
                "--input",
                path(t["input"]).read_text().strip(),
                "--resolution",
                "1",
                "--cpu-affinity",
                ",".join(map(str, plan["host_policy"]["cpus"])),
                "--max-other-cpu-fraction",
                str(plan["max_other_cpu_fraction"]),
                "--batch-telemetry",
                "--measure-start",
                str(t["measure_start"]),
                "--capture",
                "boundaries",
                "--driver-cache",
                "off",
            ]
            key = (row["title"], row["lane"], row["variant"])
            if lane["cache"] == "warm" and row["kind"] != "warmup":
                if evaluate.tree_identity(cache_seeds[key]) != cache_identities[key]:
                    raise ValueError("warmed cache seed changed after preparation")
                command += ["--seed-cache", str(cache_seeds[key])]
            write(output / (row["name"] + "-command.json"), command)
            env = dict(os.environ)
            for k in evaluate.RENDERER_ENV:
                env.pop(k, None)
            env.update(plan.get("renderer_environment", {}))
            with host_controls.Monitor(
                output / (row["name"] + "-host.json")
            ) as monitor:
                with (output / (row["name"] + "-driver.log")).open("x") as log:
                    completed = subprocess.run(
                        command,
                        cwd=ROOT,
                        env=env,
                        stdout=log,
                        stderr=subprocess.STDOUT,
                        timeout=plan["timeout_seconds"] + 60,
                    )
            receipt.update(exit_code=completed.returncode, status="failed")
            receipt["host_observation_errors"] = host_controls.observation_errors(
                monitor.samples, plan["host_policy"]
            )
            if (destination / "manifest.json").exists():
                m = json.loads((destination / "manifest.json").read_text())
                receipt.update(
                    manifest=str(destination / "manifest.json"),
                    manifest_sha256=evaluate.digest(destination / "manifest.json"),
                    status=m["status"],
                )
                if m["status"] == "complete" and row["kind"] == "warmup":
                    cache_seeds[key] = destination / "trial-01/cache"
                    cache_identities[key] = evaluate.tree_identity(cache_seeds[key])
                    receipt["prepared_cache_inventory"] = cache_identities[key]
            rows.append(receipt)
            write(output / "progress.json", rows)
            print(
                json.dumps(
                    {
                        "run": row["name"],
                        "status": receipt["status"],
                        "quiet": preflight["passed"],
                    }
                ),
                flush=True,
            )
            if receipt["status"] != "complete":
                break
        changed = [
            i["path"]
            for i in freeze["sources"]
            if evaluate.identity(Path(i["path"])) != i
        ]
        changed += [
            str(p)
            for key, p in cache_seeds.items()
            if evaluate.tree_identity(p) != cache_identities[key]
        ]
        write(output / "input-integrity.json", {"changed": changed})
        analyze(output, plan, rows, changed)
        return (
            0
            if len(rows) == len(jobs)
            and not changed
            and all(r["status"] == "complete" for r in rows)
            else 2
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--plan", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--diagnostic-on-hold",
        action="store_true",
        help="preserve rejected preflight and collect bounded diagnostic work anyway; NEVER admit its timings",
    )
    args = parser.parse_args()
    try:
        return run(args)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        parser.exit(2, str(error) + "\n")


if __name__ == "__main__":
    raise SystemExit(main())
