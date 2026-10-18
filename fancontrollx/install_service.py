"""Install the system service and the already-signed, current-kernel driver.

Run --dry-run without root to review the exact unit and selected fan policies.
This installer does not generate keys, reboot, unload drivers, or install DKMS.
"""

import argparse
import datetime
import fcntl
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

from .config import save_config, validate_config

ROOT = Path(__file__).resolve().parent.parent
UNIT = Path("/etc/systemd/system/fancontrollx.service")


def quote_unit_path(path, command=False):
    value = str(path)
    if any(c in value for c in "\n\r\0"):
        raise ValueError("安装路径不能包含换行符")
    value = value.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%")
    if command:
        value = value.replace("$", "$$")
    return '"' + value + '"'


def render_unit(project, data_dir):
    template = (ROOT / "systemd/fancontrollx.service.in").read_text()
    working_dir = str(project)
    if any(c in working_dir for c in "\n\r\0"):
        raise ValueError("安装路径不能包含换行符")
    return template.replace("@PROJECT_DIR@", quote_unit_path(project)).replace(
        "@WORKING_DIR@", working_dir.replace("%", "%%")).replace(
        "@DATA_DIR@", quote_unit_path(data_dir, command=True))


def prepare_config(raw, enable_confirmed=False):
    config = validate_config(raw)
    if enable_confirmed:
        for f in config["fans"]:
            f["autostart"] = bool(f["identified"] and f["channel_id"] and f["device_kind"] in ("fan", "pump"))
    return config


def command(args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def write_unit(path, content):
    fd, temporary = tempfile.mkstemp(prefix=".fancontrollx-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as file:
            file.write(content)
            file.flush()
            os.fsync(file.fileno())
            os.fchmod(file.fileno(), 0o644)
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def main():
    parser = argparse.ArgumentParser(description="安装 FanControlLX systemd 后台服务")
    parser.add_argument("--enable-confirmed", action="store_true", help="为当前已确认并绑定的设备启用后台持续控制")
    parser.add_argument("--dry-run", action="store_true", help="仅预览配置和服务，不修改系统、不接管硬件")
    args = parser.parse_args()
    config_path = ROOT / "data/config.json"
    module = ROOT / ".runtime/nct6687d/nct6687.signed.ko"
    try:
        if not config_path.is_file():
            raise ValueError("请先在网页中识别设备并保存配置")
        raw = json.loads(config_path.read_text())
        config = prepare_config(raw, args.enable_confirmed)
        if not module.is_file():
            raise ValueError(f"未找到已签名驱动：{module}")
        kernel = os.uname().release
        vermagic = command(["/usr/sbin/modinfo", "-F", "vermagic", str(module)], capture_output=True, text=True).stdout.strip()
        if not vermagic or vermagic.split()[0] != kernel:
            raise ValueError("驱动与当前内核不匹配，请为当前内核重新编译并签名驱动")
        signer = command(["/usr/sbin/modinfo", "-F", "signer", str(module)], capture_output=True, text=True).stdout.strip()
        if not signer:
            raise ValueError("模块没有签名；请先完成签名和证书登记")
        target = Path("/lib/modules") / kernel / "updates/fancontrollx/nct6687.ko"
        unit = render_unit(ROOT, config_path.parent)
        names = [f["name"] for f in config["fans"] if f["autostart"] and f["identified"] and f["channel_id"]]
        print(f"后台服务：{UNIT}\n驱动安装：{target}\n内核：{kernel}\n签名者：{signer}")
        print("后台自动控制：" + ("、".join(names) if names else "无（可在网页逐路启用）"))
        if args.dry_run:
            print("\n服务配置预览：\n" + unit)
            print("仅预览，未修改配置、系统服务或风扇。")
            return 0
        if os.geteuid() != 0:
            raise ValueError("请使用 sudo ./install-service.sh --enable-confirmed 执行安装")
        if not Path("/run/systemd/system").is_dir():
            raise ValueError("当前环境未运行 systemd，无法安装后台服务")
        active = subprocess.run(["/usr/bin/systemctl", "is-active", "--quiet", "fancontrollx.service"]).returncode == 0
        if active:
            command(["/usr/bin/systemctl", "stop", "fancontrollx.service"])
        # Do not kill an unrelated or foreground controller to take its lock.
        lock_path = config_path.parent / "server.lock"
        lock_fd = os.open(lock_path, os.O_RDWR | os.O_NOFOLLOW | os.O_CREAT, 0o600)
        with os.fdopen(lock_fd, "w") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise ValueError("旧的前台服务仍在运行。请先在原终端按 Ctrl+C，再执行安装") from None
            # Read again after the previous process has stopped, preserving its final save.
            raw = json.loads(config_path.read_text())
            config = prepare_config(raw, args.enable_confirmed)
            stamp = datetime.datetime.now().strftime("%Y%m%d-%H%M%S-%f")
            backup = config_path.with_name(f"config.before-service-{stamp}.json")
            save_config(backup, raw)
            if UNIT.exists():
                shutil.copy2(UNIT, UNIT.with_name(f"fancontrollx.service.backup-{stamp}"))
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(module, target)
            target.chmod(0o644)
            command(["/usr/sbin/depmod", "-a", kernel])
            save_config(config_path, config)
            write_unit(UNIT, unit)
            print(f"已备份原配置：{backup}")
        command(["/usr/bin/systemctl", "daemon-reload"])
        command(["/usr/bin/systemctl", "enable", "--now", "fancontrollx.service"])
        # Type=exec confirms exec, so also wait until our actual local API is ready.
        for _ in range(30):
            try:
                opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                with opener.open("http://127.0.0.1:8090/api/state", timeout=2) as response:
                    state = json.load(response)
                if state.get("background_service"):
                    count = sum(o["active"] for o in state["outputs"].values())
                    print(f"\n后台服务已启动并设为开机自启，当前已接管 {count} 路。")
                    if state.get("pending_autostart"):
                        print("待接管通道：" + json.dumps(state["pending_autostart"], ensure_ascii=False))
                    print("WebUI：http://127.0.0.1:8090\n关闭终端和网页不会停止后台服务。")
                    return 0
            except (OSError, ValueError, urllib.error.URLError):
                pass
            time.sleep(.5)
        raise RuntimeError("服务接口尚未就绪。请执行 sudo journalctl -u fancontrollx -n 50 --no-pager 查看原因")
    except (OSError, ValueError, RuntimeError, subprocess.CalledProcessError) as exc:
        print(f"安装未完成：{exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
