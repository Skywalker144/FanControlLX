"""Read hwmon and NVIDIA telemetry; restrict writes to documented drivers."""

import csv
import hashlib
import math
import os
import re
import shutil
import subprocess
import time
from pathlib import Path

# These use 1=manual and 2=firmware automatic. Unknown drivers remain read-only.
SUPPORTED = {"nct6687", "nct6775", "nct6776", "nct6779", "nct6791", "nct6792",
             "nct6793", "nct6795", "nct6796", "nct6797", "nct6798", "nct6799"}


def read_text(path, default=""):
    try:
        return Path(path).read_text().strip()
    except (OSError, UnicodeError):
        return default


def read_number(path, scale=1):
    try:
        value = float(Path(path).read_text().strip()) / scale
        return value if math.isfinite(value) else None
    except (OSError, ValueError):
        return None


class Hardware:
    def __init__(self, root="/sys/class/hwmon"):
        self.root = Path(root)
        self.paths = {}
        self.gpu_cache = []
        self.gpu_at = -100
        self.gpu_error = None

    def discover(self):
        sensors, fans, channels, chips = [], [], [], []
        paths = {}
        for hw in sorted(self.root.glob("hwmon*")):
            name = read_text(hw / "name", "unknown")
            # hwmon indices change across boots. The device location and chip do not.
            physical = re.sub(r"/hwmon/hwmon\d+$", "", str(hw.resolve()))
            identity = hashlib.sha256((name + ":" + physical).encode()).hexdigest()[:16]
            chips.append({"name": name, "path": str(hw), "id": identity})
            for p in sorted(hw.glob("temp*_input")):
                key = p.name.removesuffix("_input")
                value = read_number(p, 1000)
                if value is not None and not -40 <= value <= 150:
                    value = None
                sensors.append({"id": f"{identity}/{key}", "name": read_text(hw / (key + "_label"), key),
                                "chip": name, "value": value, "kind": "cpu" if name in ("coretemp", "k10temp", "zenpower") else "other"})
            for p in sorted(hw.glob("fan*_input")):
                key = p.name.removesuffix("_input")
                fans.append({"id": f"{identity}/{key}", "name": read_text(hw / (key + "_label"), key),
                             "chip": name, "rpm": read_number(p)})
            for p in sorted(hw.glob("pwm*")):
                if not re.fullmatch(r"pwm\d+", p.name):
                    continue
                enable = hw / (p.name + "_enable")
                reason = None
                if name not in SUPPORTED:
                    reason = f"{name} 的控制模式尚未验证，仅监控"
                elif not enable.exists():
                    reason = "驱动未提供自动模式恢复接口"
                elif not os.access(p, os.W_OK) or not os.access(enable, os.W_OK):
                    reason = "需要写入权限：使用 sudo ./webui.sh --control 启动"
                key = f"{identity}/{p.name}"
                duty = read_number(p)
                # This driver explicitly documents fanN and pwmN as one header.
                # Other drivers require manual pairing; matching indices is not proof.
                tach = p.name.replace("pwm", "fan")
                rpm_id = f"{identity}/{tach}" if name == "nct6687" and (hw / (tach + "_input")).exists() else None
                channels.append({"id": key, "name": read_text(hw / (p.name.replace("pwm", "fan") + "_label"), p.name),
                                 "chip": name, "duty": round(duty / 255 * 100, 1) if duty is not None else None,
                                 "mode": read_number(enable), "writable": reason is None, "reason": reason,
                                 "path": str(p), "watchdog": (hw / "fan_control_watchdog").exists(),
                                 "rpm_sensor_id": rpm_id})
                # Retain device paths, not a hwmonX symlink that can be recycled.
                paths[key] = (p.resolve(), enable.resolve(), (hw / "fan_control_watchdog").resolve())
        self.paths = paths
        return sensors + self.gpu(), fans, channels, chips

    def gpu(self):
        if time.monotonic() - self.gpu_at < 3:
            return self.gpu_cache
        self.gpu_at = time.monotonic()
        self.gpu_cache = []
        if not shutil.which("nvidia-smi"):
            self.gpu_error = "未找到 nvidia-smi"
            return []
        try:
            result = subprocess.run(["nvidia-smi", "--query-gpu=uuid,name,temperature.gpu",
                                     "--format=csv,noheader,nounits"], capture_output=True, text=True, timeout=2)
            if result.returncode:
                raise OSError((result.stderr or result.stdout).strip()[:240])
            for uuid, name, temperature in csv.reader(result.stdout.splitlines(), skipinitialspace=True):
                value = float(temperature)
                if math.isfinite(value) and -40 <= value <= 150:
                    self.gpu_cache.append({"id": "nvidia/" + uuid, "name": name,
                                           "chip": "nvidia", "kind": "gpu", "value": value})
            self.gpu_error = None if self.gpu_cache else "没有 GPU 温度读数"
        except (OSError, ValueError, subprocess.TimeoutExpired) as error:
            self.gpu_error = str(error)[:240]
        return self.gpu_cache

    def acquire(self, channel_id, duty):
        pwm, enable, watchdog = self.paths[channel_id]
        original = (read_number(pwm), read_number(enable))
        if original[0] is None or original[1] not in (0, 1, 2, 3, 4, 5):
            raise OSError("无法读取原始风扇控制状态")
        # Arm a hardware lease before changing control, when the driver provides one.
        if watchdog.exists():
            watchdog.write_text("15\n")
        try:
            # First take over at full speed; never switch to an unknown low stored PWM.
            pwm.write_text("255\n")
            enable.write_text("1\n")
            self.write(channel_id, duty)
        except OSError:
            self.restore_paths((pwm, enable, watchdog), original)
            raise
        return {"paths": (pwm, enable, watchdog), "original": original, "channel_id": channel_id}

    def write(self, channel_id, duty):
        pwm, _, watchdog = self.paths[channel_id]
        pwm.write_text(f"{round(max(0, min(100, duty)) / 100 * 255)}\n")
        if watchdog.exists():
            watchdog.write_text("15\n")

    @staticmethod
    def restore_paths(paths, original):
        pwm, enable, _ = paths
        value, mode = original
        if mode == 1:
            pwm.write_text(f"{round(value)}\n")
            enable.write_text("1\n")
        else:
            # Some drivers switch to manual on a PWM write, so restore enable last.
            pwm.write_text(f"{round(value)}\n")
            enable.write_text(f"{round(mode)}\n")

    def restore(self, lease):
        paths = self.paths.get(lease["channel_id"], lease["paths"])
        self.restore_paths(paths, lease["original"])

    @staticmethod
    def disarm(leases):
        for path in {lease["paths"][2] for lease in leases}:
            if path.exists():
                path.write_text("0\n")


