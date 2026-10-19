"""Fan policy and control loop. Configuration alone never acquires hardware."""

import copy
import hashlib
import json
import logging
import math
import threading
import time
import uuid
from collections import deque
from pathlib import Path

from .config import default_config, default_fan, number, save_config, validate_config

log = logging.getLogger(__name__)


class ConfigConflict(OSError):
    """An editor tried to overwrite settings changed by another client."""


def interpolate(points, temperature):
    if temperature <= points[0][0]:
        return points[0][1]
    for (x0, y0), (x1, y1) in zip(points, points[1:]):
        if temperature <= x1:
            return y0 + (y1 - y0) * (temperature - x0) / (x1 - x0)
    return points[-1][1]


def source_values(ids, sensors):
    result = []
    lookup = {s["id"]: s for s in sensors}
    for id_ in ids:
        if id_ in ("@cpu", "@gpu", "@cpu-gpu"):
            kinds = {"cpu", "gpu"} if id_ == "@cpu-gpu" else {id_[1:]}
            for kind in kinds:
                # Require every selected group. A missing GPU is not silently ignored.
                group = [s["value"] for s in sensors if s["kind"] == kind]
                if not group or any(v is None for v in group):
                    return None
                result.append(max(group))
        else:
            sensor = lookup.get(id_)
            if sensor is None or sensor["value"] is None:
                return None
            result.append(sensor["value"])
    return result or None


