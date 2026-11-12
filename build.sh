#!/usr/bin/env bash
# 构建并打包出可直接分发的单个 exe。
#
# 用法: ./build.sh
# 产物: release/SOCD.exe
#
# 只写 release/，不动源码目录；exe 先写临时名再改名，程序正在运行时也能
# 重新打包。
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="$HERE/release"

# 工具链位置。可以用环境变量覆盖，默认取同级 temp/ 下的自备工具链。
TEMP_ROOT="${SOCD_TOOLCHAIN_ROOT:-$HERE/../temp}"
export RUSTUP_HOME="${RUSTUP_HOME:-$TEMP_ROOT/rust}"
export CARGO_HOME="${CARGO_HOME:-$TEMP_ROOT/cargo}"
if [ -d "$TEMP_ROOT/mingw/extracted/mingw64/bin" ]; then
    export PATH="$TEMP_ROOT/mingw/extracted/mingw64/bin:$PATH"
fi

CARGO="${CARGO:-$CARGO_HOME/bin/cargo.exe}"
TOOLCHAIN="${SOCD_TOOLCHAIN:-+stable-x86_64-pc-windows-gnu}"

if [ ! -x "$CARGO" ]; then
    CARGO="$(command -v cargo || true)"
fi
if [ -z "$CARGO" ]; then
    echo "找不到 cargo。请设置 CARGO 或把 Rust 加进 PATH。" >&2
    exit 1
fi

echo "== 构建 =="
"$CARGO" "$TOOLCHAIN" build --release --manifest-path "$HERE/Cargo.toml"

echo "== 生成图标 =="
PYTHONIOENCODING=utf-8 python "$HERE/make_icon.py"

echo "== 嵌入图标 =="
mkdir -p "$OUT"
cp "$HERE/target/release/socd.exe" "$OUT/SOCD.exe.new"
# GNU 工具链下不保证有 windres，直接按 PE 结构写入 .rsrc
PYTHONIOENCODING=utf-8 python "$HERE/embed_icon.py" "$OUT/SOCD.exe.new" "$HERE/assets/icon.ico"
mv -f "$OUT/SOCD.exe.new" "$OUT/SOCD.exe"

echo
echo "产物:"
ls -lh "$OUT/SOCD.exe"
echo
echo "分发时只需要这一个文件。配置在首次运行时生成于 exe 旁的 Data/settings.socd。"
