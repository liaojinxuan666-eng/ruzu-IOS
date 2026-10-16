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


def replace_all(relpath, old, new, note):
    """无条件全量替换。

    与 edit() 的区别：**不做"目标是否已存在"的守卫**，只看源模式还在不在。
    适合 `X` → `Y` 这种源模式替换后就彻底消失的场景（比如把某个 cfg 放宽），
    因为不会出现"部分替换后守卫误判、跳过剩余位置"的问题。
    """
    global changed
    path = os.path.join(ROOT, relpath)
    if not os.path.exists(path):
        log("MISSING", relpath)
        problems.append(f"{relpath} 不存在")
        return
    text = _read(path)
    crlf = "\r\n" in text
    count = text.count(old)
    if count == 0:
        if new in text:
            log("SKIP", relpath, f"已是目标状态：{note}")
        else:
            log("MISS", relpath, f"未找到模式：{note}")
            problems.append(f"{relpath}: 未找到 {note}")
        return
    text = text.replace(old, new)
    if crlf:
        text = text.replace(old.replace("\n", "\r\n"), new.replace("\n", "\r\n"))
    _write(path, text)
    log("PATCHED", relpath, f"{note}（{count} 处）")
    changed += 1


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

        done = bool(new) and any(n and n in text for _, n in variants)
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
    ],
)

# ---------------------------------------------------------------------------
# 2b. rhazel：iOS 上把 pthread_jit_write_protect_np 改成运行时 dlsym 查找
# ---------------------------------------------------------------------------
# 上一步把 cfg 放宽到 iOS 之后，iOS 上会**静态引用** pthread_jit_write_protect_np，
# 而这个符号在 iOS SDK 的 libSystem stub 里不存在 —— 一旦真的链接（把 core /
# rdynarmic 编进 App），就会报：
#   Undefined symbols for architecture arm64: "_pthread_jit_write_protect_np"
# macOS 保持原样（直接调用），iOS 改成 dlsym：找得到就用写保护开关，找不到就
# 退化成不动写保护（依赖 JIT entitlement 直接写可执行页）。
_RH_EXT_OLD = '''#[cfg(all(any(target_os = "macos", target_os = "ios"), target_arch = "aarch64"))]
unsafe extern "C" {
    fn pthread_jit_write_protect_np(enabled: i32);
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
}'''

_RH_EXT_NEW = '''#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
unsafe extern "C" {
    fn pthread_jit_write_protect_np(enabled: i32);
}

#[cfg(all(any(target_os = "macos", target_os = "ios"), target_arch = "aarch64"))]
unsafe extern "C" {
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
}

// iOS 的 SDK stub 里没有 pthread_jit_write_protect_np（那是 macOS 的符号），
// 静态引用会让链接期报 Undefined symbols，所以这里改成运行时 dlsym 查找。
#[cfg(all(target_os = "ios", target_arch = "aarch64"))]
unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const i8) -> *mut c_void;
}

#[cfg(all(target_os = "ios", target_arch = "aarch64"))]
unsafe fn ios_set_jit_write_protect(enabled: bool) {
    use std::sync::OnceLock;
    type JitWriteProtectFn = unsafe extern "C" fn(i32);
    static FN: OnceLock<Option<JitWriteProtectFn>> = OnceLock::new();
    let f = *FN.get_or_init(|| {
        // RTLD_DEFAULT = (void *)-2：在主程序与所有已加载镜像里找符号
        let p = dlsym(
            -2isize as *mut c_void,
            b"pthread_jit_write_protect_np\\0".as_ptr() as *const i8,
        );
        if p.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut c_void, JitWriteProtectFn>(p))
        }
    });
    if let Some(f) = f {
        f(enabled as i32);
    }
}'''

_RH_MMAP_OLD = '''unsafe fn mmap_code_cache(size: usize) -> Result<NonNull<u8>, String> {
    #[cfg(target_os = "macos")]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT;
    #[cfg(not(target_os = "macos"))]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANON;

    let ptr = libc::mmap(
        std::ptr::null_mut(),
        size,
        libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
        flags,
        -1,
        0,
    );
    if ptr == libc::MAP_FAILED {
        return Err(format!(
            "failed to allocate ARM64 executable code cache: {}",
            std::io::Error::last_os_error()
        ));
    }

    NonNull::new(ptr.cast::<u8>()).ok_or_else(|| "mmap returned null".to_string())
}'''

