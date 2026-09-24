#!/bin/bash
# 生成平台包 package.json（esbuild 模式：每个平台一个 npm 包，postinstall 从它复制二进制）
#
# 用法：sh scripts/gen-platform-packages.sh <版本号，如 0.6.0>
#
# ⚠️ 包名前缀从 npm/package.json 的 name 推导，不写死 ——
#    叫 workbuddy-switch 的前缀已被上游作者占用（npm publish 会 403），
#    历史上这里写死过旧前缀，改名时漏了就会把旧名字重新建出来。
#
# ⚠️ 平台清单**必须与 .github/workflows/build.yml 的 matrix 一致**：
#    CI 里没有的平台包永远不会被发布，列出来只会让用户走到"平台包未安装"的报错分支。
set -e
V=$1
[ -z "$V" ] && echo "用法: sh scripts/gen-platform-packages.sh <版本号>" && exit 1

cd "$(dirname "$0")/.." || exit 1
PREFIX=$(node -p "require('./npm/package.json').name")
cd npm/platform || exit 1

gen() {
  local tag="$1" os="$2" cpu="$3"
  local dir="$PREFIX-$tag"
  mkdir -p "$dir/bin"
  cat > "$dir/package.json" << JSON
{
  "name": "$PREFIX-$tag",
  "version": "$V",
  "description": "$PREFIX platform binary ($tag)",
  "os": ["$os"],
  "cpu": ["$cpu"],
  "files": ["bin"],
  "license": "MIT"
}
JSON
  echo "生成 $dir"
}

gen darwin-arm64 darwin arm64
gen darwin-x64   darwin x64
gen win32-x64    win32  x64
gen linux-x64    linux  x64

echo ""
echo "平台包生成完成（$PREFIX，版本 $V）。"
echo "把对应二进制复制到各包 bin/ 后可 npm publish；CI 会自动做这一步"
echo "（见 .github/workflows/build.yml 的 Publish platform package）。"
