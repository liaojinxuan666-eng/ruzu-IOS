# ruzu → iOS 移植方案

> 目标仓库：https://github.com/vricosti/ruzu-emu (GPL-3.0, Rust, 180★, 最后推送 2026-10-07)
> 调研基线：`main` 分支源码快照（已解压在 `workspace/ruzu/ruzu-emu-main`）
> 结论：**可行，且比 C++ 版（MeloZu/Sudachi）好啃** —— 关键三块（Metal 后端、AArch64 JIT、MAP_JIT/写保护）仓库里都已经有了，只是 cfg 门控卡在 macOS。

---

## 1. 架构：ruzu 长什么样

```
Cargo workspace
├── src/common          ← 宿主抽象：host_memory(大块 VA 预留)、fs、page table
├── src/core            ← 模拟器核心（HLE 服务、内核、ARM 接口）
│   ├── arm/dynarmic    ← rdynarmic 后端（x86_64 走 rxbyak / aarch64 走 rhazel）
│   └── arm/nce         ← 原生执行后端，**只在 aarch64 linux/android 编译**
├── src/rdynarmic       ← dynarmic 的 Rust 移植，自带 IR/优化/codegen
├── src/shader_recompiler  ← Maxwell → SPIR-V
├── src/video_core      ← 4 个后端：vulkan / opengl / metal(macOS) / null
├── src/audio_core      ← cubeb + sdl3
├── src/input_common    ← sdl3-sys（libusb / joycon-hid 默认关）
├── src/hid_core / network / web_service / frontend_common
├── src/ruzu            ← **GTK4 图形前端**（gtk4 0.9, v4_6）+ 每平台 render_window_*.rs
└── src/ruzu_cmd        ← CLI 前端
```

要点：
- **没有 CI**（仓库里无 `.github/`），`.cargo/config.toml` 只处理 musl/openbsd/freebsd。
- 已有一处 iOS 痕迹：`src/core/src/game_settings.rs` 的 `detect_os()` 里有 `Os::Ios` 分支 —— 上游（yuzu/Eden）就带这个枚举，说明作者对 iOS 不排斥。

## 2. 三块关键资产（好消息）

| 资产 | 位置 | 为什么对 iOS 重要 |
|---|---|---|
| **Metal 渲染后端** | `src/video_core/src/renderer_metal/`（约 40 个文件，含 `metal_layer.rs` / `metal_presenter.rs` / `present/layer.rs`） | 已经是真·Metal 实现（不是 Vulkan 翻译层）。依赖 `objc2 0.6 / objc2-metal 0.3.2 / objc2-quartz-core / objc2-foundation / block2` —— **这些 crate 本身就是跨 macOS+iOS 的**，iOS 侧几乎零改动 |
| **AArch64 宿主 JIT** | `src/rdynarmic/src/backend/arm64/`（40+ 文件，codegen 走 `rhazel` 的 `block_of_code`） | iOS 就是 aarch64，直接复用 Apple Silicon 那条路径 |
| **MAP_JIT + 写保护** | `rhazel/src/block_of_code.rs:26,300-345`：`mmap(MAP_JIT)` + `pthread_jit_write_protect_np()`，当前门控 `all(target_os="macos", target_arch="aarch64")` | **这正是 iOS 上取得可写可执行内存的标准姿势**，函数在 iOS 上同样存在（JSC 就用它）。改成 `any(macos, ios)` 即可 |

## 3. 阻塞点清单（按严重度）

### 🔴 致命 / 必须先解决

| # | 阻塞点 | 证据 | 说明 |
|---|---|---|---|
| B1 | **JIT 权限**。iOS 禁止 W^X，模拟器必须拿到可执行内存 | `rhazel/block_of_code.rs` 用 `MAP_JIT` | 无 JIT 权限 = 无法运行（dynarmic 无解释器后备）。需要：越狱设备 / TrollStore / 或 StikDebug·StosVPN 之类「挂调试器放开 CS_DEBUGGED」的方案。**这一条决定整个项目成败** |
| B2 | **GTK4 前端无法上 iOS** | `src/ruzu/Cargo.toml`（gtk4 0.9）、`render_window_{x11,wayland,windows,macos}.rs` | GTK 没有 iOS 后端。必须新写 Swift/UIKit 外壳，或在 SDL3 上做前端 |
| B3 | **客户机内存布局** | `src/common/src/host_memory.rs` 大量 `cfg(any(target_os="linux", target_os="macos"))` | iOS 要加进这些 cfg；另需确认 iOS 上大块 VA 预留（数 GiB）+ `MAP_NORESERVE` 行为、以及 jetsam 内存上限（需要 `com.apple.developer.kernel.increased-memory-limit` 之类 entitlement） |

