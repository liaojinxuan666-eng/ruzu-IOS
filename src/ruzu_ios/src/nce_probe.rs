// SPDX-License-Identifier: GPL-3.0-or-later
//!
//! NCE（原生执行）可行性预检 —— 直接回答「非越狱设备能不能用 NCE」。
//!
//! ruzu 的 NCE（`src/core/src/arm/nce/`）不走翻译，而是把客户机的 ARM64 代码
//! 直接放在内存里执行，靠信号处理器拦截 SVC / MRS·MSR / 访存异常。
//! 要在 iOS 上跑通它，必须先满足四个前提，本模块逐个实测：
//!
//!   ① 客户机地址空间里能开出**可执行**的映射
//!      做法同 host_memory：先 PROT_NONE 预留一大片 VA，再在内部 MAP_FIXED
//!      叠一个 MAP_JIT 的 RWX 页。测试这个组合是否被内核接受。
//!   ② 能就地改写代码并立刻重执行（自修改代码）
//!      客户机有自我修改/热补丁，NCE 要靠写保护开关来回切 + 刷 I-cache。
//!   ③ 独占监视器（LDXR/STXR）在多线程竞争下语义正确
//!      NCE 直接用硬件原子，客户机的互斥/引用计数全靠它。Apple Silicon 上
//!      这是最可疑的一条，所以单线程、多线程都测。
//!   ④ 能装信号处理器并从中恢复
//!      NCE 的异常拦截机制。用 setjmp/longjmp 模拟「异常后继续跑」。
//!
//! 四条全绿 = NCE 在非越狱设备上可行（前提是拿到 JIT 权限，走 StikDebug 那条路）。

use std::ffi::{c_int, c_void};
use std::fmt::Write as _;

const MAP_JIT: c_int = 0x0800;

// 注：`pthread_jit_write_protect_np` 在 iOS SDK 里没有符号，绝不能静态引用
// （会让链接期报 Undefined symbols）。写入逻辑统一走 crate::jitmem。

// ------------------------------------------------------------------ 指令编码
//
// 手写 aarch64 机器码，等于用最小的代价模拟「客户机代码」。
// 注释里给的是对应汇编，方便逐条核对。

/// `mov w0, #imm16; ret`
fn encode_return_imm(imm: u16) -> [u32; 2] {
    [0x5280_0000 | ((imm as u32) << 5), 0xD65F_03C0]
}

/// `mov w0, #imm16; ret` —— 但 imm 由调用方在运行时决定，用于自修改测试
const CODE_RET_1: [u32; 2] = [0x5280_0000 | (1 << 5), 0xD65F_03C0];
const CODE_RET_2: [u32; 2] = [0x5280_0000 | (2 << 5), 0xD65F_03C0];

/// 一次 LDXR/ADD/STXR 尝试，返回 STXR 的状态码（0 = 成功）：
///
/// ```asm
/// mov x1, x0          ; base
/// ldxr w2, [x1]
/// add  w2, w2, #1
/// stxr w3, w2, [x1]   ; w3 = 0 成功 / 1 失败需重试
/// mov w0, w3
/// ret
/// ```
const CODE_RMW: [u32; 6] = [
    0xAA00_03E1, // mov x1, x0
    0x885F_7C22, // ldxr w2, [x1]
    0x1100_0442, // add  w2, w2, #1
    0x8803_7C22, // stxr w3, w2, [x1]
    0x2A03_03E0, // mov w0, w3
    0xD65F_03C0, // ret
];

// ------------------------------------------------------------------ 工具

unsafe fn mmap_reserve(len: usize) -> Result<*mut u8, i32> {
    let p = libc::mmap(
        std::ptr::null_mut(),
        len,
        libc::PROT_NONE,
        libc::MAP_PRIVATE | libc::MAP_ANON,
        -1,
        0,
    );
    if p == libc::MAP_FAILED {
        Err(*libc::__error())
    } else {
        Ok(p as *mut u8)
    }
}

