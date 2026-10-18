# iOS 移植：已修 Bug 与已知问题

> 这份清单是给「一次修完、少走弯路」用的。每一条都标注了**依据**（真机日志 / Apple 文档可用性 / 崩溃报告），
> 以及**修法原则**：按平台或按系统版本门控，**不针对某一台设备**。

## 一、已修（按发现顺序）

| # | 问题 | 依据 | 修法 |
|---|---|---|---|
| 1 | `mmap(MAP_JIT)` 在 iOS 上失败（EINVAL/EACCES），`pthread_jit_write_protect_np` 不存在 | 真机 errno + 链接期 Undefined symbol | iOS 走「`mmap(RW)` → 写 → `mprotect(RX)`」；符号改 `dlsym` |
| 2 | 一次性 `mmap(RWX)` 执行时 SIGBUS | 崩溃报告 `KERN_PROTECTION_FAILURE` | 同上：请求 RWX 会被内核剥掉 X 位 |
| 3 | NCE 探测崩溃（`EXC_GUARD / DEALLOC_GAP`） | 崩溃报告 | 不在预留区内「先拆内页再整片 munmap」 |
| 4 | 信号探测"时好时坏" | 多轮日志 | libdispatch 线程屏蔽信号；模拟线程要 `pthread_sigmask(SIG_UNBLOCK)` |
| 5 | `fiber stack` panic（`ExceedsMaximumSize(999424)`） | 真机 panic | iOS `RLIMIT_STACK` ≈ 976 KiB；改为阶梯分配（4 MiB → 960 KiB → …） |
| 6 | 加载 panic：`device memory must be created in initialize()` | 真机 panic | `load()` 之前必须 `system.initialize()` |
| 7 | `MTLDevice.architecture()` 在 iOS 上调用 → SIGABRT | 崩溃报告 `__rust_foreign_exception` | 该 selector 是 macOS 14+；先判平台再判版本 |
| 8 | `MTLSamplerDescriptor.lodBias` / `reductionMode` → SIGABRT | Apple 文档：iOS **26.0+** | `available!(ios = 26.0, macos = 26.0, ..)` 门控，老系统退化为默认采样 |
| 9 | `isDepth24Stencil8PixelFormatSupported` / `supportsPullModelInterpolation` 属 macOS 专有 | Apple 文档 | iOS 直接判 false（语义本来就成立） |
| 10 | `setMathMode(MTLMathMode::Safe)` 在 iOS 15~17 上会崩 | Apple 文档：iOS 18+ | 门控加 `ios = 18.0` |
| 11 | MSL 版本选择用了 macOS 版本号 | 逻辑审查 | iOS 按 iOS 版本选（18→3.2 / 17→3.1 / 16→3.0 / 15→2.4） |
| 12 | `~/.local/share/ruzu` 等目录不存在（16 条 ERROR） | 真机日志 | 启动时按需 `create_dir_all` |

## 二、已知但未修（按优先级）

### P0 —— 会影响能不能玩

| # | 问题 | 现状 / 影响 | 计划 |
|---|---|---|---|
| A | **fastmem 被禁用**（`page size 0x4000 is incompatible with 4K paging`） | iOS 页大小 16 KiB，ruz u 的 fastmem 假设 4 KiB → 走慢路径，性能损失明显 | 把 fastmem 的页粒度改为 `sysconf(_SC_PAGESIZE)`；P4 性能阶段第一件事 |
| B | **视频解码不可用**（FFmpeg 安全桩） | 游戏播过场视频时无画面 | 为 iOS 交叉编译 FFmpeg，或改用 VideoToolbox |
| C | **音频未验证**（`audio_core` + SDL3 已构造，但没跑过） | 可能需要 `AVAudioSession` category 配置 | P3 |
| D | **输入未接线**（触摸 / GCController） | 无法操作 | P2 |
| E | **NCE 未移植** | 四前提已实测通过，但 `core/src/arm/nce` 只在 aarch64 linux/android 编译 | P4：内存后端 + 信号屏蔽 + 客户机 VA 映射（三块都已验证可行） |

### P1 —— 稳定性 / 兼容性

| # | 问题 | 说明 |
|---|---|---|
| F | Metal 后端是 alpha | 上游自个儿标注的；真实绘制路径还没跑过一帧 |
| G | `.nsp/.xci` 需要 `prod.keys` + 固件 | 家酿 `.nro` 不需要；商业游戏需要，且 keys 要放进 `Documents` 下的 ruzu 数据目录 |
| H | 部署目标 15.0 vs SDL3 按 17.5 编译 | 链接时有 warning；iOS 15/16 设备可能有运行时问题 |
| I | `core` feature 默认关闭 | 一旦某轮核心编不过，CI 会静默回退纯外壳 → 必须看 job summary 的「App 组成核对」 |

## 三、给下一轮的排查方法（已验证有效）

1. **崩溃报告看 `__rust_foreign_exception`** = ObjC 异常（不是 Rust panic），`catch_unwind` 抓不到。
2. **Apple 文档可用性可直接查 JSON**：
   ```
   https://developer.apple.com/tutorials/data/documentation/metal/<类>/<成员>.json
   → metadata.platforms[].introducedAt
   ```
   凡 `objc2` 的 setter/property，先查这个再决定要不要 `available!` 门控。
3. **`objc2::available!(macos = X, ..)` 在 iOS 上不可靠**：它拿 iOS 版本号和 X 比。
   必须写成 `cfg!(target_os = "macos") && available!(...)` 或 `available!(macos = X, ios = Y, ..)`。
4. **分步打点是最省时间的定位手段**：`[metal] x/14` 这种把 14 个子系统一次缩到 1 个。
