// SPDX-License-Identifier: GPL-3.0-or-later
//!
//! 启动流程（分阶段实现）。
//!
//! ## 阶段划分（为什么不一上来就全做）
//!
//! 真正开机需要的子系统工厂（`SubsystemFactory`）要一次性建好
//! Host1x / GPU（Metal 渲染器 + `EmuWindowIos`）/ AudioCore，还要挂上
//! GPU 访存钩子。里面任何一个参数写错都会变成"跑起来黑屏"，很难定位。
//!
//! 所以拆成两阶段：
//!   * **阶段 1（本文件）**：`System::new()` → 注册工厂 → `load(path)`。
//!     验证文件访问、加载器（.nro/NSO）、客户机内存预留（8 GiB VA）、密钥路径。
//!     不接渲染器，所以不会黑屏，结论是确定的"能/不能加载"。
//!   * 阶段 2（下一步）：在工厂里创建 GPU（Metal）+ AudioCore，`gpu.start()` + `run()`。
//!
//! 参考实现：`ruzu_cmd/src/main.rs`（SDL3 前端），调用顺序与上游 yuzu_cmd 一致。

use ruzu_core::core::{System, SystemResultStatus};

/// 加载结果。
pub struct LoadOutcome {
    pub ok: bool,
    pub message: String,
}

/// 阶段 1：只做加载，验证整条非渲染路径。
pub fn load_game(path: &str) -> LoadOutcome {
    let mut system = System::new();

    // 子系统工厂：load() 内部会在 setup_for_application_process() 阶段回调它。
    // 这一版**故意不建 GPU**（渲染器接线是阶段 2 的事），直接返回 Ok，
    // 让 load() 把加载流程走完 —— 我们要的信息在返回值里。
    system.set_subsystem_factory(Box::new(|_system: &mut System| Ok(())));

    let status = system.load(path);
    if status != SystemResultStatus::Success {
        return LoadOutcome {
            ok: false,
            message: format!("加载失败：{status:?}\n路径：{path}"),
        };
    }

    let program_id = system.runtime_program_id();
    let is_64bit = system.runtime_is_64bit();

    LoadOutcome {
        ok: true,
        message: format!(
            "加载成功 ✓\n\
             · 路径：{path}\n\
             · 程序 ID：0x{program_id:016X}\n\
             · 64 位进程：{is_64bit}\n\
             · 客户机内存已按 iOS 的限制预留（VA 上限 16 GiB，jetsam 2.28 GiB）\n\
             下一步（阶段 2）：在子系统工厂里创建 Metal 渲染器 + AudioCore，然后 gpu.start() / run()"
        ),
    }
}