/// 开一段**可写**页（RW，不含 X）。
///
/// 为什么要这样：真机实测（iPhone11,6 / iOS 18.5）——**一次性请求 RWX 会被内核
/// 剥掉 X 位**（W^X 策略），执行时 SIGBUS；正确姿势是
/// 「映射 RW → 写入 → mprotect(RX) → 执行」，这条实测能跑通。
/// 写入与权限翻转由 `write_code_at`（即 `jitmem::write_code`）负责。
unsafe fn mmap_jit(addr: *mut u8, len: usize, fixed: bool) -> Result<*mut u8, i32> {
    let mut flags = libc::MAP_PRIVATE | libc::MAP_ANON;
    if fixed {
        flags |= libc::MAP_FIXED;
    }
    let p = libc::mmap(
        addr as *mut c_void,
        len,
        libc::PROT_READ | libc::PROT_WRITE,
        flags,
        -1,
        0,
    );
    if p == libc::MAP_FAILED {
        Err(*libc::__error())
    } else {
        Ok(p as *mut u8)
    }
}

/// 内核实际给了可执行权限吗？（见 RuzuJIT.h 的说明）
unsafe fn page_executable(p: *mut u8, len: usize) -> bool {
    let prot = crate::ruzu_page_protection(p as *mut c_void, len);
    prot >= 0 && prot & 4 != 0
}

/// 往 JIT 内存写机器码。写保护切换（或 mprotect 降级）由 `crate::jitmem` 统一处理。
unsafe fn write_code_at(p: *mut u8, len: usize, words: &[u32]) -> Result<(), String> {
    crate::jitmem::write_code(p as *mut c_void, len, words)
}

// ------------------------------------------------------------------ ① 可执行映射

fn probe_executable_guest_va(out: &mut String) {
    const RESERVE: usize = 256 * 1024 * 1024; // 256 MiB VA 预留
    const PAGE: usize = 64 * 1024;

    unsafe {
        match mmap_reserve(RESERVE) {
            Err(e) => {
                writeln!(out, "[FAIL] ① 预留 256 MiB VA 失败 errno={e}").unwrap();
                return;
            }
            Ok(base) => {
                writeln!(out, "[OK]   ①-a 预留 256 MiB PROT_NONE 于 {base:p}").unwrap();

                // 在预留区中间挑一个 64 KiB 对齐的地址，模拟客户机代码段
                let target = base.add(RESERVE / 2);
                match mmap_jit(target, PAGE, true) {
                    Ok(p) => {
                        writeln!(out, "[OK]   ①-b 在预留区内 MAP_FIXED 叠加 RWX 页 @ {p:p}").unwrap();
                        // 先写入（内部会把页 mprotect 成 RX），再检查是否真的可执行，最后才执行
                        crate::log_line("NCE①: 即将写入并执行 MAP_FIXED 页");
                        match write_code_at(p, PAGE, &encode_return_imm(7)) {
                            Ok(()) => {
                                if !page_executable(p, PAGE) {
                                    writeln!(
                                        out,
                                        "[FAIL] ①-c 写完后该页仍无 X（内核不给可执行）→ 不执行，免得闪退"
                                    )
                                    .unwrap();
                                } else {
                                    let f: extern "C" fn() -> u32 = std::mem::transmute(p);
                                    let v = f();
                                    if v == 7 {
                                        writeln!(out, "[OK]   ①-c 该页里的代码真的执行了（返回 {v}）").unwrap();
                                    } else {
                                        writeln!(out, "[??]   ①-c 执行了但返回 {v}（期望 7）").unwrap();
                                    }
                                }
                            }
                            Err(e) => writeln!(
                                out,
                                "[FAIL] ①-c 能映射但写不进去：{e}\n\
                                 　　→ 该页不可写，NCE 无法就地打补丁"
                            )
                            .unwrap(),
                        }
                        // **不要**在这里单独 munmap(p)：接着再 munmap 整片预留区会形成
                        // "跨越空洞的解除映射"，iOS 会判定为 EXC_GUARD / DEALLOC_GAP
                        // 并直接 SIGKILL（2026-10-17 真机崩过）。整片一次性释放即可。
                        libc::munmap(base as *mut c_void, RESERVE);
                        return;
                    }
                    Err(e) => writeln!(
                        out,
                        "[FAIL] ①-b MAP_FIXED + MAP_JIT 失败 errno={e}\n\
                         　　→ NCE 需要把客户机代码段做成可执行映射，这条不通就只能走 JIT"
                    )
                    .unwrap(),
                }
                libc::munmap(base as *mut c_void, RESERVE);
            }
        }
    }
}