_RH_MMAP_NEW = '''unsafe fn mmap_code_cache(size: usize) -> Result<NonNull<u8>, String> {
    // iOS：按顺序试三种映射，第一个成功即用。
    //   ① 普通 RWX 匿名映射（**iOS 的正确姿势**）—— 可执行性由代码签名授权
    //      （dynamic-codesigning entitlement / CS_DEBUGGED）保证。
    //      真机实测（iPhone11,6 / iOS 18.5）：这条 OK。
    //   ②③ MAP_JIT 两个变体只作兜底 —— MAP_JIT 是 macOS 专有机制，
    //      iOS 上实测 RWX+MAP_JIT 报 EINVAL、RW+MAP_JIT 报 EACCES。
    #[cfg(target_os = "ios")]
    {
        let anon = libc::MAP_PRIVATE | libc::MAP_ANON;
        let jit = anon | libc::MAP_JIT;
        let rwx = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
        let rw = libc::PROT_READ | libc::PROT_WRITE;
        for (prot, flags) in [(rwx, anon), (rwx, jit), (rw, jit)] {
            let p = libc::mmap(std::ptr::null_mut(), size, prot, flags, -1, 0);
            if p != libc::MAP_FAILED {
                if let Some(n) = NonNull::new(p.cast::<u8>()) {
                    return Ok(n);
                }
            }
        }
        return Err(format!(
            "failed to allocate ARM64 executable code cache (iOS): {}",
            std::io::Error::last_os_error()
        ));
    }

    #[cfg(not(target_os = "ios"))]
    {
        #[cfg(target_os = "macos")]
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT;
        #[cfg(not(any(target_os = "macos", target_os = "ios")))]
        let flags = libc::MAP_PRIVATE | libc::MAP_ANON;

        let ptr = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
            flags,
            -1,
            0,
        );
        if ptr == libc::MAP_FAILED {
            return Err(format!(
                "failed to allocate ARM64 executable code cache: {}",
                std::io::Error::last_os_error()
            ));
        }

        NonNull::new(ptr.cast::<u8>()).ok_or_else(|| "mmap returned null".to_string())
    }
}'''