class DemoHardware:
    """An explicit, fully isolated simulation. It never opens hardware write paths."""
    gpu_error = None

    def __init__(self):
        self.start = time.monotonic()
        self.duties = {"demo/pwm1": 38, "demo/pwm2": 42, "demo/pwm3": 42}

    def discover(self):
        t = time.monotonic() - self.start
        sensors = [{"id": "demo/cpu", "name": "CPU Package", "chip": "demo", "kind": "cpu", "value": round(48 + 8 * math.sin(t / 20), 1)},
                   {"id": "demo/gpu", "name": "RTX 5080 · 演示", "chip": "demo", "kind": "gpu", "value": round(44 + 9 * math.sin(t / 30), 1)},
                   {"id": "demo/board", "name": "主板", "chip": "demo", "kind": "other", "value": 35}]
        channels = [{"id": key, "name": ["CPU_FAN", "SYS_FAN1", "SYS_FAN2"][i], "chip": "demo", "duty": duty,
                     "mode": 1, "writable": True, "reason": None, "path": "演示设备", "watchdog": False}
                    for i, (key, duty) in enumerate(self.duties.items())]
        for channel in channels:
            channel["rpm_sensor_id"] = channel["id"].replace("pwm", "fan")
        fans = [{"id": key.replace("pwm", "fan"), "name": c["name"], "chip": "demo", "rpm": round(self.duties[key] * 21)}
                for key, c in zip(self.duties, channels)]
        return sensors, fans, channels, [{"name": "demo", "path": "演示设备", "id": "demo"}]

    def acquire(self, key, duty):
        lease = {"key": key, "duty": self.duties[key]}
        self.write(key, duty)
        return lease

    def write(self, key, duty):
        self.duties[key] = duty

    def restore(self, lease):
        self.duties[lease["key"]] = lease["duty"]

    def disarm(self, leases):
        pass
