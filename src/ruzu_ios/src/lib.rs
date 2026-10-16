// SPDX-License-Identifier: GPL-3.0-or-later
//!
//! ruzu 的 iOS 宿主门面。Swift/UIKit 外壳与 ruzu 核心之间的唯一接触面，
//! 全部是 C ABI。
//!
//! P1 阶段（本文件当前内容）只做三件事，但件件都是「能不能跑起来」的前提：
//!
//! 1. **JIT 能力探针**：真正 mmap 一段可写可执行内存、写一段 aarch64 机器码、
//!    翻转写保护、调用它并检查返回值。这正是 `rdynarmic` → `rhazel::block_of_code`
//!    在 iOS 上必须走通的路径。探针不过 = 模拟器不可能跑起来。
//!    - `ruzu_ios_jit_probe()`：安全，只测 Apple 官方路径（MAP_JIT + 写保护）。
//!    - `ruzu_ios_jit_probe_unsafe()`：越狱路径（裸 RWX、RW→mprotect(RX)）。
//!      没有 JIT 权限时可能直接闪退，所以单独一个入口，由 UI 明确提示后再调。
//! 2. **大块虚拟地址预留探针**：模拟器要给客户机预留数 GiB VA，iOS 上有上限，
//!    这里直接试 16/8/6/4/2/1 GiB 的 PROT_NONE 预留，报告实测结果。
//! 3. **环境报告**：物理内存、本进程内存上限（jetsam）、系统版本。

#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_int, c_void, CString};
use std::fmt::Write as _;

mod jitmem;
mod nce_probe;

/// Darwin 的 `MAP_JIT`。不直接用 libc 常量，避免不同 libc 版本差异。
const MAP_JIT: c_int = 0x0800;
/// Darwin 的 `MAP_NORESERVE`。
const MAP_NORESERVE: c_int = 0x0040;

/// `mov w0, #42; ret` —— 探针用的机器码。
/// movz w0, #42 = 0x5280_0000 | (42 << 5)；ret = 0xD65F_03C0
const PROBE_CODE: [u32; 2] = [0x5280_0000 | (42u32 << 5), 0xD65F_03C0];
const PROBE_EXPECT: u32 = 42;

extern "C" {
    fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
    /// 本进程在触发 jetsam 之前还能用的字节数（iOS 13+）。
    fn os_proc_available_memory() -> usize;
    /// 用来读代码签名状态 / entitlements（csops 是内核接口，只能走 syscall）。
    fn syscall(num: std::ffi::c_long, ...) -> c_int;
}

/// `csops` 的系统调用号（arm64 Darwin）。
const SYS_CSOPS: std::ffi::c_long = 169;
/// 查询代码签名标志位。
const CS_OPS_STATUS: c_int = 0;
/// 取出本进程的 entitlements blob。
const CS_OPS_ENTITLEMENTS_BLOB: c_int = 7;
/// 被调试器 attach 过 —— 有了它的进程才被允许用可执行内存。
const CS_DEBUGGED: u32 = 0x1000_0000;

unsafe fn csops(op: c_int, buf: *mut c_void, size: usize) -> c_int {
    syscall(
        SYS_CSOPS,
        libc::getpid() as std::ffi::c_long,
        op as std::ffi::c_long,
        buf as std::ffi::c_long,
        size as std::ffi::c_long,
    )
}

// ---------------------------------------------------------------- 代码签名查询

unsafe fn csops_status_flags() -> Option<u32> {
    let mut flags: u32 = 0;
    if csops(CS_OPS_STATUS, &mut flags as *mut u32 as *mut c_void, 4) == 0 {
        Some(flags)
    } else {
        None
    }
}

unsafe fn csops_entitlements() -> Option<String> {
    let mut buf = vec![0u8; 32 * 1024];
    let r = csops(CS_OPS_ENTITLEMENTS_BLOB, buf.as_mut_ptr() as *mut c_void, buf.len());
    if r < 0 {
        return None;
    }
    let mut len = if r > 0 { (r as usize).min(buf.len()) } else { buf.len() };
    while len > 0 && buf[len - 1] == 0 {
        len -= 1;
    }
    Some(String::from_utf8_lossy(&buf[..len]).into_owned())
}

