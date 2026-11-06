"""Read-only Linux benchmark admission and low-rate host observations."""

import json
from pathlib import Path
import shutil
import subprocess
import threading
import time


def read(path):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return None


def cpu_activity():
    result = {}
    for line in Path("/proc/stat").read_text().splitlines():
        fields = line.split()
        if fields and (fields[0] == "cpu" or fields[0][3:].isdigit()):
            v = list(
                map(int, fields[1:9])
            )  # guest time is already included in user/nice
            result[fields[0]] = {"total": sum(v), "busy": sum(v) - v[3] - v[4]}
    return result


def activity_delta(before, after):
    return {
        key: (after[key]["busy"] - value["busy"])
        / max(1, after[key]["total"] - value["total"])
        for key, value in before.items()
        if key in after
    }


def topology(cpus):
    return {
        str(cpu): read(
            f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
        )
        for cpu in cpus
    }


def active_jobs():
    jobs = []
    for directory in Path("/proc").iterdir():
        if not directory.name.isdigit():
            continue
        name = read(directory / "comm")
        status = read(directory / "stat")
        if (
            name in ("cargo", "rustc", "cc1", "cc1plus", "ld.lld", "pocket-native")
            and status
            and status.rsplit(")", 1)[-1].strip().split()[0] != "Z"
        ):
            jobs.append({"pid": int(directory.name), "name": name})
    return jobs


def snapshot():
    sensors = []
    for directory in Path("/sys/class/hwmon").glob("hwmon*"):
        name = read(directory / "name")
        for path in directory.glob("temp*_input"):
            value = read(path)
            if value is not None:
                try:
                    sensors.append(
                        {
                            "name": name,
                            "sensor": str(path),
                            "celsius": int(value) / 1000,
                        }
                    )
                except ValueError:
                    pass
    return {
        "monotonic": time.monotonic(),
        "cpu": cpu_activity(),
        "sensors": sensors,
        "cpu_temperatures_c": [
            s["celsius"]
            for s in sensors
            if s["name"] in ("k10temp", "coretemp", "cpu_thermal")
        ],
        "governors": {
            p.parent.name: read(p)
            for p in Path("/sys/devices/system/cpu/cpufreq").glob(
                "policy*/scaling_governor"
            )
        },
        "scaling_cur_freq_khz": {
            p.parent.name: read(p)
            for p in Path("/sys/devices/system/cpu/cpufreq").glob(
                "policy*/scaling_cur_freq"
            )
        },
        "frequency_scope": "instantaneous driver reports, not measured effective clock residency",
        "boost": read("/sys/devices/system/cpu/cpufreq/boost"),
        "active_jobs": active_jobs(),
        "ac_online": {
            p.parent.name: read(p)
            for p in Path("/sys/class/power_supply").glob("*/online")
        },
        "cpu_psi": read("/proc/pressure/cpu"),
        "io_psi": read("/proc/pressure/io"),
    }


def check_window(before, after, policy):
    fractions = activity_delta(before["cpu"], after["cpu"])
    errors = []
    if before.get("active_jobs") or after.get("active_jobs"):
        errors.append("compiler or game process active before launch")
    if fractions["cpu"] > policy["max_busy_fraction"]:
        errors.append("host CPU busy")
    watched = policy["cpus"] + policy.get("siblings", [])
    if any(
        f"cpu{cpu}" not in fractions
        or fractions[f"cpu{cpu}"] > policy["max_core_fraction"]
        for cpu in watched
    ):
        errors.append("selected core or SMT sibling busy/unknown")
    temperatures = before["cpu_temperatures_c"] + after["cpu_temperatures_c"]
    if not before["cpu_temperatures_c"] or not after["cpu_temperatures_c"]:
        errors.append("CPU temperature unknown")
    elif max(temperatures) > policy["max_cpu_c"]:
        errors.append("CPU above temperature ceiling")
    elif (
        abs(max(before["cpu_temperatures_c"]) - max(after["cpu_temperatures_c"]))
        > policy["max_temperature_span_c"]
    ):
        errors.append("CPU temperature not settled")
    if not before["governors"] or not after["governors"]:
        errors.append("CPU governor state unknown")
    if before["governors"] != after["governors"] or before["boost"] != after["boost"]:
        errors.append("governor/boost changed")
    if not after["ac_online"] or "1" not in after["ac_online"].values():
        errors.append("AC supply unknown/offline")
    return {
        "passed": not errors,
        "errors": errors,
        "fractions": fractions,
        "before": before,
        "after": after,
    }


