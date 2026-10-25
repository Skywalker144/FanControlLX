#!/usr/bin/env python3
"""Measure one binary in disposable demo mode, never the installed service.

Records idle process CPU/RSS, repeated API reads, and control sampling under load.
Use identical --requests and --idle-seconds for before/after comparisons.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import http.client
import json
import os
from pathlib import Path
import socket
import statistics
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--requests", type=int, default=10000)
    parser.add_argument("--idle-seconds", type=int, default=8)
    args = parser.parse_args()
    if not 10 <= args.requests <= 100000 or not 1 <= args.idle_seconds <= 300:
        parser.error("Use 10–100000 requests and 1–300 idle seconds")
    binary = args.binary.resolve()
    with tempfile.TemporaryDirectory(prefix="fancontrollx-runtime-") as data:
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        process = subprocess.Popen([str(binary), "--demo", "--service", "--port", str(port),
                                    "--data-dir", data], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            def request(path, body=None, token=None, headers=None):
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
                try:
                    h = dict(headers or {})
                    if body is not None:
                        h.update({"Content-Type": "application/json", "X-FanControl-Token": token})
                    connection.request("POST" if body is not None else "GET", path,
                                       json.dumps(body) if body is not None else None, h)
                    response = connection.getresponse()
                    payload = response.read()
                    return response.status, payload, dict(response.getheaders())
                finally:
                    connection.close()

            deadline = time.monotonic() + 10
            while True:
                try:
                    status, payload, _ = request("/api/state")
                    if status == 200:
                        if not json.loads(payload).get("demo"):
                            raise RuntimeError("Refusing to benchmark a non-demo service")
                        break
                except OSError:
                    pass
                if process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("Demo startup failed")
                time.sleep(.05)

            def usage():
                fields = Path(f"/proc/{process.pid}/stat").read_text().split()
                memory = dict(line.split(":", 1) for line in Path(f"/proc/{process.pid}/status").read_text().splitlines() if ":" in line)
                return {"cpu_s": (int(fields[13]) + int(fields[14])) / os.sysconf("SC_CLK_TCK"),
                        "rss_bytes": int(memory["VmRSS"].split()[0]) * 1024,
                        "threads": int(memory["Threads"]),
                        "scheduled_cpu_ns": sum(int(p.read_text().split()[0]) for p in Path(f"/proc/{process.pid}/task").glob("*/schedstat"))}

            results = []
            for size in (3, 64):
                state = json.loads(request("/api/state")[1])
                if process.poll() is not None or not state.get("demo"):
                    raise RuntimeError("Disposable demo stopped or changed")
                if size == 64:
                    original = state["config"]["fans"][0]
                    config = {"version": 2, "fans": [], "curves": []}
                    for i in range(128):
                        config["curves"].append({"id": f"curve-{i}", "name": f"Curve {i}",
                                                 "points": [[i * 3, min(100, i * 4)] for i in range(32)]})
                    for i in range(size):
                        config["fans"].append({**original, "id": f"fan-{i}", "name": f"Fan {i}",
                                               "channel_id": None, "rpm_sensor_id": None,
                                               "autostart": False, "identified": False,
                                               "curve_id": f"curve-{i}", "points": config["curves"][0]["points"]})
                    status, body, _ = request("/api/config", config, state["csrf_token"])
                    if status != 200:
                        raise RuntimeError(body.decode())
                    state = json.loads(request("/api/state")[1])

                before = usage()
                time.sleep(args.idle_seconds)
                idle = usage()
                results.append({"fans": size, "scenario": "idle", "seconds": args.idle_seconds,
                                "cpu_s": idle["cpu_s"] - before["cpu_s"], "cpu_runtime_ms": (idle["scheduled_cpu_ns"] - before["scheduled_cpu_ns"]) / 1e6, **{k: v for k, v in idle.items() if k not in ("cpu_s", "scheduled_cpu_ns")}})
                for path in ("/api/view", "/api/state", f"/api/state?since={state['config_revision']}&after=0"):
                    request(path)
                    before = usage()
                    started = time.perf_counter()

                    def one(_):
                        at = time.perf_counter()
                        status, body, _ = request(path)
                        if status != 200:
                            raise RuntimeError(f"HTTP {status}: {body[:200]!r}")
                        return (time.perf_counter() - at) * 1000, len(body)

                    with ThreadPoolExecutor(max_workers=2) as pool:
                        samples = list(pool.map(one, range(args.requests)))
                    elapsed = time.perf_counter() - started
                    after = usage()
                    final = json.loads(request("/api/state")[1])
                    intervals = [b["time"] - a["time"] for a, b in zip(final["history"], final["history"][1:])]
                    timings = sorted(s[0] for s in samples)
                    results.append({"fans": size, "scenario": path.split("?")[0] + (" delta" if "?" in path else ""),
                                    "requests": args.requests, "concurrency": 2, "seconds": elapsed,
                                    "server_cpu_s": after["cpu_s"] - before["cpu_s"],
                                    "server_cpu_runtime_ms": (after["scheduled_cpu_ns"] - before["scheduled_cpu_ns"]) / 1e6,
                                    "latency_ms_median": statistics.median(timings), "latency_ms_p95": timings[int(len(timings) * .95) - 1],
                                    "body_bytes_mean": statistics.mean(s[1] for s in samples),
                                    "rss_bytes": after["rss_bytes"], "threads": after["threads"],
                                    "control_interval_s_max": max(intervals) if intervals else None})
                    print(json.dumps(results[-1]), flush=True)

                # Fill the bounded history without a three-minute sleep. Scans
                # use only the disposable demo and do not acquire any controls.
                for _ in range(90):
                    status, body, _ = request("/api/scan", {}, state["csrf_token"])
                    if status != 200:
                        raise RuntimeError(body.decode())
                view = json.loads(request("/api/view")[1])
                full_sizes, poll_sizes = [], []
                for _ in range(20):
                    status, body, _ = request("/api/scan", {}, state["csrf_token"])
                    if status != 200:
                        raise RuntimeError(body.decode())
                    full_sizes.append(len(request("/api/view")[1]))
                    # Old binaries naturally fall back to the complete response.
                    path = "/api/view"
                    if "view_revision" in view:
                        path += f"?since={view['view_revision']}&after={view['history_cursor']}"
                    status, body, _ = request(path)
                    if status != 200:
                        raise RuntimeError(body.decode())
                    poll_sizes.append(len(body))
                    view = json.loads(body)
                results.append({"fans": size, "scenario": "viewer polling payload", "polls": 20,
                                "method": "Full 80-sample history, two demo samples between reads; body sizes only; excludes scan cost and HTTP headers",
                                "full_body_bytes_mean": statistics.mean(full_sizes),
                                "poll_body_bytes_mean": statistics.mean(poll_sizes),
                                "rss_bytes": usage()["rss_bytes"]})
                print(json.dumps(results[-1]), flush=True)

            status, body, headers = request("/icons.svg")
            etag = headers.get("ETag")
            cached_status, cached_body, _ = request("/icons.svg", headers={"If-None-Match": etag or '"baseline"'})
            result = {"label": args.label, "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                      "binary_bytes": binary.stat().st_size, "measured_at_unix": time.time(),
                      "method": "Linux per-thread schedstat runtime in ns plus process CPU ticks; isolated demo; no hardware writes; process user+system CPU from /proc/<pid>/stat (10ms granularity on this host); RSS is process memory, not cgroup; 2 concurrent closed-connection clients; short synthetic load, not production power.",
                      "results": results, "icon_response_bytes": len(body), "icon_revalidation_status": cached_status,
                      "icon_revalidation_bytes": len(cached_body)}
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(result, indent=2) + "\n")
        finally:
            process.terminate()
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise RuntimeError("Demo did not stop gracefully")


if __name__ == "__main__":
    main()
