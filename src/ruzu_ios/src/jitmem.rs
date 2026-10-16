// SPDX-License-Identifier: GPL-3.0-or-later
//!
//! JIT 内存写入的公共逻辑。
//!
//! ## 为什么不能直接 `extern "C" { fn pthread_jit_write_protect_np(...) }`
//!
//! 这个函数在 **macOS** 上存在，但 **iOS SDK 的 libSystem stub 里没有它** ——
//! 直接静态引用会让链接期报：
//!
//! ```text
//! Undefined symbols for architecture arm64:
//!   "_pthread_jit_write_protect_np", referenced from: ... in libruzu_ios.a
//! ```
//!
//! （实测踩过：CI 第一次跑 xcodebuild 就死在这。）
//!
//! 所以这里用 `dlsym(RTLD_DEFAULT, ...)` 在运行时查：
//!   * 查得到 → 按 macOS 的官方姿势用「写保护开关」切可写/可执行；
//!   * 查不到（iOS 的常见情况）→ 退回到 `mprotect` 把页先改成可写、写完改回可执行。
//!
//! `sys_icache_invalidate` 在 iOS 上是有的（JSC 也用它），可以放心静态引用。

use std::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
    fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
}

/// Darwin 的 `RTLD_DEFAULT`：`(void *)-2`，表示在主程序与所有已加载镜像里搜符号。
const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

type JitWriteProtectFn = unsafe extern "C" fn(c_int);

static WRITE_PROTECT: OnceLock<Option<JitWriteProtectFn>> = OnceLock::new();

fn lookup() -> Option<JitWriteProtectFn> {
    *WRITE_PROTECT.get_or_init(|| unsafe {
        let name = b"pthread_jit_write_protect_np\0";
        let p = dlsym(RTLD_DEFAULT, name.as_ptr() as *const c_char);
        if p.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut c_void, JitWriteProtectFn>(p))
        }
    })
}

/// 给诊断报告用的一行说明。
pub fn status_line() -> &'static str {
    if lookup().is_some() {
        "pthread_jit_write_protect_np：已找到（与 macOS 行为一致，可用写保护开关切换 W^X）"
    } else {
        "pthread_jit_write_protect_np：未找到（iOS SDK 没有此符号，改用 mprotect 切换可写/可执行）"
    }
}

pub fn write_protect_available() -> bool {
    lookup().is_some()
}

/// 把 `words` 写进 [p, p+len) 这段 JIT 内存，并刷 I-cache。
///
/// **安全前提**：只有在确认能把页改成可写时才写，绝不硬写只读页 ——
/// 硬写会直接触发 EXC_BAD_ACCESS 把进程干掉，而诊断程序不该因为探测而闪退。
pub unsafe fn write_code(p: *mut c_void, len: usize, words: &[u32]) -> Result<(), String> {
    let bytes = words.len() * 4;
    debug_assert!(bytes <= len);

    match lookup() {
        Some(f) => {
            f(0); // 关写保护 = 可写
            std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, p as *mut u8, bytes);
            f(1); // 开写保护 = 可执行
        }
        None => {
            // 先把整页改成可写
            let rw = libc::PROT_READ | libc::PROT_WRITE;
            if mprotect(p, len, rw) != 0 {
                return Err(format!(
                    "mprotect(RW) 失败 errno={}；也拿不到 pthread_jit_write_protect_np，无法安全写入",
                    *libc::__error()
                ));
            }
            std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, p as *mut u8, bytes);
            let rx = libc::PROT_READ | libc::PROT_EXEC;
            if mprotect(p, len, rx) != 0 {
                return Err(format!("mprotect(RX) 失败 errno={}", *libc::__error()));
            }
        }
    }
    sys_icache_invalidate(p, bytes);
    Ok(())
}

/// 不做任何保护切换，直接写 —— 只给「越狱路径探测」用，可能闪退。
pub unsafe fn write_code_raw(p: *mut c_void, words: &[u32]) {
    let bytes = words.len() * 4;
    std::ptr::copy_nonoverlapping(words.as_ptr() as *const u8, p as *mut u8, bytes);
    sys_icache_invalidate(p, bytes);
}
