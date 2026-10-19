"""Dependency-free loopback HTTP service."""

import argparse
import fcntl
import ipaddress
import json
import logging
import os
import secrets
import signal
import socket
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

from . import __version__
from .engine import Engine
from .hardware import DemoHardware, Hardware, read_text

ROOT = Path(__file__).resolve().parent.parent
STATIC = ROOT / "static"
MAX_BODY = 256 * 1024


class Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, address, engine):
        if ":" in address[0]:
            self.address_family = socket.AF_INET6
        super().__init__(address, Handler)
        self.engine = engine
        self.csrf = secrets.token_urlsafe(32)
        self.timeout = .5


class Handler(BaseHTTPRequestHandler):
    server_version = "FanControlLX/" + __version__

    def log_message(self, format_, *args):
        logging.getLogger("http").debug(format_, *args)

    def reply(self, value, status=200, content_type="application/json; charset=utf-8"):
        data = json.dumps(value, ensure_ascii=False, allow_nan=False).encode() if not isinstance(value, bytes) else value
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("X-Frame-Options", "DENY")
        self.send_header("Content-Security-Policy", "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'")
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def valid_host(self):
        # No wildcard Host names: prevents DNS rebinding attacks against loopback.
        try:
            host = urlsplit("http://" + self.headers.get("Host", "")).hostname
            return host == "localhost" or ipaddress.ip_address(host).is_loopback
        except (ValueError, TypeError):
            return False

    def do_GET(self):
        if not self.valid_host():
            return self.reply({"error": "仅允许通过 localhost 或回环地址访问，请使用 SSH 端口转发"}, 403)
        path = urlsplit(self.path).path
        if path == "/api/state":
            return self.reply({**self.server.engine.state(), "csrf_token": self.server.csrf,
                               "version": __version__, "hostname": socket.gethostname()})
        if path == "/api/health":
            return self.reply({"ok": not self.server.engine.error, "version": __version__,
                               "background_service": self.server.engine.managed})
        files = {"/": ("index.html", "text/html; charset=utf-8"),
                 "/app.js": ("app.js", "text/javascript; charset=utf-8"),
                 "/style.css": ("style.css", "text/css; charset=utf-8"),
                 "/favicon.svg": ("favicon.svg", "image/svg+xml")}
        if path in files:
            name, content_type = files[path]
            return self.reply((STATIC / name).read_bytes(), content_type=content_type)
        return self.reply({"error": "未找到资源"}, 404)

    def do_POST(self):
        if not self.valid_host():
            return self.reply({"error": "Host 不允许"}, 403)
        origin = self.headers.get("Origin")
        if origin and origin != "http://" + self.headers.get("Host", ""):
            return self.reply({"error": "不允许跨站请求"}, 403)
        if not secrets.compare_digest(self.headers.get("X-FanControl-Token", ""), self.server.csrf):
            return self.reply({"error": "页面会话已过期，请刷新后重试"}, 403)
        if self.headers.get("Transfer-Encoding") or self.headers.get_content_type() != "application/json":
            return self.reply({"error": "需要 application/json"}, 415)
        try:
            size = int(self.headers.get("Content-Length", "0"))
            if not 0 < size <= MAX_BODY:
                return self.reply({"error": "请求体大小无效"}, 413)
            self.connection.settimeout(5)
            value = json.loads(self.rfile.read(size))
            if not isinstance(value, dict):
                raise ValueError("请求必须为 JSON 对象")
            engine = self.server.engine
            path = urlsplit(self.path).path
            result = {}
            if path == "/api/config":
                engine.update_config(value, value.get("expected_revision"))
            elif path == "/api/fan":
                engine.patch_fan(value.get("id"), value.get("changes"), value.get("expected"))
            elif path == "/api/fan-order":
                engine.reorder_fans(value.get("ids"), value.get("expected"))
            elif path == "/api/curve":
                result["id"] = engine.save_curve(value.get("curve"), value.get("expected"),
                                                  value.get("assign_to"), value.get("expected_fan"))
            elif path == "/api/curve/delete":
                engine.delete_curve(value.get("id"), value.get("expected"))
            elif path == "/api/scan":
                result["added"] = engine.scan_connected()
            elif path == "/api/identify":
                engine.identify(value.get("channel_id"), value.get("duty", 100), value.get("device_kind", "unknown"))
            elif path == "/api/identify-stop":
                engine.stop_identification(value.get("channel_id"))
            elif path == "/api/identity":
                result["id"] = engine.confirm_identity(value.get("channel_id"), value.get("name"), value.get("device_kind"))
            elif path == "/api/activate":
                engine.request_control(value.get("id"), True)
            elif path == "/api/release":
                engine.request_control(value.get("id"), False)
                engine.tick()
            elif path == "/api/release-all":
                with engine.lock:
                    engine.request_release_all()
                    engine.tick()
            else:
                return self.reply({"error": "未找到接口"}, 404)
            return self.reply({"ok": True, **result})
        except (ValueError, TypeError, OverflowError) as exc:
            return self.reply({"error": str(exc)}, 400)
        except OSError as exc:
            return self.reply({"error": str(exc)}, 409)
        except Exception:
            logging.exception("Request failed")
            return self.reply({"error": "服务器处理失败，请查看终端日志"}, 500)