edit(
    "externals/rhazel/src/block_of_code.rs",
    [
        (_RH_EXT_OLD, _RH_EXT_NEW, "iOS 用 dlsym 拿写保护函数（避免链接期缺符号）"),
        (
            '    #[cfg(all(any(target_os = "macos", target_os = "ios"), target_arch = "aarch64"))]\n'
            "    pthread_jit_write_protect_np(enabled as i32);\n"
            "\n"
            '    #[cfg(not(all(any(target_os = "macos", target_os = "ios"), target_arch = "aarch64")))]\n'
            "    let _ = enabled;",
            '    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]\n'
            "    pthread_jit_write_protect_np(enabled as i32);\n"
            "\n"
            '    #[cfg(all(target_os = "ios", target_arch = "aarch64"))]\n'
            "    ios_set_jit_write_protect(enabled);\n"
            "\n"
            '    #[cfg(not(all(any(target_os = "macos", target_os = "ios"), target_arch = "aarch64")))]\n'
            "    let _ = enabled;",
            "set_jit_write_protect 在 iOS 上走 dlsym 版本",
        ),
        # iOS 的 mmap 姿势：分两步试，先 WebKit 姿势（RW + MAP_JIT），再退回 RWX
        (_RH_MMAP_OLD, _RH_MMAP_NEW, "iOS 的 JIT 内存映射改用梯子式尝试"),
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

# 上面那步只覆盖了 map_noreserve。文件里还有几处 macos-only 的定义/分支，
# 它们是同一套 POSIX 实现的一部分（CString / AtomicU64 导入、create_backing_fd、
# clear_backing_region 的返回分支），漏掉就会在 iOS 上报：
#   error[E0425]: cannot find function `create_backing_fd` in this scope
#   error[E0308]: mismatched types（clear_backing_region 返回 ()，期望 bool）
# 这里把 host_memory.rs 里剩下的 macos-only cfg 全部放宽到 iOS。
replace_all(
    "src/common/src/host_memory.rs",
    '#[cfg(target_os = "macos")]',
    '#[cfg(any(target_os = "macos", target_os = "ios"))]',
    "host_memory 其余 macos-only 分支纳入 iOS",
)

# ---------------------------------------------------------------------------
# 6. audio_core：iOS 上摘掉 cubeb
# ---------------------------------------------------------------------------
# cubeb 的 CoreAudio 后端在 iOS 上根本构建不了：
#   cubeb-sys → cubeb-coreaudio-rs → coreaudio-sys 0.2.14
#   其 build.rs 里写死 `assert!(target.contains("apple-darwin"))`，
#   目标三元组是 aarch64-apple-ios 时直接 panic（exit 101）。
# audio_core 本来就有 SDL3 后端（sdl3_sink.rs），SDL3 在 iOS 上走系统 CoreAudio，
# 所以 iOS 上摘掉 cubeb、把音频引擎默认切到 SDL3 即可。
#
# 这段用专门的逻辑而不是通用 edit()，因为「删除一行依赖 + 追加一个 target 块」
# 用通用守卫容易误判（空替换永远"已生效"、删除模式会误伤新加的块）。
_ac = os.path.join(ROOT, "src/audio_core/Cargo.toml")
if not os.path.exists(_ac):
    log("MISSING", "src/audio_core/Cargo.toml")
    problems.append("src/audio_core/Cargo.toml 不存在")
else:
    _t = _read(_ac)
    _nl = "\r\n" if "\r\n" in _t else "\n"
    _orig = _t
    # ① 删掉 [dependencies] 里那行无条件 cubeb（靠"后面紧跟 log"定位，不会误伤新块）
    _t = _t.replace(
        "cubeb.workspace = true" + _nl + "log.workspace = true",
        "log.workspace = true",
    )
    # ② 追加「非 iOS 才有 cubeb」的依赖块
    if "[target.'cfg(not(target_os = \"ios\"))'.dependencies]" not in _t:
        _t = _t.rstrip() + _nl + _nl.join(
            [
                "",
                "# cubeb 的 CoreAudio 后端不支持 iOS（coreaudio-sys 的 build.rs 写死 apple-darwin），",
                "# iOS 上改用 SDL3 的 CoreAudio 输出",
                "[target.'cfg(not(target_os = \"ios\"))'.dependencies]",
                "cubeb.workspace = true",
                "",
            ]
        )
    if _t != _orig:
        _write(_ac, _t)
        log("PATCHED", "src/audio_core/Cargo.toml", "cubeb 改为仅非 iOS 依赖")
        changed += 1
    else:
        log("SKIP", "src/audio_core/Cargo.toml", "cubeb 已是仅非 iOS 依赖")

edit(
    "src/audio_core/src/sink/mod.rs",
    [
        (
            "pub mod cubeb_sink;",
            '#[cfg(not(target_os = "ios"))]\npub mod cubeb_sink;',
            "cubeb sink 不在 iOS 上编译",
        ),
    ],
)

edit(
    "src/audio_core/src/sink/sink_details.rs",
    [
        (
            "use crate::sink::cubeb_sink::{get_cubeb_latency, list_cubeb_sink_devices, CubebSink};",
            '#[cfg(not(target_os = "ios"))]\n'
            "use crate::sink::cubeb_sink::{get_cubeb_latency, list_cubeb_sink_devices, CubebSink};",
            "cubeb 导入仅在非 iOS",
        ),
        (
            "    if sink_id == AudioEngine::Auto {\n"
            "        let selected = if get_cubeb_latency() > TARGET_SAMPLE_COUNT * 3 {\n"
            "            AudioEngine::Sdl3\n"
            "        } else {\n"
            "            AudioEngine::Cubeb\n"
            "        };",
            "    if sink_id == AudioEngine::Auto {\n"
            "        // iOS 上没有 cubeb，直接用 SDL3（它在 iOS 上走系统 CoreAudio）\n"
            '        #[cfg(target_os = "ios")]\n'
            "        let selected = AudioEngine::Sdl3;\n"
            '        #[cfg(not(target_os = "ios"))]\n'
            "        let selected = if get_cubeb_latency() > TARGET_SAMPLE_COUNT * 3 {\n"
            "            AudioEngine::Sdl3\n"
            "        } else {\n"
            "            AudioEngine::Cubeb\n"
            "        };",
            "Auto 在 iOS 上选 SDL3",
        ),
        (
            "    match sink_id {\n"
            "        AudioEngine::Cubeb | AudioEngine::Sdl3 | AudioEngine::Null => sink_id,",
            "    // 配置里如果写着 Cubeb，iOS 上降级到 SDL3\n"
            '    #[cfg(target_os = "ios")]\n'
            "    if sink_id == AudioEngine::Cubeb {\n"
            '        info!("audio_core: cubeb 在 iOS 上不可用，改用 SDL3");\n'
            "        return AudioEngine::Sdl3;\n"
            "    }\n"
            "\n"
            "    match sink_id {\n"
            "        AudioEngine::Cubeb | AudioEngine::Sdl3 | AudioEngine::Null => sink_id,",
            "显式 Cubeb 配置降级",
        ),
        (
            "pub fn get_sink_ids() -> Vec<AudioEngine> {\n"
            "    vec![AudioEngine::Cubeb, AudioEngine::Sdl3, AudioEngine::Null]\n"
            "}",
            "pub fn get_sink_ids() -> Vec<AudioEngine> {\n"
            '    #[cfg(target_os = "ios")]\n'
            "    {\n"
            "        return vec![AudioEngine::Sdl3, AudioEngine::Null];\n"
            "    }\n"
            '    #[cfg(not(target_os = "ios"))]\n'
            "    vec![AudioEngine::Cubeb, AudioEngine::Sdl3, AudioEngine::Null]\n"
            "}",
            "iOS 不暴露 Cubeb 引擎",
        ),
        (
            "        AudioEngine::Cubeb => list_cubeb_sink_devices(capture),",
            "        AudioEngine::Cubeb => {\n"
            '            #[cfg(not(target_os = "ios"))]\n'
            "            {\n"
            "                list_cubeb_sink_devices(capture)\n"
            "            }\n"
            '            #[cfg(target_os = "ios")]\n'
            "            {\n"
            "                list_sdl_sink_devices(capture)\n"
            "            }\n"
            "        }",
            "设备枚举在 iOS 上走 SDL3",
        ),
        (
            "        AudioEngine::Cubeb => Box::new(CubebSink::new(device_id)) as Box<dyn Sink>,",
            "        AudioEngine::Cubeb => {\n"
            '            #[cfg(not(target_os = "ios"))]\n'
            "            {\n"
            "                Box::new(CubebSink::new(device_id)) as Box<dyn Sink>\n"
            "            }\n"
            '            #[cfg(target_os = "ios")]\n'
            "            {\n"
            "                Box::new(SDLSink::new(device_id)) as Box<dyn Sink>\n"
            "            }\n"
            "        }",
            "sink 创建在 iOS 上走 SDL3",
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
# 7. 两个 build.rs：iOS 上跳过宿主 pkg-config 依赖
# ---------------------------------------------------------------------------
# 这两个 crate 的 build.rs 都用 pkg-config 探测宿主库，CI 上必然报
#   "pkg-config has not been configured to support cross-compilation"
# 而且就算探测到，宿主（macOS runner）编出来的库也不能链进 iOS App。
# 所以 iOS 上跳过探测：
#   * audio_core → libopus（Opus 解码）
#   * video_core → libavcodec/libavutil/libva（nvdec 视频解码）+ ffmpeg shim
# cargo check 不做链接，跳过探测不影响它；链接阶段（真要跑模拟器时）再单独给
# iOS 交叉编译这两个库，或换纯 Rust 解码器。
edit(
    "src/audio_core/build.rs",
    [
        (
            "    pkg_config::Config::new()\n"
            '        .probe("opus")\n'
            '        .expect("libopus is required for audio_core Opus decoding");',
            "    // iOS：跳过 libopus 探测。\n"
            "    // 1) CI 上没有配置交叉编译用的 pkg-config sysroot，直接探测会 panic；\n"
            "    // 2) 宿主（macOS runner）的 libopus 是给 macOS 用的，链进 iOS App 是错的。\n"
            "    // 真要用 Opus 时再给 iOS 交叉编译一份 libopus，或换成纯 Rust 解码器。\n"
            '    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("ios") {\n'
            "        println!(\"cargo:warning=audio_core: iOS 目标暂时跳过 libopus 探测\");\n"
            "        return;\n"
            "    }\n"
            "    pkg_config::Config::new()\n"
            '        .probe("opus")\n'
            '        .expect("libopus is required for audio_core Opus decoding");',
            "audio_core/build.rs 跳过 iOS 上的 libopus",
        ),
    ],
)

_VC_BUILD_OLD = '''    let avcodec = pkg_config::Config::new()
        .probe("libavcodec")
        .expect("libavcodec is required for video_core FFmpeg decoding");
    let avutil = pkg_config::Config::new()
        .probe("libavutil")
        .expect("libavutil is required for video_core FFmpeg decoding");
    let libva = match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("linux" | "freebsd") => pkg_config::Config::new().probe("libva").ok(),
        _ => None,
    };

    let mut build = cc::Build::new();
    build.file("src/host1x/ffmpeg/ffmpeg_shim.c");
    build.warnings(false);
    for path in avcodec
        .include_paths
        .iter()
        .chain(avutil.include_paths.iter())
    {
        build.include(path);
    }
    if let Some(libva) = &libva {
        build.define("LIBVA_FOUND", None);
        for path in &libva.include_paths {
            build.include(path);
        }
    }
    build.compile("ruzu_video_core_ffmpeg_shim");
'''

_vc_build = os.path.join(ROOT, "src/video_core/build.rs")
if not os.path.exists(_vc_build):
    log("MISSING", "src/video_core/build.rs")
    problems.append("src/video_core/build.rs 不存在")
else:
    _t = _read(_vc_build)
    _nl = "\r\n" if "\r\n" in _t else "\n"
    _old = _VC_BUILD_OLD if _nl == "\n" else _VC_BUILD_OLD.replace("\n", "\r\n")
    if "iOS_FLAG_SKIP_FFMPEG" in _t:
        log("SKIP", "src/video_core/build.rs", "已有 iOS 的 FFmpeg 跳过逻辑")
    elif _old in _t:
        _indented = "".join("    " + ln if ln.strip() else ln for ln in _old.splitlines(True))
        _new = (
            "    // iOS_FLAG_SKIP_FFMPEG：iOS 上跳过 FFmpeg 相关的一切。\n"
            "    // 理由同 audio_core 的 libopus：pkg-config 交叉编译不可用，宿主库也不能链进 iOS。\n"
            "    // 视频硬件解码（nvdec）是后期功能，先让骨架能编过。\n"
            '    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("ios") {\n'
            + _indented
            + "    }\n"
        )
        _t = _t.replace(_old, _new)
        _write(_vc_build, _t)
        log("PATCHED", "src/video_core/build.rs", "iOS 跳过 FFmpeg 探测与 shim 编译")
        changed += 1
    else:
        log("MISS", "src/video_core/build.rs", "未找到 FFmpeg 探测块")
        problems.append("src/video_core/build.rs: 未找到 FFmpeg 探测块")

# ---------------------------------------------------------------------------
print()
print(f"=== 补丁完成：{changed} 个文件被修改 ===")
if problems:
    print("以下替换未命中（不影响构建继续，但对应功能在 iOS 上可能缺失）：")
    for p in problems:
        print("  -", p)
    # 注意：这里故意返回 0，CI 不应该因为补丁没匹配上就失败
sys.exit(0)
