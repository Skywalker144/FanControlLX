"""Small, optional ctypes binding to the NVML library shipped by NVIDIA.

API: https://docs.nvidia.com/deploy/nvml-api/latest/api/index.html
No CUDA toolkit, Python package, X server or Coolbits setting is required.
"""

import ctypes as C
import math
import re

U, P = C.c_uint, C.c_void_p
UP = C.POINTER(U)
SIGNATURES = {
    "nvmlInit_v2": [], "nvmlShutdown": [],
    "nvmlDeviceGetCount_v2": [UP],
    "nvmlDeviceGetHandleByIndex_v2": [U, C.POINTER(P)],
    "nvmlDeviceGetHandleByUUID": [C.c_char_p, C.POINTER(P)],
    "nvmlDeviceGetUUID": [P, C.c_char_p, U],
    "nvmlDeviceGetName": [P, C.c_char_p, U],
    "nvmlDeviceGetTemperature": [P, U, UP],
    "nvmlDeviceGetNumFans": [P, UP],
    "nvmlDeviceGetMinMaxFanSpeed": [P, UP, UP],
    "nvmlDeviceGetFanControlPolicy_v2": [P, U, UP],
    "nvmlDeviceGetFanSpeed_v2": [P, U, UP],
    "nvmlDeviceGetTargetFanSpeed": [P, U, UP],
    "nvmlDeviceSetFanSpeed_v2": [P, U, U],
    "nvmlDeviceSetDefaultFanSpeed_v2": [P, U],
}


class FanSpeed(C.Structure):
    _fields_ = [("version", U), ("fan", U), ("speed", U)]


def parse_channel(channel_id):
    match = re.fullmatch(r"nvidia/(GPU-[0-9a-fA-F-]+)/fan(\d+)", channel_id or "")
    if match is None:
        raise OSError("无效的 NVIDIA 风扇通道")
    return match[1], int(match[2])


def bounded_duty(duty, minimum, maximum):
    if isinstance(duty, bool) or not isinstance(duty, (float, int)) or not math.isfinite(duty):
        raise OSError("风扇输出必须为有限数值")
    return round(max(minimum, min(maximum, duty)))


class Nvml:
    def __init__(self, library=None):
        self.lib = library if library is not None else C.CDLL("libnvidia-ml.so.1")
        for name, args in {**SIGNATURES, "nvmlDeviceGetFanSpeedRPM": [P, C.POINTER(FanSpeed)]}.items():
            if hasattr(self.lib, name):
                fn = getattr(self.lib, name)
                fn.argtypes, fn.restype = args, C.c_int
        self.lib.nvmlErrorString.argtypes = [C.c_int]
        self.lib.nvmlErrorString.restype = C.c_char_p
        self.call("nvmlInit_v2")

    def call(self, name, *args):
        fn = getattr(self.lib, name, None)
        if fn is None:
            raise OSError(f"驱动未提供 {name}，请更新 NVIDIA 驱动")
        code = fn(*args)
        if code:
            message = self.lib.nvmlErrorString(code).decode(errors="replace")
            raise OSError(f"{name}: {message}")

    def uint(self, name, *args):
        result = U()
        self.call(name, *args, C.byref(result))
        return result.value

    def string(self, name, handle):
        result = C.create_string_buffer(128)
        self.call(name, handle, result, len(result))
        return result.value.decode(errors="replace")

    def devices(self):
        for index in range(self.uint("nvmlDeviceGetCount_v2")):
            handle = P()
            self.call("nvmlDeviceGetHandleByIndex_v2", index, C.byref(handle))
            yield handle

    def handle(self, uuid):
        handle = P()
        self.call("nvmlDeviceGetHandleByUUID", uuid.encode(), C.byref(handle))
        return handle

    def limits(self, handle):
        minimum, maximum = U(), U()
        self.call("nvmlDeviceGetMinMaxFanSpeed", handle, C.byref(minimum), C.byref(maximum))
        if not 0 <= minimum.value <= maximum.value <= 100 or maximum.value < 20:
            raise OSError("驱动报告的 NVIDIA 风扇调速范围无效")
        # Zero RPM in automatic mode does not prove a manual stop is supported.
        return max(20, minimum.value), maximum.value

    def temperature(self, handle):
        value = self.uint("nvmlDeviceGetTemperature", handle, 0)
        if not 0 <= value <= 150:
            raise OSError("GPU 温度无效")
        return value

    def rpm(self, handle, fan):
        value = FanSpeed(C.sizeof(FanSpeed) | (1 << 24), fan, 0)
        self.call("nvmlDeviceGetFanSpeedRPM", handle, C.byref(value))
        # NVML documents this as intended RPM, not proof of physical rotation.
        return value.speed

    def original(self, channel_id):
        uuid, fan = parse_channel(channel_id)
        handle = self.handle(uuid)
        if fan >= self.uint("nvmlDeviceGetNumFans", handle):
            raise OSError("NVIDIA 风扇索引不存在")
        self.limits(handle)
        for name in ("nvmlDeviceSetFanSpeed_v2", "nvmlDeviceSetDefaultFanSpeed_v2"):
            if not hasattr(self.lib, name):
                raise OSError(f"驱动未提供 {name}")
        policy = self.uint("nvmlDeviceGetFanControlPolicy_v2", handle, fan)
        if policy not in (0, 1):
            raise OSError("未知的 NVIDIA 风扇控制策略")
        target = self.uint("nvmlDeviceGetTargetFanSpeed", handle, fan) if policy == 1 else None
        if target is not None and not 0 <= target <= 100:
            raise OSError("无法恢复原来的 NVIDIA 手动输出")
        return {"channel_id": channel_id, "policy": policy, "target": target}

    def write(self, channel_id, duty):
        uuid, fan = parse_channel(channel_id)
        handle = self.handle(uuid)
        minimum, maximum = self.limits(handle)
        automatic = duty == 0
        duty = bounded_duty(duty, minimum, maximum)
        try:
            hot = self.temperature(handle) >= 85
        except OSError:
            hot = True
        # Also enforce the GPU's own guard inside the independent writer.
        if automatic and not hot:
            # Repeated resets can disturb a card's automatic fan policy. Keep
            # the lease alive while only switching policy when needed.
            if self.uint("nvmlDeviceGetFanControlPolicy_v2", handle, fan) != 0:
                self.call("nvmlDeviceSetDefaultFanSpeed_v2", handle, fan)
            return {"automatic": True, "duty": 0, "fault": None}
        self.call("nvmlDeviceSetFanSpeed_v2", handle, fan, maximum if hot else duty)
        return {"automatic": False, "duty": maximum if hot else duty,
                "fault": "显卡自身温度异常，使用最大输出" if hot else None}

    def restore(self, lease):
        uuid, fan = parse_channel(lease["channel_id"])
        handle = self.handle(uuid)
        if lease["policy"] == 0:
            self.call("nvmlDeviceSetDefaultFanSpeed_v2", handle, fan)
        else:
            self.call("nvmlDeviceSetFanSpeed_v2", handle, fan, lease["target"])

    def close(self):
        self.call("nvmlShutdown")