def diagnose():
    h = Hardware()
    sensors, fans, channels, chips = h.discover()
    print(json.dumps({"board": read_text("/sys/class/dmi/id/board_name"),
                      "kernel": os.uname().release, "uid": os.geteuid(), "chips": chips,
                      "sensors": sensors, "fans": fans, "channels": channels,
                      "gpu_error": h.gpu_error}, ensure_ascii=False, indent=2))


def take_lock(path):
    flags = os.O_RDWR | os.O_NOFOLLOW
    try:
        fd = os.open(path, flags | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        fd = os.open(path, flags)
    file = os.fdopen(fd, "w")
    try:
        fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        file.close()
        raise RuntimeError(f"另一个 FanControlLX 实例正在使用 {path}")
    return file


def main(service=False):
    parser = argparse.ArgumentParser(description="FanControlLX 本地风扇曲线 Web 服务")
    parser.add_argument("--host", default="127.0.0.1", help="仅支持回环 IP；远程访问请使用 SSH 转发")
    parser.add_argument("--port", type=int, default=8090)
    parser.add_argument("--control", action="store_true", default=service, help="允许接管硬件；仍需在页面逐个启用")
    parser.add_argument("--demo", action="store_true", help="独立演示设备，绝不写入真实硬件")
    parser.add_argument("--diagnose", action="store_true", help="打印硬件诊断并退出")
    parser.add_argument("--data-dir", type=Path, default=ROOT / "data")
    args = parser.parse_args()
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    if args.diagnose:
        diagnose()
        return
    try:
        if not ipaddress.ip_address(args.host).is_loopback:
            parser.error("此版本仅监听回环地址；使用 ssh -L 转发访问")
    except ValueError:
        parser.error("--host 必须为回环 IP，例如 127.0.0.1")
    if not 1 <= args.port <= 65535:
        parser.error("端口必须在 1–65535 之间")
    args.data_dir.mkdir(parents=True, exist_ok=True)
    locks = []
    engine = None
    server = None
    stopped = threading.Event()
    try:
        locks.append(take_lock(args.data_dir / ("demo.lock" if args.demo else "server.lock")))
        if args.control and not args.demo:
            locks.append(take_lock(Path("/tmp/fancontrollx-hardware.lock")))
            # Do not compete with another well-known fan controller.
            for path in Path("/proc").glob("[0-9]*/comm"):
                if read_text(path) in ("coolercontrold", "fancontrol", "fan2go", "thinkfan"):
                    raise RuntimeError("检测到其他风扇控制进程，请先停止它再启用 --control")
        engine = Engine(DemoHardware() if args.demo else Hardware(data_dir=args.data_dir if args.control else None),
                        args.data_dir / ("config.demo.json" if args.demo else "config.json"),
                        args.control, args.demo, managed=service)
        server = Server((args.host, args.port), engine)
        for sig in (signal.SIGTERM, signal.SIGINT):
            signal.signal(sig, lambda *_: stopped.set())
        engine.start()
        host = f"[{args.host}]" if ":" in args.host else args.host
        print(f"\n  FanControlLX {__version__} · {'后台持续控制' if service else '演示模式' if args.demo else '允许控制（尚未接管）' if args.control else '监控与曲线编辑'}")
        stop_help = "停止后台：sudo systemctl stop fancontrollx" if service else "Ctrl+C 停止服务并恢复已接管风扇。"
        print(f"  http://{host}:{args.port}\n  配置目录：{args.data_dir.resolve()}\n  {stop_help}\n", flush=True)
        while not stopped.is_set():
            server.handle_request()
    except (OSError, ValueError, RuntimeError) as exc:
        print(f"启动失败：{exc}", file=sys.stderr)
        return 1
    finally:
        if server:
            server.server_close()
        if engine:
            engine.close()
        for lock in locks:
            lock.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
