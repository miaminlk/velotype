# Velotype.dll 当前实现记录

本文记录 Velotype 自用分支把 Markdown 编辑器嵌入为 Windows DLL 控件的当前实现。目标不是重写 Markdown 渲染器，而是把 Velotype 原有 Markdown/Theme/GPUI 渲染链路嵌入到宿主 HWND 中。

## 总体结构

当前实现分为三层：

1. `src/windows_control.rs`
   - DLL 导出入口。
   - 注册 `Velotype` Win32 窗口类。
   - 创建宿主可管理的外层 child HWND。
   - 在独立线程启动 GPUI `Application`。
   - 通过 `mpsc` 把宿主窗口消息转换为 Editor 更新命令。
   - 暴露 Markdown 显示/渲染相关 DLL API，例如设置内容、切换主题/语言、生成纯文本显示内容、生成 HTML。

2. `crates/gpui/`
   - 本地修改版 GPUI 0.2.2。
   - 根 `Cargo.toml` 通过 `gpui = { path = "crates/gpui", features = ["runtime_shaders"] }` 使用它。
   - 修改 Windows 后端，使 `WindowOptions` 可以传入父 HWND 并创建真正的 `WS_CHILD` GPUI 窗口。

3. `src/editor/*`
   - 继续使用 Velotype 原有 Markdown 解析、文档树、Block 渲染、Theme 和 GPUI 渲染流程。
   - DLL 控件使用 `Editor::from_markdown_embedded()`，只显示 Markdown 内容区，不显示 exe 上方的菜单和标题栏。

## DLL 与 exe 的功能边界

`velotype.exe` 仍然使用真实 `app_menu`、文件打开/保存、最近文件、偏好设置、CLI 安装、更新检查等应用层功能。

`velotype.dll` 是嵌入式 Markdown 显示控件，当前 library crate 在非测试构建中使用 no-op app-menu shim：

- 不安装 native/in-window menu。
- 只默认注册 `BlockEditor` 上下文的编辑快捷键，例如 Enter、Backspace、方向键、选择、复制/粘贴、Undo、格式化和缩进。
- 不默认注册 exe 的文件操作快捷键，例如 `Ctrl+S`、`Ctrl+O`、`Ctrl+N`、`Ctrl+Q`。
- 宿主注册 `save` / `save_as` 事件后，DLL 会额外注册对应保存热键，但只通过窗口消息通知宿主，不在 DLL 内部执行文件保存。
- 宿主可以通过 `Velotype_SetEditorKeyBinding` 或 `editor.keybinding.<id>` 属性覆盖编辑热键及 `save_document` / `save_document_as` 事件热键；其它文件/菜单命令仍被拒绝。
- `OpenFile` / 最近文件 / CLI 安装等菜单入口不作为 DLL 控件能力暴露。
- 宿主需要的操作通过 DLL API 完成，例如 `Velotype_SetMarkdown`、`Velotype_SetTheme`、`Velotype_SetLanguage`。

这避免 DLL 控件把宿主不需要的应用级菜单/文件操作行为带入嵌入场景，同时不影响 `velotype.exe`，因为 exe crate 仍然从 `src/main.rs` 加载真实模块。

## 子控件键盘焦点

GPUI 原 Windows 后端主要面向顶级窗口；鼠标点击由 GPUI 处理后不一定会继续走 Windows 默认焦点处理。嵌入为 `WS_CHILD` 后，这会导致控件内部可显示 caret，但 Windows 键盘焦点仍停留在宿主窗口，表现为不能输入。

本分支在 GPUI Windows backend 的 mouse-down 处理里对当前 GPUI child HWND 调用 `SetFocus`，保证后续 `WM_KEYDOWN` / `WM_CHAR` 进入 GPUI 子控件。DLL 初始化同时安装 BlockEditor 编辑快捷键，保留输入、删除、导航、选择、复制/粘贴等编辑行为，但不恢复菜单/文件操作快捷键。

## DLL 编辑热键配置

`src/components/actions.rs` 保留完整 exe 快捷键定义，但额外提供 DLL 专用解析路径：

