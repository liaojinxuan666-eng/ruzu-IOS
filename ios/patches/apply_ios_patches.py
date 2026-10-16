#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""
把 ruzu 上游源码「就地」改成能编译到 aarch64-apple-ios 的样子。

设计原则：
  * **幂等**：重复跑不会重复插入，也不会写坏文件；每次打印 SKIP / PATCHED / MISS。
  * **永不失败**：任何一步出问题只打印错误并继续，绝不中断 CI 构建 ——
    这样即使某个替换匹配不上，后面的构建步骤（以及探针报告）仍然能跑。
  * **可审查**：每个替换都是精确字符串，不是正则大扫荡，diff 一眼能看懂。

用法（仓库根目录下）：
    python3 ios/patches/apply_ios_patches.py
"""

import os
import sys

ROOT = os.getcwd()
changed = 0
problems = []


def log(kind, path, note=""):
    print(f"[{kind}] {path}{('  — ' + note) if note else ''}")


def _read(path):
    """按字节读、按 UTF-8 解码 —— 绝不让 Python 动换行符。

    ruzu 仓库里有的文件是 CRLF（例如 video_core/src/lib.rs），用文本模式读写
    会把整个文件的行尾规范化，产生几百行假 diff。所以这里全程字节读写，
    匹配时再按文件自身的行尾风格生成模式。
    """
    with open(path, "rb") as f:
        return f.read().decode("utf-8")


def _write(path, text):
    with open(path, "wb") as f:
        f.write(text.encode("utf-8"))


def edit(relpath, replacements):
    """对 relpath 依次做精确字符串替换。replacements = [(旧, 新, 说明), ...]

    幂等判据：**只要目标文本已出现就跳过**（而不是"旧文本消失"）。
    因为有些替换是"往锚点后插入"，锚点在替换后依然存在。
    """
    global changed
    path = os.path.join(ROOT, relpath)
    if not os.path.exists(path):
        log("MISSING", relpath)
        problems.append(f"{relpath} 不存在")
        return

    text = _read(path)
    original = text
    crlf = "\r\n" in text

    for old, new, note in replacements:
        # 该文件是 CRLF 时，把模式里的 \n 也换成 \r\n
        variants = [(old, new)]
        if crlf:
            variants = [(old.replace("\n", "\r\n"), new.replace("\n", "\r\n")),
                        (old, new)]

        done = any(n in text for _, n in variants)
        if done:
            log("SKIP", relpath, f"已是目标状态：{note}")
            continue

        applied = False
        for o, n in variants:
            count = text.count(o)
            if count:
                text = text.replace(o, n)
                log("PATCHED", relpath, f"{note}（{count} 处）")
                applied = True
                break
        if not applied:
            log("MISS", relpath, f"未找到模式：{note}")
            problems.append(f"{relpath}: 未找到 {note}")

    if text != original:
        _write(path, text)
        changed += 1


# ---------------------------------------------------------------------------
# 1. 把 iOS 门面 crate 加进 Cargo workspace
# ---------------------------------------------------------------------------
# 不加进 workspace 的话 `cargo build -p ruzu_ios` 会报 "package not found"。
edit(
    "Cargo.toml",
    [
        (
            "members = [\n",
            'members = [\n    "src/ruzu_ios",\n',
            "workspace 加入 src/ruzu_ios",
        ),
    ],
)

# ---------------------------------------------------------------------------
# 2. rhazel（子模块）：把 MAP_JIT + 写保护从 macOS 放开到 iOS
# ---------------------------------------------------------------------------
# 这是整个移植里最关键的一处。iOS 与 macOS(arm64) 用的是同一套官方 JIT 机制：
#   mmap(..., MAP_JIT)  +  pthread_jit_write_protect_np()  +  sys_icache_invalidate()
# 上游只在 macOS 上启用，iOS 上会退化成普通 RWX 映射 —— 未越狱设备直接失败。
RH_CFG_OLD = 'all(target_os = "macos", target_arch = "aarch64")'
RH_CFG_NEW = 'all(any(target_os = "macos", target_os = "ios"), target_arch = "aarch64")'
edit(
    "externals/rhazel/src/block_of_code.rs",
    [
        (RH_CFG_OLD, RH_CFG_NEW, "JIT 写保护/刷 I-cache 的 cfg 放开到 iOS"),
        (
            '    #[cfg(target_os = "macos")]\n'
            "    let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT;",
            '    #[cfg(any(target_os = "macos", target_os = "ios"))]\n'
            "    let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT;",
            "代码缓存 mmap 走 MAP_JIT",
        ),
        (
            '    #[cfg(not(target_os = "macos"))]\n'
            "    let flags = libc::MAP_PRIVATE | libc::MAP_ANON;",
            '    #[cfg(not(any(target_os = "macos", target_os = "ios")))]\n'
            "    let flags = libc::MAP_PRIVATE | libc::MAP_ANON;",
            "非 Apple 平台走普通 RWX",
        ),
    ],
)

# ---------------------------------------------------------------------------
# 3. video_core：Metal 后端从 macOS 放开到 iOS
# ---------------------------------------------------------------------------
edit(
    "src/video_core/src/lib.rs",
    [
        (
            '#[cfg(target_os = "macos")]\npub mod renderer_metal;',
            '#[cfg(any(target_os = "macos", target_os = "ios"))]\npub mod renderer_metal;',
            "Metal 后端参与 iOS 编译",
        ),
    ],
)

edit(
    "src/video_core/Cargo.toml",
    [
        (
            "[target.'cfg(target_os = \"macos\")'.dependencies]",
            "[target.'cfg(any(target_os = \"macos\", target_os = \"ios\"))'.dependencies]",
            "objc2 系列依赖放开到 iOS",
        ),
        (
            "[target.'cfg(target_os = \"macos\")'.dev-dependencies]",
            "[target.'cfg(any(target_os = \"macos\", target_os = \"ios\"))'.dev-dependencies]",
            "objc2 dev-deps 放开到 iOS",
        ),
    ],
)

# iOS 独有：Metal 视图要挂到 UIView 上（UIView 来自 UIKit，macOS 没有）
_vc_cargo = os.path.join(ROOT, "src/video_core/Cargo.toml")
if os.path.exists(_vc_cargo):
    _txt = _read(_vc_cargo)
    if "objc2-ui-kit" not in _txt:
        _nl = "\r\n" if "\r\n" in _txt else "\n"
        _block = _nl.join([
            "",
            "# iOS：Metal 渲染目标挂在 UIView / CAMetalLayer 上（macOS 用 NSView）",
            "[target.'cfg(target_os = \"ios\")'.dependencies]",
            'objc2-ui-kit = "0.3.2"',
            "",
        ])
        _txt += _block
        _write(_vc_cargo, _txt)
        log("PATCHED", "src/video_core/Cargo.toml", "加入 iOS 专用的 objc2-ui-kit")
        changed += 1
    else:
        log("SKIP", "src/video_core/Cargo.toml", "objc2-ui-kit 已存在")

# ---------------------------------------------------------------------------
# 4. common/host_memory.rs：客户机内存管理支持 iOS
# ---------------------------------------------------------------------------
# Linux/macOS 走同一套「shm_open 后端 + PROT_NONE 虚拟预留 + MAP_FIXED 映射」实现，
# iOS 完全沿用（Darwin 的 shm_open 在 iOS 沙盒内可用）。
edit(
    "src/common/src/host_memory.rs",
    [
        (
            'any(target_os = "linux", target_os = "macos")',
            'any(target_os = "linux", target_os = "macos", target_os = "ios")',
            "POSIX 内存实现纳入 iOS",
        ),
        (
            'any(target_os = "windows", target_os = "linux", target_os = "macos")',
            'any(target_os = "windows", target_os = "linux", target_os = "macos", target_os = "ios")',
            "三平台组合 cfg 纳入 iOS",
        ),
    ],
)

# map_noreserve()：iOS 与 macOS 一样返回 0（Darwin 不需要 MAP_NORESERVE 语义）
edit(
    "src/common/src/host_memory.rs",
    [
        (
            '#[cfg(target_os = "macos")]\nfn map_noreserve() -> i32 {\n    0\n}',
            '#[cfg(any(target_os = "macos", target_os = "ios"))]\nfn map_noreserve() -> i32 {\n    0\n}',
            "map_noreserve 覆盖 iOS",
        ),
    ],
)

# ---------------------------------------------------------------------------
# 5. core：iOS 上没有系统 OpenSSL，改用 vendored（自带源码编译）
# ---------------------------------------------------------------------------
# core 的 openssl 只用于 hle/service/ssl（联机）和一处 sha1。iOS SDK 不带 OpenSSL
# 头文件，所以必须走 vendored。实测这会在 CI 上多花几分钟，但不改代码。
edit(
    "src/core/Cargo.toml",
    [
        (
            '[target.\'cfg(target_os = "openbsd")\'.dependencies]',
            '[target.\'cfg(target_os = "ios")\'.dependencies]\n'
            "# iOS SDK 不自带 OpenSSL，只能让 openssl-src 交叉编译一份（iOS 目标已支持）\n"
            'openssl = { workspace = true, features = ["vendored"] }\n'
            "\n"
            '[target.\'cfg(target_os = "openbsd")\'.dependencies]',
            "iOS 使用 vendored OpenSSL",
        ),
    ],
)

# ---------------------------------------------------------------------------
print()
print(f"=== 补丁完成：{changed} 个文件被修改 ===")
if problems:
    print("以下替换未命中（不影响构建继续，但对应功能在 iOS 上可能缺失）：")
    for p in problems:
        print("  -", p)
    # 注意：这里故意返回 0，CI 不应该因为补丁没匹配上就失败
sys.exit(0)
