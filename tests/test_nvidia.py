"""NVIDIA tests use an in-memory driver; no test opens a real GPU library."""

import json
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from fancontrollx.engine import Engine
from fancontrollx.hardware import Hardware
from fancontrollx.install_service import render_unit
from fancontrollx.nvidia import LEASE_SECONDS, NvidiaBackend, Writer, serve
from fancontrollx.nvml import Nvml, bounded_duty, parse_channel

GPU = "GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
SECOND_GPU = "GPU-11111111-2222-3333-4444-555555555555"
KEY = f"nvidia/{GPU}/fan0"


class FakeNvml(Nvml):
    """Emulate device state, preserving the real write/restore guard code."""
    def __init__(self):
        self.lib = SimpleNamespace(nvmlDeviceSetFanSpeed_v2=True, nvmlDeviceSetDefaultFanSpeed_v2=True)
        self.temperatures = {GPU: 40}
        self.states = {(GPU, i): {"policy": 0, "target": 30, "percent": 0, "rpm": 0} for i in range(3)}
        self.writes, self.restored = [], []
        self.fail_write, self.fail_restore, self.missing_rpm = False, False, False

    def devices(self):
        return iter(self.temperatures)

    def handle(self, gpu):
        if gpu not in self.temperatures:
            raise OSError("GPU lost")
        return gpu

    def string(self, name, handle):
        return handle if name == "nvmlDeviceGetUUID" else "RTX Test"

    def limits(self, handle):
        return 30, 100

    def temperature(self, handle):
        value = self.temperatures[handle]
        if value is None:
            raise OSError("temperature unavailable")
        return value

    def uint(self, name, handle, fan=None):
        if name == "nvmlDeviceGetNumFans":
            return sum(gpu == handle for gpu, _ in self.states)
        field = {"nvmlDeviceGetFanControlPolicy_v2": "policy", "nvmlDeviceGetFanSpeed_v2": "percent",
                 "nvmlDeviceGetTargetFanSpeed": "target"}[name]
        return self.states[handle, fan][field]

    def rpm(self, handle, fan):
        if self.missing_rpm:
            raise OSError("not supported")
        return self.states[handle, fan]["rpm"]

    def call(self, name, handle, fan, *args):
        state = self.states[handle, fan]
        if name == "nvmlDeviceSetDefaultFanSpeed_v2":
            if self.fail_restore:
                raise OSError("restore unavailable")
            state.update(policy=0, percent=0, rpm=0)
            self.restored.append((handle, fan))
        elif name == "nvmlDeviceSetFanSpeed_v2":
            if self.fail_write:
                raise OSError("write rejected")
            self.writes.append((handle, fan, args[0]))
            state.update(policy=1, target=args[0], percent=args[0], rpm=args[0] * 20)
        else:
            raise AssertionError(name)

    def close(self):
        pass


class WriterTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "nvidia-leases.json"
        self.nvml = FakeNvml()
        self.now = 100
        self.writer = Writer(self.nvml, self.path, lambda: self.now)

    def acquire(self, key=KEY, duty=45):
        return self.writer.dispatch({"action": "acquire", "channel_id": key, "duty": duty})

    def test_journal_is_durable_before_any_manual_write(self):
        write = self.nvml.write

        def verify(key, duty):
            leases = json.loads(self.path.read_text())["leases"]
            self.assertEqual(leases[0]["channel_id"], key)
            self.assertEqual(leases[0]["policy"], 0)
            write(key, duty)

        with patch.object(self.nvml, "write", side_effect=verify):
            lease = self.acquire()
        self.assertEqual([w[2] for w in self.nvml.writes], [100, 45])
        self.writer.dispatch({"action": "restore", **lease})
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 0)
        self.assertEqual(json.loads(self.path.read_text())["leases"], [])

    def test_failed_journal_write_never_changes_hardware(self):
        with patch.object(self.writer, "persist", side_effect=OSError("disk full")):
            with self.assertRaises(OSError):
                self.acquire()
        self.assertFalse(self.nvml.writes)
        self.assertFalse(self.writer.leases)

    def test_manual_policy_restores_target_rather_than_current_percent(self):
        self.nvml.states[GPU, 0].update(policy=1, target=62, percent=47)
        lease = self.acquire(duty=80)
        self.writer.dispatch({"action": "restore", **lease})
        self.assertEqual(self.nvml.states[GPU, 0]["target"], 62)
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 1)

    def test_write_failure_restores_and_does_not_leak_lease(self):
        self.nvml.fail_write = True
        with self.assertRaisesRegex(OSError, "write rejected"):
            self.acquire()
        self.assertFalse(self.writer.leases)
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 0)

    def test_failed_restore_survives_restart_and_retries(self):
        lease = self.acquire()
        self.nvml.fail_restore = True
        with self.assertRaises(OSError):
            self.writer.dispatch({"action": "restore", **lease})
        self.assertTrue(json.loads(self.path.read_text())["leases"])
        restarted = Writer(self.nvml, self.path)
        self.nvml.fail_restore = False
        self.assertEqual(restarted.expire(), [])
        self.assertFalse(restarted.leases)
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 0)

    def test_each_channel_expires_independently_and_rejects_old_token(self):
        first = self.acquire()
        second = self.acquire(KEY[:-1] + "1")
        self.now += LEASE_SECONDS - 1
        self.writer.dispatch({"action": "write", **second, "duty": 60})
        self.now += 2
        self.assertEqual(self.writer.expire(), [])
        self.assertNotIn(KEY, self.writer.leases)
        self.assertIn(second["channel_id"], self.writer.leases)
        with self.assertRaisesRegex(OSError, "失效"):
            self.writer.dispatch({"action": "write", **first, "duty": 50})
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 0)

    def test_parent_eof_restores_all_channels(self):
        parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        self.addCleanup(child.close)
        thread = threading.Thread(target=serve, args=(child, self.writer, lambda: False))
        thread.start()
        parent.settimeout(2)
        self.assertTrue(json.loads(parent.recv(8192))["ready"])
        parent.sendall(json.dumps({"action": "acquire", "channel_id": KEY, "duty": 40}).encode())
        self.assertIn("result", json.loads(parent.recv(8192)))
        parent.close()
        thread.join(timeout=3)
        self.assertFalse(thread.is_alive())
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 0)

    def test_driver_limits_and_gpu_guard_apply_inside_writer(self):
        lease = self.acquire(duty=1)
        self.assertEqual(self.nvml.states[GPU, 0]["target"], 30)
        for temperature in (85, None):
            self.nvml.temperatures[GPU] = temperature
            self.writer.dispatch({"action": "write", **lease, "duty": 30})
            self.assertEqual(self.nvml.states[GPU, 0]["target"], 100)

    def test_zero_uses_auto_policy_and_renews_the_same_lease(self):
        lease = self.acquire(duty=50)
        original = json.loads(self.path.read_text())["leases"]
        self.now += LEASE_SECONDS - 1
        result = self.writer.dispatch({"action": "write", **lease, "duty": 0})
        self.assertTrue(result["automatic"])
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 0)
        self.assertEqual(json.loads(self.path.read_text())["leases"], original)
        self.now += LEASE_SECONDS - 1
        self.assertFalse(self.writer.expire())
        self.assertIn(KEY, self.writer.leases)
        self.writer.dispatch({"action": "write", **lease, "duty": 1})
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 1)
        self.assertEqual(self.nvml.states[GPU, 0]["target"], 30)

    def test_auto_is_idempotent_and_does_not_force_native_rpm_to_zero(self):
        self.nvml.states[GPU, 0].update(percent=37, rpm=740)
        lease = self.acquire(duty=0)
        for _ in range(3):
            self.writer.dispatch({"action": "write", **lease, "duty": 0})
        self.assertFalse(self.nvml.writes)
        self.assertFalse(self.nvml.restored)
        self.assertEqual(self.nvml.states[GPU, 0]["percent"], 37)
        self.assertEqual(self.nvml.states[GPU, 0]["rpm"], 740)

    def test_auto_mode_keeps_original_manual_policy_for_release_and_crash(self):
        self.nvml.states[GPU, 0].update(policy=1, target=62, percent=47)
        self.acquire(duty=0)
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 0)
        restarted = Writer(self.nvml, self.path)
        self.assertFalse(restarted.expire(all_=True))
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 1)
        self.assertEqual(self.nvml.states[GPU, 0]["target"], 62)

    def test_worker_gpu_guard_overrides_zero_auto_request(self):
        lease = self.acquire(duty=0)
        for temperature in (85, None):
            self.nvml.temperatures[GPU] = temperature
            result = self.writer.dispatch({"action": "write", **lease, "duty": 0})
            self.assertFalse(result["automatic"])
            self.assertTrue(result["fault"])
            self.assertEqual(self.nvml.states[GPU, 0]["policy"], 1)
            self.assertEqual(self.nvml.states[GPU, 0]["target"], 100)

    def test_auto_switch_failure_preserves_journal_and_original_lease(self):
        lease = self.acquire(duty=50)
        self.nvml.fail_restore = True
        with self.assertRaises(OSError):
            self.writer.dispatch({"action": "write", **lease, "duty": 0})
        self.assertTrue(json.loads(self.path.read_text())["leases"])
        self.assertEqual(self.writer.leases[KEY]["token"], lease["token"])
        self.nvml.fail_restore = False
        self.writer.dispatch({"action": "restore", **lease})
        self.assertFalse(self.writer.leases)

    def test_expired_lease_cannot_be_renewed_when_restore_fails(self):
        lease = self.acquire()
        self.now += LEASE_SECONDS + 1
        self.nvml.fail_restore = True
        self.assertTrue(self.writer.expire())
        with self.assertRaisesRegex(OSError, "超时"):
            self.writer.dispatch({"action": "write", **lease, "duty": 50})
        self.assertEqual(self.writer.deadlines[KEY], 100 + LEASE_SECONDS)