// ------------------------------------------------------------------ ② 自修改代码

fn probe_self_modifying(out: &mut String) {
    unsafe {
        match mmap_jit(std::ptr::null_mut(), 64 * 1024, false) {
            Err(e) => {
                writeln!(out, "[FAIL] ② 分配 JIT 页失败 errno={e}").unwrap();
                return;
            }
            Ok(p) => {
                if let Err(e) = write_code_at(p, 64 * 1024, &CODE_RET_1) {
                    writeln!(out, "[FAIL] ② 页面无法写入：{e}").unwrap();
                    libc::munmap(p as *mut c_void, 64 * 1024);
                    return;
                }
                if !page_executable(p, 64 * 1024) {
                    writeln!(out, "[FAIL] ② 写完仍未拿到 X 权限（内核不给可执行）→ 跳过执行").unwrap();
                    libc::munmap(p as *mut c_void, 64 * 1024);
                    return;
                }
                crate::log_line("NCE②: 即将执行第一版代码");
                let f: extern "C" fn() -> u32 = std::mem::transmute(p);
                let first = f();

                // 就地改写成返回 2 的版本，再执行一次
                let rewrite = write_code_at(p, 64 * 1024, &CODE_RET_2);
                let second = if rewrite.is_ok() { f() } else { 0 };

                if first == 1 && second == 2 {
                    writeln!(out, "[OK]   ② 自修改代码可用（第一次返回 {first}，改写后返回 {second}）").unwrap();
                } else if let Err(e) = rewrite {
                    writeln!(out, "[FAIL] ② 无法就改写代码：{e}").unwrap();
                } else {
                    writeln!(out, "[FAIL] ② 自修改代码异常：第一次 {first}（期望 1），改写后 {second}（期望 2）").unwrap();
                }
                libc::munmap(p as *mut c_void, 64 * 1024);
            }
        }
    }
}

// ------------------------------------------------------------------ ③ 独占监视器

