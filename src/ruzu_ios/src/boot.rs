// SPDX-License-Identifier: GPL-3.0-or-later
//!
//! 真正开机：`System::new` → 子系统工厂（Host1x / GPU(Metal) / AudioCore）
//! → `load` → `apply_settings` → `gpu.start` → `run`。
//!
//! 调用顺序与 `src/ruzu/src/boot.rs`（GTK 前端）和 `ruzu_cmd`（SDL3 前端）一致，
//! 只是把窗口来源换成我们的 `EmuWindowIos`（CAMetalLayer）。
//!
//! 注意：这个模块在 `core` feature 后面。**它编不过不会影响 IPA 产出**
//! （CI 会自动回退纯外壳），所以可以放心大胆地迭代。

use std::ffi::c_void;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use ruzu_core::core::{System, SystemRef, SystemResultStatus};
use ruzu_core::frontend::framebuffer_layout::FramebufferLayout;
use video_core::renderer_base::RendererBase;

use crate::emu_window_ios::EmuWindowIos;

/// 开机结果（成功时返回给界面的说明文字）。
pub struct BootSession {
    pub message: String,
}

/// 加载结果（诊断用）。
pub struct LoadOutcome {
    pub ok: bool,
    pub message: String,
}

/// 只加载、不启动（诊断用）：验证 loader / 文件访问 / 客户机内存预留。
pub fn load_game(path: &str) -> LoadOutcome {
    let mut system = System::new();
    system.set_subsystem_factory(Box::new(|_system: &mut System| Ok(())));
    let status = system.load(path);
    if status != SystemResultStatus::Success {
        return LoadOutcome {
            ok: false,
            message: format!("加载失败：{status:?}\n路径：{path}"),
        };
    }
    LoadOutcome {
        ok: true,
        message: format!(
            "加载成功 ✓（未启动渲染）\n· 程序 ID：0x{:016X}\n· 64 位：{}",
            system.runtime_program_id(),
            system.runtime_is_64bit()
        ),
    }
}

