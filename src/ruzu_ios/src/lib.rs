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

mod core_link;
#[cfg(feature = "core")]
mod boot;
#[cfg(feature = "core")]
mod emu_window_ios;
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

// 这些是同一 App 里用 C 实现的（ios/RuzuApp/JITEnable.c）——
// Mach / 代码签名相关的结构体布局只有跟着 SDK 头文件走才不会错。
extern "C" {
    fn ruzu_cs_flags(out: *mut u32) -> c_int;
    fn ruzu_page_protection(addr: *mut c_void, len: usize) -> c_int;
    fn ruzu_prot_to_string(prot: c_int, out: *mut c_char, out_len: usize);
    fn ruzu_try_self_trace(why: *mut c_char, why_len: usize) -> c_int;
}

/// 把权限位掩码转成 "RWX"/"R-X" 这样的人话。
fn prot_str(prot: c_int) -> String {
    let mut buf = [0 as c_char; 8];
    unsafe {
        ruzu_prot_to_string(prot, buf.as_mut_ptr(), buf.len());
        std::ffi::CStr::from_ptr(buf.as_ptr())
    }
    .to_string_lossy()
    .into_owned()
}

/// `csops` 的系统调用号（arm64 Darwin）。
const SYS_CSOPS: std::ffi::c_long = 169;
/// 查询代码签名标志位。
const CS_OPS_STATUS: c_int = 0;
/// 取出本进程的 entitlements blob。
const CS_OPS_ENTITLEMENTS_BLOB: c_int = 7;
/// 被调试器 attach 过 —— 有了它的进程才被允许用可执行内存。
const CS_DEBUGGED: u32 = 0x1000_0000;
/// 签名里带 get-task-allow（**不等于**"正在被调试"，别拿它当 JIT 判据）。
const CS_GET_TASK_ALLOW: u32 = 0x0000_0004;

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

/// 代码签名给出的「线索」。注意：**这些位不能直接当 JIT 判据**。
///
/// 真机实测（iPhone11,6 / iOS 18.5）：`CS_DEBUGGED` 位是置上的（来自签名里的
/// get-task-allow），但可执行内存依然拿不到 —— 内核在 mmap/mprotect 时会把
/// EXEC 位剥掉。所以判据只能是「读页面真实权限」，见 `probe_official`。
///
/// 返回 (是否 CS_DEBUGGED, 是否带 get-task-allow, 是否有 dynamic-codesigning)
fn codesign_hints() -> (bool, bool, bool) {
    unsafe {
        let flags = csops_status_flags().unwrap_or(0);
        let debugged = flags & CS_DEBUGGED != 0;
        let get_task_allow = flags & CS_GET_TASK_ALLOW != 0;
        let blob = csops_entitlements().unwrap_or_default();
        (debugged, get_task_allow, blob.contains("dynamic-codesigning"))
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
                    (CS_GET_TASK_ALLOW, "CS_GET_TASK_ALLOW(签名带 get-task-allow)"),
                    (0x0000_0100u32, "CS_HARD"),
                    (0x0000_0200, "CS_KILL"),
                    (0x0000_0800, "CS_RESTRICT"),
                    (0x0000_2000, "CS_REQUIRE_LV"),
                    (0x0400_0000, "CS_PLATFORM_BINARY"),
                    (CS_DEBUGGED, "CS_DEBUGGED(运行时被调试过)"),
                ];
                let mut names = Vec::new();
                for (bit, name) in bits {
                    if flags & bit != 0 {
                        names.push(name);
                    }
                }
                writeln!(out, "  置位: {}", if names.is_empty() { "（无已知位）".to_string() } else { names.join(" ") }).unwrap();
                writeln!(out, "  提示: 上面这些位**不足以判定 JIT 可用性** —— 真机上它们全置位，\n        可执行内存照样拿不到。真正的判据看下面「真实页面权限」。").unwrap();
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

// ---------------------------------------------------------------- 探测日志
//
// 真机上的危险探测一旦闪退，界面上的输出就看不到了。所以把关键步骤**先写进文件**
// 再执行：崩了之后打开 App 的 Documents/nce_probe.log 就能看到崩在哪一步。
// 日志路径由 Swift 侧在启动时通过 ruzu_ios_set_log_path 传进来。

static LOG_PATH: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

#[no_mangle]
pub extern "C" fn ruzu_ios_set_log_path(path: *const c_char) {
    if path.is_null() {
        return;
    }
    let p = unsafe { std::ffi::CStr::from_ptr(path) }
        .to_string_lossy()
        .into_owned();
    if let Ok(mut g) = LOG_PATH.lock() {
        *g = Some(std::path::PathBuf::from(p));
    }
}

/// 追加一行到探测日志（没有配置路径、或写失败时静默忽略）。
pub(crate) fn log_line(msg: &str) {
    use std::io::Write;
    if let Ok(g) = LOG_PATH.lock() {
        if let Some(p) = g.as_ref() {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                let _ = writeln!(f, "{msg}");
            }
        }
    }
}

