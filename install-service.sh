#!/usr/bin/env bash
set -euo pipefail
cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
if [[ ! -x ./target/release/fancontrollx ]]; then
  echo "请先运行 ./build.sh 构建 Rust 后台" >&2
  exit 1
fi
exec ./target/release/fancontrollx --install-service --project-dir "$PWD" "$@"