### 🟠 困难但可控

| # | 阻塞点 | 证据 | 处理 |
|---|---|---|---|
| B4 | Metal 后端的 iOS 适配 | `video_core/src/lib.rs` L32：`#[cfg(target_os = "macos")] pub mod renderer_metal;`；Cargo.toml 里 objc2 系列也在 macos cfg 下 | cfg 扩到 ios；把 `metal_layer.rs` 的 `NSView`/`QuartzCore` 换成 `UIView`（`objc2-ui-kit`）挂 `CAMetalLayer`；presenter 的 present/drawable 尺寸逻辑复用 |
| B5 | 音频后端 | `src/audio_core/Cargo.toml`：`cubeb` + `sdl3` | cubeb 的 CoreAudio 后端在 Darwin 上编译，但 iOS 支持需实测。**建议**：先试 cubeb，失败就写个 ~200 行 AudioUnit（`AudioUnit`/`AVAudioEngine`）后端替换，音频是后期需求 |
| B6 | 输入后端 | `src/input_common`：sdl3-sys | iOS 用 `GCController` + 触摸屏映射。SDL3 在 iOS 上会抢 `UIApplication`/UIWindow，和自建 UIKit 外壳冲突，**建议 cfg 掉 SDL 输入引擎**，写 UIKit 驱动 |
| B7 | OpenSSL | `src/core/Cargo.toml`：`openssl`（除 openbsd 外全平台）；用途仅 `hle/service/ssl/ssl_backend_openssl.rs` + `am/application_functions.rs` 一处 sha1 | iOS 无系统 OpenSSL，`vendored` 交叉编译又重。**建议**：iOS 上 cfg 掉 SSL 服务（联机功能本来也不是第一优先级），sha1 换纯 Rust 实现 |

### 🟡 会咬人但不致命

- `video_core` 无条件依赖 `rxbyak`（x86 汇编器）—— aarch64 编译时是否通过需实测，不行就在 Cargo.toml 里 target-gate 掉。
- `spirv-cross2` 需要为 ios 交叉编译 C++（走 `cc` + iOS SDK，CI 上可行）。
- `common` 的路径逻辑要走 iOS 沙盒（`Documents/`），存档/固件/keys 放哪要定规矩。
- Metal 后端在仓库里标着 alpha，iOS 上先行验证的是「能出画面」，不是「能玩 3A」。

## 4. 建议路线（选定）

**Swift/UIKit 外壳 + Rust 静态库（.a）+ 原生 Metal 后端**，不做 GTK、不做 SDL 窗口、不做 MoltenVK。

```
┌─────────────────────────────────────────────┐
│ Ruzu.app (Swift/UIKit)                      │
│  • UIViewController + CAMetalLayer 视图     │
│  • 文件导入 / 游戏库列表（后期）            │
│  • 触摸 + GCController 输入                 │
│           ↓ C ABI (static lib)              │
│  libr uzu_ios.a                             │
│  • ruzu_ios_* FFI 层 (新 crate: src/ruzu_ios)│
│  • EmuWindow 实现 → 把 CAMetalLayer 交给    │
│    renderer_metal 的 presenter              │
│  • 模拟线程 / 生命周期管理                  │
│           ↓                                 │
│  core + video_core(metal) + audio_core + …  │
└─────────────────────────────────────────────┘
```

备选路线（不推荐，仅记录）：
- **SDL3 前端**：SDL3 原生支持 iOS（窗口/Metal/CoreAudio/GCController），改动量可能更小，但现有 `src/ruzu` 是 GTK，SDL 前端得自己搭，且 SDL 的 `UIApplication` 接管会和自建外壳打架 → 除非放弃自建 UI。
- **MoltenVK + Vulkan 后端**：绕开 Metal 后端适配，但多一层翻译、性能与包体都更差，且 Metal 后端已经存在，没必要。

## 5. 分阶段路线图