- `resolved_block_editor_keybindings(config)`
- `install_block_editor_keybindings_with_config(cx, config)`
- `resolved_dll_host_event_keybindings(config, events)`
- `install_dll_host_event_keybindings_with_config(cx, config, events)`
- `is_dll_host_shortcut_id(id)`

`src/windows_control.rs` 的 `ControlOptions::editor_keybindings` 保存宿主配置。初始化 GPUI 子控件时安装 BlockEditor keymap，并在宿主事件中包含 `save` / `save_as` 时额外安装对应保存事件 keymap。运行中调用 `Velotype_SetEditorKeyBinding` 或更新事件集合会清空当前 DLL keymap 并按最新配置重新安装。因为 DLL GPUI application 不安装菜单/open/quit 等 keymap，`clear_key_bindings()` 不会移除宿主需要的菜单能力。

## 统一属性和事件通知

`Velotype_SetProperty` / `Velotype_GetProperty` 提供类似 `libmpv.dll` 的统一字符串属性入口。当前内置属性覆盖：

- 控制层：初始化状态、外层背景色、内部 GPUI child HWND、可见性。
- 文档层：Markdown 源文本、长度、display text、HTML。
- 主题/语言层：主题 ID、语言 ID、`theme.parameter.<name>` 参数。
- 编辑器层：caret 位置/隐藏状态、编辑与保存事件热键。
- 事件层：`event.names`、`event.message`、`event.notify_hwnd`、`event.last`。

未知属性会保存在 `ControlOptions::properties`，方便宿主先通过统一入口写入，再逐步把需要的属性扩展为真实行为。

事件由 `EmbeddedEventBridge` 连接 Editor 与 Windows 控制层：

1. 宿主调用 `Velotype_RegisterEventCallback(hwnd, notify_hwnd, message_id, L"save|change|")` 或设置 `event.names` 属性。
2. DLL 保存事件集合并把保存事件 keymap 安装到 GPUI。
3. `Editor::mark_dirty()` 触发 `change`；`Editor::on_save_document()` 触发 `save`。
4. 如果对应事件已注册，bridge 记录 `event.last` 并调用 `PostMessageW(notify_hwnd, message_id, (WPARAM)hwnd, event_code)`。
5. `save` 事件已注册时，`on_save_document()` 直接返回，不调用原始保存流程，因此不会弹出保存对话框或自行处理文件。

## GPUI 修改点

本地 GPUI 在 `crates/gpui/`，来源是 `gpui 0.2.2`。主要修改：

- `crates/gpui/src/platform.rs`
  - `WindowOptions` 增加 Windows-only `parent_handle: Option<isize>`。
  - `WindowParams` 增加 Windows-only `parent_handle: Option<isize>`。
  - 默认值为 `None`，所以普通 exe/top-level window 不受影响。

- `crates/gpui/src/window.rs`
  - 从 `WindowOptions` 读取 `parent_handle`。
  - 传递给平台层 `WindowParams`。

- `crates/gpui/src/platform/windows/window.rs`
  - 当 `parent_handle.is_some()` 时创建：
    - `WS_CHILD`
    - `WS_VISIBLE`
    - `WS_CLIPCHILDREN`
    - `WS_CLIPSIBLINGS`
  - 父窗口设为传入的 HWND。
  - 初始位置使用 `(0, 0)` 和外部传入尺寸，而不是顶级窗口的 `CW_USEDEFAULT`。
  - 跳过 top-level window 专用逻辑，例如 DWM border/drag-drop/titlebar 偏移。

GPUI 仍然使用自己的 Windows message procedure、DirectX renderer、swap chain 和渲染管线；DLL 只负责给它一个可嵌入的父 HWND。

## DLL 控件生命周期

外层窗口是 `src/windows_control.rs` 注册的 Win32 child control，窗口类名为 `Velotype`。外层窗口用于：

- 接收宿主的 Win32 消息。
- 保存当前 Markdown 源文本。
- 管理内部 GPUI child HWND。
- 转发 `WM_SIZE` 到内部 GPUI child HWND。
- 在 `WM_NCDESTROY` 时发送 `ControlCommand::Close` 结束 GPUI Application。

内部 GPUI child HWND 由 `start_gpui_child()` 创建：