/// 本进程到底有没有 JIT 资格 —— 纯靠代码签名状态判断，不做任何危险操作。
///
/// 这就是「正确的检测方式」：**不要拿 `mmap(MAP_JIT)` 当判据**。
/// 真机实测（iPhone11,6 / iOS 18.5）：即使 entitlements 齐全且 CS_DEBUGGED 置位，
/// `mmap(MAP_JIT)` 依然 `errno=13 (EACCES)` —— 因为 **MAP_JIT 是 macOS 专有机制，
/// iOS 内核不接受**。iOS 走的是「entitlements 授权 + 普通 RWX 匿名映射」。
fn jit_qualification() -> (bool, bool, bool) {
    // 返回 (是否具备 JIT 资格, 是否 CS_DEBUGGED, 是否有 dynamic-codesigning)
    unsafe {
        let debugged = csops_status_flags().map_or(false, |f| f & CS_DEBUGGED != 0);
        let blob = csops_entitlements().unwrap_or_default();
        let dyn_codesign = blob.contains("dynamic-codesigning");
        let allow_jit = blob.contains("com.apple.security.cs.allow-jit")
            || blob.contains("allow-unsigned-executable-memory");
        (debugged || dyn_codesign || allow_jit, debugged, dyn_codesign)
    }
}

/// 直接向内核问「我有没有 JIT 权限」，而不是靠 mmap 试错。
///
/// 输出三块：
///   1. `csops(CS_OPS_STATUS)` 的代码签名标志（含 CS_DEBUGGED 位解码）
///   2. `csops(CS_OPS_ENTITLEMENTS_BLOB)` 的 entitlements 原文（找关键键 + 报格式）
///   3. 四种「拿可执行内存」策略逐一实测，各报自己的 errno
///
/// 注意：策略 A/B 在 iOS 上**注定失败**（MAP_JIT 不被接受），它们存在只是为了
/// 证明这一点；iOS 的可用路径是 D（普通 RWX 匿名映射）。
fn codesign_report() -> String {
    let mut out = String::new();
    unsafe {
        // ---------- 1. 代码签名标志 ----------
        match csops_status_flags() {
            Some(flags) => {
                writeln!(out, "代码签名标志: 0x{flags:08x}").unwrap();
                // 位含义取自 XNU cs_blobs.h，只解码几个与 JIT 相关的
                let bits = [
                    (0x0000_0100u32, "CS_HARD"),
                    (0x0000_0200, "CS_KILL"),
                    (0x0000_0800, "CS_RESTRICT"),
                    (0x0000_2000, "CS_REQUIRE_LV"),
                    (0x0400_0000, "CS_PLATFORM_BINARY"),
                    (0x1000_0000, "CS_DEBUGGED"),
                ];
                let mut names = Vec::new();
                for (bit, name) in bits {
                    if flags & bit != 0 {
                        names.push(name);
                    }
                }
                writeln!(
                    out,
                    "  置位: {}{}",
                    if names.is_empty() { "（无已知位）".to_string() } else { names.join(" ") },
                    if flags & CS_DEBUGGED != 0 { "  ← 有 JIT 资格" } else { "" }
                )
                .unwrap();
            }
            None => writeln!(out, "csops(STATUS) 失败 errno={}（沙盒拦了 syscall？）", *libc::__error()).unwrap(),
        }

        // ---------- 2. entitlements 原文 ----------
        match csops_entitlements() {
            Some(text) => {
                let kind = if text.contains("<?xml") || text.contains("<plist") {
                    "XML plist"
                } else if text.trim().is_empty() {
                    "空（本 App 没有任何 entitlements！）"
                } else {
                    "非 XML（可能是 DER 二进制）"
                };
                writeln!(out, "entitlements: {} 字节，格式={kind}", text.len()).unwrap();
                let has = |k: &str| if text.contains(k) { "有" } else { "无" };
                writeln!(
                    out,
                    "  dynamic-codesigning={} | get-task-allow={} | increased-memory-limit={} | extended-virtual-addressing={}",
                    has("dynamic-codesigning"),
                    has("get-task-allow"),
                    has("increased-memory-limit"),
                    has("extended-virtual-addressing"),
                )
                .unwrap();
            }
            None => writeln!(out, "csops(ENTITLEMENTS_BLOB) 失败 errno={}", *libc::__error()).unwrap(),
        }

        // ---------- 3. 四种拿可执行内存的策略 ----------
        let page = 64 * 1024usize;
        let rw = libc::PROT_READ | libc::PROT_WRITE;
        let rwx = rw | libc::PROT_EXEC;

        writeln!(out, "可执行内存策略实测:").unwrap();

        // A：RWX + MAP_JIT（macOS 官方姿势，iOS 上注定 EINVAL/EACCES）
        match mmap_region(page, rwx, true) {
            Ok(p) => {
                writeln!(out, "  A. RWX + MAP_JIT（macOS 姿势） → [OK] @ {p:p}").unwrap();
                libc::munmap(p, page);
            }
            Err(e) => writeln!(out, "  A. RWX + MAP_JIT（macOS 姿势） → errno={e}（iOS 上正常）").unwrap(),
        }

        // B：RW + MAP_JIT
        match mmap_region(page, rw, true) {
            Ok(p) => {
                writeln!(out, "  B. RW  + MAP_JIT              → [OK] @ {p:p}").unwrap();
                if libc::mprotect(p, page, rwx) == 0 {
                    writeln!(out, "     └ 追加 mprotect(RWX)       → [OK]").unwrap();
                } else {
                    writeln!(out, "     └ 追加 mprotect(RWX)       → errno={}", *libc::__error()).unwrap();
                }
                libc::munmap(p, page);
            }
            Err(e) => writeln!(out, "  B. RW  + MAP_JIT              → errno={e}（iOS 上正常）").unwrap(),
        }

        // C：RW（无 MAP_JIT）→ mprotect(RX)
        match mmap_region(page, rw, false) {
            Ok(p) => {
                let r2 = libc::mprotect(p, page, libc::PROT_READ | libc::PROT_EXEC);
                writeln!(
                    out,
                    "  C. RW → mprotect(RX)          → {}",
                    if r2 == 0 { "[OK]".to_string() } else { format!("errno={}", *libc::__error()) }
                )
                .unwrap();
                libc::munmap(p, page);
            }
            Err(e) => writeln!(out, "  C. RW（无 MAP_JIT）           → errno={e}").unwrap(),
        }

        // D：RWX（无 MAP_JIT）—— **iOS 的正确姿势**
        match mmap_region(page, rwx, false) {
            Ok(p) => {
                writeln!(out, "  D. RWX（无 MAP_JIT）          → [OK] ← iOS 的可用路径").unwrap();
                libc::munmap(p, page);
            }
            Err(e) => writeln!(out, "  D. RWX（无 MAP_JIT）          → errno={e}").unwrap(),
        }
    }
    out
}

