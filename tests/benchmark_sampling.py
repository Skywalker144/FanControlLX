#!/usr/bin/env python3
"""Compare original Python and Rust read-only hardware sampling.

Export the original commit with git archive to --reference first. This does not
start either controller, acquire fans, or modify hardware. It measures only the
sampling path, so results must not be presented as whole-service savings.
"""
import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys

PYTHON_SAMPLE = r"""
import json,time
from pathlib import Path
from fancontrollx.hardware import Hardware
h=Hardware()
h.discover()
start=time.monotonic();cpu=time.process_time();durations=[]
try:
    for _ in range(COUNT):
        at=time.perf_counter();h.discover();durations.append((time.perf_counter()-at)*1000);time.sleep(1)
    print(json.dumps({"runtime":"python","samples":len(durations),"elapsed_s":time.monotonic()-start,
        "cpu_s":time.process_time()-cpu,"sample_wall_ms":durations,
        "process_status":[line for line in Path("/proc/self/status").read_text().splitlines() if line.startswith(("VmRSS:","VmHWM:"))]}))
finally:
    h.close()
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--samples", type=int, default=20)
    parser.add_argument("--rounds", type=int, default=3)
    args = parser.parse_args()
    if not 1 <= args.samples <= 300 or not 1 <= args.rounds <= 10:
        parser.error("Invalid sample/round count")
    root = Path(__file__).resolve().parents[1]
    reference = args.reference.resolve()
    if reference == root or not (reference / "fancontrollx/hardware.py").is_file():
        parser.error("--reference must be a separate original checkout/export")
    result = {"method": "Alternating warmed read-only scans, one second sleep after each scan; no control writes",
              "reference": str(reference), "rounds": []}
    for index in range(args.rounds):
        for runtime in (["python", "rust"] if index % 2 == 0 else ["rust", "python"]):
            if runtime == "python":
                command = [sys.executable, "-c", PYTHON_SAMPLE.replace("COUNT", str(args.samples))]
                env = {**os.environ, "PYTHONPATH": str(reference), "PYTHONDONTWRITEBYTECODE": "1"}
                cwd = reference
            else:
                command = [str(root / "target/release/fancontrollx"), "--benchmark", str(args.samples)]
                env, cwd = os.environ, root
            value = json.loads(subprocess.check_output(command, env=env, cwd=cwd, timeout=args.samples * 5 + 20))
            value["round"] = index + 1
            value["cpu_ms_per_sample"] = value["cpu_s"] * 1000 / value["samples"]
            value["sample_wall_ms_median"] = statistics.median(value["sample_wall_ms"])
            result["rounds"].append(value)
            print(json.dumps({k: v for k, v in value.items() if k != "sample_wall_ms"}), flush=True)
    result["summary"] = {}
    for runtime in ("python", "rust"):
        runs = [v for v in result["rounds"] if v["runtime"] == runtime]
        result["summary"][runtime] = {
            "cpu_ms_per_sample_mean": statistics.mean(v["cpu_ms_per_sample"] for v in runs),
            "sample_wall_ms_median": statistics.median(t for v in runs for t in v["sample_wall_ms"]),
        }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(result["summary"]), flush=True)


if __name__ == "__main__":
    main()
