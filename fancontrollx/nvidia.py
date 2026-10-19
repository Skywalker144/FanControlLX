"""NVIDIA discovery and a leased writer process with crash recovery.

Only the writer changes fan state. It journals original policies before writes,
and restores them on lease expiry, parent EOF, SIGTERM or the next startup.
"""

import argparse
import fcntl
import json
import logging
import os
import signal
import socket
import subprocess
import sys
import time
import uuid
from pathlib import Path

from .config import save_config
from .nvml import Nvml, parse_channel

log = logging.getLogger(__name__)
LEASE_SECONDS = 10


class Writer:
    def __init__(self, nvml, journal, clock=time.monotonic):
        self.nvml, self.journal, self.clock = nvml, Path(journal), clock
        self.leases, self.deadlines = {}, {}
        if self.journal.exists():
            raw = json.loads(self.journal.read_text())
            if not isinstance(raw, dict) or raw.get("version") != 1 or not isinstance(raw.get("leases"), list):
                raise OSError("NVIDIA 恢复记录格式无效")
            for lease in raw["leases"]:
                if not isinstance(lease, dict):
                    raise OSError("NVIDIA 恢复记录无效")
                parse_channel(lease.get("channel_id"))
                if lease.get("policy") not in (0, 1) or not isinstance(lease.get("token"), str):
                    raise OSError("NVIDIA 恢复记录无效")
                if lease["policy"] == 1 and (type(lease.get("target")) is not int or not 0 <= lease["target"] <= 100):
                    raise OSError("NVIDIA 恢复记录的手动输出无效")
                self.leases[lease["channel_id"]] = lease
                self.deadlines[lease["channel_id"]] = 0

    def persist(self):
        save_config(self.journal, {"version": 1, "leases": list(self.leases.values())})

    def restore(self, key):
        lease = self.leases.get(key)
        if lease is None:
            return
        self.nvml.restore(lease)
        del self.leases[key]
        try:
            self.persist()
        except OSError:
            self.leases[key] = lease
            raise
        self.deadlines.pop(key, None)

    def expire(self, all_=False):
        errors = []
        for key in list(self.leases):
            if all_ or self.clock() >= self.deadlines[key]:
                try:
                    self.restore(key)
                except OSError as exc:
                    errors.append(str(exc))
        return errors

    def dispatch(self, request):
        action, key = request["action"], request["channel_id"]
        parse_channel(key)
        if action == "acquire":
            if key in self.leases:
                raise OSError("NVIDIA 通道仍有未恢复的租约")
            lease = {**self.nvml.original(key), "token": str(uuid.uuid4())}
            self.leases[key] = lease
            self.deadlines[key] = self.clock() + LEASE_SECONDS
            try:
                self.persist()  # Durable before the first manual write.
            except OSError:
                del self.leases[key]
                del self.deadlines[key]
                raise
            try:
                if request["duty"] != 0:
                    self.nvml.write(key, 100)
                self.nvml.write(key, request["duty"])
            except OSError:
                self.deadlines[key] = 0
                self.expire()
                raise
            return {"backend": "nvidia", "channel_id": key, "token": lease["token"]}
        lease = self.leases.get(key)
        if action == "restore" and lease is None:
            return None  # Already restored by expiry or startup recovery.
        if lease is None or lease["token"] != request.get("token"):
            raise OSError("NVIDIA 控制租约已失效，请重新启用控制")
        if action == "write" and self.clock() >= self.deadlines[key]:
            raise OSError("NVIDIA 控制租约已超时，等待恢复后重新启用控制")
        if action == "restore":
            self.deadlines[key] = 0
            self.restore(key)
        elif action == "write":
            result = self.nvml.write(key, request["duty"])
            self.deadlines[key] = self.clock() + LEASE_SECONDS
            return result
        else:
            raise OSError("未知的 NVIDIA 控制请求")


