//
//  ruzu iOS 外壳 —— 与 Rust 门面（src/ruzu_ios）的唯一接口声明。
//  这个文件被 RuzuApp-Bridging-Header.h 引用，Swift 侧就能直接调用。
//

#ifndef RUZU_IOS_ABI_H
#define RUZU_IOS_ABI_H

#include <stdint.h>

/// 返回版本字符串。**返回值由 Rust 分配，使用后必须调用 ruzu_ios_free_string。**
char *ruzu_ios_version(void);

/// 环境 + JIT（官方路径）+ VA 预留 三合一报告。安全，不会闪退。
char *ruzu_ios_diagnostics(void);

/// 只测官方 JIT 路径（MAP_JIT + pthread_jit_write_protect_np）。安全。
char *ruzu_ios_jit_probe(void);

/// 越狱路径探测（裸 RWX / mprotect(RX)）。**没有 JIT 权限时可能直接闪退。**
char *ruzu_ios_jit_probe_unsafe(void);

/// NCE（原生执行）可行性预检。会做 MAP_FIXED+MAP_JIT、自修改代码、多线程
/// 独占监视器、SIGSEGV 处理器实验。**可能闪退。**
char *ruzu_ios_nce_precheck(void);

/// 尝试「自持 JIT」（ptrace(PT_TRACE_ME) 让内核以为在被调试）。
/// 尽力而为：iOS 26+ 内核已封这条路。成功后请重跑一次诊断。
char *ruzu_ios_try_self_trace(void);

/// 释放上面几个函数返回的字符串。
void ruzu_ios_free_string(char *s);

#endif /* RUZU_IOS_ABI_H */