# Exercise the real entrypoint and signal/IPC lifecycle in a separate Python
# interpreter, replacing NVML there before main runs. It never loads a GPU lib.
FAKE_WORKER = """
import sys
from pathlib import Path
from unittest.mock import patch
sys.path.insert(0, 'tests')
from test_nvidia import FakeNvml
from fancontrollx.nvidia import main
nvml = FakeNvml()
restore = nvml.restore
def logged_restore(lease):
    restore(lease)
    path = Path(sys.argv[sys.argv.index('--data-dir') + 1]) / 'restored.txt'
    with path.open('a') as file:
        file.write(lease['channel_id'] + '\\n')
nvml.restore = logged_restore
with patch('fancontrollx.nvidia.Nvml', return_value=nvml):
    sys.exit(main())
"""


class ProcessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        self.addCleanup(self.parent.close)
        self.parent.settimeout(3)
        self.process = subprocess.Popen(
            [sys.executable, '-c', FAKE_WORKER, '--data-dir', str(self.root), '--fd', str(child.fileno())],
            pass_fds=(child.fileno(),), start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        child.close()
        self.addCleanup(self.cleanup)
        self.assertTrue(json.loads(self.parent.recv(8192))["ready"])
        self.parent.sendall(json.dumps({"action": "acquire", "channel_id": KEY, "duty": 50}).encode())
        self.lease = json.loads(self.parent.recv(8192))["result"]

    def cleanup(self):
        self.parent.close()
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=3)
        self.process.stderr.close()

    def test_sigterm_restores_before_exit(self):
        self.process.send_signal(signal.SIGTERM)
        self.assertEqual(self.process.wait(timeout=4), 0)
        self.assertEqual(json.loads((self.root / 'nvidia-leases.json').read_text())["leases"], [])
        self.assertEqual((self.root / 'restored.txt').read_text().strip(), KEY)

    def test_sigkill_retains_journal_for_separate_recovery_hook(self):
        self.process.kill()
        self.process.wait(timeout=3)
        self.assertTrue(json.loads((self.root / 'nvidia-leases.json').read_text())["leases"])
        recovery = subprocess.run([sys.executable, '-c', FAKE_WORKER, '--data-dir', str(self.root), '--recover'],
                                  capture_output=True, text=True, timeout=5)
        self.assertEqual(recovery.returncode, 0, recovery.stderr)
        self.assertEqual(json.loads((self.root / 'nvidia-leases.json').read_text())["leases"], [])
        self.assertEqual((self.root / 'restored.txt').read_text().strip(), KEY)

    def test_parent_socket_close_restores_in_independent_process(self):
        self.parent.close()
        self.assertEqual(self.process.wait(timeout=3), 0)
        self.assertEqual((self.root / 'restored.txt').read_text().strip(), KEY)

    def test_auto_and_manual_switches_round_trip_over_writer_protocol(self):
        for duty, automatic in ((0, True), (1, False), (0, True)):
            self.parent.sendall(json.dumps({"action": "write", **self.lease, "duty": duty}).encode())
            result = json.loads(self.parent.recv(8192))["result"]
            self.assertEqual(result["automatic"], automatic)
            self.assertEqual(result["duty"], 0 if automatic else 30)
        self.assertTrue(json.loads((self.root / 'nvidia-leases.json').read_text())["leases"])


class IntegrationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.nvml = FakeNvml()
        self.backend = NvidiaBackend(self.root)
        self.backend.nvml = self.nvml
        self.writer = Writer(self.nvml, self.root / "nvidia-leases.json")
        self.backend.request = lambda action, channel_id, **kw: self.writer.dispatch(
            {"action": action, "channel_id": channel_id, **kw})
        self.permission = patch("fancontrollx.nvidia.os.geteuid", return_value=0)
        self.permission.start()
        self.addCleanup(self.permission.stop)
        self.hw = Hardware(self.root / "empty-hwmon", nvidia=self.backend)
        self.engine = Engine(self.hw, self.root / "config.json", allow_control=True)
        self.addCleanup(self.engine.close)

    def confirm(self):
        return self.engine.confirm_identity(KEY, "GPU fan 1", "fan")

    def zero_curve(self, **settings):
        id_ = self.confirm()
        self.engine.patch_fan(id_, {"points": [[25, 0], [45, 0], [55, 50], [85, 100]],
                                    "smoothing_s": 0, "hysteresis": 0, "response_s": 0, **settings})
        return id_

    def test_zero_curve_bypasses_minimum_and_initial_full_speed_pulse(self):
        id_ = self.zero_curve(min_duty=60, response_s=30, step_down=1)
        self.engine.activate(id_)
        output = self.engine.state()["outputs"][id_]
        self.assertTrue(output["active"])
        self.assertTrue(output["automatic"])
        self.assertEqual(output["target"], 0)
        self.assertEqual(output["requested"], 0)
        self.assertEqual(output["status"], "显卡自动调速")
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 0)
        self.assertFalse(self.nvml.writes)
        self.nvml.temperatures[GPU] = 50
        self.engine.tick()
        self.assertEqual(self.engine.state()["outputs"][id_]["target"], 60)

    def test_managed_restart_resumes_zero_curve_without_manual_pulse(self):
        self.engine.managed = True
        id_ = self.zero_curve()
        self.engine.request_control(id_, True)
        self.engine.close()
        backend = NvidiaBackend(self.root)
        backend.nvml = self.nvml
        writer = Writer(self.nvml, self.root / "nvidia-leases.json")
        backend.request = lambda action, channel_id, **kw: writer.dispatch(
            {"action": action, "channel_id": channel_id, **kw})
        hw = Hardware(self.root / "empty-hwmon", nvidia=backend)
        restarted = Engine(hw, self.root / "config.json", allow_control=True, managed=True)
        self.addCleanup(restarted.close)
        restarted.resume_controls()
        self.assertTrue(restarted.state()["outputs"][id_]["active"])
        self.assertTrue(restarted.state()["outputs"][id_]["automatic"])
        self.assertFalse(self.nvml.writes)

    def test_zero_positive_zero_switch_keeps_original_lease(self):
        id_ = self.zero_curve()
        self.engine.activate(id_)
        lease = self.engine.leases[id_]
        self.nvml.temperatures[GPU] = 50
        self.engine.tick()
        output = self.engine.state()["outputs"][id_]
        self.assertFalse(output["automatic"])
        self.assertEqual(output["target"], 30)
        self.assertEqual(self.nvml.states[GPU, 0]["target"], 30)
        self.nvml.temperatures[GPU] = 40
        self.engine.tick()
        self.assertTrue(self.engine.state()["outputs"][id_]["automatic"])
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 0)
        self.assertIs(self.engine.leases[id_], lease)

    def test_existing_temperature_hysteresis_avoids_boundary_chatter(self):
        id_ = self.zero_curve(hysteresis=2)
        self.engine.activate(id_)
        for temperature, automatic in ((45.5, False), (44.5, False), (43, True)):
            self.nvml.temperatures[GPU] = temperature
            self.engine.tick()
            self.assertEqual(self.engine.state()["outputs"][id_]["automatic"], automatic)

    def test_manual_zero_stays_at_running_minimum(self):
        id_ = self.zero_curve()
        self.engine.activate(id_)
        self.engine.patch_fan(id_, {"mode": "manual", "manual_duty": 0})
        output = self.engine.state()["outputs"][id_]
        self.assertFalse(output["automatic"])
        self.assertEqual(output["target"], 30)
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 1)

    def test_shared_zero_curve_switches_without_reacquiring(self):
        id_ = self.confirm()
        shared = {"name": "shared", "points": [[25, 0], [85, 0]]}
        curve_id = self.engine.save_curve(shared, assign_to=id_)
        self.engine.activate(id_)
        lease = self.engine.leases[id_]
        self.assertTrue(self.engine.state()["outputs"][id_]["automatic"])
        self.engine.save_curve({**shared, "id": curve_id, "points": [[25, 50], [85, 50]]})
        self.assertFalse(self.engine.state()["outputs"][id_]["automatic"])
        self.assertGreaterEqual(self.nvml.states[GPU, 0]["target"], 30)
        self.assertIs(self.engine.leases[id_], lease)

    def test_gpu_fault_overrides_zero_curve_and_auto_resumes_after_recovery(self):
        id_ = self.zero_curve()
        self.engine.activate(id_)
        for temperature in (90, None, 40):
            self.nvml.temperatures[GPU] = temperature
            self.engine.tick()
            output = self.engine.state()["outputs"][id_]
            self.assertEqual(output["automatic"], temperature == 40)
            self.assertEqual(output["target"], 0 if temperature == 40 else 100)
            self.assertEqual(bool(output["fault"]), temperature != 40)

    def test_independent_writer_fault_is_reflected_in_engine_state(self):
        id_ = self.zero_curve()
        self.engine.activate(id_)
        # First read is discovery; the writer observes heat after that sample.
        with patch.object(self.nvml, "temperature", side_effect=[40, 85]):
            self.engine.tick()
        output = self.engine.state()["outputs"][id_]
        self.assertFalse(output["automatic"])
        self.assertEqual(output["target"], 100)
        self.assertTrue(output["fault"])

    def test_zero_rpm_devices_are_discovered_without_any_writes(self):
        self.assertEqual(len(self.engine.config["fans"]), 3)
        fan = self.engine.config["fans"][0]
        self.assertEqual(fan["sensor_ids"], ["nvidia/" + GPU])
        self.assertEqual(fan["min_duty"], 30)
        self.assertFalse(fan["identified"])
        self.assertFalse(fan["autostart"])
        self.assertFalse(self.nvml.writes)
        self.assertTrue(all(c["connection"] == "present" for c in self.engine.discovery()))

    def test_acquire_curve_edit_and_release_keep_one_original_lease(self):
        id_ = self.confirm()
        self.engine.activate(id_)
        lease = self.engine.leases[id_]
        self.engine.patch_fan(id_, {"mode": "manual", "manual_duty": 55, "step_down": 100})
        self.assertIs(self.engine.leases[id_], lease)
        self.engine.tick(now=time.monotonic() + 5)
        self.assertEqual(self.nvml.states[GPU, 0]["target"], 55)
        self.engine.release(id_)
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 0)

    def test_cpu_curve_cannot_hide_gpu_overheat_or_missing_temperature(self):
        id_ = self.confirm()
        # Use the second GPU as a deliberately cool, unrelated curve source.
        self.nvml.temperatures[SECOND_GPU] = 25
        self.engine.patch_fan(id_, {"sensor_ids": ["nvidia/" + SECOND_GPU], "max_duty": 40,
                                    "mode": "manual", "manual_duty": 30})
        self.engine.activate(id_)
        for temperature in (86, None):
            self.nvml.temperatures[GPU] = temperature
            self.engine.tick()
            output = self.engine.state()["outputs"][id_]
            self.assertEqual(output["target"], 100)
            self.assertIn("显卡", output["fault"])

    def test_intended_rpm_is_not_used_as_physical_stall_feedback(self):
        id_ = self.confirm()
        self.engine.activate(id_)
        self.nvml.missing_rpm = True
        self.engine.tick(now=time.monotonic() + 15)
        self.assertIsNone(self.engine.state()["outputs"][id_]["fault"])

    def test_missing_rpm_support_still_allows_discovery_and_control(self):
        self.nvml.missing_rpm = True
        self.engine.tick()
        self.assertTrue(self.engine.snapshot["channels"][0]["writable"])
        id_ = self.confirm()
        self.engine.activate(id_)
        self.engine.release(id_)

    def test_identification_recovers_prior_auto_state_and_protects_gpu(self):
        self.engine.identify(KEY, 50, "fan")
        self.nvml.temperatures[GPU] = None
        self.engine.tick()
        self.assertEqual(self.engine.identification["effective_duty"], 100)
        self.engine.stop_identification()
        self.assertEqual(self.nvml.states[GPU, 0]["policy"], 0)

    def test_gpu_cannot_be_identified_as_a_pump(self):
        with self.assertRaisesRegex(ValueError, "只能"):
            self.engine.confirm_identity(KEY, "GPU", "pump")

    def test_monitor_mode_remains_read_only(self):
        self.engine.allow_control = False
        id_ = self.confirm()
        self.engine.patch_fan(id_, {"mode": "manual", "manual_duty": 70})
        with self.assertRaises(ValueError):
            self.engine.activate(id_)
        self.assertFalse(self.nvml.writes)

    def test_unavailable_nvml_leaves_hwmon_usable(self):
        hwmon = self.root / "hwmon" / "hwmon0"
        hwmon.mkdir(parents=True)
        for key, value in {"name": "nct6687", "pwm1": "128", "pwm1_enable": "2"}.items():
            (hwmon / key).write_text(value)
        with patch("fancontrollx.nvidia.Nvml", side_effect=OSError("no NVIDIA driver")):
            hardware = Hardware(hwmon.parent)
            hardware.gpu = lambda: []
            self.assertEqual(len(hardware.discover()[2]), 1)
            self.assertIn("no NVIDIA", hardware.gpu_control_error)


class UtilityTests(unittest.TestCase):
    def test_channel_validation_and_finite_output(self):
        self.assertEqual(parse_channel(KEY), (GPU, 0))
        for key in ("nvidia/0/fan0", "nvidia/../../fan0", "nvidia/GPU-ab/fan-1"):
            with self.assertRaises(OSError):
                parse_channel(key)
        for value in (True, float("nan"), float("inf")):
            with self.assertRaises(OSError):
                bounded_duty(value, 30, 100)

    def test_service_recovery_hook_and_optional_motherboard_driver(self):
        normal = render_unit(Path("/tmp/app"), Path("/tmp/app/data"))
        gpu_only = render_unit(Path("/tmp/app"), Path("/tmp/app/data"), load_nct6687=False)
        self.assertIn("modprobe nct6687", normal)
        self.assertNotIn("modprobe", gpu_only)
        self.assertIn("ExecStopPost=", gpu_only)
        self.assertIn("--recover --data-dir", gpu_only)
        self.assertNotIn("@", gpu_only)


if __name__ == "__main__":
    unittest.main()