// ---------------------------------------------------------------- 内部工具

fn to_c_string(s: String) -> *mut c_char {
    CString::new(s.replace('\0', "")).unwrap_or_default().into_raw()
}

unsafe fn sysctl_string(name: &str) -> Option<String> {
    let cname = CString::new(name).ok()?;
    let mut len: usize = 0;
    if sysctlbyname(cname.as_ptr(), std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) != 0 {
        return None;
    }
    let mut buf = vec![0u8; len];
    if sysctlbyname(
        cname.as_ptr(),
        buf.as_mut_ptr() as *mut c_void,
        &mut len,
        std::ptr::null_mut(),
        0,
    ) != 0
    {
        return None;
    }
    buf.truncate(len);
    while buf.last() == Some(&0) {
        buf.pop();
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

unsafe fn sysctl_u64(name: &str) -> Option<u64> {
    let cname = CString::new(name).ok()?;
    let mut value: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    if sysctlbyname(
        cname.as_ptr(),
        &mut value as *mut u64 as *mut c_void,
        &mut len,
        std::ptr::null_mut(),
        0,
    ) != 0
    {
        return None;
    }
    Some(value)
}

fn human(bytes: u64) -> String {
    const G: f64 = 1024.0 * 1024.0 * 1024.0;
    const M: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= G {
        format!("{:.2} GiB", b / G)
    } else if b >= M {
        format!("{:.1} MiB", b / M)
    } else {
        format!("{bytes} B")
    }
}

// ---------------------------------------------------------------- JIT 探针

unsafe fn mmap_region(len: usize, prot: c_int, map_jit: bool) -> Result<*mut c_void, i32> {
    let mut flags = libc::MAP_PRIVATE | libc::MAP_ANON;
    if map_jit {
        flags |= MAP_JIT;
    }
    let p = libc::mmap(std::ptr::null_mut(), len, prot, flags, -1, 0);
    if p == libc::MAP_FAILED {
        Err(*libc::__error())
    } else {
        Ok(p)
    }
}

/// 写入探针机器码。写保护切换（或 mprotect 降级）都在 `jitmem` 里统一处理。
unsafe fn write_code(p: *mut c_void, len: usize) -> Result<(), String> {
    jitmem::write_code(p, len, &PROBE_CODE)
}

/// 执行已写入的探针机器码。**这是唯一可能因无权限而触发 EXC_BAD_ACCESS 的地方。**
unsafe fn call_code(p: *mut c_void) -> u32 {
    let f: extern "C" fn() -> u32 = std::mem::transmute(p);
    f()
}

fn describe(result: Result<u32, i32>) -> String {
    match result {
        Ok(PROBE_EXPECT) => format!("机器码已执行，返回 {PROBE_EXPECT}"),
        Ok(v) => format!("执行了但返回 {v}（期望 {PROBE_EXPECT}）"),
        Err(e) => format!("mmap 失败 errno={e}"),
    }
}

/// 官方路径探针。
///
/// **真机实测结论（iPhone11,6 / iOS 18.5，2026-10-17）**：
/// * macOS(arm64) 的姿势是 `mmap(MAP_JIT)` + `pthread_jit_write_protect_np` 翻转；
/// * **iOS 根本不接受 MAP_JIT**（RWX+MAP_JIT → EINVAL，RW+MAP_JIT → EACCES），
///   iOS SDK 里也没有 `pthread_jit_write_protect_np` 这个符号；
/// * iOS 的正确姿势是：**普通 RWX 匿名映射**，可执行性由代码签名授权
///   （`dynamic-codesigning` entitlement / `CS_DEBUGGED`）。
///
/// 所以这里先按 iOS 姿势来，并且**执行前先查代码签名资格**：没资格就不执行，
/// 免得拿你的 App 冒闪退风险（无授权时执行 RWX 页必然 SIGBUS，已实测）。
fn probe_official() -> String {
    let mut out = String::new();
    let len = 4096usize;
    let rw = libc::PROT_READ | libc::PROT_WRITE;
    let rwx = rw | libc::PROT_EXEC;
    let (jit_ok, debugged, dyn_codesign) = jit_qualification();

    unsafe {
        // ---------- 路径 1：iOS 姿势（普通 RWX 匿名映射） ----------
        match mmap_region(len, rwx, false) {
            Ok(p) => {
                writeln!(out, "[OK]  mmap(RWX，不带 MAP_JIT) 成功 @ {p:p}").unwrap();
                if jit_ok {
                    writeln!(
                        out,
                        "[OK]  代码签名已授权 JIT（CS_DEBUGGED={debugged} / dynamic-codesigning={dyn_codesign}），开始执行测试"
                    )
                    .unwrap();
                    jitmem::write_code_raw(p, &PROBE_CODE);
                    let r = call_code(p);
                    if r == PROBE_EXPECT {
                        writeln!(out, "[OK]  ★ 机器码真的执行了（返回 {r}）—— iOS 的 JIT 通路就是这条").unwrap();
                    } else {
                        writeln!(out, "[??]  执行了但返回 {r}（期望 {PROBE_EXPECT}）").unwrap();
                    }
                } else {
                    writeln!(
                        out,
                        "[SKIP] 代码签名未授权 JIT（CS_DEBUGGED={debugged} / dynamic-codesigning={dyn_codesign}），跳过执行\n\
                         　　　无授权时执行可执行页必然 SIGBUS（已实测），不拿你的 App 冒险"
                    )
                    .unwrap();
                }
                libc::munmap(p, len);
            }
            Err(e) => writeln!(out, "[FAIL] mmap(RWX，不带 MAP_JIT) 失败 errno={e}").unwrap(),
        }

        // ---------- 路径 2：macOS 姿势（MAP_JIT），只作对照 ----------
        writeln!(out, "对照（macOS 的 MAP_JIT 姿势在 iOS 上）：").unwrap();
        for (label, prot) in [("RW  + MAP_JIT", rw), ("RWX + MAP_JIT", rwx)] {
            match mmap_region(len, prot, true) {
                Ok(p) => {
                    writeln!(out, "  {label} → [OK]（意外，iOS 上通常失败）").unwrap();
                    libc::munmap(p, len);
                }
                Err(e) => writeln!(out, "  {label} → errno={e}（符合预期：iOS 不支持 MAP_JIT）").unwrap(),
            }
        }
    }
    out
}

/// 越狱路径：裸 RWX、以及 RW → mprotect(RX)。**可能闪退**，故单独入口。
fn probe_unsigned() -> String {
    let mut out = String::new();
    let len = 4096usize;
    let rw = libc::PROT_READ | libc::PROT_WRITE;
    let rwx = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;

    // 判据改成「代码签名资格」，而不是 mmap(MAP_JIT) ——
    // 后者在 iOS 上永远失败，用它当判据会让这个按钮永远跳过。
    let (jit_ok, debugged, dyn_codesign) = jit_qualification();
    if !jit_ok {
        out.push_str(&format!(
            "[跳过] 代码签名未授权 JIT（CS_DEBUGGED={debugged} / dynamic-codesigning={dyn_codesign}）。\n\
             无授权时执行可执行页必然 SIGBUS（真机已实测），所以不再尝试。\n\
             请先把 JIT 权限搞定，再回来点这个按钮。\n"
        ));
        return out;
    }
    unsafe {
        // 路径 B：裸 RWX，无 MAP_JIT、无写保护（这条就是"直接硬写"，越狱环境才有）
        match mmap_region(len, rwx, false) {
            Ok(p) => {
                jitmem::write_code_raw(p, &PROBE_CODE);
                let r = call_code(p);
                writeln!(out, "[{}] 裸 RWX（无 MAP_JIT）：{}", if r == PROBE_EXPECT { "OK" } else { "??" }, describe(Ok(r))).unwrap();
                libc::munmap(p, len);
            }
            Err(e) => writeln!(out, "[FAIL] mmap(RWX, 无 MAP_JIT) 失败 errno={e}").unwrap(),
        }

        // 路径 C：RW 写入 → mprotect(RX) → 执行
        match mmap_region(len, rw, false) {
            Ok(p) => {
                jitmem::write_code_raw(p, &PROBE_CODE);
                if libc::mprotect(p, len, libc::PROT_READ | libc::PROT_EXEC) == 0 {
                    let r = call_code(p);
                    writeln!(out, "[{}] RW → mprotect(RX) → 执行：{}", if r == PROBE_EXPECT { "OK" } else { "??" }, describe(Ok(r))).unwrap();
                } else {
                    writeln!(out, "[FAIL] mprotect(PROT_EXEC) 失败 errno={}", *libc::__error()).unwrap();
                }
                libc::munmap(p, len);
            }
            Err(e) => writeln!(out, "[FAIL] mmap(RW) 失败 errno={e}").unwrap(),
        }
    }
    out
}

/// 试各种尺寸的 PROT_NONE 预留，看 iOS 允许多大的客户机 VA。
fn probe_reservation() -> String {
    let mut out = String::new();
    unsafe {
        for gib in [16u64, 8, 6, 4, 2, 1] {
            let len = (gib as usize) * 1024 * 1024 * 1024;
            let flags = libc::MAP_PRIVATE | libc::MAP_ANON | MAP_NORESERVE;
            let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_NONE, flags, -1, 0);
            if p == libc::MAP_FAILED {
                writeln!(out, "[FAIL] {gib} GiB 预留失败 errno={}", *libc::__error()).unwrap();
            } else {
                writeln!(out, "[OK]   {gib} GiB 预留成功 @ {p:p}").unwrap();
                libc::munmap(p, len);
            }
        }
    }
    out
}

// ---------------------------------------------------------------- 环境报告

fn environment_report() -> String {
    let mut out = String::new();
    unsafe {
        let model = sysctl_string("hw.machine").unwrap_or_else(|| "?".into());
        let osver = sysctl_string("kern.osversion").unwrap_or_else(|| "?".into());
        let memsize = sysctl_u64("hw.memsize").unwrap_or(0);
        let avail = os_proc_available_memory();
        writeln!(out, "机型: {model}").unwrap();
        writeln!(out, "系统: {osver}").unwrap();
        writeln!(out, "物理内存: {}", human(memsize)).unwrap();
        writeln!(out, "本进程可用内存上限（jetsam）: {}", human(avail as u64)).unwrap();
        if memsize > 0 && avail > 0 {
            writeln!(out, "上限 / 物理 = {:.1}%", 100.0 * avail as f64 / memsize as f64).unwrap();
        }
    }
    out
}

// ---------------------------------------------------------------- C ABI

#[no_mangle]
pub extern "C" fn ruzu_ios_version() -> *mut c_char {
    to_c_string(format!(
        "ruzu_ios {} ({} 位)",
        env!("CARGO_PKG_VERSION"),
        std::mem::size_of::<usize>() * 8
    ))
}

/// 环境 + JIT（官方路径）+ VA 预留 三合一报告。真机上跑一次就能定方案。
#[no_mangle]
pub extern "C" fn ruzu_ios_diagnostics() -> *mut c_char {
    let mut s = String::new();
    writeln!(s, "=== 运行环境 ===").unwrap();
    s.push_str(&environment_report());
    writeln!(s, "\n=== 代码签名 / JIT 资格 ===").unwrap();
    s.push_str(&codesign_report());
    writeln!(s, "\n=== JIT 通路 ===").unwrap();
    writeln!(s, "（macOS：MAP_JIT + 写保护翻转 ｜ iOS：普通 RWX + 代码签名授权）").unwrap();
    writeln!(s, "{}", jitmem::status_line()).unwrap();
    let jit = probe_official();
    s.push_str(&jit);
    writeln!(s, "\n=== 客户机内存预留（PROT_NONE + MAP_NORESERVE）===").unwrap();
    s.push_str(&probe_reservation());
    writeln!(s, "\n=== 判定 ===").unwrap();
    let (jit_ok, debugged, dyn_codesign) = jit_qualification();
    if jit.contains('★') {
        writeln!(s, "→ JIT 通路已打通：机器码真的执行了。地基全齐，可以开始接 ruzu 核心了。").unwrap();
    } else if jit_ok {
        writeln!(
            s,
            "→ 代码签名已授权 JIT（CS_DEBUGGED={debugged} / dynamic-codesigning={dyn_codesign}），\n\
             　 但执行测试没成功 —— 把上面「JIT 通路」一节的原文发出来定位。"
        )
        .unwrap();
    } else {
        writeln!(
            s,
            "→ 还没有 JIT 权限（CS_DEBUGGED={debugged} / dynamic-codesigning={dyn_codesign}）。\n\
             　 越狱/TrollStore 环境：用带 entitlements 的 IPA，或在设备上用 ldid 重签（见 README）。"
        )
        .unwrap();
    }
    to_c_string(s)
}

/// 只测官方 JIT 路径，安全。
#[no_mangle]
pub extern "C" fn ruzu_ios_jit_probe() -> *mut c_char {
    to_c_string(probe_official())
}

/// 越狱路径探测，**可能闪退**。
#[no_mangle]
pub extern "C" fn ruzu_ios_jit_probe_unsafe() -> *mut c_char {
    to_c_string(probe_unsigned())
}

/// NCE（原生执行）可行性预检。
///
/// 会做四件有副作用的事：MAP_FIXED 叠加 MAP_JIT 页、自修改代码、多线程独占
/// 监视器压力测试、注册 SIGSEGV 处理器并从信号中恢复。因此可能闪退，UI 上
/// 必须明确提示后再调。
#[no_mangle]
pub extern "C" fn ruzu_ios_nce_precheck() -> *mut c_char {
    to_c_string(nce_probe::precheck())
}

#[no_mangle]
pub extern "C" fn ruzu_ios_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}
