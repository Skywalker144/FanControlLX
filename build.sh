#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
if ! command -v cargo >/dev/null 2>&1; then
  echo "构建需要 Rust 1.82+ 和 C 链接器；安装 Rust 后执行 cargo build --release --locked。" >&2
  exit 1
fi
exec cargo build --release --locked "$@"