1. 新线程 `VelotypeGpuiControl` 启动 `Application::new().with_assets(...).run(...)`。
2. 初始化：
   - `I18nManager::init_with_language_id`
   - `ThemeManager::init_with_theme_id`
   - 应用 `ControlOptions::theme_params`
   - 安装 BlockEditor 编辑热键和已注册的宿主事件热键
   - `init_editor`
3. `WindowOptions.parent_handle = Some(host_hwnd)`。
4. `cx.open_window(...)` 创建 GPUI child window。
5. 创建 `Editor::from_markdown_embedded(...)`。
6. 通过 `PostMessageW(host_hwnd, VTM_CHILD_READY, child_hwnd, 0)` 通知外层窗口内部 HWND 已经创建。

## 创建、初始化、显示分离

旧入口 `Velotype_CreateAsChildControl(...)` 仍保留，内部等价于：

1. 创建外层 HWND。
2. 初始化 GPUI/Editor。
3. 显示控件。

新的分离式流程是：

1. `Velotype_CreateControlEx(&params)`
   - 只创建外层 Win32 child control。
   - 默认不初始化 GPUI，不显示窗口。

2. `Velotype_InitializeControl(hwnd, markdown)`
   - 启动 GPUI Application。
   - 创建内部 GPUI child HWND。
   - 使用完整 Velotype Markdown 渲染链路渲染传入 Markdown。

3. `Velotype_ShowControl(hwnd, TRUE)`
   - 显示外层 control。

这样宿主可以先创建控件，再配置/传入内容，最后显示。

## 初始化背景、页面背景和 caret

外层 Win32 控件类的 `hbrBackground` 使用 `CreateSolidBrush(0x00F0F0F0)`，并且在 GPUI child HWND 创建完成前的 `WM_PAINT` 中用同一颜色填充客户区，避免初始化阶段出现黑色矩形。宿主可通过 `Velotype_SetControlBackgroundColor(hwnd, color_bgr)` 覆盖外层背景色。

DLL 默认主题仍为 `velotype-light`，但 Light theme 的 `editor_background` 调整为 `0xFFFFFF`，使 Markdown 页面默认是白色。宿主可通过 `Velotype_SetThemeParameter` 覆盖页面背景、字体、字号、行高、段落间距、padding、cursor 宽度等参数；该配置可在初始化前保存，初始化后动态应用。

嵌入式 Editor 默认创建为无 active focus：加载 Markdown 后不显示 caret。宿主需要显示输入光标时调用 `Velotype_SetCaretPosition(hwnd, line, column)`；需要再次隐藏时调用 `Velotype_HideCaret(hwnd)`。

## Markdown 渲染相关 DLL API

菜单中和显示/渲染相关、但对宿主仍有价值的能力迁移为显式 DLL API：

- `Velotype_SetMarkdown`
  - 设置控件 Markdown 内容。
- `Velotype_GetMarkdownLength` / `Velotype_GetMarkdown`
  - 读取控件当前 Markdown 源文本。
- `Velotype_SetTheme`
  - 切换内部 GPUI Editor 主题，默认 `velotype-light`。
- `Velotype_SetLanguage`
  - 切换内部 GPUI Editor 语言，默认 `en-US`。
- `Velotype_SetControlBackgroundColor`
  - 设置外层 Win32 child control 在 GPUI 子窗口就绪前的背景色。
- `Velotype_SetThemeParameter`
  - 设置页面背景、字体、字号、行高、块间距等主题/排版参数。
- `Velotype_SetCaretPosition` / `Velotype_HideCaret`
  - 显式控制 caret 显示位置；默认不显示 caret。
- `Velotype_MarkdownToDisplayText`
  - 把 Markdown 转为纯文本显示内容。
- `Velotype_RenderMarkdownToHtml`
  - 使用 Velotype HTML export renderer 生成带主题 CSS 的 HTML 字符串。

文件选择、保存路径 prompt、最近文件、偏好设置窗口等不迁移为 DLL API；这些属于宿主应用职责。

## 去掉菜单和标题栏

DLL 控件不再调用 `app_menu::init()`，并且使用：

```rust
Editor::from_markdown_embedded(cx, markdown, None, hide_caret)
```

`from_markdown_embedded()` 会把 `chrome_visible` 设为 `false`。渲染时：

