import copy
import http.client
import json
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from fancontrollx.config import default_config, default_fan, save_config, validate_config
from fancontrollx.engine import ConfigConflict, Engine, interpolate, source_values
from fancontrollx.hardware import DemoHardware, Hardware
from fancontrollx.server import Server
from fancontrollx.install_service import prepare_config


class FixedHardware(DemoHardware):
    def __init__(self):
        super().__init__()
        self.cpu = 45
        self.gpu_temp = 40
        self.writes = []
        self.fail_restore = False
        self.rpm_overrides = {}
        self.channel_names = {}
        self.missing_channels = set()

    def discover(self):
        sensors, fans, channels, chips = super().discover()
        sensors[0]["value"] = self.cpu
        sensors[1]["value"] = self.gpu_temp
        for fan in fans:
            if fan["id"] in self.rpm_overrides:
                fan["rpm"] = self.rpm_overrides[fan["id"]]
        for channel in channels:
            channel["name"] = self.channel_names.get(channel["id"], channel["name"])
        return sensors, fans, [c for c in channels if c["id"] not in self.missing_channels], chips

    def write(self, key, duty):
        self.writes.append((key, duty))
        super().write(key, duty)

    def restore(self, lease):
        if self.fail_restore:
            raise OSError("device unavailable")
        super().restore(lease)


class CoreTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.hw = FixedHardware()
        self.path = Path(self.temp.name) / "config.json"
        self.engine = Engine(self.hw, self.path, demo=True)
        self.addCleanup(self.engine.close)
        self.cpu_id = self.engine.config["fans"][0]["id"]
        self.case_id = self.engine.config["fans"][1]["id"]
        for fan in self.engine.config["fans"]:
            fan.update(identified=True, device_kind="fan")

    def test_interpolation_bounds(self):
        points = [[30, 20], [60, 80], [90, 100]]
        self.assertEqual(interpolate(points, -5), 20)
        self.assertEqual(interpolate(points, 45), 50)
        self.assertEqual(interpolate(points, 120), 100)

    def test_config_editing_never_acquires_or_writes(self):
        self.engine.update_config(self.engine.config)
        self.engine.tick()
        self.assertFalse(self.engine.enabled)
        self.assertEqual(self.hw.writes, [])

    def test_stop_restores_original_duty(self):
        original = self.hw.duties["demo/pwm1"]
        self.engine.activate(self.cpu_id)
        self.assertIn(self.cpu_id, self.engine.enabled)
        self.engine.release(self.cpu_id)
        self.assertEqual(self.hw.duties["demo/pwm1"], original)

    def test_saving_retains_control_but_foreground_restart_does_not_resume(self):
        self.engine.activate(self.cpu_id)
        lease = self.engine.leases[self.cpu_id]
        self.engine.update_config(self.engine.config)
        self.assertIn(self.cpu_id, self.engine.enabled)
        self.assertIs(self.engine.leases[self.cpu_id], lease)
        self.engine.close()
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)
        other = Engine(FixedHardware(), self.path, demo=True)
        self.addCleanup(other.close)
        self.assertFalse(other.enabled)

    def test_missing_input_commands_full_speed_immediately(self):
        self.engine.activate(self.cpu_id)
        self.hw.cpu = None
        self.engine.tick()
        self.assertEqual(self.hw.duties["demo/pwm1"], 100)
        self.assertIn("不可用", self.engine.state()["outputs"][self.cpu_id]["fault"])

    def test_critical_raw_input_bypasses_average_limits_and_delays(self):
        config = copy.deepcopy(self.engine.config)
        fan = config["fans"][1]
        fan.update(sensor_ids=["@cpu-gpu"], aggregation="average", max_duty=60, response_s=30, smoothing_s=60, step_up=1)
        self.engine.update_config(config)
        self.engine.activate(self.case_id)
        self.hw.cpu = 100
        self.hw.gpu_temp = 20
        self.engine.tick()
        output = self.engine.state()["outputs"][self.case_id]
        self.assertEqual(output["temperature"], 60)
        self.assertEqual(output["target"], 100)
        self.assertIn("保护温度", output["fault"])

    def test_missing_gpu_is_not_hidden_by_cpu(self):
        sensors = [{"id": "cpu", "kind": "cpu", "value": 45}]
        self.assertIsNone(source_values(["@cpu-gpu"], sensors))

    def test_monitor_mode_cannot_activate(self):
        engine = Engine(self.hw, Path(self.temp.name) / "readonly.json")
        self.addCleanup(engine.close)
        with self.assertRaisesRegex(ValueError, "监控模式"):
            engine.activate(self.cpu_id)

    def test_bad_config_preserves_active_control(self):
        self.engine.activate(self.cpu_id)
        invalid = copy.deepcopy(self.engine.config)
        invalid["fans"][0]["points"] = [[40, 50], [40, 60]]
        with self.assertRaises(ValueError):
            self.engine.update_config(invalid)
        self.assertIn(self.cpu_id, self.engine.enabled)

    def test_duplicate_channel_cannot_be_owned_twice(self):
        invalid = copy.deepcopy(self.engine.config)
        invalid["fans"][1]["channel_id"] = invalid["fans"][0]["channel_id"]
        with self.assertRaisesRegex(ValueError, "只能绑定"):
            validate_config(invalid)

    def test_invalid_numbers_and_empty_sources_rejected(self):
        for key, value in [("critical_temp", float("nan")), ("manual_duty", True), ("min_duty", 0), ("sensor_ids", [])]:
            with self.subTest(key=key):
                invalid = copy.deepcopy(self.engine.config)
                invalid["fans"][0][key] = value
                with self.assertRaises(ValueError):
                    validate_config(invalid)

    def test_restore_failure_keeps_lease_for_retry(self):
        self.engine.activate(self.cpu_id)
        self.hw.fail_restore = True
        with self.assertRaises(OSError):
            self.engine.release(self.cpu_id)
        self.assertIn(self.cpu_id, self.engine.leases)
        self.assertNotIn(self.cpu_id, self.engine.enabled)
        self.hw.fail_restore = False
        self.engine.release_all()
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)

    def test_failed_restoration_is_retried_in_control_loop(self):
        self.engine.activate(self.cpu_id)
        self.hw.fail_restore = True
        with self.assertRaises(OSError):
            self.engine.release(self.cpu_id)
        self.hw.fail_restore = False
        self.engine.tick()
        self.assertFalse(self.engine.leases)
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)

    def test_invalid_saved_file_is_not_overwritten(self):
        self.path.write_text('{broken')
        with self.assertRaises(ValueError):
            Engine(self.hw, self.path)
        self.assertEqual(self.path.read_text(), '{broken')


class DiscoveryIdentificationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.hw = FixedHardware()
        self.path = Path(self.temp.name) / "config.json"
        self.engine = Engine(self.hw, self.path, demo=True)
        self.addCleanup(self.engine.close)

    def test_startup_discovers_all_rotating_channels_without_writes(self):
        self.assertEqual(len(self.engine.config["fans"]), 3)
        self.assertEqual({f["channel_id"] for f in self.engine.config["fans"]}, set(self.hw.duties))
        self.assertTrue(all(not f["identified"] for f in self.engine.config["fans"]))
        self.assertEqual(self.hw.writes, [])
        self.assertFalse(self.engine.enabled)

    def test_zero_rpm_channel_remains_available_but_is_not_a_connected_fan(self):
        self.hw.rpm_overrides["demo/fan2"] = 0
        other = Engine(self.hw, Path(self.temp.name) / "sparse.json", demo=True)
        self.addCleanup(other.close)
        self.assertEqual(len(other.config["fans"]), 2)
        candidates = other.state()["discovery"]
        self.assertEqual(len(candidates), 3)
        self.assertEqual(next(c for c in candidates if c["id"] == "demo/pwm2")["connection"], "unconfirmed")
        self.assertEqual(self.hw.writes, [])

    def test_no_pwm_means_no_invented_fans(self):
        original = self.hw.discover
        def without_channels():
            sensors, fans, _, chips = original()
            return sensors, fans, [], chips
        self.hw.discover = without_channels
        other = Engine(self.hw, Path(self.temp.name) / "empty.json", demo=True)
        self.addCleanup(other.close)
        self.assertEqual(other.config["fans"], [])

    def test_identification_overrides_then_restores_without_changing_curves(self):
        config = copy.deepcopy(self.engine.config)
        self.engine.identify("demo/pwm1", 100)
        self.assertEqual(self.hw.duties["demo/pwm1"], 100)
        self.engine.identify("demo/pwm1", 70)
        self.assertEqual(self.hw.duties["demo/pwm1"], 70)
        self.engine.stop_identification()
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)
        self.assertEqual(config, self.engine.config)
        self.assertFalse(self.path.exists())

    def test_identification_timeout_does_not_need_a_browser_request(self):
        self.engine.identify("demo/pwm1", 100)
        deadline = self.engine.identification["expires_at"]
        self.engine.tick(now=deadline + .1)
        self.assertIsNone(self.engine.identification)
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)

    def test_identification_still_has_raw_temperature_protection(self):
        self.hw.cpu = 95
        self.engine.identify("demo/pwm1", 60)
        self.assertEqual(self.hw.duties["demo/pwm1"], 100)
        self.assertIn("温度异常", self.engine.state()["identification"]["fault"])

    def test_only_one_identification_test_is_active(self):
        self.engine.identify("demo/pwm1", 100)
        self.engine.identify("demo/pwm2", 100)
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)
        self.assertEqual(self.hw.duties["demo/pwm2"], 100)
        self.engine.stop_identification()
        self.assertEqual(self.hw.duties["demo/pwm2"], 42)

    def test_curve_activation_requires_confirmed_identity(self):
        with self.assertRaisesRegex(ValueError, "先识别"):
            self.engine.activate(self.engine.config["fans"][0]["id"])
        self.assertEqual(self.hw.writes, [])

    def test_confirm_name_preserves_curve_and_does_not_start_control(self):
        self.engine.config["fans"][0]["points"][1] = [42, 39]
        self.engine.identify("demo/pwm1", 100)
        id_ = self.engine.confirm_identity("demo/pwm1", "后置排风", "fan")
        self.assertIsNone(self.engine.identification)
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)
        self.assertFalse(self.engine.enabled)
        f = next(f for f in self.engine.config["fans"] if f["id"] == id_)
        self.assertTrue(f["identified"])
        self.assertEqual(f["points"][1], [42, 39])
        other = Engine(FixedHardware(), self.path, demo=True)
        self.addCleanup(other.close)
        self.assertEqual(next(f for f in other.config["fans"] if f["id"] == id_)["name"], "后置排风")
        self.assertFalse(other.enabled)

    def test_rescan_keeps_names_and_does_not_duplicate_channels(self):
        id_ = self.engine.confirm_identity("demo/pwm1", "塔扇", "fan")
        self.engine.scan_connected()
        self.engine.scan_connected()
        self.assertEqual(len(self.engine.config["fans"]), 3)
        self.assertEqual(next(f for f in self.engine.config["fans"] if f["id"] == id_)["name"], "塔扇")
        self.assertEqual(self.hw.writes, [])

    def test_unconfirmed_channel_can_be_named_after_manual_identification(self):
        self.engine.config["fans"] = [f for f in self.engine.config["fans"] if f["channel_id"] != "demo/pwm2"]
        self.hw.rpm_overrides["demo/fan2"] = 0
        self.engine.tick()
        id_ = self.engine.confirm_identity("demo/pwm2", "停转机箱风扇", "fan")
        self.assertEqual(next(f for f in self.engine.config["fans"] if f["id"] == id_)["channel_id"], "demo/pwm2")

    def test_pump_interface_only_allows_full_speed_until_confirmed_as_fan(self):
        self.hw.channel_names["demo/pwm2"] = "Pump Fan"
        with self.assertRaisesRegex(ValueError, "只允许 100%"):
            self.engine.identify("demo/pwm2", 70)
        self.engine.identify("demo/pwm2", 100)
        self.engine.identify("demo/pwm2", 70, "fan")
        self.assertEqual(self.hw.duties["demo/pwm2"], 70)

    def test_confirmed_pump_has_full_speed_defaults(self):
        id_ = self.engine.confirm_identity("demo/pwm2", "水冷水泵", "pump")
        f = next(f for f in self.engine.config["fans"] if f["id"] == id_)
        self.assertEqual((f["manual_duty"], f["min_duty"], f["max_duty"]), (100, 100, 100))
        self.assertEqual(f["mode"], "manual")

    def test_invalid_identification_inputs_cannot_write(self):
        for duty in (0, 49, 101, float("nan"), True, "100"):
            with self.subTest(duty=duty), self.assertRaises(ValueError):
                self.engine.identify("demo/pwm1", duty)
        self.assertEqual(self.hw.writes, [])

    def test_monitoring_mode_cannot_run_identification(self):
        other = Engine(self.hw, Path(self.temp.name) / "monitor.json")
        self.addCleanup(other.close)
        with self.assertRaisesRegex(ValueError, "识别测试需要"):
            other.identify("demo/pwm1", 100)
        self.assertEqual(self.hw.writes, [])

    def test_identification_restoration_is_retried_on_error(self):
        self.engine.identify("demo/pwm1", 100)
        self.hw.fail_restore = True
        with self.assertRaises(OSError):
            self.engine.stop_identification()
        self.assertTrue(self.engine.identification["stopping"])
        self.hw.fail_restore = False
        self.engine.tick()
        self.assertIsNone(self.engine.identification)
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)

    def test_identification_restores_previous_curve_and_original_lease(self):
        id_ = self.engine.confirm_identity("demo/pwm1", "塔扇", "fan")
        self.engine.activate(id_)
        lease = self.engine.leases[id_]
        self.engine.identify("demo/pwm1", 70)
        self.assertNotIn(id_, self.engine.enabled)
        self.engine.tick()
        self.assertEqual(self.hw.duties["demo/pwm1"], 70)
        self.engine.stop_identification()
        self.assertIn(id_, self.engine.enabled)
        self.assertIs(self.engine.leases[id_], lease)
        self.engine.release(id_)
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)

    def test_shutdown_restores_temporary_identification(self):
        self.engine.identify("demo/pwm1", 100)
        self.engine.close()
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)

    def test_untouched_legacy_placeholders_removed_but_custom_curves_preserved(self):
        old = [default_fan("cpu-fan", "CPU 散热风扇", "@cpu"),
               default_fan("case-fan", "机箱风扇", "@cpu-gpu")]
        old[1]["points"][1][1] = 43
        path = Path(self.temp.name) / "legacy.json"
        path.write_text(json.dumps({"version": 1, "fans": old}))
        other = Engine(FixedHardware(), path, demo=True)
        self.addCleanup(other.close)
        self.assertNotIn("cpu-fan", [f["id"] for f in other.config["fans"]])
        self.assertIn("case-fan", [f["id"] for f in other.config["fans"]])
        self.assertEqual(next(f for f in other.config["fans"] if f["id"] == "case-fan")["points"][1][1], 43)


class ManagedControllerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "config.json"
        seed = Engine(FixedHardware(), self.path, demo=True)
        config = copy.deepcopy(seed.config)
        seed.close()
        for f in config["fans"]:
            f.update(identified=True, device_kind="fan", autostart=False)
        config["fans"][0]["autostart"] = True
        self.path.write_text(json.dumps(config))
        self.hw = FixedHardware()
        self.engine = Engine(self.hw, self.path, demo=True, managed=True)
        self.id = self.engine.config["fans"][0]["id"]
        self.addCleanup(self.engine.close)

    def test_only_explicitly_enabled_confirmed_fans_resume(self):
        self.assertEqual(self.hw.writes, [])
        self.engine.resume_controls()
        self.assertEqual(self.engine.enabled, {self.id})
        self.assertTrue(self.engine.state()["background_service"])
        self.assertFalse(self.engine.pending_autostart)

    def test_service_restart_restores_saved_policy_without_browser(self):
        self.engine.resume_controls()
        self.engine.close()
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)
        self.assertTrue(json.loads(self.path.read_text())["fans"][0]["autostart"])
        other = Engine(FixedHardware(), self.path, demo=True, managed=True)
        self.addCleanup(other.close)
        other.resume_controls()
        self.assertIn(self.id, other.enabled)

    def test_stop_request_persists_across_restart(self):
        self.engine.resume_controls()
        self.engine.request_control(self.id, False)
        self.assertFalse(json.loads(self.path.read_text())["fans"][0]["autostart"])
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)
        other = Engine(FixedHardware(), self.path, demo=True, managed=True)
        self.addCleanup(other.close)
        other.resume_controls()
        self.assertFalse(other.enabled)

    def test_stop_all_disables_future_automatic_takeover(self):
        config = copy.deepcopy(self.engine.config)
        for f in config["fans"]:
            f["autostart"] = True
        self.engine.update_config(config)
        self.assertEqual(len(self.engine.enabled), 3)
        self.engine.request_release_all()
        self.assertFalse(self.engine.enabled)
        self.assertFalse(any(f["autostart"] for f in json.loads(self.path.read_text())["fans"]))
        self.engine.queue_autostart()
        self.engine.resume_controls()
        self.assertFalse(self.engine.enabled)

    def test_save_applies_settings_to_background_control(self):
        self.engine.resume_controls()
        config = copy.deepcopy(self.engine.config)
        config["fans"][0].update(mode="manual", manual_duty=70, response_s=0, step_down=100)
        self.engine.update_config(config)
        self.assertIn(self.id, self.engine.enabled)
        self.engine.tick(now=time.monotonic() + 1)
        self.assertAlmostEqual(self.hw.duties["demo/pwm1"], 70)

    def test_curve_edit_retains_existing_control_lease(self):
        self.engine.resume_controls()
        lease = self.engine.leases[self.id]
        config = copy.deepcopy(self.engine.config)
        config["fans"][0]["points"][1] = [41, 37]
        self.engine.update_config(config)
        self.assertIs(self.engine.leases[self.id], lease)
        self.assertIn(self.id, self.engine.enabled)

    def test_unchecking_background_control_releases_hardware(self):
        self.engine.resume_controls()
        config = copy.deepcopy(self.engine.config)
        config["fans"][0]["autostart"] = False
        self.engine.update_config(config)
        self.engine.resume_controls()
        self.assertFalse(self.engine.enabled)
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)

    def test_unidentified_or_unbound_devices_never_autostart(self):
        config = copy.deepcopy(self.engine.config)
        config["fans"][0]["identified"] = False
        config["fans"][1].update(channel_id=None, autostart=True)
        self.engine.update_config(config)
        self.assertFalse(self.engine.enabled)
        self.assertEqual(self.hw.writes, [])

    def test_monitoring_mode_cannot_autostart_even_with_saved_policy(self):
        hw = FixedHardware()
        other = Engine(hw, self.path, managed=True)
        self.addCleanup(other.close)
        other.resume_controls()
        self.assertEqual(hw.writes, [])
        self.assertFalse(other.enabled)

    def test_late_device_is_retried_without_losing_its_policy(self):
        self.hw.missing_channels.add("demo/pwm1")
        self.engine.tick()
        self.engine.resume_controls()
        self.assertFalse(self.engine.enabled)
        self.assertIn(self.id, self.engine.pending_autostart)
        self.assertEqual(self.hw.writes, [])
        self.hw.missing_channels.clear()
        future = time.monotonic() + 11
        with patch("fancontrollx.engine.time.monotonic", return_value=future):
            self.engine.tick()
            self.engine.resume_controls()
        self.assertIn(self.id, self.engine.enabled)

    def test_retry_never_overwrites_an_unrestored_lease(self):
        self.engine.resume_controls()
        original = self.engine.leases[self.id]
        self.hw.fail_restore = True
        with self.assertRaises(OSError):
            self.engine.release(self.id)
        self.engine.queue_autostart()
        writes = list(self.hw.writes)
        self.engine.resume_controls()
        self.assertIs(self.engine.leases[self.id], original)
        self.assertEqual(writes, self.hw.writes)
        self.hw.fail_restore = False
        self.engine.tick()
        future = time.monotonic() + 11
        with patch("fancontrollx.engine.time.monotonic", return_value=future):
            self.engine.resume_controls()
        self.assertIn(self.id, self.engine.enabled)

    def test_identification_preserves_durable_policy_and_resumes_after_restart(self):
        self.engine.resume_controls()
        self.engine.identify("demo/pwm1", 100)
        self.assertTrue(json.loads(self.path.read_text())["fans"][0]["autostart"])
        self.engine.queue_autostart()
        self.engine.resume_controls()
        self.assertIsNotNone(self.engine.identification)
        self.engine.stop_identification()
        self.engine.resume_controls()
        self.assertIn(self.id, self.engine.enabled)
        self.engine.close()
        self.assertEqual(self.hw.duties["demo/pwm1"], 38)
        other = Engine(FixedHardware(), self.path, demo=True, managed=True)
        self.addCleanup(other.close)
        other.resume_controls()
        self.assertIn(self.id, other.enabled)

    def test_background_loop_runs_without_any_browser_client(self):
        self.engine.start()
        self.hw.cpu = 95
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            if self.engine.state()["outputs"][self.id]["fault"]:
                break
            time.sleep(.05)
        self.assertEqual(self.hw.duties["demo/pwm1"], 100)
        self.assertIn("保护温度", self.engine.state()["outputs"][self.id]["fault"])

    def test_installer_enables_only_confirmed_bindings_and_preserves_curves(self):
        original = copy.deepcopy(self.engine.config)
        original["fans"][1]["identified"] = False
        original["fans"][2]["channel_id"] = None
        prepared = prepare_config(original, enable_confirmed=True)
        self.assertTrue(prepared["fans"][0]["autostart"])
        self.assertFalse(prepared["fans"][1]["autostart"])
        self.assertFalse(prepared["fans"][2]["autostart"])
        self.assertEqual(prepared["fans"][0]["points"], original["fans"][0]["points"])
        self.assertFalse(original["fans"][1]["autostart"])


class HardwareTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.hw = self.root / "class" / "hwmon1"
        self.hw.mkdir(parents=True)
        for name, value in {"name": "nct6687", "pwm1": "100", "pwm1_enable": "2", "temp1_input": "45000"}.items():
            (self.hw / name).write_text(value)
        self.backend = Hardware(self.root / "class")
        self.backend.gpu = lambda: []

    def test_full_acquire_restore_cycle_on_fake_sysfs(self):
        _, _, channels, _ = self.backend.discover()
        key = channels[0]["id"]
        lease = self.backend.acquire(key, 60)
        self.assertEqual((self.hw / "pwm1_enable").read_text().strip(), "1")
        self.assertEqual((self.hw / "pwm1").read_text().strip(), "153")
        self.backend.restore(lease)
        self.assertEqual((self.hw / "pwm1_enable").read_text().strip(), "2")
        self.assertEqual((self.hw / "pwm1").read_text().strip(), "100")

    def test_unknown_driver_is_read_only(self):
        (self.hw / "name").write_text("unknown_ec")
        _, _, channels, _ = self.backend.discover()
        self.assertFalse(channels[0]["writable"])

    def test_id_survives_hwmon_renumbering(self):
        real = self.root / "devices" / "chip0" / "hwmon" / "hwmon1"
        real.mkdir(parents=True)
        (real / "name").write_text("nct6687")
        (real / "pwm1").write_text("100")
        (real / "pwm1_enable").write_text("2")
        link = self.root / "links"
        link.mkdir()
        (link / "hwmon1").symlink_to(real)
        backend = Hardware(link)
        backend.gpu = lambda: []
        id1 = backend.discover()[2][0]["id"]
        (link / "hwmon1").unlink()
        renamed = real.rename(real.with_name("hwmon8"))
        (link / "hwmon8").symlink_to(renamed)
        self.assertEqual(id1, backend.discover()[2][0]["id"])


class HTTPTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.engine = Engine(FixedHardware(), Path(self.temp.name) / "config.json", demo=True)
        self.cpu_id = self.engine.config["fans"][0]["id"]
        for fan in self.engine.config["fans"]:
            fan.update(identified=True, device_kind="fan")
        self.server = Server(("127.0.0.1", 0), self.engine)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.cleanup)

    def cleanup(self):
        self.server.shutdown()
        self.server.server_close()
        self.engine.close()
        self.temp.cleanup()

    def request(self, method, path, value=None, headers=None):
        connection = http.client.HTTPConnection("127.0.0.1", self.server.server_port, timeout=5)
        body = json.dumps(value) if value is not None else None
        connection.request(method, path, body=body, headers=headers or {})
        response = connection.getresponse()
        status, content = response.status, response.read()
        connection.close()
        return status, content

    def test_no_token_cannot_change_configuration(self):
        status, _ = self.request("POST", "/api/config", default_config(), {"Content-Type": "application/json"})
        self.assertEqual(status, 403)
        self.assertFalse(self.engine.config_path.exists())

    def test_foreign_host_and_origin_are_rejected(self):
        self.assertEqual(self.request("GET", "/api/state", headers={"Host": "attacker.example"})[0], 403)
        self.assertEqual(self.request("POST", "/api/config", default_config(), {
            "Content-Type": "application/json", "Origin": "https://evil.example",
            "X-FanControl-Token": self.server.csrf})[0], 403)

    def test_save_and_activate_are_separate(self):
        headers = {"Content-Type": "application/json", "X-FanControl-Token": self.server.csrf}
        config = self.engine.config
        self.assertEqual(self.request("POST", "/api/config", config, headers)[0], 200)
        self.assertFalse(self.engine.enabled)
        self.assertEqual(self.request("POST", "/api/activate", {"id": self.cpu_id}, headers)[0], 200)
        self.assertTrue(self.engine.enabled)

    def test_static_path_traversal_is_rejected(self):
        self.assertEqual(self.request("GET", "/../fancontrollx/server.py")[0], 404)

    def test_identification_state_is_json_serializable_and_excludes_lease(self):
        headers = {"Content-Type": "application/json", "X-FanControl-Token": self.server.csrf}
        self.assertEqual(self.request("POST", "/api/identify", {"channel_id": "demo/pwm1", "duty": 100}, headers)[0], 200)
        status, data = self.request("GET", "/api/state")
        self.assertEqual(status, 200)
        state = json.loads(data)
        self.assertEqual(state["identification"]["effective_duty"], 100)
        self.assertNotIn("lease", state["identification"])
        self.assertEqual(self.request("POST", "/api/identify-stop", {"channel_id": "demo/pwm1"}, headers)[0], 200)
        self.assertIsNone(self.engine.identification)

    def test_identification_write_requires_request_token(self):
        status, _ = self.request("POST", "/api/identify", {"channel_id": "demo/pwm1", "duty": 100}, {"Content-Type": "application/json"})
        self.assertEqual(status, 403)
        self.assertIsNone(self.engine.identification)

    def test_identity_confirmation_and_scan_preserve_name(self):
        headers = {"Content-Type": "application/json", "X-FanControl-Token": self.server.csrf}
        status, response = self.request("POST", "/api/identity", {"channel_id": "demo/pwm1", "name": "机箱后置", "device_kind": "fan"}, headers)
        self.assertEqual(status, 200)
        id_ = json.loads(response)["id"]
        self.assertEqual(self.request("POST", "/api/scan", {}, headers)[0], 200)
        self.assertEqual(next(f for f in self.engine.config["fans"] if f["id"] == id_)["name"], "机箱后置")

    def test_managed_web_start_and_stop_are_persistent(self):
        self.engine.managed = True
        headers = {"Content-Type": "application/json", "X-FanControl-Token": self.server.csrf}
        self.assertEqual(self.request("POST", "/api/activate", {"id": self.cpu_id}, headers)[0], 200)
        self.assertTrue(json.loads(self.engine.config_path.read_text())["fans"][0]["autostart"])
        self.assertIn(self.cpu_id, self.engine.enabled)
        self.assertEqual(self.request("POST", "/api/release", {"id": self.cpu_id}, headers)[0], 200)
        self.assertFalse(json.loads(self.engine.config_path.read_text())["fans"][0]["autostart"])
        self.assertNotIn(self.cpu_id, self.engine.enabled)

    def test_managed_web_save_applies_control_policy(self):
        self.engine.managed = True
        config = copy.deepcopy(self.engine.config)
        config["fans"][0]["autostart"] = True
        headers = {"Content-Type": "application/json", "X-FanControl-Token": self.server.csrf}
        self.assertEqual(self.request("POST", "/api/config", config, headers)[0], 200)
        self.assertIn(self.cpu_id, self.engine.enabled)

    def test_state_and_assets_serve(self):
        for path in ("/", "/app.js", "/style.css", "/favicon.svg", "/api/health", "/api/state"):
            with self.subTest(path=path):
                self.assertEqual(self.request("GET", path)[0], 200)


if __name__ == "__main__":
    unittest.main()
