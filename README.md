# ruzu → iOS：构建与使用说明

给 [liaojinxuan666-eng/ruzu-IOS](https://github.com/liaojinxuan666-eng/ruzu-IOS) 的 iOS 移植增量。
**以「不越狱」为主路线**，越狱只当作可选的加速/兜底手段。

```
.github/workflows/ios.yml            CI：纯手动触发，探针 → 交叉编译 → Xcode → 未签名 IPA
ios/project.yml                      XcodeGen 工程描述（CI 里生成 Ruzu.xcodeproj）
ios/RuzuApp/                         Swift/UIKit 外壳
ios/RuzuApp/RuzuApp.entitlements            不越狱路线（只含 get-task-allow）
ios/RuzuApp/RuzuApp-jailbreak.entitlements  越狱路线附加权限（可选）
ios/patches/apply_ios_patches.py     把上游源码就地改成能编 iOS（幂等，已本地实测）
ios/ci/probe.sh                      逐 crate 交叉编译探针
src/ruzu_ios/                        Rust 门面 crate（C ABI）→ libr uzu_ios.a
```

---

## 零、怎么把这些文件放进仓库

iOS 的「文件」App 建不了 `.` 开头的文件夹，所以 `.github/workflows/ios.yml` 必须**在 GitHub 网页上新建**：

1. 仓库页 → **Add file** → **Create new file**
2. 文件名框里直接输入整条路径：`.github/workflows/ios.yml`
   （斜杠不用管，GitHub 会自动把目录建出来）
3. 把本仓库 README 里附的 YAML 内容粘进去 → Commit

其余文件（`ios/`、`src/ruzu_ios/`）用 **Add file → Upload files** 直接传目录即可。

---

## 一、怎么触发 CI

**纯手动，push 不会自动跑**（改代码不用担心误触发）：

> 仓库页 → **Actions** → 左侧选 **ios** → 右上 **Run workflow** → 勾选项 → **Run workflow**

两个勾选项：

| 输入 | 默认 | 作用 |
|---|---|---|
| `run_probe` | ✅ | 逐 crate 交叉编译探针（收集 iOS 编译错误），失败不阻塞 |
| `build_ipa` | ✅ | 构建 Rust 静态库 + Xcode 工程 + 打包 IPA |

想快速拿 IPA 就把 `run_probe` 取消勾选（省十几分钟）；想摸底上游代码对 iOS 的适配度就两个都勾。

一次运行约 15~30 分钟，产出 artifact `ruzu-ios-<sha>`：

| 文件 | 用途 |
|---|---|
| `Ruzu-unsigned.ipa` | 未签名 IPA，SideStore / AltStore / TrollStore 侧载 |
| `RuzuApp.entitlements` | 默认权限（不越狱用） |
| `Ruzu-jailbreak.entitlements` | 越狱附加权限（不越狱别用） |
| `probe-logs/` | 各 crate 编译日志 + 通过/失败清单 + 首要错误汇总 |

---

## 二、装到手机上（不越狱路线）

1. 用 **SideStore**（或 AltStore）侧载 `Ruzu-unsigned.ipa`，它用你的开发者证书重签。
   侧载时**不要**套 `Ruzu-jailbreak.entitlements` —— 里面有普通证书没有授权的键，装了会失败。
2. **JIT 要单独开**（这是唯一一道硬门槛）：
   免费证书签出来的 App 默认没有可执行内存权限。在设备上装 **StikDebug**（或同类工具），
   在启动 ruzu **之前**先给 ruzu 开 JIT。原理是让调试器 attach 本进程，
   进程被标成 `CS_DEBUGGED` 后 `mmap(MAP_JIT)` 就放行了。
3. 打开 App →「① 运行诊断」，看 JIT 那一行：
   - `[OK] MAP_JIT + 写保护` → 通了；
   - `[FAIL] ... errno=1/12` → JIT 没开成功，回到第 2 步重开一次再启动 App。

> 越狱设备同样可以走这条路（当前主路线），越狱只用来换取更大的内存上限和更少的签名麻烦。

## 三、越狱路线（可选，不作为主路线）

1. **TrollStore** 安装同一个 IPA，然后用越狱 entitlements 重签：
   ```
   ldid -SRuzuApp-jailbreak.entitlements Ruzu.app/Ruzu
   ```
2. 打开 App →「① 运行诊断」；官方路径不通再点「③ 越狱路径探测」。

「③ 越狱路径探测」在**没有** JIT 权限时可能直接闪退 —— 这是预期结果，闪退本身就是结论。重开 App 即可。

---

## 四、App 里三个按钮在验什么

| 按钮 | 验的东西 | 对应 ruzu 的哪块 |
|---|---|---|
| ① 运行诊断（安全） | `mmap(MAP_JIT)` + `pthread_jit_write_protect_np` + 真执行一段写进去的 aarch64 机器码；16/8/6/4/2/1 GiB 客户机 VA 预留实测；jetsam 内存上限 | `rdynarmic` → `rhazel::block_of_code` 的代码缓存；`common::host_memory` 的客户机内存 |
| ② Metal 自检 | 建 CAMetalLayer → 取 drawable → 清屏 present 一帧 | `video_core/renderer_metal` |
| ③ 越狱路径探测（可能闪退） | 裸 RWX、RW → `mprotect(RX)` 两条非官方路径 | 越狱兜底 |

①通 + ②通 = 模拟器所需的地基就位，可以开始接 `core` 跑家酿。
**内存和性能的数字（jetsam 上限、能预留多大的 VA）直接显示在诊断里，不用猜。**

---

## 五、本地（Mac）手动构建

```bash
git clone --recurse-submodules https://github.com/liaojinxuan666-eng/ruzu-IOS.git
cd ruzu-IOS

python3 ios/patches/apply_ios_patches.py     # 打补丁（幂等，可重复跑）

rustup target add aarch64-apple-ios
cargo build --release --target aarch64-apple-ios -p ruzu_ios

brew install xcodegen
cd ios && xcodegen generate && cd ..
xcodebuild -project ios/Ruzu.xcodeproj -scheme Ruzu -configuration Debug \
  -sdk iphoneos -destination 'generic/platform=iOS' \
  CODE_SIGNING_ALLOWED=NO build
```

用 Xcode 直接跑到真机时是**自带 JIT 的**（Xcode 启动的进程本来就被调试器 attach），
这是验证 JIT 通路最省事的办法，不用折腾 StikDebug。

---

## 六、补丁清单（`apply_ios_patches.py` 改了什么）

| 文件 | 改动 | 为什么 |
|---|---|---|
| `Cargo.toml` | workspace 加入 `src/ruzu_ios` | CI 才能 `cargo build -p ruzu_ios` |
| `externals/rhazel/src/block_of_code.rs` | `cfg(macos)` → `cfg(any(macos, ios))`（5 处），mmap 走 `MAP_JIT` | **最关键**：iOS 与 macOS(arm64) 的 JIT 机制完全相同，官方路径直接复用 |
| `src/video_core/src/lib.rs` | `renderer_metal` 参与 iOS 编译 | Metal 后端本就是跨 Apple 平台的 |
| `src/video_core/Cargo.toml` | objc2 系列依赖放开到 iOS；新增 `objc2-ui-kit` | Metal 视图在 iOS 上挂到 `UIView` / `CAMetalLayer` |
| `src/common/src/host_memory.rs` | 14 处 cfg 纳入 iOS；`map_noreserve()` 覆盖 iOS | 客户机内存走 POSIX 那套实现 |
| `src/core/Cargo.toml` | iOS 上用 `vendored` OpenSSL | iOS SDK 不自带 OpenSSL（只影响联机 SSL 服务） |

脚本**永远返回 0**：某个替换没匹配上（上游改了这几行）只打印 `[MISS]`，不中断构建。
看到 `[MISS]` 说明上游代码变了，需要同步更新补丁模式。

---

## 七、下一步（等第一轮诊断结果）

1. **①② 全绿** → 把 `core` 接进 `ruzu_ios`，实现 `EmuWindow`（把 `CAMetalLayer` 交给 Metal presenter），跑作者自己的家酿 `freebrick.nro` 验证「能出画面」。
2. **① 不通** → 先解决签名/权限（StikDebug 流程或换 Xcode 直连），这是硬门槛，绕不过。
3. 之后：输入（`GCController` + 触摸屏映射）、音频（cubeb 在 iOS 上能否编过，不行就写 AudioUnit 后端）、游戏库 UI、固件/keys 导入。
4. **性能与内存**（重点）：
   - 等输入/音频后端定了再统一 profile，避免过早优化；
   - 越狱设备可以启用 NCE（`core/src/arm/nce`，目前只在 aarch64 Linux/Android 编译）—— 原生执行，性能远超 JIT，但要改写客户机代码页，只有越狱做得到；
   - 内存上限：普通证书拿不到 `increased-memory-limit`，先靠诊断里的实测数字决定客户机内存给多大。