def preflight(policy):
    deadline = time.monotonic() + policy["timeout_seconds"]
    attempts, consecutive = [], 0
    before = snapshot()
    while time.monotonic() + policy["window_seconds"] <= deadline:
        time.sleep(policy["window_seconds"])
        after = snapshot()
        result = check_window(before, after, policy)
        attempts.append(result)
        consecutive = consecutive + 1 if result["passed"] else 0
        if consecutive >= policy["consecutive_windows"]:
            return {"passed": True, "attempts": attempts}
        before = after
    return {"passed": False, "attempts": attempts}


class Monitor:
    """One-second sysfs/proc samples and optional separate GPU query process.

    Observer CPU is external load in evaluate.py, not charged to the child.
    No persistent system settings, process affinity or user apps are changed.
    """

    def __init__(self, output):
        self.output = Path(output)
        self.stop = threading.Event()
        self.samples = []
        self.thread = None
        self.gpu = None
        self.gpu_file = None

    def __enter__(self):
        if shutil.which("nvidia-smi"):
            self.gpu_file = self.output.with_suffix(".gpu.csv").open("x")
            self.gpu = subprocess.Popen(
                [
                    "nvidia-smi",
                    "--query-gpu=timestamp,name,temperature.gpu,clocks.current.graphics,clocks.current.memory,power.draw,utilization.gpu,pstate",
                    "--format=csv,noheader",
                    "-l",
                    "1",
                ],
                stdout=self.gpu_file,
                stderr=subprocess.DEVNULL,
            )

        def sample():
            while not self.stop.is_set():
                self.samples.append(snapshot())
                self.stop.wait(1)

        self.thread = threading.Thread(target=sample, daemon=True)
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        self.thread.join()
        self.samples.append(snapshot())
        if self.gpu:
            self.gpu.terminate()
            try:
                self.gpu.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.gpu.kill()
                self.gpu.wait()
            self.gpu_file.close()
        self.output.write_text(
            json.dumps(
                {
                    "samples": self.samples,
                    "gpu_csv": str(self.output.with_suffix(".gpu.csv"))
                    if self.gpu
                    else None,
                    "scope": "observer includes evaluator setup/hash time; match monotonic intervals to trial; readings are not energy or effective CPU-clock measurements",
                },
                indent=2,
            )
            + "\n"
        )


def observation_errors(samples, policy):
    """Post-run gate uses the same declared temperature/SMT policy."""
    errors = set()
    if len(samples) < 2:
        errors.add("insufficient host observations")
    for sample in samples:
        temperatures = sample["cpu_temperatures_c"]
        if not temperatures or max(temperatures) > policy["max_cpu_c"]:
            errors.add("CPU temperature unknown/above ceiling during run")
        if not sample["ac_online"] or "1" not in sample["ac_online"].values():
            errors.add("AC supply unknown/offline during run")
        if (
            sample["governors"] != samples[0]["governors"]
            or sample["boost"] != samples[0]["boost"]
        ):
            errors.add("governor/boost changed during run")
        if any(j["name"] != "pocket-native" for j in sample.get("active_jobs", [])):
            errors.add("compiler active during run")
    for a, b in zip(samples, samples[1:]):
        fractions = activity_delta(a["cpu"], b["cpu"])
        if any(
            fractions.get(f"cpu{n}", 1) > policy["max_core_fraction"]
            for n in policy.get("siblings", [])
        ):
            errors.add("SMT sibling contention during run")
    return sorted(errors)
