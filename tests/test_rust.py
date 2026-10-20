"""Black-box Rust API, persistence, and independent GPU recovery tests.

Build first: cargo build --release --locked
Then: python3 -m unittest discover -s tests -p test_rust.py -v
All writes target temporary demo state or a fake NVML shared library.
"""
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import unittest
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/release/fancontrollx"
KEY = "nvidia/GPU-abcd/fan0"

@unittest.skipUnless(BINARY.exists(), "Build Rust release binary first")
class RustInstallTests(unittest.TestCase):
    def test_dry_run_preserves_config_and_unit_passes_systemd_parser(self):
        with tempfile.TemporaryDirectory(prefix="fancontrollx-install-") as directory:
            root = Path(directory) / "project with spaces"
            (root / "data").mkdir(parents=True)
            (root / "target/release").mkdir(parents=True)
            (root / "target/release/fancontrollx").symlink_to(BINARY)
            config = root / "data/config.json"
            original = b'{"version":1,"fans":[]}  \n'
            config.write_bytes(original)
            result = subprocess.run([str(BINARY), "--install-service", "--project-dir", str(root),
                                     "--dry-run", "--skip-driver"], check=True, capture_output=True, text=True)
            self.assertEqual(config.read_bytes(), original)
            self.assertEqual(list((root / "data").iterdir()), [config])
            unit = result.stdout.split("[Unit]", 1)[1].split("\n仅预览", 1)[0]
            path = Path(directory) / "fancontrollx-test.service"
            path.write_text("[Unit]" + unit)
            self.assertNotIn("python", unit.lower())
            self.assertIn("--recover", unit)
            if shutil.which("systemd-analyze"):
                subprocess.run(["systemd-analyze", "verify", str(path)], check=True, capture_output=True)


