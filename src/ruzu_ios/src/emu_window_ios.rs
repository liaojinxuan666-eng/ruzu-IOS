// SPDX-License-Identifier: GPL-3.0-or-later
//!
//! iOS 的 `EmuWindow` 实现（仅在 `core` feature 打开时编译）。
//!
//! ruzu 的渲染后端通过 `WindowSystemInfo.render_surface` 拿到宿主窗口句柄：
//! `video_core::renderer_metal::metal_layer` 里就是
//! `Retained::retain(raw_layer as *mut CAMetalLayer)` —— 也就是说，
//! **把 Swift 侧的 `CAMetalLayer*` 塞进 render_surface 就够了**，
//! 不需要额外的 Metal 桥接层。
//!
//! `EmuWindow` trait 只有三个方法（`on_frame_displayed` / `create_shared_context` /
//! `is_shown`），而 `GraphicsContext` 的方法全都有默认实现，所以这里用一个空
//! 上下文即可（Metal 后端不使用 OpenGL 共享上下文）。

use std::ffi::c_void;

use ruzu_core::frontend::emu_window::{
    EmuWindow, EmuWindowBase, WindowSystemInfo, WindowSystemType,
};
use ruzu_core::frontend::graphics_context::GraphicsContext;

/// 空的图形上下文：`GraphicsContext` 的所有方法都有默认实现。
struct NullGraphicsContext;

impl GraphicsContext for NullGraphicsContext {}

/// iOS 的 EmuWindow：只负责把 `CAMetalLayer*` 和尺寸交给渲染后端。
pub struct EmuWindowIos {
    base: EmuWindowBase,
    /// CAMetalLayer 的指针（仅用于诊断显示，真正取值走 base.window_info）
    layer: usize,
}

impl EmuWindowIos {
    /// `layer` 必须是有效的 `CAMetalLayer*`（Swift 侧用
    /// `Unmanaged.passUnretained(metalLayer).toOpaque()` 取到）。
    pub fn new(layer: *mut c_void, width: u32, height: u32, scale: f32) -> Self {
        let mut base = EmuWindowBase::new();
        base.window_info = WindowSystemInfo {
            // Apple 平台走 Cocoa 分支（Metal 后端据此决定 present 路径）
            type_: WindowSystemType::Cocoa,
            display_connection: 0,
            render_surface: layer as usize,
            render_surface_scale: scale,
        };
        base.client_area_width = width;
        base.client_area_height = height;
        base.update_current_framebuffer_layout(width, height);
        Self {
            base,
            layer: layer as usize,
        }
    }

    pub fn base(&self) -> &EmuWindowBase {
        &self.base
    }

    pub fn base_mut(&mut self) -> &mut EmuWindowBase {
        &mut self.base
    }

    /// 视图尺寸变化时调用（旋转、分屏、窗口化）。
    pub fn on_resize(&mut self, width: u32, height: u32, scale: f32) {
        self.base.client_area_width = width;
        self.base.client_area_height = height;
        self.base.window_info.render_surface_scale = scale;
        self.base.update_current_framebuffer_layout(width, height);
    }

    /// 诊断用：确认 layer 指针确实被后端拿到。
    pub fn describe(&self) -> String {
        let info = self.base.get_window_info();
        format!(
            "EmuWindow 已就绪：\n\
             · CAMetalLayer* = 0x{:x}（Metal 后端从 WindowSystemInfo.render_surface 读取）\n\
             · 窗口系统类型 = {:?}（Apple 平台走 Cocoa 分支）\n\
             · 客户区 = {}×{}，缩放 = {}\n\
             · 帧布局已计算 = {}（{}）",
            self.layer,
            info.type_,
            self.base.client_area_width,
            self.base.client_area_height,
            info.render_surface_scale,
            self.base.get_framebuffer_layout().width,
            self.base.get_framebuffer_layout().height,
        )
    }
}

impl EmuWindow for EmuWindowIos {
    fn create_shared_context(&self) -> Box<dyn GraphicsContext> {
        // Metal 后端不需要 GL 共享上下文
        Box::new(NullGraphicsContext)
    }

    fn is_shown(&self) -> bool {
        // 目前是单窗口全屏渲染；将来支持后台时改为真实状态。
        true
    }
}
