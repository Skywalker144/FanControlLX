"""Fan policy and control loop. Configuration alone never acquires hardware."""

import copy
import hashlib
import json
import logging
import math
import threading
import time
from collections import deque
from pathlib import Path

from .config import default_config, default_fan, number, save_config, validate_config

log = logging.getLogger(__name__)


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
                if id_ not in self.enabled:
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
                    duty, fault = self.identification_duty(test["duty"], sensors)
                    self.hardware.write(test["channel_id"], duty)
                    test["effective_duty"], test["fault"] = duty, fault
                except (OSError, KeyError):
                    self.stop_identification()
                    raise
            outputs = {}
            for f in self.config["fans"]:
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
                    duty = f["manual_duty"] if f["mode"] == "manual" else interpolate(f["points"], effective)
                    duty = max(f["min_duty"], min(f["max_duty"], duty))
                    previous = memory["duty"]
                    if previous is not None:
                        if now - memory["changed_at"] < f["response_s"]:
                            duty = previous
                        dt = max(0, min(5, now - memory["last_time"]))
                        duty = max(previous - f["step_down"] * dt, min(previous + f["step_up"] * dt, duty))
                        if abs(duty - previous) >= .1:
                            memory["changed_at"] = now
                tach = rpm.get(f["rpm_sensor_id"])
                active_since = memory.get("active_since", now)
                if f["id"] in self.enabled and now - active_since > 10 and f["rpm_sensor_id"] and (tach is None or tach <= 0):
                    duty, fault = 100, "测速信号丢失或风扇停转，保护输出 100%"
                memory["last_time"], memory["duty"] = now, duty
                channel = lookup.get(f["channel_id"])
                status = "待绑定通道" if not f["channel_id"] else ("通道离线" if channel is None else "未接管")
                if f["id"] in self.enabled:
                    try:
                        if channel is None or not channel["writable"]:
                            raise OSError("PWM 通道离线或不可写")
                        self.hardware.write(f["channel_id"], duty)
                        status = "保护运行" if fault else "正在控制"
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
                                  "discovery": self.discovery(), "identification": identification,
                                  "background_service": self.managed,
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
                           "connection": "rotating" if rpm is not None and rpm > 0 else "unconfirmed",
                           "fan_id": bound["id"] if bound else None,
                           "display_name": bound["name"] if bound else channel["name"],
                           "identified": bound["identified"] if bound else False,
                           "device_kind": bound["device_kind"] if bound else "unknown"})
        return result

    def discovered_fan(self, channel):
        id_ = "fan-" + hashlib.sha256(channel["id"].encode()).hexdigest()[:16]
        source = "@cpu" if "cpu" in channel["name"].lower() or not any(
            s["kind"] == "gpu" for s in self.snapshot["sensors"]) else "@cpu-gpu"
        f = default_fan(id_, channel["name"], source)
        f.update(channel_id=channel["id"], rpm_sensor_id=channel.get("rpm_sensor_id"))
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
                if candidate["connection"] == "rotating" and candidate["id"] not in bound:
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
                self.event(f"扫描完成，新增 {added} 个有测速信号的设备；未启用新的控制")
            return added

    @staticmethod
    def identification_duty(duty, sensors):
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
            for f in self.config["fans"]:
                if f["channel_id"] == channel_id:
                    if self.managed and f["autostart"]:
                        # Identification temporarily overrides a channel; do not
                        # let a restart silently turn its old curve back on.
                        self.request_control(f["id"], False)
                    elif f["id"] in self.enabled:
                        self.release(f["id"])
            effective, fault = self.identification_duty(duty, self.snapshot["sensors"])
            if self.identification is None:
                lease = self.hardware.acquire(channel_id, effective)
                self.identification = {"channel_id": channel_id, "lease": lease,
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

    def stop_identification(self, channel_id=None):
        with self.lock:
            test = self.identification
            if test is None or (channel_id is not None and test["channel_id"] != channel_id):
                return
            test["stopping"] = True
            try:
                self.hardware.restore(test["lease"])
            except OSError as exc:
                self.error = f"识别结束后恢复失败，正在重试：{exc}"
                self.event(self.error)
                raise
            self.identification = None
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
            self.event(f"已确认设备：{f['name']}；未自动启用曲线控制")
            return f["id"]

    def update_config(self, value):
        config = validate_config(value)
        with self.lock:
            if self.managed:
                self.stop_identification()
                wanted = {f["id"]: f for f in config["fans"]
                          if f["autostart"] and f["identified"] and f["channel_id"]}
                previous = {f["id"]: f for f in self.config["fans"]}
                for id_ in list(self.leases):
                    if id_ not in wanted or wanted[id_]["channel_id"] != previous[id_]["channel_id"]:
                        self.release(id_)
                # Keep existing leases for unchanged channels. A curve edit must
                # not hand every other fan back to firmware or run it at 100%.
                retained = {id_: memory for id_, memory in self.memory.items() if id_ in self.enabled}
                for memory in retained.values():
                    memory["samples"].clear()
                    memory["temp"] = None
                self.memory = retained
            else:
                # Foreground mode retains its explicit save-then-enable workflow.
                self.release_all()
                self.memory.clear()
            save_config(self.config_path, config)
            self.config = config
            self.tick()
            self.queue_autostart()
            if self.managed:
                self.resume_controls()
                self.event("配置已保存并应用后台控制策略")
            else:
                self.event("配置已保存；如需调速，请重新启用相应风扇")

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
                raise ValueError("请先绑定一个在线 PWM 通道")
            if not channel["writable"]:
                raise ValueError(channel["reason"])
            lease = self.hardware.acquire(f["channel_id"], 100)
            self.leases[id_] = lease
            self.enabled.add(id_)
            self.memory[id_]["active_since"] = time.monotonic()
            self.memory[id_]["duty"] = 100
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
            self.stop_identification()
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
            self.release_all()
