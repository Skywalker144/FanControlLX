#!/usr/bin/env python3
"""Compare isolated GPU writers using fake NVML; never accesses physical fans.

The before binary uses individual writes; the after binary uses write-batch.
Measures IPC/driver call counts and mock-process CPU, not production savings.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import statistics
import subprocess
import tempfile
import time


def measure(binary, library, batched, iterations):
    with tempfile.TemporaryDirectory(prefix="fancontrollx-gpu-perf-") as directory:
        data = Path(directory)
        (data / "gpu-state").write_text("0 0\n")
        (data / "fan-count").write_text("3")
        parent, child = socket.socketpair()
        parent.settimeout(15)
        env = {**os.environ, "LD_LIBRARY_PATH": str(library), "FANCONTROLLX_MOCK_DIR": directory}
        process = subprocess.Popen([str(binary), "--gpu-writer", "--fd", str(child.fileno()),
                                    "--data-dir", directory], pass_fds=(child.fileno(),), env=env,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        child.close()
        connection = parent.makefile("rwb", buffering=0)
        try:
            assert json.loads(connection.readline())["ready"]

            def request(body):
                connection.write(json.dumps(body).encode() + b"\n")
                response = json.loads(connection.readline())
                if "error" in response:
                    raise RuntimeError(response["error"])
                return response["result"]

            writes = []
            for i in range(3):
                key = f"nvidia/GPU-abcd/fan{i}"
                lease = request({"action": "acquire", "channel_id": key, "duty": 0})
                writes.append({"channel_id": key, "token": lease["token"], "duty": 0})

            def cycle():
                if batched:
                    rows = request({"action": "write-batch", "writes": writes})
                    assert all("error" not in row and row["result"]["automatic"] for row in rows)
                else:
                    for write in writes:
                        assert request({"action": "write", **write})["automatic"]

            def cpu_ns():
                return sum(int(p.read_text().split()[0])
                           for p in Path(f"/proc/{process.pid}/task").glob("*/schedstat"))

            for _ in range(30):
                cycle()
            counter = data / "temperature-reads"
            reads_before = len(counter.read_text().splitlines())
            cpu_before = cpu_ns()
            started = time.perf_counter()
            for _ in range(iterations):
                cycle()
            elapsed = time.perf_counter() - started
            cpu_ms = (cpu_ns() - cpu_before) / 1e6
            return {"mode": "batch" if batched else "individual", "cycles": iterations,
                    "ipc_requests_per_cycle": 1 if batched else 3,
                    "temperature_reads": len(counter.read_text().splitlines()) - reads_before,
                    "writer_cpu_ms": cpu_ms, "wall_ms_per_cycle": elapsed * 1000 / iterations}
        finally:
            connection.close()
            parent.close()
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise RuntimeError("Mock writer failed to restore and exit")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--iterations", type=int, default=1000)
    args = parser.parse_args()
    if not 10 <= args.iterations <= 10000:
        parser.error("Use 10–10000 iterations")
    with tempfile.TemporaryDirectory(prefix="fancontrollx-gpu-library-") as directory:
        library = Path(directory)
        subprocess.run(["cc", "-shared", "-fPIC", "-O2", "-Wall", "-Wextra", "-Werror",
                        str(Path(__file__).with_name("mock_nvml.c")), "-o", str(library / "libnvidia-ml.so.1")],
                       check=True)
        samples = []
        for _ in range(3):
            for binary, batch in [(args.before.resolve(), False), (args.after.resolve(), True)]:
                row = measure(binary, library, batch, args.iterations)
                samples.append(row)
                print(json.dumps(row), flush=True)
        result = {"method": "Three fans on one fake GPU, automatic mode, three alternating runs; "
                  "per-thread Linux schedstat CPU. Fake driver file I/O included; excludes parent CPU, "
                  "real driver costs and real service memory.", "samples": samples,
                  "writer_cpu_ms_median": {mode: statistics.median(s["writer_cpu_ms"] for s in samples
                                                                   if s["mode"] == mode)
                                           for mode in ["individual", "batch"]}}
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