class Engine:
    def __init__(self, hardware, config_path, allow_control=False, demo=False, managed=False):
        self.hardware = hardware
        self.config_path = Path(config_path)
        self.allow_control = allow_control or demo
        self.demo = demo
        self.managed = managed
        self.pending_autostart = {}
        self.lock = threading.RLock()
        self.stop_event = threading.Event()
        self.thread = None
        self.leases = {}
        self.enabled = set()
        self.identification = None
        self.memory = {}
        self.history = deque(maxlen=180)
        self.events = deque(maxlen=60)
        self.error = None
        if self.config_path.exists():
            # Fail clearly on malformed files; never overwrite them with defaults.
            self.config = validate_config(json.loads(self.config_path.read_text()))
        else:
            self.config = default_config()
        self.snapshot = {"sensors": [], "fans": [], "channels": [], "chips": [], "outputs": {}}
        self.tick()
        self.scan_connected(persist=False)
        self.queue_autostart()

    def event(self, message):
        self.events.appendleft({"time": time.time(), "message": message})
        log.info(message)

    def start(self):
        self.resume_controls()
        self.thread = threading.Thread(target=self.run, name="fan-control", daemon=True)
        self.thread.start()

    def run(self):
        while not self.stop_event.wait(1):
            try:
                self.tick()
                self.resume_controls()
            except Exception as exc:
                log.exception("Control cycle failed")
                with self.lock:
                    self.error = str(exc)
                    try:
                        self.release_all()
                    except OSError:
                        log.exception("Could not restore all channels; will retry")
                    self.queue_autostart()

    def queue_autostart(self):
        if not self.managed:
            return
        with self.lock:
            self.pending_autostart = {
                f["id"]: {"reason": "等待后台接管", "retry_at": 0}
                for f in self.config["fans"]
                if f["autostart"] and f["identified"] and f["channel_id"] and f["id"] not in self.enabled}

    def resume_controls(self):
        if not self.managed or not self.allow_control or self.stop_event.is_set() or self.identification:
            return
        with self.lock:
            if self.identification:
                return
            for id_, pending in list(self.pending_autostart.items()):
                if time.monotonic() < pending["retry_at"]:
                    continue
                try:
                    self.activate(id_)
                except (OSError, ValueError, KeyError) as exc:
                    reason = str(exc)
                    if pending["reason"] != reason:
                        self.event(f"后台接管等待：{reason}")
                    pending.update(reason=reason, retry_at=time.monotonic() + 10)
                else:
                    self.pending_autostart.pop(id_, None)

    def request_control(self, id_, enabled):
        """Web requests set a durable control policy when managed by systemd."""
        with self.lock:
            if self.identification:
                self.stop_identification()
            if not self.managed:
                return self.activate(id_) if enabled else self.release(id_)
            config = copy.deepcopy(self.config)
            f = next((f for f in config["fans"] if f["id"] == id_), None)
            if f is None:
                raise ValueError("风扇不存在")
            if enabled and (not f["identified"] or not f["channel_id"]):
                raise ValueError("请先识别并绑定设备，再启用后台控制")
            f["autostart"] = enabled
            save_config(self.config_path, validate_config(config))
            self.config = config
            if enabled:
                self.pending_autostart[id_] = {"reason": "等待后台接管", "retry_at": 0}
                self.resume_controls()
            else:
                self.release(id_)
            self.tick()

    def request_release_all(self):
        with self.lock:
            if self.managed:
                config = copy.deepcopy(self.config)
                for f in config["fans"]:
                    f["autostart"] = False
                save_config(self.config_path, config)
                self.config = config
            self.release_all()

    def tick(self, now=None):
        now = time.monotonic() if now is None else now
        with self.lock:
            sensors, fans, channels, chips = self.hardware.discover()
            test = self.identification
            if test and (test["stopping"] or now >= test["expires_at"]):
                self.stop_identification()
                test = None
            for id_ in list(self.leases):
                if id_ not in self.enabled and not (test and test.get("resume_id") == id_):
                    try:
                        self.release(id_, cancel_pending=False)
                    except OSError:
                        pass  # Retain the lease and retry on the next cycle.
            lookup = {c["id"]: c for c in channels}
            rpm = {f["id"]: f["rpm"] for f in fans}
            if test:
                try:
                    channel = lookup.get(test["channel_id"])
                    if channel is None or not channel["writable"]:
                        raise OSError("识别通道离线或不可写")
                    duty, fault = self.identification_duty(test["duty"], sensors, channel)
                    self.hardware.write(test["channel_id"], duty)
                    test["effective_duty"], test["fault"] = duty, fault
                except (OSError, KeyError):
                    self.stop_identification()
                    raise
            outputs = {}
            for f in self.config["fans"]:
                channel = lookup.get(f["channel_id"])
                values = source_values(f["sensor_ids"], sensors)
                raw = None if values is None else (sum(values) / len(values) if f["aggregation"] == "average"
                                                   else (min(values) if f["aggregation"] == "min" else max(values)))
                memory = self.memory.setdefault(f["id"], {"samples": deque(), "last_time": now,
                                                         "changed_at": now, "temp": None, "duty": None})
                samples = memory["samples"]
                if raw is not None:
                    samples.append((now, raw))
                while samples and samples[0][0] < now - f["smoothing_s"]:
                    samples.popleft()
                smoothed = sum(x[1] for x in samples) / len(samples) if samples and raw is not None else raw
                fault = None
                requested = None
                automatic = False
                # Emergency threshold sees the hottest RAW input, bypassing averaging and all delays.
                if values is None:
                    duty, fault = 100, "温度源不可用，保护输出 100%"
                elif max(values) >= f["critical_temp"]:
                    duty, fault = 100, "达到保护温度，输出 100%"
                else:
                    effective = smoothed
                    if memory["temp"] is not None and abs(effective - memory["temp"]) < f["hysteresis"]:
                        effective = memory["temp"]
                    else:
                        memory["temp"] = effective
                    points = next((c["points"] for c in self.config["curves"] if c["id"] == f["curve_id"]), f["points"])
                    duty = f["manual_duty"] if f["mode"] == "manual" else interpolate(points, effective)
                    automatic = bool(channel and channel.get("curve_zero_auto") and f["mode"] == "curve" and duty == 0)
                    if not automatic:
                        duty = max(f["min_duty"], min(f["max_duty"], duty))
                    requested = duty
                    previous = memory["duty"]
                    # Zero is a policy change, not a manual speed to approach.
                    # Ramping towards it would get stuck at the driver's floor.
                    if automatic:
                        if previous != 0:
                            memory["changed_at"] = now
                    elif previous is not None:
                        if channel and channel.get("curve_zero_auto") and previous == 0:
                            # Leaving automatic control starts at a valid
                            # running output, rather than ramping through zero.
                            previous = max(f["min_duty"], channel.get("min_duty", 0))
                        if f["mode"] != "manual" and now - memory["changed_at"] < f["response_s"]:
                            duty = previous
                        dt = max(0, min(5, now - memory["last_time"]))
                        duty = max(previous - f["step_down"] * dt, min(previous + f["step_up"] * dt, duty))
                        if abs(duty - previous) >= .1:
                            memory["changed_at"] = now
                # A GPU must protect itself even when its curve follows CPU or
                # another custom source. Never average away its own hot input.
                if channel and channel.get("temperature_sensor_id"):
                    own = source_values([channel["temperature_sensor_id"]], sensors)
                    if own is None:
                        duty, fault = 100, "显卡自身温度不可用，保护输出 100%"
                    elif max(own) >= min(85, f["critical_temp"]):
                        duty, fault = 100, "显卡达到保护温度，输出 100%"
                tach = rpm.get(f["rpm_sensor_id"])
                active_since = memory.get("active_since", now)
                tach_sensor = next((s for s in fans if s["id"] == f["rpm_sensor_id"]), {})
                if (f["id"] in self.enabled and now - active_since > 10 and f["rpm_sensor_id"]
                        and tach_sensor.get("rpm_is_feedback", True) and (tach is None or tach <= 0)):
                    duty, fault = 100, "测速信号丢失或风扇停转，保护输出 100%"
                    requested = 100
                if fault:
                    automatic = False
                if channel and not automatic:
                    duty = max(channel.get("min_duty", 0), min(channel.get("max_duty", 100), duty))
                    if requested is not None:
                        requested = max(channel.get("min_duty", 0), min(channel.get("max_duty", 100), requested))
                if fault:
                    requested = duty
                memory["last_time"], memory["duty"] = now, duty
                status = "待绑定通道" if not f["channel_id"] else ("通道离线" if channel is None else "未接管")
                if f["id"] in self.enabled:
                    try:
                        if channel is None or not channel["writable"]:
                            raise OSError("控制通道离线或不可写")
                        result = self.hardware.write(f["channel_id"], duty)
                        if isinstance(result, dict) and result.get("fault"):
                            # The independent writer can observe a new GPU
                            # fault after this cycle's telemetry was sampled.
                            automatic = False
                            duty = requested = result["duty"]
                            memory["duty"] = duty
                            fault = fault or result["fault"]
                        status = "保护运行" if fault else "显卡自动调速" if automatic else "正在控制"
                    except (OSError, KeyError) as exc:
                        self.release(f["id"])
                        if self.managed and f["autostart"]:
                            self.pending_autostart[f["id"]] = {"reason": str(exc), "retry_at": now + 10}
                        status = "控制失败"
                        fault = str(exc)
                        self.event(f"{f['name']}：{exc}")
                outputs[f["id"]] = {"temperature": round(raw, 1) if raw is not None else None,
                                      "smoothed": round(smoothed, 1) if smoothed is not None else None,
                                      "target": round(duty, 1), "rpm": tach, "status": status,
                                      "requested": round(requested if requested is not None else duty, 1),
                                      "actual": channel.get("duty") if channel else None,
                                      "automatic": automatic,
                                      "active": f["id"] in self.enabled, "fault": fault}
            self.snapshot = {"sensors": sensors, "fans": fans, "channels": channels,
                             "chips": chips, "outputs": outputs}
            self.history.append({"time": time.time(),
                                 "cpu": self.group_temperature(sensors, "cpu"),
                                 "gpu": self.group_temperature(sensors, "gpu")})

    @staticmethod
    def group_temperature(sensors, kind):
        values = [s["value"] for s in sensors if s["kind"] == kind and s["value"] is not None]
        return max(values) if values else None

    def state(self):
        with self.lock:
            test = self.identification
            identification = None if test is None else {
                "channel_id": test["channel_id"], "duty": test["duty"],
                "effective_duty": test["effective_duty"], "fault": test["fault"],
                "remaining_s": max(0, math.ceil(test["expires_at"] - time.monotonic())),
                "stopping": test["stopping"]}
            return copy.deepcopy({**self.snapshot, "config": self.config, "history": list(self.history),
                                  "events": list(self.events), "demo": self.demo,
                                  "control_allowed": self.allow_control, "error": self.error,
                                  "gpu_error": self.hardware.gpu_error, "time": time.time(),
                                  "gpu_control_error": getattr(self.hardware, "gpu_control_error", None),
                                  "discovery": self.discovery(), "identification": identification,
                                  "background_service": self.managed,
                                  "config_revision": self.config_revision(),
                                  "workspace_api": 2,
                                  "pending_autostart": {id_: p["reason"] for id_, p in self.pending_autostart.items()}})

    def discovery(self):
        tachometers = {f["id"]: f for f in self.snapshot["fans"]}
        bindings = {f["channel_id"]: f for f in self.config["fans"] if f["channel_id"]}
        result = []
        for channel in self.snapshot["channels"]:
            bound = bindings.get(channel["id"])
            rpm_id = bound["rpm_sensor_id"] if bound and bound["rpm_sensor_id"] else channel.get("rpm_sensor_id")
            tach = tachometers.get(rpm_id, {})
            rpm = tach.get("rpm")
            result.append({**channel, "rpm_sensor_id": rpm_id, "rpm": rpm,
                           "connection": "rotating" if rpm is not None and rpm > 0 else
                                         "present" if channel.get("present") else "unconfirmed",
                           "fan_id": bound["id"] if bound else None,
                           "display_name": bound["name"] if bound else channel["name"],
                           "identified": bound["identified"] if bound else False,
                           "device_kind": bound["device_kind"] if bound else "unknown"})
        return result

    def discovered_fan(self, channel):
        id_ = "fan-" + hashlib.sha256(channel["id"].encode()).hexdigest()[:16]
        # Hardware names never determine which temperature a user should follow.
        f = default_fan(id_, channel["name"], channel.get("temperature_sensor_id", "@cpu"))
        f.update(channel_id=channel["id"], rpm_sensor_id=channel.get("rpm_sensor_id"))
        if channel.get("backend") == "nvidia":
            f.update(min_duty=channel["min_duty"], max_duty=channel["max_duty"])
        return f

    def scan_connected(self, persist=True):
        with self.lock:
            self.tick()
            config = copy.deepcopy(self.config)
            # Remove only the exact, untouched placeholders shipped in 0.1.
            placeholders = [default_fan("cpu-fan", "CPU 散热风扇", "@cpu"),
                            default_fan("case-fan", "机箱风扇", "@cpu-gpu")]
            config["fans"] = [f for f in config["fans"] if f not in placeholders]
            bound = {f["channel_id"] for f in config["fans"]}
            added = 0
            for candidate in self.discovery():
                if candidate["connection"] in ("rotating", "present") and candidate["id"] not in bound:
                    if len(config["fans"]) >= 64:
                        break
                    f = self.discovered_fan(candidate)
                    # Respect an imported user ID even if it resembles our generated ID.
                    if any(existing["id"] == f["id"] for existing in config["fans"]):
                        continue
                    config["fans"].append(f)
                    bound.add(candidate["id"])
                    added += 1
            config = validate_config(config)
            if persist:
                save_config(self.config_path, config)
            self.config = config
            self.tick()
            if persist:
                self.event(f"扫描完成，新增 {added} 个在线设备；未启用新的控制")
            return added

    @staticmethod
    def identification_duty(duty, sensors, channel=None):
        if channel and channel.get("temperature_sensor_id"):
            own = source_values([channel["temperature_sensor_id"]], sensors)
            if own is None or max(own) >= 85:
                return 100, "显卡温度异常，识别测试保持 100%"
            return duty, None
        cpu = source_values(["@cpu"], sensors)
        readings = [s["value"] for s in sensors if s["kind"] in ("cpu", "gpu") and s["value"] is not None]
        if cpu is None or (readings and max(readings) >= 85):
            return 100, "温度异常，识别测试保持 100%"
        return duty, None

    def identify(self, channel_id, duty=100, device_kind="unknown"):
        number(duty, 50, 100, "识别输出")
        if device_kind not in ("unknown", "fan", "pump"):
            raise ValueError("设备类型无效")
        with self.lock:
            if not self.allow_control:
                raise ValueError("识别测试需要 sudo ./webui.sh --control；当前仅扫描和编辑")
            self.tick()
            channel = next((c for c in self.discovery() if c["id"] == channel_id), None)
            if channel is None:
                raise ValueError("识别通道不存在或已离线")
            if not channel["writable"]:
                raise ValueError(channel["reason"])
            pump = device_kind == "pump" or ("pump" in channel["name"].lower() and device_kind != "fan")
            if pump and duty != 100:
                raise ValueError("水泵或未确认的 Pump 接口只允许 100% 识别；确认接的是普通风扇后再降低输出")
            # Finish a previous test before taking a different channel.
            if self.identification and self.identification["channel_id"] != channel_id:
                self.stop_identification()
            effective, fault = self.identification_duty(duty, self.snapshot["sensors"], channel)
            if self.identification is None:
                active = next((f["id"] for f in self.config["fans"]
                               if f["channel_id"] == channel_id and f["id"] in self.enabled), None)
                if active:
                    # Borrow the existing lease: keep its original firmware state
                    # and durable policy intact throughout the temporary test.
                    lease = self.leases[active]
                    self.hardware.write(channel_id, effective)
                    self.enabled.remove(active)
                else:
                    lease = self.hardware.acquire(channel_id, effective)
                self.identification = {"channel_id": channel_id, "lease": lease,
                                       "resume_id": active,
                                       "stopping": False, "duty": duty,
                                       "effective_duty": effective, "fault": fault,
                                       "expires_at": time.monotonic() + 15}
                self.event(f"开始识别 {channel['name']}；15 秒无操作后恢复")
            else:
                try:
                    self.hardware.write(channel_id, effective)
                except (OSError, KeyError):
                    self.stop_identification()
                    raise
                self.identification.update(duty=duty, effective_duty=effective,
                                           fault=fault, expires_at=time.monotonic() + 15)

    def stop_identification(self, channel_id=None, resume=True):
        with self.lock:
            test = self.identification
            if test is None or (channel_id is not None and test["channel_id"] != channel_id):
                return
            if test["stopping"]:
                resume = resume and test.get("resume_requested", True)
            test["resume_requested"] = resume
            test["stopping"] = True
            active = test.get("resume_id")
            if active and resume and not self.stop_event.is_set():
                self.enabled.add(active)
                self.identification = None
                # Begin at the last test output and let the normal limits apply.
                self.memory[active]["duty"] = test["effective_duty"]
                self.event("识别测试结束，已恢复此前的控制模式")
                return
            try:
                self.hardware.restore(test["lease"])
            except OSError as exc:
                self.error = f"识别结束后恢复失败，正在重试：{exc}"
                self.event(self.error)
                raise
            self.identification = None
            if active:
                self.leases.pop(active, None)
            if not self.leases:
                self.hardware.disarm([test["lease"]])
            self.event("识别测试结束，已恢复接管前状态")

    def confirm_identity(self, channel_id, name, device_kind):
        if not isinstance(name, str) or not 1 <= len(name.strip()) <= 100:
            raise ValueError("请输入 1–100 个字符的设备名称")
        if device_kind not in ("fan", "pump"):
            raise ValueError("请确认接入的是普通风扇还是水泵")
        with self.lock:
            self.tick()
            candidate = next((c for c in self.discovery() if c["id"] == channel_id), None)
            if candidate is None:
                raise ValueError("通道不存在或已离线")
            if candidate.get("backend") == "nvidia" and device_kind != "fan":
                raise ValueError("NVIDIA 显卡通道只能确认为风扇")
            config = copy.deepcopy(self.config)
            f = next((f for f in config["fans"] if f["channel_id"] == channel_id), None)
            if f is None:
                f = self.discovered_fan(candidate)
                config["fans"].append(f)
            f.update(name=name.strip(), identified=True, device_kind=device_kind)
            if device_kind == "pump":
                f.update(mode="manual", manual_duty=100, min_duty=100, max_duty=100)
            config = validate_config(config)
            self.stop_identification(channel_id)
            save_config(self.config_path, config)
            self.config = config
            self.tick()
            self.event(f"已保存设备：{f['name']}")
            return f["id"]

    def config_revision(self):
        return hashlib.sha256(json.dumps(self.config, sort_keys=True).encode()).hexdigest()

    def update_config(self, value, expected_revision=None):
        with self.lock:
            if expected_revision is not None and expected_revision != self.config_revision():
                raise ConfigConflict("配置已在另一页面更新，请重新打开后再操作")
            config = validate_config(value)
            previous = {f["id"]: f for f in self.config["fans"]}
            for f in config["fans"]:
                old = previous.get(f["id"])
                if old and old["channel_id"] != f["channel_id"]:
                    f.update(identified=False, device_kind="unknown", autostart=False)
            # Persist first. A disk error must not silently stop running fans.
            save_config(self.config_path, config)
            self.stop_identification()
            wanted = {f["id"]: f for f in config["fans"] if f["identified"] and f["channel_id"]
                      and (f["autostart"] if self.managed else f["id"] in self.enabled)}
            for id_ in list(self.leases):
                if id_ not in wanted or wanted[id_]["channel_id"] != previous[id_]["channel_id"]:
                    self.release(id_)
            for f in config["fans"]:
                old = previous.get(f["id"])
                if f["id"] in self.memory and old and any(f[k] != old[k] for k in ("sensor_ids", "aggregation", "smoothing_s")):
                    self.memory[f["id"]]["samples"].clear()
                    self.memory[f["id"]]["temp"] = None
            self.config = config
            self.tick()
            self.queue_autostart()
            if self.managed:
                self.resume_controls()
                self.event("配置已保存并应用后台控制策略")
            else:
                self.event("配置已保存，正在控制的设备已更新")

    def patch_fan(self, id_, changes, expected=None):
        with self.lock:
            if not isinstance(changes, dict) or not changes or set(changes) - (set(default_fan()) - {"id", "identified"}):
                raise ValueError("风扇设置字段无效")
            config = copy.deepcopy(self.config)
            fan = next((f for f in config["fans"] if f["id"] == id_), None)
            if fan is None:
                raise ValueError("风扇不存在")
            if expected is not None and (not isinstance(expected, dict) or any(fan.get(k) != v for k, v in expected.items())):
                raise ConfigConflict("此风扇的设置已更新，请重新打开后再操作")
            fan.update(changes)
            self.update_config(config)

    def save_curve(self, curve, expected=None, assign_to=None, expected_fan=None):
        with self.lock:
            if not isinstance(curve, dict):
                raise ValueError("曲线必须是对象")
            config = copy.deepcopy(self.config)
            current = next((c for c in config["curves"] if c["id"] == curve.get("id")), None)
            if expected is not None and current != expected:
                raise ConfigConflict("此曲线已更新，请重新打开后再操作")
            id_ = curve.get("id") or str(uuid.uuid4())
            saved = {"id": id_, "name": curve.get("name"), "points": curve.get("points")}
            if current:
                config["curves"][config["curves"].index(current)] = saved
            else:
                config["curves"].append(saved)
            if assign_to:
                fan = next((f for f in config["fans"] if f["id"] == assign_to), None)
                if fan is None:
                    raise ValueError("风扇不存在")
                if expected_fan is not None and any(fan.get(k) != v for k, v in expected_fan.items()):
                    raise ConfigConflict("风扇曲线已更新，请重新打开后再操作")
                fan["curve_id"] = id_
            self.update_config(config)
            return id_

    def delete_curve(self, id_, expected=None):
        with self.lock:
            config = copy.deepcopy(self.config)
            curve = next((c for c in config["curves"] if c["id"] == id_), None)
            if curve is None:
                raise ValueError("曲线不存在")
            if expected is not None and curve != expected:
                raise ConfigConflict("此曲线已更新，请刷新后重试")
            if any(f["curve_id"] == id_ for f in config["fans"]):
                raise ValueError("请先为引用此曲线的风扇选择其他曲线")
            config["curves"].remove(curve)
            self.update_config(config)

    def activate(self, id_):
        with self.lock:
            if not self.allow_control:
                raise ValueError("当前为监控模式。实际调速请使用 sudo ./webui.sh --control 启动")
            f = next((f for f in self.config["fans"] if f["id"] == id_), None)
            if f is None:
                raise ValueError("风扇不存在")
            if not f["identified"]:
                raise ValueError("请先识别设备位置、确认类型并命名，再启用曲线控制")
            if id_ in self.enabled:
                return
            if id_ in self.leases:
                raise ValueError("上一次控制状态尚未恢复，等待恢复完成后再接管")
            if self.identification:
                self.stop_identification()
            self.tick()
            channel = next((c for c in self.snapshot["channels"] if c["id"] == f["channel_id"]), None)
            if channel is None:
                raise ValueError("请先绑定一个在线控制通道")
            if not channel["writable"]:
                raise ValueError(channel["reason"])
            # A zero curve should retain native automatic control without a
            # needless full-speed pulse when enabling or restarting control.
            initial = 0 if self.snapshot["outputs"][id_]["automatic"] else 100
            lease = self.hardware.acquire(f["channel_id"], initial)
            self.leases[id_] = lease
            self.enabled.add(id_)
            self.memory[id_]["active_since"] = time.monotonic()
            self.memory[id_]["duty"] = initial
            self.event(f"已接管 {f['name']}")
            self.tick()

    def release(self, id_, cancel_pending=True):
        with self.lock:
            if cancel_pending:
                self.pending_autostart.pop(id_, None)
            if id_ not in self.leases:
                self.enabled.discard(id_)
                return
            lease = self.leases[id_]
            # Keep the lease on failure so shutdown can retry restoration.
            try:
                self.hardware.restore(lease)
            except OSError as exc:
                self.enabled.discard(id_)
                self.error = f"恢复控制状态失败：{exc}"
                self.event(self.error)
                raise
            del self.leases[id_]
            self.enabled.discard(id_)
            if not self.leases and self.identification is None:
                self.hardware.disarm([lease])
            self.event(f"已释放 {id_}，恢复接管前状态")

    def release_all(self):
        self.pending_autostart.clear()
        errors = []
        try:
            self.stop_identification(resume=False)
        except OSError as exc:
            errors.append(str(exc))
        for id_ in list(self.leases):
            try:
                self.release(id_)
            except OSError as exc:
                errors.append(str(exc))
        if errors:
            raise OSError("；".join(errors))

    def close(self):
        self.stop_event.set()
        if self.thread:
            self.thread.join(timeout=6)
        with self.lock:
            try:
                self.release_all()
            finally:
                close = getattr(self.hardware, "close", None)
                if close:
                    close()
