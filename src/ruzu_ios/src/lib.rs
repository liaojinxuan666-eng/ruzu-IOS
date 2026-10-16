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

/// 直接向内核问「我有没有 JIT 权限」，而不是靠 mmap 试错。
///
/// 这是这一版新增的关键诊断：之前只能看到 `mmap(MAP_JIT) errno=22`，
/// 现在能明确区分三种原因 ——
///   * 没被调试器 attach（无 CS_DEBUGGED）→ 用 StikDebug / debugserver 挂一下
///   * entitlements 里没有 dynamic-codesigning → 重新签名时带上
///   * 两者都有却还是失败 → 那就是内核策略问题
fn codesign_report() -> String {
    let mut out = String::new();
    unsafe {
        let mut flags: u32 = 0;
        if csops(CS_OPS_STATUS, &mut flags as *mut u32 as *mut c_void, 4) == 0 {
            let tag = if flags & CS_DEBUGGED != 0 {
                "有 CS_DEBUGGED（调试器已 attach → 具备 JIT 资格）"
            } else {
                "无 CS_DEBUGGED（没有调试器 attach）"
            };
            writeln!(out, "代码签名标志: 0x{flags:08x} —— {tag}").unwrap();
        } else {
            writeln!(out, "csops(STATUS) 失败 errno={}（被沙盒拦了？）", *libc::__error()).unwrap();
        }

        let mut buf = vec![0u8; 16 * 1024];
        if csops(CS_OPS_ENTITLEMENTS_BLOB, buf.as_mut_ptr() as *mut c_void, buf.len()) == 0 {
            let text = String::from_utf8_lossy(&buf);
            let has = |k: &str| if text.contains(k) { "有" } else { "无" };
            writeln!(
                out,
                "entitlements: dynamic-codesigning={} | get-task-allow={} | increased-memory-limit={} | extended-virtual-addressing={}",
                has("dynamic-codesigning"),
                has("get-task-allow"),
                has("increased-memory-limit"),
                has("extended-virtual-addressing"),
            )
            .unwrap();
        } else {
            writeln!(out, "csops(ENTITLEMENTS_BLOB) 失败 errno={}", *libc::__error()).unwrap();
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

/// 官方路径：MAP_JIT + `pthread_jit_write_protect_np`。安全入口。
fn probe_official() -> String {
    let mut out = String::new();
    let len = 4096usize;
    let rwx = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
    unsafe {
        match mmap_region(len, rwx, true) {
            Ok(p) => {
                match write_code(p, len) {
                    Ok(()) => {
                        let r = call_code(p);
                        if r == PROBE_EXPECT {
                            writeln!(out, "[OK]  MAP_JIT + 写保护：{}", describe(Ok(r))).unwrap();
                        } else {
                            writeln!(out, "[??]  MAP_JIT + 写保护：{}", describe(Ok(r))).unwrap();
                        }
                    }
                    Err(e) => writeln!(out, "[FAIL] MAP_JIT 页写入失败：{e}").unwrap(),
                }
                libc::munmap(p, len);
            }
            Err(e) => writeln!(
                out,
                "[FAIL] mmap(MAP_JIT) 失败 errno={e} —— 没有 JIT 权限。\n\
                 errno 12(ENOMEM)/1(EPERM) 都可能出现：未越狱时需先用 StikDebug 之类挂上调试器再启动本 App。"
            )
            .unwrap(),
        }
    }
    out
}

/// 能不能 mmap(MAP_JIT) —— 最直接的"有没有 JIT 权限"判据。
fn has_jit_permission() -> bool {
    let len = 4096usize;
    let rwx = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
    unsafe {
        match mmap_region(len, rwx, true) {
            Ok(p) => {
                libc::munmap(p, len);
                true
            }
            Err(_) => false,
        }
    }
}

/// 越狱路径：裸 RWX、以及 RW → mprotect(RX)。**可能闪退**，故单独入口。
fn probe_unsigned() -> String {
    let mut out = String::new();
    let len = 4096usize;
    let rw = libc::PROT_READ | libc::PROT_WRITE;
    let rwx = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;

    // 2026-10-17 真机实测：无 JIT 权限时「裸 RWX」能 mmap 成功，但一执行就
    // EXC_BAD_ACCESS / SIGBUS / KERN_PROTECTION_FAILURE（内核拒绝执行该页）。
    // 既然已经知道结果，就没必要再拿你的 App 冒一次闪退的风险。
    if !has_jit_permission() {
        out.push_str(
            "[跳过] 未检测到 JIT 权限，故不再尝试执行这两条路径。\n\
             原因：无权限时「裸 RWX」映射会成功但执行必然 SIGBUS（真机已实测）。\n\
             请先按上面「代码签名 / JIT 资格」一节的结论把 JIT 开起来，再回来点这个按钮。\n",
        );
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
    writeln!(s, "\n=== JIT（官方路径）===").unwrap();
    writeln!(s, "{}", jitmem::status_line()).unwrap();
    let jit = probe_official();
    s.push_str(&jit);
    writeln!(s, "\n=== 客户机内存预留（PROT_NONE + MAP_NORESERVE）===").unwrap();
    s.push_str(&probe_reservation());
    writeln!(s, "\n=== 判定 ===").unwrap();
    if jit.contains("[OK]") {
        writeln!(s, "→ JIT 可用：rdynarmic 可以在这台设备上工作。").unwrap();
    } else {
        writeln!(
            s,
            "→ 官方 JIT 路径不可用。若已越狱，点「越狱路径探测」再试；\n   否则请用 StikDebug / SideStore 之类先给本 App 开 JIT。"
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