class NvidiaBackend:
    def __init__(self, data_dir=None):
        self.data_dir = Path(data_dir).resolve() if data_dir is not None else None
        self.nvml, self.retry_at, self.error = None, 0, None
        self.process, self.socket = None, None

    def discover(self):
        sensors, fans, channels, chips = [], [], [], []
        if self.nvml is None and time.monotonic() < self.retry_at:
            return sensors, fans, channels, chips
        try:
            if self.nvml is None:
                self.nvml = Nvml()
            errors = []
            for handle in self.nvml.devices():
                gpu_uuid = self.nvml.string("nvmlDeviceGetUUID", handle)
                name = self.nvml.string("nvmlDeviceGetName", handle)
                sensor_id = "nvidia/" + gpu_uuid
                try:
                    temperature = self.nvml.temperature(handle)
                except OSError:
                    temperature = None
                sensors.append({"id": sensor_id, "name": name, "chip": "nvidia", "kind": "gpu", "value": temperature})
                chips.append({"id": sensor_id, "name": name, "path": "NVML"})
                try:
                    count = self.nvml.uint("nvmlDeviceGetNumFans", handle)
                except OSError as exc:
                    errors.append(f"{name}: {exc}")
                    continue
                for index in range(count):
                    key = f"{sensor_id}/fan{index}"
                    reason, minimum, maximum, policy, duty = None, 30, 100, None, None
                    try:
                        minimum, maximum = self.nvml.limits(handle)
                        policy = self.nvml.original(key)["policy"]
                        duty = self.nvml.uint("nvmlDeviceGetFanSpeed_v2", handle, index)
                    except OSError as exc:
                        reason = str(exc)
                    if reason is None and (os.geteuid() != 0 or self.data_dir is None):
                        reason = "NVIDIA 调速需要 sudo 控制会话或后台服务"
                    try:
                        rpm = self.nvml.rpm(handle, index)
                    except OSError:
                        rpm = None
                    label = f"{name} · 风扇 {index + 1}"
                    fans.append({"id": key + "/rpm", "name": label, "chip": "nvidia", "rpm": rpm,
                                 "rpm_is_feedback": False})
                    channels.append({"id": key, "name": label, "chip": "nvidia", "backend": "nvidia",
                                     "duty": duty, "mode": policy, "writable": reason is None, "reason": reason,
                                     "path": "NVML", "watchdog": False, "software_watchdog": True,
                                     "min_duty": minimum, "max_duty": maximum, "present": True,
                                     "curve_zero_auto": True,
                                     "temperature_sensor_id": sensor_id, "rpm_sensor_id": key + "/rpm",
                                     "rpm_is_feedback": False})
            self.error = "; ".join(errors) or None
        except OSError as exc:
            self.error, self.retry_at = str(exc), time.monotonic() + 10
            if self.nvml is not None:
                try:
                    self.nvml.close()
                except OSError:
                    pass
                self.nvml = None
        return sensors, fans, channels, chips

    def request(self, action, channel_id, **values):
        if self.data_dir is None or os.geteuid() != 0:
            raise OSError("NVIDIA 调速需要 sudo 控制会话或后台服务")
        if self.socket is None:
            parent, child = socket.socketpair(socket.AF_UNIX, socket.SOCK_SEQPACKET)
            parent.settimeout(5)
            try:
                self.process = subprocess.Popen(
                    [sys.executable, "-m", "fancontrollx.nvidia", "--fd", str(child.fileno()),
                     "--data-dir", str(self.data_dir)], pass_fds=(child.fileno(),),
                    stdin=subprocess.DEVNULL, start_new_session=True,
                    cwd=Path(__file__).resolve().parent.parent)
                self.socket = parent
                ready = json.loads(parent.recv(8192))
                if ready.get("error"):
                    raise OSError(ready["error"])
            except (OSError, ValueError) as exc:
                parent.close()
                self.socket = None
                raise OSError(f"NVIDIA 守护进程启动失败：{exc}") from exc
            finally:
                child.close()
        try:
            self.socket.sendall(json.dumps({"action": action, "channel_id": channel_id, **values}).encode())
            response = json.loads(self.socket.recv(8192))
        except (OSError, ValueError) as exc:
            self.socket.close()  # EOF tells the writer to restore every held lease.
            self.socket = None
            raise OSError(f"NVIDIA 守护进程失联，正在恢复自动控制：{exc}") from exc
        if response.get("error"):
            raise OSError(response["error"])
        return response.get("result")

    def acquire(self, key, duty):
        return self.request("acquire", key, duty=duty)

    def write(self, key, duty, token):
        return self.request("write", key, duty=duty, token=token)

    def restore(self, lease):
        self.request("restore", lease["channel_id"], token=lease["token"])

    def close(self):
        if self.socket is not None:
            self.socket.close()
            self.socket = None
        if self.process is not None:
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                log.error("NVIDIA 恢复仍在后台重试；记录保存在 %s", self.data_dir)
        if self.nvml is not None:
            self.nvml.close()
            self.nvml = None