pub fn boot(
    path: &str,
    layer: *mut c_void,
    width: u32,
    height: u32,
    scale: f32,
) -> Result<BootSession, String> {
    crate::log_line(&format!("=== boot 开始：{path} ==="));

    // 把关键设置打出来：CPU 后端选的是不是 dynarmic（iOS 没编译 NCE）、
    // 单核还是多核、GPU 是否异步 —— 这些直接决定客人跑不跑。
    {
        let values = common::settings::values();
        crate::log_line(&format!(
            "boot: 设置 cpu_backend={:?} multicore={} async_gpu={} renderer={:?}",
            *values.cpu_backend.get_value(),
            *values.use_multi_core.get_value(),
            *values.is_async_gpu.get_value(),
            *values.renderer_backend.get_value(),
        ));
    }

    // ruzu 按 Linux 习惯把数据放在 ~/.local/share/ruzu 等目录下；iOS 沙盒里
    // 这些目录默认不存在 → 启动日志会刷 16 条 ERROR，且 keys/nand/sdmc 都
    // 指向不存在的路径。这里在启动时一次性建好（HOME 就是 App 的沙盒根）。
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        for rel in [
            ".local/share/ruzu",
            ".local/share/ruzu/amiibo",
            ".local/share/ruzu/crash_dumps",
            ".local/share/ruzu/dump",
            ".local/share/ruzu/keys",
            ".local/share/ruzu/load",
            ".local/share/ruzu/log",
            ".local/share/ruzu/nand",
            ".local/share/ruzu/play_time",
            ".local/share/ruzu/screenshots",
            ".local/share/ruzu/sdmc",
            ".local/share/ruzu/tas",
            ".local/share/ruzu/icons",
            ".cache/ruzu",
            ".cache/ruzu/shader",
            ".config/ruzu",
        ] {
            let _ = std::fs::create_dir_all(home.join(rel));
        }
        crate::log_line("boot: ruzu 数据目录已就绪");
    }
    unsafe { crate::ruzu_install_crash_handler(); }
    crate::log_line("boot: 崩溃记录器已安装");

    // ---------------------------------------------------------------- 渲染后端
    // iOS 只有 Metal 这一条原生路径（Vulkan 需要 MoltenVK，OpenGL 已废弃）。
    {
        let mut values = common::settings::values_mut();
        values
            .renderer_backend
            .set_value(common::settings_enums::RendererBackend::Metal);
        values.use_disk_shader_cache.set_value(false);
    }
    crate::log_line("boot: 渲染后端 = Metal");

    // ---------------------------------------------------------------- 窗口
    let win = EmuWindowIos::new(layer, width, height, scale);
    let window_info = win.base().get_window_info().clone();
    crate::log_line(&format!(
        "boot: EmuWindow layer=0x{:x} {}x{} scale={}",
        layer as usize, width, height, scale
    ));

    let window_shown = Arc::new(AtomicBool::new(true));
    let framebuffer_layout = Arc::new(RwLock::new(FramebufferLayout::default()));

    // ---------------------------------------------------------------- 系统
    let mut system = System::new();
    // **必须**在 load() 之前调用：initialize() 会分配设备内存、创建 Memory 桥、
    // 按设置决定多核/异步 GPU 等。漏了它，load() 里第一句
    // `.expect("VFS must be created in initialize()")` 就会 panic。
    system.initialize();
    crate::log_line("boot: system.initialize() 完成");

    let shown_for_factory = Arc::clone(&window_shown);
    let layout_for_factory = Arc::clone(&framebuffer_layout);
    let info_for_factory = window_info.clone();

    system.set_subsystem_factory(Box::new(move |system: &mut System| {
        // --- Host1x / 同步点 / 设备内存 ---
        let host1x =
            video_core::host1x::host1x::Host1x::new_with_system(SystemRef::from_ref(system));
        let syncpoints = host1x.syncpoint_manager().clone();
        let device_memory = host1x.memory_manager().clone();
        system.set_host1x_core(Box::new(host1x));
        crate::log_line("boot(factory): Host1x 就绪");

        // --- GPU + Metal 渲染器 ---
        let system_ref = SystemRef::from_ref(&*system);
        crate::log_line("boot(factory): 即将 create_gpu");
        let gpu = video_core::video_core::create_gpu(system_ref, |backend, gpu| {
            // 帧计数：用来回答「客人到底有没有在出帧」这个最关键的问题。
            static DISPLAYED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            static SUBMITTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let frame_displayed_notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {
                let n = DISPLAYED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n == 1 || n % 60 == 0 {
                    crate::log_line(&format!("[frame] 已显示 {n} 帧"));
                }
            });
            let frame_end_notify: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {
                let n = SUBMITTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n == 1 || n % 60 == 0 {
                    crate::log_line(&format!("[frame] GPU 已提交 {n} 次"));
                }
            });

            let renderer: Box<dyn RendererBase> = match backend {
                common::settings_enums::RendererBackend::Metal => {
                    crate::log_line("boot(factory): 即将 RendererMetal::new");
                    Box::new(
                    video_core::renderer_metal::renderer_metal::RendererMetal::new(
                        &info_for_factory,
                        Arc::clone(&shown_for_factory),
                        Arc::clone(&layout_for_factory),
                        frame_displayed_notify,
                        frame_end_notify,
                        syncpoints.clone(),
                        Arc::clone(&device_memory),
                        Some(unsafe { gpu.shader_notify_handle() }),
                    )
                    .map_err(|error| format!("Metal 渲染器创建失败：{error}"))?,
                    )
                }
                other => {
                    return Err(format!("iOS 只支持 Metal 渲染器（当前后端：{other:?}）"));
                }
            };
            Ok::<_, String>(renderer)
        })?;
        system.set_gpu_core(gpu);
        crate::log_line("boot(factory): GPU + Metal 渲染器就绪");

        // --- 音频 ---
        let audio = audio_core::AudioCore::new(SystemRef::from_ref(&*system));
        system.set_audio_core(Box::new(audio));
        crate::log_line("boot(factory): AudioCore 就绪");

        Ok(())
    }));

    // ---------------------------------------------------------------- 加载
    crate::log_line("boot: 调用 system.load()");
    let status = system.load(path);
    if status != SystemResultStatus::Success {
        return Err(format!("加载失败：{status:?}"));
    }
    let program_id = system.runtime_program_id();
    let is_64bit = system.runtime_is_64bit();
    crate::log_line(&format!(
        "boot: 加载成功 program_id=0x{program_id:016X} 64bit={is_64bit}"
    ));

    system.apply_settings();

    // ---------------------------------------------------------------- 启动 GPU
    if let Some(gpu_any) = system.gpu_core() {
        if let Some(gpu) = gpu_any.as_any().downcast_ref::<video_core::gpu::Gpu>() {
            gpu.start();
            crate::log_line("boot: gpu.start() 完成");
        } else {
            crate::log_line("boot: gpu_core 不是 video_core::Gpu，跳过 start()");
        }
    }

    // ---------------------------------------------------------------- 跑起来
    system.run();
    crate::log_line("boot: system.run() 完成 —— 模拟已开始");

    // 模拟会话必须活到进程结束：不能在这里 drop。
    std::mem::forget(system);

    Ok(BootSession {
        message: format!(
            "已启动 ✓\n\
             · 程序 ID：0x{program_id:016X}（{} 位）\n\
             · 渲染器：Metal（原生，无 MoltenVK）\n\
             · 窗口：CAMetalLayer @ 0x{:x}，{width}×{height} @{scale}x\n\
             · 客户机内存：8 GiB VA 预留 / 按需提交\n\
             现在应该开始出画面了。若黑屏请点「⑥ 查看探测日志」，里面记录了每一步。",
            if is_64bit { 64 } else { 32 },
            layer as usize
        ),
    })
}
