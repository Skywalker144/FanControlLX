#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
# The browser is a configuration client. When installed, the system service
# owns the controller and HTTP endpoint; this launcher never starts a rival.
if [[ $# -eq 0 || ( $# -eq 1 && "$1" == "--control" ) ]]; then
  if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet fancontrollx.service 2>/dev/null; then
    echo "FanControlLX 后台服务正在运行。"
    echo "WebUI：http://127.0.0.1:8090"
    echo "关闭此终端或网页不会停止调速。"
    exit 0
  elif [[ -f /etc/systemd/system/fancontrollx.service ]]; then
    echo "后台服务尚未运行，请执行：sudo systemctl start fancontrollx" >&2
    echo "查看原因：sudo journalctl -u fancontrollx -n 50 --no-pager" >&2
    exit 1
  fi
fi
if [[ "${1:-}" == "--foreground" ]]; then
  shift
fi
if [[ ! -x target/release/fancontrollx ]]; then
  echo "请先执行 ./build.sh 构建 Rust 后台。" >&2
  exit 1
fi
exec ./target/release/fancontrollx "$@"
