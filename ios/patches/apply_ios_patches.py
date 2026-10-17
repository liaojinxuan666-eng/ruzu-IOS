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

_RH_MMAP_NEW = '''unsafe fn mmap_code_cache(size: usize) -> Result<(NonNull<u8>, NonNull<u8>, bool), String> {
    // iOS：**双视图映射**（V8 在 Apple 平台的 JIT 设计）
    //   ① 映射一片 RW 作为"写视图"
    //   ② 用 mach_vm_remap 把同一片**物理页**再映射成一个"执行视图"，并设成 R-X
    //   这样写入和执行互不干扰：不需要任何权限翻转（零开销），也不会因为
    //   某个线程正在 emit 而让另一个线程执行到 RW 页上（macOS 靠
    //   pthread_jit_write_protect_np 的**每线程**写保护解决，iOS 没有那个符号）。
    //   万一别名创建失败：退化为单视图，由 set_jit_write_protection() 用
    //   mprotect 翻转"已使用的那部分"，慢一些但正确。
    #[cfg(target_os = "ios")]
    {
        let rw = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        );
        if rw == libc::MAP_FAILED {
            return Err(format!(
                "failed to allocate ARM64 code cache (iOS, RW): {}",
                std::io::Error::last_os_error()
            ));
        }
        let rw_ptr = match NonNull::new(rw.cast::<u8>()) {
            Some(p) => p,
            None => return Err("mmap returned null".to_string()),
        };

        let mut exec_addr: u64 = 0;
        let mut cur_prot: i32 = 0;
        let mut max_prot: i32 = 0;
        let kr = mach_vm_remap(
            mach_task_self_,
            &mut exec_addr,
            size as u64,
            0,
            VM_FLAGS_ANYWHERE,
            mach_task_self_,
            rw as u64,
            0, // copy = FALSE：共享同一片物理页
            &mut cur_prot,
            &mut max_prot,
            0, // VM_INHERIT_NONE
        );
        if kr == 0 && exec_addr != 0 {
            let exec_raw = exec_addr as *mut libc::c_void;
            if libc::mprotect(exec_raw, size, libc::PROT_READ | libc::PROT_EXEC) == 0 {
                if let Some(exec_ptr) = NonNull::new(exec_raw.cast::<u8>()) {
                    return Ok((rw_ptr, exec_ptr, true));
                }
            }
        }
        // 退路：没有别名可用
        return Ok((rw_ptr, rw_ptr, false));
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

        let p = NonNull::new(ptr.cast::<u8>()).ok_or_else(|| "mmap returned null".to_string())?;
        Ok((p, p, false))
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
        # iOS 的 mmap 姿势 + 双视图 W^X
        (_RH_MMAP_OLD, _RH_MMAP_NEW, "iOS 的 JIT 内存映射改用双视图（RW 写 + RX 执行）"),
        # 双视图所需的 Mach 接口（挂在 ios_set_jit_write_protect 之后）
        (
            "    if let Some(f) = f {\n        f(enabled as i32);\n    }\n}",
            "    if let Some(f) = f {\n        f(enabled as i32);\n    }\n"
            "}\n\n"
            "// iOS 双视图 JIT 映射所需的 Mach 接口。\n"
            "// mach_vm_remap 可以把同一片**物理页**再映射成一个新地址（别名），\n"
            "// 从而做到「一个视图写、一个视图执行」，完全不需要翻转权限。\n"
            '#[cfg(all(target_os = "ios", target_arch = "aarch64"))]\n'
            'extern "C" {\n'
            "    static mach_task_self_: u32;\n"
            "    fn mach_vm_remap(\n"
            "        target_task: u32,\n"
            "        target_address: *mut u64,\n"
            "        size: u64,\n"
            "        mask: u64,\n"
            "        flags: i32,\n"
            "        src_task: u32,\n"
            "        src_address: u64,\n"
            "        copy: i32,\n"
            "        cur_protection: *mut i32,\n"
            "        max_protection: *mut i32,\n"
            "        inheritance: u32,\n"
            "    ) -> i32;\n"
            "}\n\n"
            '#[cfg(all(target_os = "ios", target_arch = "aarch64"))]\n'
            "const VM_FLAGS_ANYWHERE: i32 = 1;",
            "加入 mach_vm_remap 的 FFI 声明",
        ),
        # 结构体加两个字段
        (
            "/// Executable AArch64 code buffer.\n"
            "pub struct BlockOfCode {\n"
            "    ptr: NonNull<u8>,\n"
            "    size: usize,\n"
            "    cursor: usize,\n"
            "    jit_write_protection_enabled: bool,\n"
            "}",
            "/// Executable AArch64 code buffer.\n"
            "pub struct BlockOfCode {\n"
            "    /// 写入视图（RW）。\n"
            "    ptr: NonNull<u8>,\n"
            "    /// 执行视图（R-X）。iOS 上它与 `ptr` 是**同一片物理页的另一个映射**，\n"
            "    /// 所以写入和执行互不干扰、无需任何权限翻转（性能关键）。\n"
            "    /// 别名创建失败时退化为与 `ptr` 相同。\n"
            "    exec_ptr: NonNull<u8>,\n"
            "    /// 双视图是否生效\n"
            "    has_exec_alias: bool,\n"
            "    size: usize,\n"
            "    cursor: usize,\n"
            "    jit_write_protection_enabled: bool,\n"
            "}",
            "BlockOfCode 增加执行视图字段",
        ),
        # with_size 接新返回值
        (
            "        let ptr = unsafe { mmap_code_cache(size)? };\n"
            "        Ok(Self {\n"
            "            ptr,\n"
            "            size,\n"
            "            cursor: 0,\n"
            "            jit_write_protection_enabled: true,\n"
            "        })",
            "        let (ptr, exec_ptr, has_exec_alias) = unsafe { mmap_code_cache(size)? };\n"
            "        Ok(Self {\n"
            "            ptr,\n"
            "            exec_ptr,\n"
            "            has_exec_alias,\n"
            "            size,\n"
            "            cursor: 0,\n"
            "            jit_write_protection_enabled: true,\n"
            "        })",
            "with_size 接收双视图返回值",
        ),
        # 对外暴露的代码基址 = 执行视图
        (
            "    pub fn code_base_ptr(&self) -> *const u8 {\n        self.ptr.as_ptr()\n    }",
            "    /// 注意：返回的是**执行视图**。写入走 `ptr`，执行走这里 —— \n"
            "    /// 上层所有「算出来的代码地址」都会指向可执行的那份映射。\n"
            "    pub fn code_base_ptr(&self) -> *const u8 {\n        self.exec_ptr.as_ptr()\n    }",
            "code_base_ptr 返回执行视图",
        ),
        # 权限翻转：有别名就什么都不用做
        (
            "    unsafe fn set_jit_write_protection(&mut self, enabled: bool) {\n"
            "        if self.jit_write_protection_enabled == enabled {\n"
            "            return;\n"
            "        }\n"
            "        set_jit_write_protect(enabled);\n"
            "        self.jit_write_protection_enabled = enabled;\n"
            "    }",
            "    unsafe fn set_jit_write_protection(&mut self, enabled: bool) {\n"
            "        if self.jit_write_protection_enabled == enabled {\n"
            "            return;\n"
            "        }\n"
            '        #[cfg(target_os = "ios")]\n'
            "        {\n"
            "            // 有执行别名时**什么都不用做**：写视图永远可写、执行视图永远可执行，\n"
            "            // 这正是双视图设计的价值（零开销、也不会和别的线程抢权限）。\n"
            "            if !self.has_exec_alias {\n"
            "                // 退路：只能把「已使用的那部分」按需翻转，比整片翻转便宜得多。\n"
            "                let page: usize = 0x1000;\n"
            "                let used = ((self.cursor + page - 1) & !(page - 1)).min(self.size);\n"
            "                if used > 0 {\n"
            "                    let prot = if enabled {\n"
            "                        libc::PROT_READ | libc::PROT_EXEC\n"
            "                    } else {\n"
            "                        libc::PROT_READ | libc::PROT_WRITE\n"
            "                    };\n"
            "                    libc::mprotect(self.ptr.as_ptr() as *mut libc::c_void, used, prot);\n"
            "                }\n"
            "            }\n"
            "        }\n"
            "        set_jit_write_protect(enabled);\n"
            "        self.jit_write_protection_enabled = enabled;\n"
            "    }",
            "iOS 上有别名时跳过权限翻转",
        ),
        # Drop 要释放两个视图
        (
            "impl Drop for BlockOfCode {\n"
            "    fn drop(&mut self) {\n"
            "        unsafe {\n"
            "            libc::munmap(self.ptr.as_ptr().cast::<c_void>(), self.size);\n"
            "        }\n"
            "    }\n"
            "}",
            "impl Drop for BlockOfCode {\n"
            "    fn drop(&mut self) {\n"
            "        unsafe {\n"
            "            // 两个视图都要释放（它们各自是一次独立的映射）\n"
            "            if self.exec_ptr != self.ptr {\n"
            "                libc::munmap(self.exec_ptr.as_ptr().cast::<c_void>(), self.size);\n"
            "            }\n"
            "            libc::munmap(self.ptr.as_ptr().cast::<c_void>(), self.size);\n"
            "        }\n"
            "    }\n"
            "}",
            "Drop 释放执行视图",
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
# 3b. video_core：把 retained_len 的诊断 cfg 也放开到 iOS
# ---------------------------------------------------------------------------
# `delayed_destruction_ring.rs` 里 retained_len() 的 cfg 是
#   #[cfg(any(test, target_os = "macos"))]
# 而 renderer_metal/metal_texture_cache.rs 在**非测试代码**里调用了它 3 次。
# 以前 renderer_metal 只在 macOS 编译所以没事；我们把 Metal 后端放开到 iOS 之后，
# 这 3 个调用就在 iOS 上被编译 → 3 个 E0599。把 cfg 补上 ios 即可。
edit(
    "src/video_core/src/delayed_destruction_ring.rs",
    [
        (
            '#[cfg(any(test, target_os = "macos"))]\n    pub(crate) fn retained_len',
            '#[cfg(any(test, target_os = "macos", target_os = "ios"))]\n    pub(crate) fn retained_len',
            "retained_len 的诊断 cfg 纳入 iOS",
        ),
    ],
)

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
# 8. common/fiber.rs：iOS 上按 RLIMIT_STACK 做阶梯式栈分配
# ---------------------------------------------------------------------------
# `context` crate 的 ProtectedFixedSizeStack 会把请求大小和 RLIMIT_STACK 比较，
# iOS 的 rlimit 是 999424 字节（约 976 KiB），而 DEFAULT_STACK_SIZE 是 4 MiB
# → Err(ExceedsMaximumSize) → panic "failed to allocate fiber stack"。
# iOS 上没法提 rlimit，所以按上限递减地试，取第一个能成的。
edit(
    "src/common/src/fiber.rs",
    [
        (
            '        let stack =\n'
            '            FixedSizeStack::new(DEFAULT_STACK_SIZE).expect("failed to allocate fiber stack");\n'
            '        let rewind_stack =\n'
            '            FixedSizeStack::new(DEFAULT_STACK_SIZE).expect("failed to allocate rewind stack");',
            '        let stack = allocate_stack();\n'
            '        let rewind_stack = allocate_stack();',
            "fiber 栈改用阶梯分配",
        ),
        (
            'const DEFAULT_STACK_SIZE: usize = 4 * 1024 * 1024;',
            'const DEFAULT_STACK_SIZE: usize = 4 * 1024 * 1024;\n'
            '\n'
            '/// 按「能拿到的最大可用栈」阶梯式分配。\n'
            '///\n'
            '/// iOS 的 RLIMIT_STACK 是 999424 字节（约 976 KiB），context crate 会拿请求大小\n'
            '/// 和它比较 —— 4 MiB 直接 Err(ExceedsMaximumSize)。这里从 4 MiB 往下递减，\n'
            '/// 取第一个成功的；桌面平台第一条就成，行为不变。\n'
            'fn allocate_stack() -> FixedSizeStack {\n'
            '    const CANDIDATES: [usize; 6] = [\n'
            '        4 * 1024 * 1024,\n'
            '        960 * 1024,\n'
            '        512 * 1024,\n'
            '        256 * 1024,\n'
            '        128 * 1024,\n'
            '        64 * 1024,\n'
            '    ];\n'
            '    let mut last = String::new();\n'
            '    for size in CANDIDATES {\n'
            '        match FixedSizeStack::new(size) {\n'
            '            Ok(stack) => {\n'
            '                log::info!("fiber stack: allocated {size} bytes");\n'
            '                return stack;\n'
            '            }\n'
            '            Err(error) => last = format!("{error}"),\n'
            '        }\n'
            '    }\n'
            '    panic!("failed to allocate fiber stack: {last}");\n'
            '}',
            "加入 allocate_stack 阶梯分配",
        ),
    ],
)


# ---------------------------------------------------------------------------
# 9. renderer_metal：初始化分步日志
# ---------------------------------------------------------------------------
# 真机实测：开机日志停在 "即将 RendererMetal::new"，然后进程直接消失。
# 这里在 4 个子步骤之间插日志，下一次运行就能定位到底哪一步炸的。
edit(
    "src/video_core/src/renderer_metal/renderer_metal.rs",
    [
        (
            "        let device = MetalDevice::new()?;\n"
            "        let layer = unsafe { MetalLayer::from_raw(window_info.render_surface, &device)? };\n"
            "        let presenter = MetalPresenter::new(layer, &device)?;\n"
            "        let mut rasterizer = MetalRasterizer::new(device.clone(), syncpoints, device_memory)?;",
            '        log::info!("[metal] 1/4 创建 MetalDevice");\n'
            "        let device = MetalDevice::new()?;\n"
            '        log::info!("[metal] 2/4 MetalDevice OK: {}", device.name());\n'
            '        log::info!("[metal] 3/4 包装 CAMetalLayer(0x{:x})", window_info.render_surface);\n'
            "        let layer = unsafe { MetalLayer::from_raw(window_info.render_surface, &device)? };\n"
            '        log::info!("[metal] layer OK，创建 presenter");\n'
            "        let presenter = MetalPresenter::new(layer, &device)?;\n"
            '        log::info!("[metal] presenter OK，创建 rasterizer");\n'
            "        let mut rasterizer = MetalRasterizer::new(device.clone(), syncpoints, device_memory)?;\n"
            '        log::info!("[metal] rasterizer OK");',
            "Metal 初始化分步日志",
        ),
    ],
)


# ---------------------------------------------------------------------------
# 10. renderer_metal：iOS 上不能用 macOS 专有的 MTLDevice selector
# ---------------------------------------------------------------------------
# 真机实测（iPhone11,6 / iOS 18.5）：在 MetalDevice::new() 里 SIGABRT，
# 原因是能力查询里用 `objc2::available!(macos = 14.0, ..)` 判断能不能调
# `device.architecture()` —— 而 iOS 上 `available!` 会拿 **iOS 的版本号** 跟
# 14.0 比（18.5 ≥ 14.0 → true），于是真的去调了一个 macOS 专有 selector，
# 抛 unrecognized selector → NSInvalidArgumentException → abort。
#
# 修法：这类判断必须先判平台再判版本。
edit(
    "src/video_core/src/renderer_metal/metal_device.rs",
    [
        (
            "        let supports_metal3_family =\n"
            "            objc2::available!(macos = 13.0, ..) && device.supportsFamily(MTLGPUFamily::Metal3);",
            "        let supports_metal3_family = cfg!(target_os = \"macos\")\n"
            "            && objc2::available!(macos = 13.0, ..)\n"
            "            && device.supportsFamily(MTLGPUFamily::Metal3);",
            "Metal3 family 判定加平台限制",
        ),
        (
            "        let supports_metal4_family =\n"
            "            objc2::available!(macos = 26.0, ..) && device.supportsFamily(MTLGPUFamily::Metal4);",
            "        let supports_metal4_family = cfg!(target_os = \"macos\")\n"
            "            && objc2::available!(macos = 26.0, ..)\n"
            "            && device.supportsFamily(MTLGPUFamily::Metal4);",
            "Metal4 family 判定加平台限制",
        ),
        (
            "            architecture_name: if objc2::available!(macos = 14.0, ..) {\n"
            "                device.architecture().name().to_string()\n"
            "            } else {\n"
            "                device.name().to_string()\n"
            "            },",
            "            // `MTLDevice.architecture()` 是 macOS 14+ 专有；iOS 上调用会\n"
            "            // 抛 unrecognized selector 直接 abort（真机踩过）。\n"
            "            architecture_name: if cfg!(target_os = \"macos\")\n"
            "                && objc2::available!(macos = 14.0, ..)\n"
            "            {\n"
            "                device.architecture().name().to_string()\n"
            "            } else {\n"
            "                device.name().to_string()\n"
            "            },",
            "architecture() 只在 macOS 上调用",
        ),
    ],
)


# ---------------------------------------------------------------------------
# 11. renderer_metal：MSL 版本与 mathMode 必须按「所在平台」的版本判断
# ---------------------------------------------------------------------------
# `objc2::available!(macos = X, ..)` 在 iOS 上会拿 **iOS 版本号** 和 X 比。
# 后果不是崩溃就是选错版本：
#   * select_msl_language_version() 在 iOS 18.5 上会选 V3_2（因为 18.5 >= 15）
#     —— 恰好 iOS 18 支持，但换到 iOS 16 设备就会选中根本不支持的语言版本，
#     着色器编译必然失败；
#   * setMathMode(MTLMathMode::Safe) 的门控同理（该属性 iOS 18+ 才有，
#     iOS 15~17 调用会 unrecognized selector → abort）。
# 修法：显式增加 ios = 版本 的条件（两个平台各自比较自己的版本号）。
edit(
    "src/video_core/src/renderer_metal/metal_device.rs",
    [
        (
            "fn select_msl_language_version(is_apple_silicon: bool) -> MslVersion {",
            "fn select_msl_language_version(is_apple_silicon: bool) -> MslVersion {\n"
            "    // iOS：按 iOS 运行时版本选择（A 系列 GPU 上 MSL 3.x 均受支持）。\n"
            "    #[cfg(target_os = \"ios\")]\n"
            "    {\n"
            "        let _ = is_apple_silicon;\n"
            "        return if objc2::available!(ios = 18.0, ..) {\n"
            "            MslVersion::V3_2\n"
            "        } else if objc2::available!(ios = 17.0, ..) {\n"
            "            MslVersion::V3_1\n"
            "        } else if objc2::available!(ios = 16.0, ..) {\n"
            "            MslVersion::V3_0\n"
            "        } else if objc2::available!(ios = 15.0, ..) {\n"
            "            MslVersion::V2_4\n"
            "        } else {\n"
            "            MslVersion::V2_3\n"
            "        };\n"
            "    }\n"
            "    #[cfg(not(target_os = \"ios\"))]\n"
            "    select_msl_language_version_macos(is_apple_silicon)\n"
            "}\n"
            "\n"
            "#[cfg(not(target_os = \"ios\"))]\n"
            "fn select_msl_language_version_macos(is_apple_silicon: bool) -> MslVersion {",
            "MSL 版本按平台选择（iOS 分支）",
        ),
    ],
)

edit(
    "src/video_core/src/renderer_metal/metal_shader.rs",
    [
        (
            "    if objc2::available!(macos = 15.0, ..) {\n"
            "        compile_options.setMathMode(MTLMathMode::Safe);",
            "    if objc2::available!(macos = 15.0, ios = 18.0, ..) {\n"
            "        compile_options.setMathMode(MTLMathMode::Safe);",
            "mathMode 门控加 ios 版本条件",
        ),
    ],
)

edit(
    "src/video_core/src/renderer_metal/metal_vertex_pulling.rs",
    [
        (
            "            if objc2::available!(macos = 15.0, ..) {\n"
            "                options.setMathMode(MTLMathMode::Safe);",
            "            if objc2::available!(macos = 15.0, ios = 18.0, ..) {\n"
            "                options.setMathMode(MTLMathMode::Safe);",
            "vertex pulling 的 mathMode 门控加 ios 条件",
        ),
    ],
)


# ---------------------------------------------------------------------------
# 12. renderer_metal：MetalDeviceProfile::query 分段日志
# ---------------------------------------------------------------------------
# 修掉 architecture() 之后仍在同一个 SIGABRT 崩（pc 相同 = objc 异常 abort），
# 说明 query() 里还有 macOS 专有 selector。逐段打日志，一次定位到具体调用。
edit(
    "src/video_core/src/renderer_metal/metal_device.rs",
    [
        (
            "        let highest_apple_family = [",
            '        log::info!("[metal] query 1/6 supportsFamily 阶梯");\n'
            "        let highest_apple_family = [",
            "query 检查点 1",
        ),
        (
            "        let max_threads = device.maxThreadsPerThreadgroup();",
            '        log::info!("[metal] query 2/6 maxThreadsPerThreadgroup");\n'
            "        let max_threads = device.maxThreadsPerThreadgroup();",
            "query 检查点 2",
        ),
        (
            "        let supports_mac2_family = device.supportsFamily(MTLGPUFamily::Mac2);",
            '        log::info!("[metal] query 3/6 mac2/metal3/metal4 family");\n'
            "        let supports_mac2_family = device.supportsFamily(MTLGPUFamily::Mac2);",
            "query 检查点 3",
        ),
        (
            "        let msl_language_version = select_msl_language_version(highest_apple_family.is_some());",
            '        log::info!("[metal] query 4/6 选择 MSL 版本");\n'
            "        let msl_language_version = select_msl_language_version(highest_apple_family.is_some());",
            "query 检查点 4",
        ),
        (
            "        let family_limits = family_limits(highest_apple_family);",
            '        log::info!("[metal] query 5/6 直接查询设备上限");\n'
            "        let family_limits = family_limits(highest_apple_family);",
            "query 检查点 5",
        ),
    ],
)

# ---------------------------------------------------------------------------
# 13. renderer_metal：iOS 上不要查询 BC 压缩纹理支持
# ---------------------------------------------------------------------------
# iOS 的 GPU 从不支持 BC（那是 macOS/桌面显卡的格式），而这个 selector 在
# 部分 SDK 上属于 macOS 专有 —— 在 iOS 上调用同样会抛 unrecognized selector。
# 直接按平台判定为 false，既正确又避免风险。
edit(
    "src/video_core/src/renderer_metal/metal_device.rs",
    [
        (
            "            supports_bc_texture_compression: device.supportsBCTextureCompression(),",
            "            // iOS GPU 不支持 BC 压缩纹理（桌面格式），也避免调用 macOS 专有 selector\n"
            "            supports_bc_texture_compression: cfg!(target_os = \"macos\")\n"
            "                && device.supportsBCTextureCompression(),",
            "supportsBCTextureCompression 在 iOS 上直接为 false",
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
