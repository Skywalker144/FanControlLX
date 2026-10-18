"""Strict configuration validation and atomic persistence."""

import copy
import json
import math
import os
import tempfile
from pathlib import Path


def default_fan(id_="", name="未命名风扇", source="@cpu"):
    return {"id": id_, "name": name, "channel_id": None, "rpm_sensor_id": None,
            "identified": False, "device_kind": "unknown", "autostart": False,
            "sensor_ids": [source], "aggregation": "max", "mode": "curve",
            "points": [[25, 30], [40, 35], [55, 50], [70, 75], [85, 100]],
            "manual_duty": 50, "min_duty": 30, "max_duty": 100,
            "hysteresis": 2, "response_s": 3, "smoothing_s": 5,
            "step_up": 25, "step_down": 5, "critical_temp": 85}


def default_config():
    # Actual devices are discovered by the engine. Never invent physical fans.
    return {"version": 1, "fans": []}


def number(value, low, high, name):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        raise ValueError(f"{name} 必须是有限数值")
    if not low <= value <= high:
        raise ValueError(f"{name} 必须在 {low}–{high} 之间")
    return value


def validate_config(value):
    if not isinstance(value, dict) or value.get("version") != 1:
        raise ValueError("配置版本必须为 1")
    fans = value.get("fans")
    if not isinstance(fans, list) or len(fans) > 64:
        raise ValueError("fans 必须是列表，最多 64 个风扇")
    result, ids, channels = [], set(), set()
    defaults = default_fan()
    for item in fans:
        if not isinstance(item, dict):
            raise ValueError("风扇配置必须是对象")
        f = {key: copy.deepcopy(item.get(key, default)) for key, default in defaults.items()}
        for key in ("id", "name"):
            if not isinstance(f[key], str) or not 1 <= len(f[key].strip()) <= 100:
                raise ValueError(f"{key} 不能为空或超过 100 个字符")
        if f["id"] in ids:
            raise ValueError("风扇 ID 重复")
        ids.add(f["id"])
        if not isinstance(f["identified"], bool) or f["device_kind"] not in ("unknown", "fan", "pump"):
            raise ValueError("设备确认状态或设备类型无效")
        if not isinstance(f["autostart"], bool):
            raise ValueError("后台持续控制必须为布尔值")
        for key in ("channel_id", "rpm_sensor_id"):
            if f[key] is not None and (not isinstance(f[key], str) or len(f[key]) > 256):
                raise ValueError(f"{key} 无效")
        if f["channel_id"]:
            if f["channel_id"] in channels:
                raise ValueError("一个 PWM 通道只能绑定一个风扇配置")
            channels.add(f["channel_id"])
        if f["mode"] not in ("curve", "manual") or f["aggregation"] not in ("max", "min", "average"):
            raise ValueError("控制模式或传感器混合方式无效")
        sensors = f["sensor_ids"]
        if not isinstance(sensors, list) or not 1 <= len(sensors) <= 64 or any(
                not isinstance(s, str) or not s or len(s) > 256 for s in sensors):
            raise ValueError("请至少选择一个有效温度源")
        if len(set(sensors)) != len(sensors):
            raise ValueError("温度源不能重复")
        for key, bounds in {"manual_duty": (0, 100), "min_duty": (20, 100),
                            "max_duty": (20, 100), "hysteresis": (0, 15),
                            "response_s": (0, 30), "smoothing_s": (0, 60),
                            "step_up": (1, 100), "step_down": (1, 100),
                            "critical_temp": (50, 105)}.items():
            number(f[key], *bounds, key)
        if f["min_duty"] > f["max_duty"]:
            raise ValueError("最低输出不能高于最高输出")
        points = f["points"]
        if not isinstance(points, list) or not 2 <= len(points) <= 32:
            raise ValueError("曲线需要 2–32 个节点")
        previous = -21
        for p in points:
            if not isinstance(p, list) or len(p) != 2:
                raise ValueError("曲线节点必须是 [温度, 百分比]")
            number(p[0], -20, 120, "节点温度")
            number(p[1], 0, 100, "节点输出")
            if p[0] <= previous:
                raise ValueError("曲线温度必须递增，不能重复")
            previous = p[0]
        result.append(f)
    return {"version": 1, "fans": result}


def save_config(path, config):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".config-", dir=path.parent)
    try:
        if os.geteuid() == 0:
            # A sudo control session must not make an existing user-owned data
            # directory's config unreadable to later monitoring sessions.
            owner = path.parent.stat()
            os.fchown(fd, owner.st_uid, owner.st_gid)
        with os.fdopen(fd, "w") as file:
            json.dump(config, file, ensure_ascii=False, indent=2, allow_nan=False)
            file.write("\n")
            file.flush()
            os.fsync(file.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
