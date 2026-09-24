#!/bin/bash
# 统一 bump 版本：usage: sh scripts/bump-version.sh 0.1.5
#
# 真正的逻辑在 scripts/version.mjs（跨平台、会刷新 Cargo.lock、改完自校验）。
# 这里只保留旧入口，方便沿用以前敲的命令。
set -e
V=$1
[ -z "$V" ] && echo "用法: sh scripts/bump-version.sh <新版本>" && exit 1
cd "$(dirname "$0")/.."
exec node scripts/version.mjs set "$V"