def serve(sock, writer, stopping):
    sock.settimeout(1)
    sock.sendall(json.dumps({"ready": True}).encode())
    try:
        last_error = None
        while not stopping():
            errors = writer.expire()
            error = "; ".join(errors) or None
            if error and error != last_error:
                log.error("NVIDIA 超时恢复失败：%s", error)
            last_error = error
            try:
                packet = sock.recv(8192)
            except socket.timeout:
                continue
            if not packet:
                break
            try:
                result = writer.dispatch(json.loads(packet))
                response = {"result": result}
            except (OSError, ValueError, KeyError, TypeError) as exc:
                response = {"error": str(exc)}
            sock.sendall(json.dumps(response).encode())
    finally:
        # Keep retrying a temporarily inaccessible GPU until systemd's stop
        # timeout; the durable journal survives even a forced group kill.
        for _ in range(10):
            errors = writer.expire(all_=True)
            if not errors:
                break
            log.error("NVIDIA 恢复失败：%s", "; ".join(errors))
            time.sleep(1)


def main():
    parser = argparse.ArgumentParser(description="FanControlLX NVIDIA 控制守护与恢复")
    parser.add_argument("--data-dir", type=Path, required=True)
    parser.add_argument("--fd", type=int)
    parser.add_argument("--recover", action="store_true")
    args = parser.parse_args()
    journal = args.data_dir / "nvidia-leases.json"
    if args.recover and not journal.exists():
        return 0
    sock = socket.socket(fileno=args.fd) if args.fd is not None else None
    nvml = None
    stopped = False

    def stop(*_):
        nonlocal stopped
        stopped = True

    for sig in (signal.SIGTERM, signal.SIGINT):
        signal.signal(sig, stop)
    try:
        args.data_dir.mkdir(parents=True, exist_ok=True)
        fd = os.open(args.data_dir / "nvidia-writer.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        with os.fdopen(fd, "w") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            # A missing driver must not fail a motherboard-only service stop.
            if args.recover:
                raw = json.loads(journal.read_text())
                if not isinstance(raw, dict) or raw.get("version") != 1 or not isinstance(raw.get("leases"), list):
                    raise OSError("NVIDIA 恢复记录格式无效")
                if not raw["leases"]:
                    return 0
            nvml = Nvml()
            writer = Writer(nvml, journal)
            errors = writer.expire(all_=True)
            if errors:
                raise OSError("NVIDIA 旧租约尚未恢复：" + "; ".join(errors))
            if not args.recover:
                if sock is None:
                    raise OSError("控制守护进程需要父进程连接")
                serve(sock, writer, lambda: stopped)
            return 0
    except (OSError, ValueError) as exc:
        if sock is not None:
            try:
                sock.sendall(json.dumps({"error": str(exc)}).encode())
            except OSError:
                pass
        log.error("NVIDIA 守护进程：%s", exc)
        return 1
    finally:
        if sock is not None:
            sock.close()
        if nvml is not None:
            nvml.close()


if __name__ == "__main__":
    sys.exit(main())
