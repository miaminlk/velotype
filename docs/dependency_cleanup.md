# Velotype.dll 冗余与无用依赖库清理计划

作为 Windows 平台下的 Markdown 渲染控件（编译目标为 `Velotype.dll`），控件的轻量性、零外部环境依赖和极速编译是核心要求。本篇文档详细梳理了当前项目中“0机率使用”或控件化后不再需要的依赖库，作为我们后续逐个清理的实施指南。

---

## 1. 待清理依赖库清单（按模块/类型分类）

### 类别一：非 Windows 平台的系统支撑库（0% 使用率）
这类库是 GPUI 框架为 macOS 或 Linux 平台设计的底层桥接依赖。在 Windows DLL 控件中，有 Windows SDK / DirectWrite / DirectX11 驱动，以下库完全冗余：
*   **macOS 特有**：
    *   `objc2`、`objc2-metal`（苹果系统的 OC 运行时与 Metal 渲染底层）
    *   `cocoa`、`cocoa-foundation`、`core-foundation`、`core-foundation-sys`、`core-graphics`、`core-text`、`core-video`、`metal`、`objc`
*   **Linux/Unix 特有**：
    *   `x11`、`x11rb`、`x11-clipboard`（X11 窗口与剪贴板）
    *   `wayland`、`wayland-backend`、`wayland-client`、`wayland-cursor`、`wayland-protocols`（Wayland 窗口系统接口）
    *   `xkbcommon`、`xim`（键盘布局与输入法处理）
    *   `ashpd`（Flatpak 沙箱桌面门户桥接）
    *   `oo7`（Linux D-Bus 密钥环服务）
    *   `open`（Linux 下调用默认浏览器打开链接，Windows 直接使用系统 `ShellExecuteW` 实现）
*   **清理方案**：在 `crates/gpui/Cargo.toml` 中，通过移除平台特定的 `features` 选项，或将对应依赖从平台 `[target.'cfg(...)'.dependencies]` 中禁用。

---

### 类别二：多媒体与屏幕捕获组件（0% 使用率）
渲染控件不包含音视频流捕获或播放场景：
*   **`scap` (zed-scap)**：跨平台屏幕截图/录像库。
*   **`media` (gpui_media)**：GPUI 的多媒体播放层。
*   **清理方案**：完全移除 `scap` 相关的 feature 标记以及代码中的条件编译块；在 Cargo.toml 中彻底删除。

---

### 类别三：无头浏览器控制器（0% 使用率）
*   **`chromiumoxide`**（位置：根目录 [Cargo.toml](file:///d:/float/OneDrive/ONE/velotype/Cargo.toml)）：
    *   **分析**：用于自动化控制 Headless Chrome/Chromium 浏览器。渲染控件不需要调用或运行外部浏览器内核进行文档导出或渲染。
    *   **清理方案**：直接从主项目的 `Cargo.toml` 依赖中删除，并清理 `src/` 中与其相关的任何桥接/调用逻辑。

---

### 类别四：跨平台排版引擎与转译器（0% 使用率）
*   **`font-kit` (zed-font-kit)** & **`cosmic-text`**：
    *   **分析**：GPUI 在 Windows 平台上直接调用 DirectX 的 `DirectWrite` API 执行高保真字体查找与段落排版，完全不需要这两款跨平台垫片库。
*   **`naga`**：
    *   **分析**：在构建期用于将 WGSL 着色器代码转译。Windows 平台由于使用 DirectX11 渲染器且采用 HLSL 原生驱动，编译期完全无须编译 WGSL。

---

### 类别五：网络请求协议栈（建议剥离）
*   **`reqwest`**（位置：主项目 Cargo.toml） & **`gpui_http_client`**（位置：GPUI 依赖）：
    *   **分析**：原项目用于拉取 Markdown 中的网络图片。内置一整套网络协议栈会让 DLL 文件的最终体积增大数 MB，且会带来各种网络配置及安全沙箱问题。
    *   **清理方案**：彻底剥离 DLL 内部的网络发起逻辑。控件在解析到网络图片时，通过 FFI 回调接口向外（宿主程序）发送事件，由宿主下载并直接传递 BGRA 像素缓冲指针给控件，从而使 DLL 内部网络依赖清零。

---

## 2. 逐个清理的执行计划（TODO 检查列表）

我们将在接下来的步骤中，按照“依赖依赖性最弱 -> 逐步深入”的原则依次进行清理：

* [x] **任务 1**：移除主项目 `Cargo.toml` 中的 **`chromiumoxide`** 依赖。
    *   清除可能残留的无头浏览器驱动或 PDF 导出逻辑。
* [x] **任务 2**：在主项目和子项目中移除 **`scap`** 依赖。
    *   关闭 `scap` 相关的特征标记（Feature flags），修改 `gpui` 中与录屏相关的条件编译宏。
* [x] **任务 3**：关闭/禁用 Windows target 下无用的跨平台 Features。
    *   已成功移除 macOS (Metal, Cocoa, Core-Video等)、Linux (X11, Wayland, ashpd等) 特有的目标依赖项与特征配置。
* [ ] **任务 4**：重构网络请求逻辑。
    *   在 `gpui` 中隔离 `http_client` 和主项目的 `reqwest`，设计简易的图片加载外部 FFI 回调接口。
