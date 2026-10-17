// SPDX-License-Identifier: GPL-3.0-or-later
//!
//! 核心链接探针（仅在 `core` feature 打开时编译）。
//!
//! 目的**不是**跑模拟器，而是回答一个具体问题：
//! 「把 ruzu 的模拟器核心编进 iOS 静态库，能不能过？」
//!
//! 这一关要一起过的东西很多：`core` 自己的几十个模块、vendored OpenSSL、
//! rdynarmic（AArch64 JIT）、以及它们带的所有 C/C++ 依赖。
//! 这一步先解决「编得过 + 知道要链哪些本地库」，
//! 之后才是 EmuWindow / 真正开机。

/// 触碰一个真实符号，逼链接器把 `core` 这棵依赖树拉进来。
#[cfg(feature = "core")]
pub fn link_probe() -> String {
    // constants::ACCOUNT_BACKUP_JPEG 是 core 里一个 20 KB 的常量数组，
    // 引用它足以让 core 及其依赖真的参与编译与链接。
    let len = ruzu_core::constants::ACCOUNT_BACKUP_JPEG.len();
    format!("core 已成功链入：constants::ACCOUNT_BACKUP_JPEG = {len} 字节")
}
