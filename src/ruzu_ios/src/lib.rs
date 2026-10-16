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

/// Darwin 的 `MAP_JIT`。不直接用 libc 常量，避免不同 libc 版本差异。
const MAP_JIT: c_int = 0x0800;
/// Darwin 的 `MAP_NORESERVE`。
const MAP_NORESERVE: c_int = 0x0040;

/// `mov w0, #42; ret` —— 探针用的机器码。
/// movz w0, #42 = 0x5280_0000 | (42 << 5)；ret = 0xD65F_03C0
const PROBE_CODE: [u32; 2] = [0x5280_0000 | (42u32 << 5), 0xD65F_03C0];
const PROBE_EXPECT: u32 = 42;

extern "C" {
    /// iOS 14+ / macOS 11+。开写保护 = JIT 页可执行不可写；关 = 可写不可执行。
    /// 这是 Apple Silicon 上 W^X 的官方逃逸口。
    fn pthread_jit_write_protect_np(enabled: c_int);
    /// 写完机器码必须刷 I-cache，否则 CPU 执行的还是旧内容。
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
    fn sysctlbyname(
        name: *const c_char,
        oldp: *mut c_void,
        oldlenp: *mut usize,
        newp: *mut c_void,
        newlen: usize,
    ) -> c_int;
    /// 本进程在触发 jetsam 之前还能用的字节数（iOS 13+）。
    fn os_proc_available_memory() -> usize;
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

/// 写入探针机器码。`toggle_write_protect` 为真时先关写保护、写完再打开。
unsafe fn write_code(p: *mut c_void, toggle_write_protect: bool) {
    if toggle_write_protect {
        pthread_jit_write_protect_np(0);
    }
    std::ptr::copy_nonoverlapping(PROBE_CODE.as_ptr() as *const u8, p as *mut u8, 8);
    if toggle_write_protect {
        pthread_jit_write_protect_np(1);
    }
    sys_icache_invalidate(p, 8);
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
                write_code(p, true);
                let r = call_code(p);
                if r == PROBE_EXPECT {
                    writeln!(out, "[OK]  MAP_JIT + 写保护：{}", describe(Ok(r))).unwrap();
                } else {
                    writeln!(out, "[??]  MAP_JIT + 写保护：{}", describe(Ok(r))).unwrap();
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

/// 越狱路径：裸 RWX、以及 RW → mprotect(RX)。**可能闪退**，故单独入口。
fn probe_unsigned() -> String {
    let mut out = String::new();
    let len = 4096usize;
    let rw = libc::PROT_READ | libc::PROT_WRITE;
    let rwx = libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC;
    unsafe {
        // 路径 B：裸 RWX，无 MAP_JIT、无写保护
        match mmap_region(len, rwx, false) {
            Ok(p) => {
                write_code(p, false);
                let r = call_code(p);
                writeln!(out, "[{}] 裸 RWX（无 MAP_JIT）：{}", if r == PROBE_EXPECT { "OK" } else { "??" }, describe(Ok(r))).unwrap();
                libc::munmap(p, len);
            }
            Err(e) => writeln!(out, "[FAIL] mmap(RWX, 无 MAP_JIT) 失败 errno={e}").unwrap(),
        }

        // 路径 C：RW 写入 → mprotect(RX) → 执行
        match mmap_region(len, rw, false) {
            Ok(p) => {
                write_code(p, false);
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
    writeln!(s, "\n=== JIT（官方路径）===").unwrap();
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

#[no_mangle]
pub extern "C" fn ruzu_ios_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}