fn probe_exclusive_monitor(out: &mut String) {
    unsafe {
        let page = match mmap_jit(std::ptr::null_mut(), 64 * 1024, false) {
            Err(e) => {
                writeln!(out, "[FAIL] ③ 分配 JIT 页失败 errno={e}").unwrap();
                return;
            }
            Ok(p) => p,
        };

        // 代码页（RW → 写入 → 变 RX）
        if let Err(e) = write_code_at(page, 64 * 1024, &CODE_RMW) {
            writeln!(out, "[FAIL] ③ 页面无法写入：{e}").unwrap();
            libc::munmap(page as *mut c_void, 64 * 1024);
            return;
        }
        if !page_executable(page, 64 * 1024) {
            writeln!(out, "[FAIL] ③ 写完仍未拿到 X 权限（内核不给可执行）→ 跳过执行").unwrap();
            libc::munmap(page as *mut c_void, 64 * 1024);
            return;
        }
        crate::log_line("NCE③: 即将多线程执行独占监视器测试");

        // **数据绝对不能放在代码页里**：写完代码后整页是 R-X（只读），
        // 往里面写会立刻 SIGBUS —— 2026-10-17 真机就是崩在这一行。
        // 用栈上的变量当计数器即可（独占监视器对任何地址都有效）。
        let rmw: extern "C" fn(*mut u32) -> u32 = std::mem::transmute(page);
        let mut local_counter: u32 = 0;
        let counter: *mut u32 = &mut local_counter;

        // 多线程要用地址（usize），裸指针不是 Send，不能直接跨线程捕获
        let counter_addr = counter as usize;

        // 单线程：一次成功
        let mut attempts = 0u64;
        loop {
            attempts += 1;
            if rmw(counter) == 0 {
                break;
            }
            if attempts > 1000 {
                break;
            }
        }
        let single = *counter;

        // 多线程竞争：4 线程各做 50_000 次成功递增，最终必须刚好等于 200_000。
        // 如果独占监视器语义不对（比如 STXR 假成功），这里会少算 —— 那 NCE 的
        // 客户机互斥/引用计数就会出错，属于致命问题。
        *counter = 0;
        const THREADS: u32 = 4;
        const PER_THREAD: u32 = 50_000;
        let total = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let total = total.clone();
            let rmw = rmw;
            handles.push(std::thread::spawn(move || {
                let ptr = counter_addr as *mut u32;
                let mut done = 0u32;
                let mut spins = 0u64;
                while done < PER_THREAD && spins < 50_000_000 {
                    spins += 1;
                    if rmw(ptr) == 0 {
                        done += 1;
                    }
                }
                total.fetch_add(done as u64, std::sync::atomic::Ordering::Relaxed);
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        let expected = (THREADS * PER_THREAD) as u32;
        let actual = *counter;
        let reported = total.load(std::sync::atomic::Ordering::Relaxed);

        if single == 1 && actual == expected && reported == expected as u64 {
            writeln!(out, "[OK]   ③ 独占监视器正确（单线程 {single}；{THREADS} 线程 × {PER_THREAD} 次 → 期望 {expected}，实测 {actual}）").unwrap();
        } else {
            writeln!(
                out,
                "[FAIL] ③ 独占监视器异常：单线程 {single}（期望 1）；多线程期望 {expected}，实测计数 {actual}（线程自报 {reported}）\n\
                 　　→ 丢更新意味着 NCE 下客户机互斥会出问题，得改用别的同步策略"
            )
            .unwrap();
        }

        libc::munmap(page as *mut c_void, 64 * 1024);
    }
}

// ------------------------------------------------------------------ ④ 信号处理器

const SIGSEGV: c_int = 11;
const SIGBUS: c_int = 10;
const SIGUSR1: c_int = 30;

#[repr(C)]
struct SigAction {
    handler: usize,
    mask: u32,
    flags: c_int,
}

static mut CAUGHT: c_int = 0;
static mut USR1_HITS: c_int = 0;

extern "C" {
    fn sigaction(sig: c_int, act: *const SigAction, old: *mut SigAction) -> c_int;
    fn raise(sig: c_int) -> c_int;
}

extern "C" fn segv_handler(_sig: c_int) {
    // 只计数，**绝不 longjmp** —— 见下面 probe_signal_recovery 的说明。
    unsafe {
        CAUGHT += 1;
    }
}

extern "C" fn usr1_handler(_sig: c_int) {
    unsafe {
        USR1_HITS += 1;
    }
}

/// ④ 信号机制可用性。
///
/// **重要教训（2026-10-17 真机崩溃报告）**：上一版在这个测试里用
/// `raise(SIGSEGV)` + 处理器内 `longjmp` 来模拟「从异常恢复」，结果真机上直接
/// 崩了（`EXC_BAD_ACCESS`，跳到了垃圾地址 0xd0495e7e0）——
/// 在信号处理器里 longjmp 需要配合 sigaltstack + sigsetjmp/siglongjmp，
/// 而且不能想当然地跨 dispatch 线程乱跳。所以这一版改成**只验证不动跳转的部分**：
///
///   ① 能否注册 SIGSEGV / SIGBUS 处理器（NCE 靠它拦客户机访存异常）
///   ② 能否安装备用信号栈（栈损坏时处理器还能跑）
///   ③ 自己发一个 SIGUSR1，确认信号确实能送到我们的处理器
///
/// 「从硬件异常恢复」留到真正写 NCE 时，配 sigaltstack + sigsetjmp 一起做。
fn probe_signal_recovery(out: &mut String) {
    unsafe {
        // ① 注册 SIGSEGV / SIGBUS 处理器
        let act = SigAction {
            handler: segv_handler as usize,
            mask: 0,
            flags: 0,
        };
        let mut ok_count = 0;
        for sig in [SIGSEGV, SIGBUS] {
            if sigaction(sig, &act, std::ptr::null_mut()) == 0 {
                ok_count += 1;
            } else {
                writeln!(out, "[FAIL] ④ 注册信号 {sig} 处理器失败 errno={}", *libc::__error()).unwrap();
            }
        }
        if ok_count == 2 {
            writeln!(out, "[OK]   ④-a 可以注册 SIGSEGV / SIGBUS 处理器（NCE 的异常拦截机制可用）").unwrap();
        }

        // ② 备用信号栈
        let mut alt = libc::stack_t {
            ss_sp: std::ptr::null_mut(),
            ss_flags: 0,
            ss_size: 0,
        };
        let stack_size = 64 * 1024;
        let alt_stack = libc::mmap(
            std::ptr::null_mut(),
            stack_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        );
        if alt_stack != libc::MAP_FAILED {
            alt.ss_sp = alt_stack;
            alt.ss_size = stack_size;
            if libc::sigaltstack(&alt, std::ptr::null_mut()) == 0 {
                writeln!(out, "[OK]   ④-b 备用信号栈安装成功（64 KiB）").unwrap();
            } else {
                writeln!(out, "[FAIL] ④-b sigaltstack 失败 errno={}", *libc::__error()).unwrap();
            }
        } else {
            writeln!(out, "[FAIL] ④-b 给备用信号栈分配内存失败 errno={}", *libc::__error()).unwrap();
        }

        // ③ 自己发一个信号，确认能送到处理器（这一路完全不涉及跳转，不会崩）
        let act2 = SigAction {
            handler: usr1_handler as usize,
            mask: 0,
            flags: 0,
        };
        if sigaction(SIGUSR1, &act2, std::ptr::null_mut()) == 0 {
            raise(SIGUSR1);
            if USR1_HITS == 1 {
                writeln!(out, "[OK]   ④-c 信号送达处理器（SIGUSR1 命中 1 次）").unwrap();
            } else {
                writeln!(out, "[FAIL] ④-c 信号没送到处理器（命中 {USR1_HITS} 次）").unwrap();
            }
        } else {
            writeln!(out, "[FAIL] ④-c 注册 SIGUSR1 处理器失败 errno={}", *libc::__error()).unwrap();
        }
    }
}

// ------------------------------------------------------------------ 入口

pub fn precheck() -> String {
    let mut out = String::new();
    crate::log_line("");
    crate::log_line("========== NCE 预检开始 ==========");
    writeln!(out, "NCE（原生执行）可行性预检 —— 四个前提逐条实测\n").unwrap();
    writeln!(out, "① 客户机 VA 里开可执行映射").unwrap();
    crate::log_line("precheck: 进入 ①");
    probe_executable_guest_va(&mut out);
    writeln!(out, "\n② 自修改代码（改完立刻重执行）").unwrap();
    crate::log_line("precheck: 进入 ②");
    probe_self_modifying(&mut out);
    writeln!(out, "\n③ 独占监视器 LDXR/STXR（含多线程竞争）").unwrap();
    crate::log_line("precheck: 进入 ③");
    probe_exclusive_monitor(&mut out);
    writeln!(out, "\n④ 信号处理器拦截与恢复").unwrap();
    crate::log_line("precheck: 进入 ④");
    probe_signal_recovery(&mut out);
    crate::log_line("precheck: 全部完成");

    let ok = out.matches("[OK]").count();
    let fail = out.matches("[FAIL]").count();
    writeln!(out, "\n=== 结论：{ok} 项通过 / {fail} 项失败 ===").unwrap();
    if fail == 0 {
        writeln!(out, "→ 四个前提全绿：NCE 在非越狱设备上可行（需要 JIT 权限，用 StikDebug 那套开一次即可）。").unwrap();
    } else {
        writeln!(out, "→ 有前提不成立。若失败的是①②，说明 JIT 权限还没开；若是③，需要针对 Apple 平台重做同步策略。").unwrap();
    }
    out
}
