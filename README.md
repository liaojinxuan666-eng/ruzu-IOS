# ruzu → iOS：构建与使用说明

这个目录是给 [liaojinxuan666-eng/ruzu-IOS](https://github.com/liaojinxuan666-eng/ruzu-IOS) 的 iOS 移植增量。
把本目录的内容按原样复制到仓库根目录（`.github/`、`ios/`、`src/ruzu_ios/` 三个位置），
push 到 `main` 就会自动跑完整流水线。

```
.github/workflows/ios.yml          CI：探针 → 交叉编译 → Xcode 构建 → 未签名 IPA
ios/project.yml                    XcodeGen 工程描述（CI 里生成 Ruzu.xcodeproj）
ios/RuzuApp/                       Swift/UIKit 外壳（AppDelegate / ViewController / Metal 自检）
ios/patches/apply_ios_patches.py   把上游源码就地改成能编 iOS（幂等，测试通过）
ios/ci/probe.sh                    逐 crate 交叉编译探针
src/ruzu_ios/                      Rust 门面 crate（C ABI），产出 libr uzu_ios.a
```

---

## 一、CI 产出什么

每次 push 到 `main`，Actions 里 `ios` 这个 workflow 会产出 artifact `ruzu-ios-<sha>`：

| 文件 | 用途 |
|---|---|
| `Ruzu-unsigned.ipa` | 未签名 IPA，用 TrollStore / SideStore / AltStore 侧载 |
| `RuzuApp.entitlements` | 侧载时需要的权限清单（JIT、内存上限） |
| `probe-logs/` | 每个 crate 对 `aarch64-apple-ios` 的编译日志 + 通过/失败清单 |

**设计要点**：探针步骤是 `continue-on-error` 的，所以哪怕 `core`/`audio_core` 现在还编不过，
IPA 也照样能出来 —— 保证你手机里随时有一个能打开、能跑诊断的 App。

---

## 二、装到手机上

### A. 不越狱（SideStore / AltStore + JIT）

1. 用 SideStore / AltStore 侧载 `Ruzu-unsigned.ipa`（它们会用你的免费开发者证书重签）。
2. **JIT 必须单独开**：免费证书签出来的 App 默认没有可执行内存权限。
   在设备上装 **StikDebug**（或等价工具），在启动 ruzu 之前先给它「开 JIT」。
3. 打开 App →「① 运行诊断」，看第一行 JIT 结果：
   - `[OK] MAP_JIT + 写保护` → 成了，可以继续；
   - `[FAIL] mmap(MAP_JIT) 失败 errno=...` → JIT 没开成功，回第 2 步。

### B. 越狱（TrollStore / 越狱环境）

1. 用 **TrollStore** 直接装 `Ruzu-unsigned.ipa`（它会自动签名）。
   需要 `dynamic-codesigning` 权限时，用 `RuzuApp.entitlements` 走 ldid：
   ```
   ldid -SEntitlements.plist Ruzu.app/Ruzu
   ```
2. 打开 App →「① 运行诊断」；如果官方路径失败，再点「③ 越狱路径探测」。
   越狱环境通常至少能过「裸 RWX」那条。

> 「③ 越狱路径探测」在**没有** JIT 权限时可能直接闪退 —— 这是预期结果，闪退本身就是结论。

---

## 三、App 里三个按钮在验什么

| 按钮 | 验的东西 | 对应 ruzu 的哪块 |
|---|---|---|
| ① 运行诊断（安全） | `mmap(MAP_JIT)` + `pthread_jit_write_protect_np` + 真执行一段 aarch64 机器码；再加 16/8/6/4/2/1 GiB 的 VA 预留测试；再报 jetsam 内存上限 | `rdynarmic` → `rhazel::block_of_code` 的代码缓存；`common::host_memory` 的客户机内存预留 |
| ② Metal 自检 | 建 CAMetalLayer → 拿 drawable → 清屏 present 一帧 | `video_core/renderer_metal`（真机 Metal 通路） |
| ③ 越狱路径探测（可能闪退） | 裸 RWX、RW→`mprotect(RX)` 两条非官方可执行内存路径 | 越狱环境下替 JIT 兜底 |

三个都过 = 模拟器所需的地基全部就位，可以开始接 `core` 了。

---

## 四、本地（Mac 上）手动构建

```bash
# 1. 取源码
git clone --recurse-submodules https://github.com/liaojinxuan666-eng/ruzu-IOS.git
cd ruzu-IOS

# 2. 打 iOS 补丁（幂等，可重复跑）
python3 ios/patches/apply_ios_patches.py

# 3. 交叉编译 Rust 静态库
rustup target add aarch64-apple-ios
cargo build --release --target aarch64-apple-ios -p ruzu_ios

# 4. 生成 Xcode 工程并构建
brew install xcodegen
cd ios && xcodegen generate && cd ..
xcodebuild -project ios/Ruzu.xcodeproj -scheme Ruzu -configuration Debug \
  -sdk iphoneos -destination 'generic/platform=iOS' \
  CODE_SIGNING_ALLOWED=NO build
```

想在真机调试时直接拿到 JIT，用 Xcode 跑（Xcode 启动的进程自带调试权限），
`RuzuApp.entitlements` 里的 `get-task-allow` 就是为这条路准备的。

---

## 五、补丁清单（`apply_ios_patches.py` 到底改了什么）

| 文件 | 改动 | 为什么 |
|---|---|---|
| `Cargo.toml` | workspace 加入 `src/ruzu_ios` | 让 CI 能 `cargo build -p ruzu_ios` |
| `externals/rhazel/src/block_of_code.rs` | `cfg(macos)` → `cfg(any(macos, ios))`（5 处），mmap 走 `MAP_JIT` | **最关键的一处**：iOS 与 macOS(arm64) 的 JIT 机制完全相同 |
| `src/video_core/src/lib.rs` | `renderer_metal` 参与 iOS 编译 | Metal 后端本来就是跨 Apple 平台的 |
| `src/video_core/Cargo.toml` | objc2 系列依赖放开到 iOS；新增 `objc2-ui-kit` | Metal 视图在 iOS 上要挂到 `UIView` / `CAMetalLayer` |
| `src/common/src/host_memory.rs` | 14 处 cfg 纳入 iOS；`map_noreserve()` 覆盖 iOS | 客户机内存管理走 POSIX 那套实现 |
| `src/core/Cargo.toml` | iOS 上用 `vendored` OpenSSL | iOS SDK 不自带 OpenSSL，只影响联机 SSL 服务 |

补丁脚本**永远返回 0**：某个替换没匹配上（比如上游改了这几行）只会打印 `[MISS]`，
不会中断构建。看到 `[MISS]` 就说明上游代码变了，需要同步更新补丁模式。

---

## 六、下一步（等诊断结果出来再定）

1. **诊断三连全绿** → 把 `core` 接进 `ruzu_ios`，实现 `EmuWindow`（把 `CAMetalLayer` 交给 Metal presenter），跑作者自己的家酿 `freebrick.nro` 验证「能出画面」。
2. **官方 JIT 路径不通** → 优先解决签名/权限（换越狱环境或确认 StikDebug 流程），这是硬门槛。
3. **之后**：输入（`GCController` + 触摸屏映射）、音频（cubeb 在 iOS 上能否编过，不行就写 AudioUnit 后端）、游戏库 UI、固件/keys 导入。
4. **性能与内存**（你的重点）：
   - 音频/输入后端确定后再做一轮 profile，避免过早优化；
   - 考虑把 NCE（`core/src/arm/nce`，目前只在 aarch64 Linux/Android 编译）在越狱环境下启用 —— 那是原生执行，性能远超 JIT，但需要改写客户机代码页，只有越狱才做得到；
   - 内存上限靠 `com.apple.developer.kernel.increased-memory-limit`；诊断里的 jetsam 数字会告诉你实际能拿到多少，不用瞎猜。