- `custom_titlebar_height = 0`
- `in_window_menu_bar_height = 0`
- 不渲染 `render_custom_titlebar`
- 不渲染 in-window menu bar/panel

因此 DLL 控件顶部不再出现 exe 的标题栏或菜单，只保留 Markdown 内容显示效果。

## Markdown 更新

宿主可以使用：

- `WM_SETTEXT`
- `VTM_SETMARKDOWN`
- `Velotype_InitializeControl(hwnd, markdown)`

更新源 Markdown。已初始化时，外层窗口向 GPUI 线程发送：

```rust
ControlCommand::SetMarkdown(markdown)
```

GPUI 线程调用：

```rust
editor.replace_markdown(markdown, cx)
```

这会重新解析 Markdown、替换文档根节点、重建表格/图片 runtime，并触发 GPUI 刷新。

## 测试脚本

`scripts/test_velotype_dll.ahk` 使用 AHK v2：

- 加载 `target/release/velotype.dll`
- 创建 AHK GUI
- 读取仓库 `README.md`
- 调用 `Velotype_CreateControlEx`
- 调用 `Velotype_InitializeControl`
- 调用 `Velotype_ShowControl`
- 调用 `Velotype_SetTheme` / `Velotype_SetLanguage`
- 调用 `Velotype_SetEditorKeyBinding` 验证编辑热键可自定义，并验证 `open_file` 等文件命令会被拒绝
- 调用 `Velotype_MarkdownToDisplayText` / `Velotype_RenderMarkdownToHtml` 验证渲染相关 API 可用
- 可选 `--test-input` 会点击 GPUI 子控件、发送文本，并用 `Velotype_GetMarkdown` 验证实际文档已更新
- 在 AHK `WM_SIZE` 中用 `MoveWindow` 调整控件大小

## Release DLL 体积调查

当前 `target/release/velotype.dll` 在剥离 DLL 菜单/快捷键初始化前约 37 MB；引入 DLL app-menu no-op shim、移除 DLL 默认 keybinding/http client 初始化后约 35 MB（实测 36,405,248 bytes）。`target/release/velotype.exe` 约 34 MB。根 `Cargo.toml` 已启用：

```toml
[profile.release]
codegen-units = 1
lto = true
opt-level = "s"
panic = "abort"
strip = true
```

因此体积不是因为未 strip 或未 LTO，而是来自完整 Velotype/GPUI 功能链路。

`cargo bloat --release --crates` 对 exe 的 `.text` 分析显示主要来源包括：

- `std`
- `velotype`
- `serde`
- `mermaid_rs_renderer`
- `gpui`
- `chromiumoxide_cdp`
- `tiny_skia`
- `regex_automata`
- `rustls`
- `ratex_parser`
- `usvg`
- 多个 tree-sitter grammar
- image/webp/exr/jpeg/svg/text shaping 相关 crates

这说明 DLL 体积大主要来自“复用完整 Markdown/GPUI 渲染能力”的依赖集合，而不是单独 GPUI child-window 胶合代码。尤其是：

- Mermaid 渲染链路：`mermaid-rs-renderer`、`chromiumoxide`、`chromiumoxide_cdp`
- 网络/更新/http 链路：`reqwest`、`rustls`、`aws_lc_*`
- 代码高亮：多个 `tree-sitter-*`
- 图片/SVG/字体排版：`image`、`usvg`、`tiny-skia`、`rustybuzz`、`ttf-parser`
- LaTeX/公式：`ratex-*`

当前已经能通过 no-op app-menu shim 去掉 DLL 菜单/文件操作行为，但更明显的体积下降需要继续 feature-split 渲染能力。若未来要把 DLL 从“完整 Velotype 渲染能力”裁剪成更小的“Markdown 阅读控件”，建议新增专用 feature，例如 `dll-control`：

- 默认保留 core Markdown + GPUI + Light theme。
- 可选关闭 Mermaid。
- 可选关闭在线 http client / update check。
- 可选关闭部分 tree-sitter 官方语言包。
- 可选关闭 heavyweight image formats，例如 EXR/WebP。
- 可选关闭 exe-only app menu、配置导入导出、CLI 安装等逻辑。

但这会改变“DLL 和 exe 显示效果完全一致”的能力边界，需要按功能逐项确认后再做。
