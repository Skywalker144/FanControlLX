#!/usr/bin/env python3
"""Read-only benchmark of the installed service, including its GPU worker.

CPU is measured from cgroup counters, with 100% meaning one full CPU core.
Memory is the service cgroup's current usage, not the sum of process RSS.
No API requests are made during each resource-measurement window.
"""
import argparse
import hashlib
import json
from pathlib import Path
import statistics
import subprocess
import time
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--label", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--windows", type=int, default=3)
    parser.add_argument("--seconds", type=int, default=30)
    parser.add_argument("--url", default="http://127.0.0.1:8090")
    args = parser.parse_args()
    if not 1 <= args.windows <= 20 or not 1 <= args.seconds <= 300:
        parser.error("Use 1–20 windows and 1–300 seconds per window")
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def state():
        with opener.open(args.url + "/api/state", timeout=10) as response:
            payload = response.read()
            return json.loads(payload), len(payload)

    properties = dict(line.split("=", 1) for line in subprocess.check_output(
        ["systemctl", "show", "fancontrollx.service", "-p", "MainPID", "-p", "ControlGroup",
         "-p", "ActiveEnterTimestamp", "-p", "ExecStart"], text=True).splitlines())
    cg = Path("/sys/fs/cgroup") / properties["ControlGroup"].lstrip("/")

    def usage():
        return int(dict(line.split() for line in (cg / "cpu.stat").read_text().splitlines())["usage_usec"])

    def memory():
        return int((cg / "memory.current").read_text()) / 1048576

    initial, size = state()
    result = {
        "label": args.label, "measured_at_unix": time.time(), "version": initial["version"],
        "runtime": initial.get("runtime", "python"), "service": properties,
        "config_sha256": hashlib.sha256(json.dumps(initial["config"], sort_keys=True).encode()).hexdigest(),
        "active_fans": sum(o["active"] for o in initial["outputs"].values()),
        "automatic_fans": sum(o.get("active", False) and o.get("automatic", False) for o in initial["outputs"].values()),
        "sensors": len(initial["sensors"]), "channels": len(initial["channels"]),
        "initial_faults": {k: v["fault"] for k, v in initial["outputs"].items() if v["fault"]},
        "full_state_bytes": size, "windows": [],
        "method": "cgroup v2 CPU and memory; no benchmark HTTP traffic during sampling; 100% CPU = one core; browser excluded",
    }
    for index in range(args.windows):
        start = time.monotonic()
        cpu = usage()
        samples = [memory()]
        for _ in range(args.seconds):
            time.sleep(1)
            samples.append(memory())
        elapsed = time.monotonic() - start
        cpu_s = (usage() - cpu) / 1e6
        window = {"seconds": elapsed, "cpu_seconds": cpu_s, "cpu_percent_one_core": 100 * cpu_s / elapsed,
                  "memory_MiB_min": min(samples), "memory_MiB_median": statistics.median(samples),
                  "memory_MiB_max": max(samples), "memory_samples_MiB": samples}
        result["windows"].append(window)
        print(json.dumps({"label": args.label, "window": index + 1,
                          **{k: v for k, v in window.items() if k != "memory_samples_MiB"}}), flush=True)
    final, _ = state()
    if sum(o["active"] for o in final["outputs"].values()) != result["active_fans"]:
        raise RuntimeError("Active controls changed during benchmark; discard this run")
    if final["config"] != initial["config"]:
        raise RuntimeError("Configuration changed during benchmark; discard this run")
    result["final_faults"] = {k: v["fault"] for k, v in final["outputs"].items() if v["fault"]}
    history = [row["time"] for row in final["history"]]
    intervals = [b - a for a, b in zip(history, history[1:]) if b > a]
    result["control_history_interval_s_median"] = statistics.median(intervals) if intervals else None
    result["control_history_interval_s_max"] = max(intervals) if intervals else None
    result["cpu_percent_one_core_mean"] = statistics.mean(w["cpu_percent_one_core"] for w in result["windows"])
    result["memory_MiB_median"] = statistics.median(s for w in result["windows"] for s in w["memory_samples_MiB"])
    # A separate, short sequential request test; excluded from resource windows.
    timings = []
    for _ in range(30):
        start = time.perf_counter()
        state()
        timings.append((time.perf_counter() - start) * 1000)
    result["full_state_request_ms_median"] = statistics.median(timings)
    result["full_state_request_ms_p95"] = sorted(timings)[int(len(timings) * .95) - 1]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    print(f"Saved {args.output}", flush=True)


if __name__ == "__main__":
    main()
