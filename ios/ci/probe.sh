#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# P0 探针：逐个 crate 对 aarch64-apple-ios 做 cargo check，输出通过/失败清单。
# 这个脚本**永远返回 0**，因为它的目的不是"通过"，而是"收集错误"。
#
# 用法：仓库根目录下 `bash ios/ci/probe.sh [输出目录]`

set -u
OUT_DIR="${1:-probe-logs}"
mkdir -p "$OUT_DIR"
TARGET=aarch64-apple-ios

# 顺序 = 依赖从底到顶，前面的先能过关，后面的错误才有意义
CRATES="common rdynarmic shader_recompiler hid_core network web_service frontend_common core video_core input_common audio_core"

: > "$OUT_DIR/summary.txt"
pass=0
fail=0

for crate in $CRATES; do
  echo "=================== $crate ==================="
  cargo check --target "$TARGET" -p "$crate" > "$OUT_DIR/$crate.log" 2>&1
  rc=$?
  if [ "$rc" -eq 0 ]; then
    echo "PASS  $crate" | tee -a "$OUT_DIR/summary.txt"
    pass=$((pass + 1))
  else
    echo "FAIL($rc)  $crate" | tee -a "$OUT_DIR/summary.txt"
    fail=$((fail + 1))
    # CI 日志里只留最有用的几行
    grep -nE '^error(\[|:)|error: could not|failed to run custom build|Could not find directory|not found in this scope|unresolved import|no method named' \
      "$OUT_DIR/$crate.log" | head -12
  fi
done

echo
echo "=== 探针结果：$pass 通过 / $fail 失败 ==="
cat "$OUT_DIR/summary.txt"
exit 0