@unittest.skipUnless(BINARY.exists(), "Build Rust release binary first")
class RustHTTPTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="fancontrollx-rust-http-")
        self.addCleanup(self.temp.cleanup)
        self.data = Path(self.temp.name)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        self.start()
        self.addCleanup(self.stop)

    def start(self):
        self.process = subprocess.Popen(
            [str(BINARY), "--demo", "--service", "--port", str(self.port),
             "--data-dir", str(self.data)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                self.state = self.get()
                return
            except OSError:
                if self.process.poll() is not None:
                    self.fail("Rust demo server exited during startup")
                time.sleep(.05)
        self.fail("Rust demo server did not become ready")

    def stop(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
                self.fail("Rust server failed to stop gracefully")

    def request(self, path, body=None, headers=None):
        h = {"Content-Type": "application/json",
             "X-FanControl-Token": self.state["csrf_token"]} if body is not None else {}
        h.update(headers or {})
        req = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}",
                                     data=json.dumps(body).encode() if body is not None else None, headers=h)
        try:
            with self.opener.open(req, timeout=10) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()

    def get(self, query=""):
        status, body = self.request("/api/state" + query)
        self.assertEqual(status, 200)
        return json.loads(body)

    def post(self, path, body, status=200):
        code, result = self.request(path, body)
        self.assertEqual(code, status, result)
        return json.loads(result)

    def identify(self):
        f = self.get()["config"]["fans"][0]
        self.post("/api/identity", {"channel_id": f["channel_id"], "name": "Case", "device_kind": "fan"})
        return f["id"]

    def test_assets_security_and_input_validation(self):
        self.assertEqual(self.state["runtime"], "rust")
        for asset in ("/", "/view", "/edit", "/view.js", "/view.css", "/app.js", "/curve.js", "/style.css", "/favicon.svg", "/api/health"):
            self.assertEqual(self.request(asset)[0], 200)
        self.assertEqual(self.request("/../../data/config.json")[0], 404)
        self.assertEqual(self.request("/api/state", headers={"Host": "evil.example"})[0], 403)
        self.assertEqual(self.request("/api/scan", {}, {"Origin": "http://evil.example"})[0], 403)
        self.assertEqual(self.request("/api/scan", {}, {"X-FanControl-Token": "invalid"})[0], 403)
        self.post("/api/config", {"version": True, "fans": []}, 400)
        self.post("/api/config", {"version": 2, "fans": [{"id": "x", "name": "X", "step_up": True}]}, 400)

    def test_viewer_is_read_only_and_heartbeat_requires_token(self):
        before = self.get()
        code, body = self.request("/api/view")
        self.assertEqual(code, 200)
        view = json.loads(body)
        for field in ("config", "csrf_token", "discovery", "history", "events"):
            self.assertNotIn(field, view)
        self.assertLess(len(body), len(json.dumps(before)))
        self.assertEqual([f["id"] for f in view["fans"]], [f["id"] for f in before["config"]["fans"]])
        self.assertEqual(self.request("/api/editor-heartbeat", {}, {"X-FanControl-Token": ""})[0], 403)
        self.post("/api/editor-heartbeat", {})
        self.assertEqual(self.get()["config"], before["config"])
        self.assertNotIn(b"/app.js", self.request("/")[1])
        self.assertNotIn(b"dialog", self.request("/")[1])
        self.assertIn(b"/app.js", self.request("/edit")[1])

    def test_all_edit_endpoints_reject_missing_tokens(self):
        for path in ("/api/fan", "/api/fan-order", "/api/curve", "/api/curve/delete",
                     "/api/identify", "/api/identity", "/api/config", "/api/activate",
                     "/api/release-all", "/api/scan"):
            with self.subTest(path=path):
                self.assertEqual(self.request(path, {}, {"X-FanControl-Token": ""})[0], 403)

    def test_delta_and_concurrent_configuration(self):
        state = self.get()
        delta = self.get(f"?since={state['config_revision']}&after={state['history'][-1]['time']}")
        self.assertNotIn("config", delta)
        self.assertTrue(delta["history_delta"])
        self.assertLess(len(json.dumps(delta)), len(json.dumps(state)))
        f = state["config"]["fans"][0]
        self.post("/api/fan", {"id": f["id"], "changes": {"name": "A"}, "expected": {"name": f["name"]}})
        self.post("/api/fan", {"id": f["id"], "changes": {"name": "B"}, "expected": {"name": f["name"]}}, 409)
        self.assertIn("config", self.get(f"?since={state['config_revision']}"))
        self.post("/api/config", {**state["config"], "expected_revision": state["config_revision"]}, 409)

    def test_curve_assignment_reordering_and_numeric_roundtrip(self):
        state = self.get()
        first = state["config"]["fans"][0]["id"]
        curve = self.post("/api/curve", {"curve": {"name": "Shared", "points": [[20, 0], [80, 100]]},
                                         "expected": None, "assign_to": first, "expected_fan": None})
        self.post("/api/curve/delete", {"id": curve["id"]}, 400)
        self.post("/api/fan", {"id": first, "changes": {"manual_duty": 50.0}})
        self.post("/api/fan", {"id": first, "changes": {"manual_duty": 60}, "expected": {"manual_duty": 50}})
        ids = [f["id"] for f in state["config"]["fans"]]
        self.post("/api/fan-order", {"ids": ids[::-1], "expected": ids})
        self.post("/api/fan-order", {"ids": ids, "expected": ids}, 409)
        self.assertEqual([f["id"] for f in self.get()["config"]["fans"]], ids[::-1])

    def test_service_restart_persists_control_and_stop(self):
        id_ = self.identify()
        self.post("/api/activate", {"id": id_})
        self.assertTrue(self.get()["outputs"][id_]["active"])
        self.stop()
        self.start()
        self.assertTrue(self.get()["outputs"][id_]["active"])
        self.post("/api/release-all", {})
        self.stop()
        self.start()
        self.assertFalse(self.get()["outputs"][id_]["active"])

    def test_identification_timeout_without_browser(self):
        id_ = self.identify()
        self.post("/api/activate", {"id": id_})
        self.post("/api/identify", {"channel_id": "demo/pwm1", "device_kind": "fan", "duty": 70})
        self.assertNotIn("lease", self.get()["identification"])
        time.sleep(16.5)
        s = self.get()
        self.assertIsNone(s["identification"])
        self.assertTrue(s["outputs"][id_]["active"])
        self.assertTrue(s["config"]["fans"][0]["autostart"])

    def test_v1_backup_preserves_exact_bytes(self):
        self.stop()
        original = b'{"version": 1, "fans": [{"id": "f", "name": "Custom"}]} \n'
        path = self.data / "config.demo.json"
        path.write_bytes(original)
        self.start()
        self.post("/api/fan", {"id": "f", "changes": {"name": "Updated"}})
        self.assertEqual((self.data / "config.demo.before-v2.json").read_bytes(), original)
        self.post("/api/fan", {"id": "f", "changes": {"name": "Again"}})
        self.assertEqual((self.data / "config.demo.before-v2.json").read_bytes(), original)


@unittest.skipUnless(BINARY.exists() and shutil.which("cc"), "Build Rust binary; C compiler needed for mock NVML")
class RustGPUProcessTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.library = tempfile.TemporaryDirectory(prefix="fancontrollx-mock-nvml-")
        subprocess.run(["cc", "-shared", "-fPIC", "-Wall", "-Wextra", "-Werror",
                        str(ROOT / "tests/mock_nvml.c"), "-o", str(Path(cls.library.name) / "libnvidia-ml.so.1")],
                       check=True, capture_output=True)

    @classmethod
    def tearDownClass(cls):
        cls.library.cleanup()

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="fancontrollx-rust-gpu-")
        self.addCleanup(self.temp.cleanup)
        self.data = Path(self.temp.name)
        self.env = {**os.environ, "LD_LIBRARY_PATH": self.library.name, "FANCONTROLLX_MOCK_DIR": self.temp.name}
        self.process = None
        self.sock = None
        self.file = None
        self.addCleanup(self.cleanup)

    def start(self, policy=0, target=0):
        (self.data / "gpu-state").write_text(f"{policy} {target}\n")
        self.sock, child = socket.socketpair()
        self.sock.settimeout(10)
        self.process = subprocess.Popen(
            [str(BINARY), "--gpu-writer", "--fd", str(child.fileno()), "--data-dir", self.temp.name],
            env=self.env, pass_fds=(child.fileno(),), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        child.close()
        self.file = self.sock.makefile("rwb", buffering=0)
        self.assertTrue(json.loads(self.file.readline())["ready"])

    def cleanup(self):
        if self.file:
            self.file.close()
        if self.sock:
            self.sock.close()
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()

    def request(self, action, **kwargs):
        self.file.write(json.dumps({"action": action, "channel_id": KEY, **kwargs}).encode() + b"\n")
        return json.loads(self.file.readline())

    def state(self):
        return tuple(map(int, (self.data / "gpu-state").read_text().split()))

    def journal(self):
        return json.loads((self.data / "nvidia-leases.json").read_text())

    def test_parent_eof_restores_original_manual_target(self):
        self.start(1, 42)
        lease = self.request("acquire", duty=60)["result"]
        self.assertEqual(self.state(), (1, 60))
        self.request("write", duty=0, token=lease["token"])
        self.assertEqual(self.state(), (0, 0))
        self.file.close(); self.sock.close()
        self.process.wait(timeout=5)
        self.assertEqual(self.state(), (1, 42))
        self.assertEqual(self.journal()["leases"], [])

    def test_sigterm_restores(self):
        self.start()
        self.request("acquire", duty=60)
        self.process.send_signal(signal.SIGTERM)
        self.process.wait(timeout=5)
        self.assertEqual(self.state(), (0, 0))
        self.assertEqual(self.journal()["leases"], [])

    def test_sigkill_recovery_hook_uses_durable_journal(self):
        self.start(1, 42)
        self.request("acquire", duty=70)
        self.process.kill(); self.process.wait(timeout=5)
        self.assertEqual(self.state(), (1, 70))
        self.assertEqual(len(self.journal()["leases"]), 1)
        subprocess.run([str(BINARY), "--recover", "--data-dir", self.temp.name], env=self.env, check=True,
                       capture_output=True, timeout=5)
        self.assertEqual(self.state(), (1, 42))
        self.assertEqual(self.journal()["leases"], [])

    def test_worker_temperature_guard_and_idempotent_auto(self):
        self.start()
        lease = self.request("acquire", duty=0)["result"]
        self.assertFalse((self.data / "writes").exists())
        self.request("write", duty=0, token=lease["token"])
        self.assertFalse((self.data / "writes").exists())
        (self.data / "temperature").write_text("90")
        result = self.request("write", duty=0, token=lease["token"])["result"]
        self.assertIsNotNone(result["fault"])
        self.assertEqual(self.state(), (1, 100))
        (self.data / "temperature").write_text("invalid")
        self.assertIsNotNone(self.request("write", duty=0, token=lease["token"])["result"]["fault"])
        (self.data / "temperature").write_text("40")
        self.request("write", duty=0, token=lease["token"])
        self.assertEqual(self.state(), (0, 0))

    def test_timeout_expires_without_parent_request(self):
        self.start()
        lease = self.request("acquire", duty=70)["result"]
        time.sleep(11.5)
        self.assertEqual(self.state(), (0, 0))
        self.assertIn("error", self.request("write", duty=60, token=lease["token"]))

    def test_driver_bounds_and_invalid_values_preserve_lease(self):
        self.start()
        lease = self.request("acquire", duty=10)["result"]
        self.assertEqual(self.state(), (1, 30))
        for duty in (True, -1, 101, float("nan")):
            self.assertIn("error", self.request("write", duty=duty, token=lease["token"]))
            self.assertEqual(self.state(), (1, 30))
        self.assertEqual(len(self.journal()["leases"]), 1)
        self.request("restore", token=lease["token"])
        self.assertEqual(self.state(), (0, 0))

    def test_failed_restore_retains_journal_and_retries(self):
        self.start()
        lease = self.request("acquire", duty=60)["result"]
        (self.data / "fail-write").touch()
        self.assertIn("error", self.request("restore", token=lease["token"]))
        self.assertEqual(len(self.journal()["leases"]), 1)
        self.assertIn("error", self.request("write", duty=70, token=lease["token"]))
        (self.data / "fail-write").unlink()
        deadline = time.monotonic() + 4
        # Driver restoration precedes the atomic journal update. Wait for both
        # observable effects instead of racing the worker's fsync/rename.
        while time.monotonic() < deadline and (
                self.state() != (0, 0) or self.journal()["leases"]):
            time.sleep(.1)
        self.assertEqual(self.state(), (0, 0))
        self.assertEqual(self.journal()["leases"], [])


if __name__ == "__main__":
    unittest.main()