| 阶段 | 目标 | 产出/判定标准 |
|---|---|---|
| **P0 探针** | 证明核心能交叉编译到 `aarch64-apple-ios` | CI 上 `cargo check --target aarch64-apple-ios` 过 common/core/rdynarmic/shader_recompiler/video_core/hid_core/input_common/audio_core。产出完整错误清单 → 逐个开补丁 |
| **P1 JIT 冒烟** | 在真机上跑起 ARM64 JIT | `rhazel` cfg 放开 ios；一个只做 `mmap(MAP_JIT)` → 写 aarch64 指令 → 调用 → 返回的 App；用 StikDebug/越狱验通 |
| **P2 出画面** | Metal 后端在 iOS 上渲染一帧 | Metal crate cfg 放开；`UIView`+`CAMetalLayer` 接入 presenter；先画"清屏 + 三角形/空跑 present" |
| **P3 跑家酿** | 跑 FreeBrick（作者自己的免费 .nro） | 无音频、无输入，能进画面/能按键 |
| **P4 可用** | 输入 + 音频 + 游戏库 + 固件/keys 导入 | 真机玩商业游戏（性能另说） |

## 6. 补丁清单（P0/P1 需要动的具体位置）

```rust
// ☑ P1 — externals/rhazel/src/block_of_code.rs  (JIT 内存)
- #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
+ #[cfg(all(any(target_os = "macos", target_os = "ios"), target_arch = "aarch64"))]
  extern "C" { fn pthread_jit_write_protect_np(enabled: i32); }
  // 共 4 处 cfg，含 mmap_code_cache 的 MAP_JIT 分支

// ☑ P1 — src/core/src/arm/mod.rs
  #[cfg(all(target_arch = "aarch64", any(target_os="linux", target_os="android")))]
  pub mod nce;   // iOS 不编 NCE（它要改写代码页），走 dynarmic JIT

// ☑ P2 — src/video_core/src/lib.rs
- #[cfg(target_os = "macos")]
+ #[cfg(any(target_os = "macos", target_os = "ios"))]
  pub mod renderer_metal;

// ☑ P2 — src/video_core/Cargo.toml
- [target.'cfg(target_os = "macos")'.dependencies]
+ [target.'cfg(any(target_os = "macos", target_os = "ios"))'.dependencies]
  objc2 / objc2-metal / objc2-quartz-core / objc2-foundation / block2
  (+ 新增 objc2-ui-kit)

// ☑ P3 — src/common/src/host_memory.rs
- #[cfg(any(target_os = "linux", target_os = "macos"))]
+ #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]

// ☑ P3 — src/core：iOS 上 cfg 掉 SSL/OpenSSL
```

新增：
- `src/ruzu_ios/`（cdylib + staticlib crate，C ABI 门面）
- `ios/RuzuApp/`（Xcode 工程 / Swift Package，UIKit 外壳）
- `ios/RuzuApp/Ruzu.entitlements`（`get-task-allow` / JIT 相关）
- `.github/workflows/ios.yml`（macos-14 runner 交叉编译 + 打未签名 IPA）

## 7. CI 方案（免费 macOS runner）

```yaml
# 要点
runs-on: macos-14              # Apple Silicon
rustup target add aarch64-apple-ios
git clone --recurse-submodules  # rhazel 必须；rxbyak 若在 aarch64 上编译失败则 target-gate
cargo build --release --target aarch64-apple-ios -p ruzu_ios
xcodebuild -project ios/RuzuApp.xcodeproj -sdk iphoneos \
  CODE_SIGNING_ALLOWED=NO -configuration Release
# 组装 .app + Payload/ → 未签名 .ipa（用 AltStore/SideStore/TrollStore 侧载）
```

风险：SwiftPM/crates.io 拉取偶发超时（MeloZu 已踩过），workflow 里加 HTTP/1.1 + 重试。

## 8. 待确认（决定下一步怎么做）

1. **设备与 JIT**：机器是越狱 / TrollStore / 还是只能用 StikDebug·SideStore 那种「挂调试器开 JIT」？——决定 B1 怎么解，也决定最低 iOS 版本。
2. **仓库**：是把 `ruzu-emu` fork 到你账号下（例如 `ruzu-emu-ios`）改，还是新建一个「只放 iOS 增量」的仓库？我需要一个可供 push 的 token 或你自己来 push。
3. **优先级**：先跑通「家酿 .nro 出画面」（技术验证），还是直奔商业游戏？
