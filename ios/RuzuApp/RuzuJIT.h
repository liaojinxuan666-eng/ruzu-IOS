//
//  RuzuJIT.h
//
//  JIT 相关的 Mach / 代码签名层面的能力查询。
//
//  为什么要单独写 C：
//    `mach_vm_region`、`task_set_exception_ports`、`ptrace` 这些接口的
//    结构体布局和常量只有跟着 SDK 头文件走才不会错，从 Rust 手写 FFI 太容易翻车。
//    这里用真头文件写，Rust 侧只把它们当普通 extern "C" 调用。
//

#ifndef RUZU_JIT_H
#define RUZU_JIT_H

#include <stddef.h>
#include <stdint.h>

/// 读本进程的代码签名标志（csops CS_OPS_STATUS）。
/// 成功返回 0 并把标志写进 *out；失败返回 errno。
int ruzu_cs_flags(uint32_t *out);

/// 查询 [addr, addr+len) 所在区域的**真实**内存权限。
///
/// 这是判断「有没有 JIT 权限」的正确方法：
/// 内核允许 mmap 请求 PROT_EXEC，但如果没有 JIT 授权，它会在实际映射里
/// 把 EXEC 位悄悄剥掉 —— 页表面上是 RWX，一执行就 SIGBUS。
/// 读真实权限则不需要执行任何代码，永远不会闪退。
///
/// 返回位掩码：1=可读 2=可写 4=可执行；查询失败返回 -1。
int ruzu_page_protection(void *addr, size_t len);

/// 把权限位掩码转成人话（"RW-" / "R-X" / "RWX"）。
void ruzu_prot_to_string(int prot, char *out, size_t out_len);

/// 尝试「自持 JIT」：照 RetroArch / UTM 的做法给本进程自己发 PT_TRACE_ME，
/// 让内核认为它在被 Xcode 调试，从而放开可执行内存。
///
/// 注意：**这是尽力而为**，iOS 26+ 的内核已经封掉了这条路（RetroArch 里明确
/// 用 `apple_runtime_available(0, 26.0, 0)` 跳过）。所以它只是「能成就赚到」，
/// 真正可靠的路线还是挂调试器（StikDebug / AltStore / Xcode）。
///
/// 成功返回 0；失败返回非 0，并把原因写进 why。
int ruzu_try_self_trace(char *why, size_t why_len);

#endif /* RUZU_JIT_H */