/// 把探测日志读出来给界面显示。
#[no_mangle]
pub extern "C" fn ruzu_ios_read_log() -> *mut c_char {
    let text = LOG_PATH
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_else(|| "（还没有日志，先点一次 ① 或 ④）".to_string());
    to_c_string(text)
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

/// JIT 通路探针 —— 核心思路：**先读页面的真实权限，再决定要不要执行**。
///
/// 为什么必须这样：内核允许你 `mmap` 请求 `PROT_EXEC`，但在没有 JIT 授权时会于
/// 建立映射时**悄悄剥掉 EXEC 位**（Apple 的 W^X 策略）。页表面上是 RWX，一执行就
/// SIGBUS —— 这正是我们连续两次闪退的原因。用 `mach_vm_region` 读真实权限则完全
/// 无害：不执行任何东西就能得到结论，也不会再拿 App 冒险。
fn probe_official() -> String {
    let mut out = String::new();
    let len = 4096usize;
    let rw = libc::PROT_READ | libc::PROT_WRITE;
    let rx = libc::PROT_READ | libc::PROT_EXEC;
    let rwx = rw | libc::PROT_EXEC;
    let (debugged, get_task_allow, dyn_codesign) = codesign_hints();
    let mut executed = false;

    unsafe {
        writeln!(
            out,
            "签名线索: CS_DEBUGGED={debugged} get-task-allow={get_task_allow} dynamic-codesigning={dyn_codesign}（仅供参考，不作判据）"
        )
        .unwrap();
        writeln!(out, "真实页面权限（对照内核实际给了什么）：").unwrap();

        // ---------- 方案 A：直接请求 RWX ----------
        match mmap_region(len, rwx, false) {
            Ok(p) => {
                let prot = ruzu_page_protection(p, len);
                writeln!(out, "  A. mmap(RWX) → 实际 {}", prot_str(prot)).unwrap();
                if prot >= 0 && prot & 4 != 0 {
                    jitmem::write_code_raw(p, &PROBE_CODE);
                    let r = call_code(p);
                    report_exec(&mut out, "A", r);
                    executed = true;
                }
                libc::munmap(p, len);
            }
            Err(e) => writeln!(out, "  A. mmap(RWX) → errno={e}").unwrap(),
        }

        // ---------- 方案 B：RW → 写入 → mprotect(RX)（W^X 安全的经典写法）----------
        if !executed {
            match mmap_region(len, rw, false) {
                Ok(p) => {
                    let prot_rw = ruzu_page_protection(p, len);
                    let mr = libc::mprotect(p, len, rx);
                    let prot_rx = ruzu_page_protection(p, len);
                    writeln!(
                        out,
                        "  B. mmap(RW)={} → mprotect(RX)={} → 实际 {}",
                        prot_str(prot_rw),
                        if mr == 0 { "OK".to_string() } else { format!("errno={}", *libc::__error()) },
                        prot_str(prot_rx)
                    )
                    .unwrap();
                    if prot_rx >= 0 && prot_rx & 4 != 0 {
                        // 写入前先改回可写
                        if libc::mprotect(p, len, rw) == 0 {
                            jitmem::write_code_raw(p, &PROBE_CODE);
                            if libc::mprotect(p, len, rx) == 0 {
                                let r = call_code(p);
                                report_exec(&mut out, "B", r);
                                executed = true;
                            }
                        }
                    }
                    libc::munmap(p, len);
                }
                Err(e) => writeln!(out, "  B. mmap(RW) → errno={e}").unwrap(),
            }
        }

        // ---------- 结论 ----------
        if executed {
            writeln!(out, "[OK]  ★ 可执行内存可用，机器码真的执行了 —— JIT 通路已打通。").unwrap();
        } else {
            writeln!(
                out,
                "[FAIL] 请求了 RWX/RX，内核实际给回来都没有 X 位 → 本进程当前没有 JIT 权限。\n\
                 　　　（这个结论来自**读页面真实权限**，不是靠闪退试出来的）\n\
                 　　　下一步：\n\
                 　　　　· 越狱 / TrollStore：先点「⑤ 自持 JIT」试试（老系统可能直接成功）\n\
                 　　　　· 其它情况：用 StikDebug / SideStore 给本 App 开 JIT（等于挂个调试器）后重开本 App"
            )
            .unwrap();
        }
    }
    out
}

fn report_exec(out: &mut String, tag: &str, r: u32) {
    if r == PROBE_EXPECT {
        writeln!(out, "[OK]  {tag}. 执行返回 {r} ✓").unwrap();
    } else {
        writeln!(out, "[??]  {tag}. 执行返回 {r}（期望 {PROBE_EXPECT}）").unwrap();
    }
}

/// 越狱路径对照：把几种「拿可执行内存」的映射方式都试一遍，**只报真实权限**，
/// 只有在真实权限里确实有 X 时才执行（所以这个按钮现在也不会闪退了）。
fn probe_unsigned() -> String {
    let mut out = String::new();
    let len = 4096usize;
    let rw = libc::PROT_READ | libc::PROT_WRITE;
    let rx = libc::PROT_READ | libc::PROT_EXEC;
    let rwx = rw | libc::PROT_EXEC;

    unsafe {
        writeln!(out, "各映射方式的真实权限（请求 → 内核实际给）：").unwrap();

        for (label, prot, map_jit) in [
            ("裸 RWX（无 MAP_JIT）", rwx, false),
            ("RW + MAP_JIT", rw, true),
            ("RWX + MAP_JIT", rwx, true),
        ] {
            match mmap_region(len, prot, map_jit) {
                Ok(p) => {
                    let real = ruzu_page_protection(p, len);
                    writeln!(out, "  {label:22} → mmap OK，实际 {}", prot_str(real)).unwrap();
                    if real >= 0 && real & 4 != 0 && prot & 4 != 0 {
                        jitmem::write_code_raw(p, &PROBE_CODE);
                        let r = call_code(p);
                        report_exec(&mut out, label, r);
                    }
                    libc::munmap(p, len);
                }
                Err(e) => writeln!(out, "  {label:22} → mmap 失败 errno={e}").unwrap(),
            }
        }

        // RW → mprotect(RX)
        match mmap_region(len, rw, false) {
            Ok(p) => {
                let mr = libc::mprotect(p, len, rx);
                let real = ruzu_page_protection(p, len);
                writeln!(
                    out,
                    "  RW → mprotect(RX)      → mprotect {}，实际 {}",
                    if mr == 0 { "OK".to_string() } else { format!("errno={}", *libc::__error()) },
                    prot_str(real)
                )
                .unwrap();
                if real >= 0 && real & 4 != 0 {
                    if libc::mprotect(p, len, rw) == 0 {
                        jitmem::write_code_raw(p, &PROBE_CODE);
                        if libc::mprotect(p, len, rx) == 0 {
                            let r = call_code(p);
                            report_exec(&mut out, "RW→RX", r);
                        }
                    }
                }
                libc::munmap(p, len);
            }
            Err(e) => writeln!(out, "  RW → mprotect(RX)      → mmap 失败 errno={e}").unwrap(),
        }
    }
    out
}

/// 尝试「自持 JIT」（照 RetroArch / UTM 的 ptrace hack）。
///
/// **明确标注为尽力而为**：这条路的可用性随系统版本变化，iOS 26+ 内核已封。
/// 真正稳妥的路线仍是「挂调试器」（StikDebug / AltStore / Xcode）。
#[no_mangle]
pub extern "C" fn ruzu_ios_try_self_trace() -> *mut c_char {
    let mut buf = vec![0 as c_char; 1024];
    let rc = unsafe { ruzu_try_self_trace(buf.as_mut_ptr(), buf.len()) };
    let why = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let mut out = String::new();
    if rc == 0 {
        writeln!(out, "[OK] 自持 JIT 调用成功。").unwrap();
    } else {
        writeln!(out, "[FAIL] 自持 JIT 失败（rc={rc}）。").unwrap();
    }
    out.push_str(&why);
    out.push_str("\n\n请重新点「① 运行诊断」看「真实页面权限」有没有变成带 X。");
    to_c_string(out)
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
    writeln!(s, "\n=== 模拟器核心接入状态 ===").unwrap();
    writeln!(s, "{}", core_status()).unwrap();
    writeln!(s, "\n=== 判定 ===").unwrap();
    if jit.contains('★') {
        writeln!(s, "→ JIT 通路已打通：机器码真的执行了。地基全齐，可以开始接 ruzu 核心了。").unwrap();
    } else if jit.contains("没有 JIT 权限") {
        writeln!(
            s,
            "→ 本进程当前没有 JIT 权限（内核剥掉了 EXEC 位）。\n\
             \x20  越狱/TrollStore：点「⑤ 自持 JIT」再重测；\n\
             \x20  其它环境：用 StikDebug / SideStore 开 JIT 后重开本 App（等于挂个调试器）。"
        )
        .unwrap();
    } else {
        writeln!(s, "→ JIT 状态未定，把上面「JIT 通路」一节的原文发出来定位。").unwrap();
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

/// 当前二进制有没有把模拟器核心接进来。
fn core_status() -> String {
    #[cfg(feature = "core")]
    {
        core_link::link_probe()
    }
    #[cfg(not(feature = "core"))]
    {
        "core 未接入（Cargo feature `core` 关闭）—— 默认状态：先保证 IPA 永远能编出来，\
         链路验证由 CI 的独立步骤做"
            .to_string()
    }
}

/// core 链接探针（给 Swift 用）。
#[no_mangle]
pub extern "C" fn ruzu_ios_core_link_probe() -> *mut c_char {
    to_c_string(core_status())
}

/// 窗口接入自检：用 Swift 传来的 `CAMetalLayer*` 构造 iOS 的 EmuWindow，
/// 并确认渲染后端能拿到的窗口信息（尺寸/缩放/layout/类型）都对。
///
/// **不启动模拟器**，只是把「Swift ↔ core ↔ video_core」这条数据通路打通并验证。
#[no_mangle]
pub extern "C" fn ruzu_ios_window_probe(
    layer: *mut c_void,
    width: u32,
    height: u32,
    scale: f32,
) -> *mut c_char {
    #[cfg(feature = "core")]
    {
        let mut win = emu_window_ios::EmuWindowIos::new(layer, width, height, scale);
        let mut s = win.describe();
        // 顺便验证 resize 路径
        win.on_resize(width, height, scale);
        s.push_str(&format!(
            "\n· resize 后客户区 = {}×{}\n· is_shown() = {}（EmuWindow trait 已实现）",
            win.base().client_area_width,
            win.base().client_area_height,
            ruzu_core::frontend::emu_window::EmuWindow::is_shown(&win),
        ));
        to_c_string(s)
    }
    #[cfg(not(feature = "core"))]
    {
        let _ = (layer, width, height, scale);
        to_c_string("core 未接入（feature `core` 关闭），窗口自检跳过".to_string())
    }
}

/// 阶段 1：加载一个游戏/家酿（.nro / .nsi / NSP 内 NCA 等，交给 loader 判断）。
///
/// 只做加载，不启动渲染 —— 用来确定性地验证：文件访问、加载器、客户机内存预留。
#[no_mangle]
pub extern "C" fn ruzu_ios_load_game(path: *const c_char) -> *mut c_char {
    #[cfg(feature = "core")]
    {
        if path.is_null() {
            return to_c_string("路径为空".to_string());
        }
        let p = unsafe { std::ffi::CStr::from_ptr(path) }
            .to_string_lossy()
            .into_owned();
        log_line(&format!("=== 加载测试：{p} ==="));
        let out = boot::load_game(&p);
        log_line(&format!("结果 ok={}", out.ok));
        log_line(&out.message);
        to_c_string(out.message)
    }
    #[cfg(not(feature = "core"))]
    {
        let _ = path;
        to_c_string("core 未接入（feature `core` 关闭），无法加载".to_string())
    }
}

#[no_mangle]
pub extern "C" fn ruzu_ios_free_string(p: *mut c_char) {
    if !p.is_null() {
        unsafe {
            drop(CString::from_raw(p));
        }
    }
}
