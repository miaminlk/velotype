# Velotype.dll API

本文档描述当前 Windows x64 `velotype.dll` 导出 API。字符串均使用 UTF-16 little-endian、以 `NUL` 结尾，调用约定为 `extern "system"`。

## 窗口类

DLL 注册的窗口类名：

```text
Velotype
```

窗口过程支持常规 child HWND 消息，并支持本文档中的自定义消息。

## 自定义消息

所有自定义消息基于 `WM_USER`：

```c
#define VTM_SETMARKDOWN        (WM_USER + 1)
#define VTM_GETMARKDOWNLENGTH  (WM_USER + 2)
#define VTM_GETMARKDOWN        (WM_USER + 3)
#define VTM_INITIALIZE         (WM_USER + 4)
#define VTM_SHOW               (WM_USER + 5)
#define VTM_SETTHEME           (WM_USER + 6)
#define VTM_SETLANGUAGE        (WM_USER + 7)
#define VTM_NOTIFY_EVENT       (WM_USER + 65)
```

### `VTM_SETMARKDOWN`

```c
SendMessage(hwnd, VTM_SETMARKDOWN, 0, (LPARAM)L"markdown");
```

设置 Markdown 内容。控件已初始化时会异步更新 GPUI Editor。

### `VTM_GETMARKDOWNLENGTH`

```c
LRESULT len = SendMessage(hwnd, VTM_GETMARKDOWNLENGTH, 0, 0);
```

返回当前 Markdown 源文本的 UTF-16 code unit 数，不含结尾 `NUL`。控件初始化后会同步查询 GPUI Editor 的当前文档，因此包含用户在 DLL 控件内直接输入后的内容。

### `VTM_GETMARKDOWN`

```c
SendMessage(hwnd, VTM_GETMARKDOWN, capacity, (LPARAM)buffer);
```

复制当前 Markdown 源文本到调用方 buffer。`capacity` 是 UTF-16 code unit 容量，包含结尾 `NUL`。

### `VTM_INITIALIZE`

```c
SendMessage(hwnd, VTM_INITIALIZE, 0, (LPARAM)L"markdown");
```

初始化控件：启动内部 GPUI Application，并以传入 Markdown 创建 Editor。已初始化时返回成功，并把非空 Markdown 当作一次内容更新。

### `VTM_SHOW`

```c
SendMessage(hwnd, VTM_SHOW, TRUE, 0);  // show
SendMessage(hwnd, VTM_SHOW, FALSE, 0); // hide
```

显示或隐藏外层 child control。

### `VTM_SETTHEME`

```c
SendMessage(hwnd, VTM_SETTHEME, 0, (LPARAM)L"velotype-light");
```

设置内部 GPUI Editor 的主题。空字符串或 `NULL` 使用 `velotype-light`。

### `VTM_SETLANGUAGE`

```c
SendMessage(hwnd, VTM_SETLANGUAGE, 0, (LPARAM)L"en-US");
```

设置内部 GPUI Editor 的语言。空字符串或 `NULL` 使用 `en-US`。

## 导出函数

### `DllMain`

```c
BOOL WINAPI DllMain(HINSTANCE hInstance, DWORD reason, LPVOID reserved);
```

DLL attach 时注册 `Velotype` 窗口类，detach 时注销。

### `Velotype_RegisterClasses`

```c
BOOL WINAPI Velotype_RegisterClasses(HINSTANCE hInstance);
```

手动注册 `Velotype` 窗口类。`hInstance == NULL` 时使用 DLL module instance。

### `Velotype_UnregisterClasses`

```c
BOOL WINAPI Velotype_UnregisterClasses(HINSTANCE hInstance);
```

手动注销窗口类。

### `Velotype_DirectFunction`

```c
LRESULT WINAPI Velotype_DirectFunction(HWND hwnd, UINT message, WPARAM wParam, LPARAM lParam);
```

直接调用控件窗口过程，提供类似 Scintilla direct-function 的入口。

### `Velotype_CreateControlEx`

```c
HWND WINAPI Velotype_CreateControlEx(const VelotypeControlCreateParams *params);
```

创建外层 Win32 child control。默认只创建 HWND，不初始化 GPUI，不显示；初始化和显示由单独 API 完成。

结构体布局：

```c
typedef struct VelotypeControlCreateParams {
    uint32_t cb_size;
    HWND     parent;
    int32_t  x;
    int32_t  y;
    int32_t  width;
    int32_t  height;
    uint32_t ex_style;
    uint32_t style;
    intptr_t control_id;
    uint32_t flags;
    const wchar_t *markdown;
    const wchar_t *theme_id;
    const wchar_t *language_id;
} VelotypeControlCreateParams;
```

在 Windows x64 下当前结构体大小为 80 bytes。调用方应设置：

```c
params.cb_size = sizeof(VelotypeControlCreateParams);
```

`style == 0` 时默认使用：

```c
WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS
```

如果设置 `VEL_CREATE_VISIBLE`，则额外加 `WS_VISIBLE`。

`theme_id == NULL` 或空字符串时默认：

```text
velotype-light
```

`language_id == NULL` 或空字符串时默认：

```text
en-US
```

#### flags

```c
#define VEL_CREATE_VISIBLE        0x00000001
#define VEL_CREATE_INITIALIZE     0x00000002
#define VEL_CREATE_GPUI_FOCUS     0x00000004
#define VEL_CREATE_GPUI_RESIZABLE 0x00000008
```

- `VEL_CREATE_VISIBLE`
  - 创建外层 HWND 时带 `WS_VISIBLE`。
- `VEL_CREATE_INITIALIZE`
  - 创建后在 `WM_CREATE` 中立即初始化 GPUI。
- `VEL_CREATE_GPUI_FOCUS`
  - 内部 GPUI `WindowOptions.focus = true`。
- `VEL_CREATE_GPUI_RESIZABLE`
  - 内部 GPUI `WindowOptions.is_resizable = true`。普通 child control 建议保持关闭，由宿主通过 `MoveWindow` 控制尺寸。

推荐分离式用法是不设置 `VEL_CREATE_VISIBLE` 和 `VEL_CREATE_INITIALIZE`，之后显式调用 `Velotype_InitializeControl` 和 `Velotype_ShowControl`。

### `Velotype_CreateAsChildControl`

```c
HWND WINAPI Velotype_CreateAsChildControl(
    HWND parent,
    int32_t x,
    int32_t y,
    int32_t width,
    int32_t height,
    const wchar_t *markdown
);
```

兼容旧入口。等价于：

```c
Velotype_CreateControlEx({
    .parent = parent,
    .x = x,
    .y = y,
    .width = width,
    .height = height,
    .flags = VEL_CREATE_VISIBLE | VEL_CREATE_INITIALIZE,
    .markdown = markdown,
});
```

### `Velotype_InitializeControl`

```c
BOOL WINAPI Velotype_InitializeControl(HWND hwnd, const wchar_t *markdown);
```

初始化控件内部 GPUI child window。`markdown != NULL` 时会先设置 Markdown 内容。

返回：

- `TRUE`: 初始化已启动或此前已经初始化。
- `FALSE`: HWND 无效或 GPUI 线程启动失败。

### `Velotype_ShowControl`

```c
BOOL WINAPI Velotype_ShowControl(HWND hwnd, BOOL show);
```

显示或隐藏外层 child control。

### `Velotype_SetMarkdown`

```c
BOOL WINAPI Velotype_SetMarkdown(HWND hwnd, const wchar_t *markdown);
```

设置控件 Markdown 内容。等价于发送 `VTM_SETMARKDOWN`。

### `Velotype_GetMarkdownLength`

```c
size_t WINAPI Velotype_GetMarkdownLength(HWND hwnd);
```

返回当前 Markdown 源文本的 UTF-16 code unit 数，不含结尾 `NUL`。

### `Velotype_GetMarkdown`

```c
size_t WINAPI Velotype_GetMarkdown(HWND hwnd, wchar_t *buffer, size_t capacity);
```

复制当前 Markdown 源文本到 `buffer`。返回完整内容所需 UTF-16 code unit 数，不含结尾 `NUL`；`capacity` 包含结尾 `NUL`。可先用 `buffer = NULL, capacity = 0` 查询长度。控件初始化后会同步查询 GPUI Editor 的当前文档，因此包含用户在 DLL 控件内直接输入后的内容。

### `Velotype_SetTheme`

```c
BOOL WINAPI Velotype_SetTheme(HWND hwnd, const wchar_t *theme_id);
```

设置控件主题。当前内置 ID：

- `velotype-light`
- `velotype`

DLL 默认使用 `velotype-light`。

### `Velotype_SetClassName`

```c
BOOL WINAPI Velotype_SetClassName(const wchar_t *class_name);
```

设置/自定义控件注册的窗口类名。此函数应当在调用 `Velotype_RegisterClasses` 或 `Velotype_CreateControlEx` 之前调用。

调用此函数后，不仅外层 Win32 控件的类名会被修改，内部 GPUI 引擎注册的窗口类（默认的 `Zed::Window` 与 `Zed::PlatformWindow`）也会被同步更新为 `<class_name>::Window` 与 `<class_name>::PlatformWindow`。这能彻底解决与宿主进程中其他 GPUI 应用（例如 ZED）的类名冲突。

返回：

- `TRUE`: 成功设置类名。
- `FALSE`: 传入参数无效。

### `Velotype_SetLanguage`

```c
BOOL WINAPI Velotype_SetLanguage(HWND hwnd, const wchar_t *language_id);
```

设置控件语言。默认 `en-US`。

### `Velotype_SetControlBackgroundColor`

```c
BOOL WINAPI Velotype_SetControlBackgroundColor(HWND hwnd, uint32_t color_bgr);
```

设置外层 Win32 child control 的初始化背景色，用于覆盖 GPUI child window 创建前的空白区域。默认值为 `0x00F0F0F0`。参数使用 Win32 `COLORREF` 排列（`0x00BBGGRR`）；灰度色如 `0x00F0F0F0` 与 RGB 写法一致。

推荐在 `Velotype_ShowControl` 前调用。

### `Velotype_SetThemeParameter`

```c
BOOL WINAPI Velotype_SetThemeParameter(
    HWND hwnd,
    const wchar_t *param_name,
    const wchar_t *param_value
);
```

设置当前控件的主题/排版参数。此 API 可在 `Velotype_InitializeControl` 前调用；初始化时会应用已保存参数，初始化后调用会刷新 GPUI 窗口。

当前支持的参数：

| 参数 | 值示例 | 说明 |
| --- | --- | --- |
| `editor_background` | `FFFFFF` / `FFFFFFFF` | 页面背景色，`RRGGBB` 或 `RRGGBBAA` |
| `font_family`, `text_font_family` | `.SystemUIFont` / `Segoe UI` | 正文字体族 |
| `font_size`, `text_size` | `17` | 正文字号 |
| `text_line_height`, `line_height`, `line_spacing` | `1.6` | 正文行高倍率 |
| `h1_size`, `h2_size`, `h3_size` | `32` | 标题字号 |
| `code_size` | `15` | 代码字号 |
| `block_gap`, `paragraph_spacing` | `4` | 块/段落间距 |
| `editor_padding` | `24` | 页面内边距 |
| `block_padding_x`, `block_padding_y` | `6` | 块内边距 |
| `cursor_width` | `2` | 光标宽度 |

DLL 默认主题为 `velotype-light`，其页面背景默认是 `0xFFFFFF`。

### `Velotype_SetCaretPosition`

```c
BOOL WINAPI Velotype_SetCaretPosition(HWND hwnd, uint32_t line, uint32_t column);
```

显示输入光标并将其放到指定可见 block 行。`line` 从 `0` 开始，对应当前文档的可见 block 顺序；`column` 从 `0` 开始，并会被截断到该 block 的可见文本长度。

DLL 控件加载 Markdown 后默认不显示 caret；只有调用此 API 后才主动聚焦并显示 caret。

### `Velotype_HideCaret`

```c
BOOL WINAPI Velotype_HideCaret(HWND hwnd);
```

清除内部 GPUI 焦点并隐藏 caret。

### `Velotype_SetProperty` / `Velotype_GetProperty`

```c
BOOL WINAPI Velotype_SetProperty(HWND hwnd, const wchar_t *name, const wchar_t *value);
size_t WINAPI Velotype_GetProperty(HWND hwnd, const wchar_t *name, wchar_t *buffer, size_t capacity);
```

提供类似 `libmpv.dll` 的统一字符串属性读写入口。`Velotype_GetProperty` 返回完整值所需 UTF-16 code unit 数，不含结尾 `NUL`；可先传 `buffer = NULL, capacity = 0` 查询长度。未知属性会作为宿主自定义字符串保存，便于后续由宿主逐步实现。

当前内置属性覆盖控制层、文档层、主题/语言、编辑器、热键和事件模块：

| 属性 | get | set | 说明 |
| --- | --- | --- | --- |
| `document.markdown`, `markdown` | yes | yes | 当前 Markdown 源文本 |
| `document.length`, `markdown.length` | yes | no | 当前 Markdown UTF-16 长度 |
| `document.display_text` | yes | no | Markdown 转纯显示文本 |
| `document.html` | yes | no | Markdown 转 HTML |
| `theme.id`, `control.theme` | yes | yes | 当前主题 ID，默认 `velotype-light` |
| `language.id`, `control.language` | yes | yes | 当前语言 ID，默认 `en-US` |
| `theme.parameter.<name>`, `theme.<name>` | yes | yes | 主题参数；`name` 同 `Velotype_SetThemeParameter` |
| `control.background_color`, `background_color` | yes | yes | 外层 Win32 背景色，`BBGGRR`/`0xBBGGRR` |
| `control.visible` | no | yes | `1/true/show` 显示，其它值隐藏 |
| `control.child_hwnd` | yes | no | 内层 GPUI child HWND 数值 |
| `control.initialized` | yes | no | `1` 表示 GPUI 已初始化 |
| `editor.caret`, `caret.position` | no | yes | `line,column`，等价于 `Velotype_SetCaretPosition` |
| `editor.hide_caret`, `caret.hidden` | yes | yes | `1/true` 隐藏 caret |
| `editor.keybinding.<command_id>` | yes | yes | 编辑命令热键；值使用 `|` 分隔 |
| `event.names` | yes | yes | 已注册事件，如 `save|change|` |
| `event.last` | yes | no | 最近一次通知事件，如 `save|` |
| `event.message` | yes | yes | 通知宿主的窗口消息 ID |
| `event.notify_hwnd` | yes | yes | 接收事件通知的宿主 HWND 数值 |

示例：

```c
Velotype_SetProperty(hwnd, L"theme.parameter.font_size", L"17");
Velotype_SetProperty(hwnd, L"editor.caret", L"0,0");
Velotype_SetProperty(hwnd, L"editor.keybinding.save_document", L"ctrl-s");
```

### `Velotype_RegisterEventCallback`

```c
BOOL WINAPI Velotype_RegisterEventCallback(
    HWND hwnd,
    HWND notify_hwnd,
    uint32_t message_id,
    const wchar_t *event_names
);
```

注册宿主事件通知。`event_names` 使用 `|`、`;`、`,` 或换行分隔，例如 `L"save|change|"`。`message_id == 0` 时使用默认 `VTM_NOTIFY_EVENT`。

当前事件：

| 事件 | 触发时机 | `lParam` |
| --- | --- | --- |
| `change` | 文档内容发生编辑变更 | `2` |
| `save` | 用户按已注册的保存热键（默认 `Ctrl+S`，或 `editor.keybinding.save_document` 覆盖） | `1` |
| `save_as` | 用户按另存为热键（默认 `Ctrl+Shift+S`，或 `editor.keybinding.save_document_as` 覆盖） | `3` |

触发时 DLL 调用 `PostMessageW(notify_hwnd, message_id, (WPARAM)hwnd, event_code)`，并把 `event.last` 更新为 `save|` / `change|`。`save` 事件只通知宿主，不在 DLL 内部执行文件保存或弹保存对话框。

### `Velotype_SetEditorKeyBinding`

```c
BOOL WINAPI Velotype_SetEditorKeyBinding(
    HWND hwnd,
    const wchar_t *command_id,
    const wchar_t *keys
);
```

为 DLL 控件定义/覆盖编辑相关热键。`keys` 可用 `;`、`,`、`|` 或换行分隔多个快捷键，例如：

```c
Velotype_SetEditorKeyBinding(hwnd, L"bold_selection", L"ctrl-alt-b;ctrl-b");
```

此 API 默认接受 `BlockEditor` 上下文的编辑命令；另外接受 `save_document` / `save_document_as` 作为宿主事件热键。`open_file`、`new_window` 等其它文件/菜单命令会返回 `FALSE`，不会被注册到 DLL 控件。

可用编辑命令 ID：

- `newline`
- `delete_back`
- `delete`
- `word_delete_back`
- `word_delete_forward`
- `focus_prev`
- `focus_next`
- `move_left`
- `move_right`
- `word_move_left`
- `word_move_right`
- `home`
- `end`
- `block_up`
- `block_down`
- `select_left`
- `select_right`
- `word_select_left`
- `word_select_right`
- `select_home`
- `select_end`
- `select_all`
- `copy`
- `cut`
- `paste`
- `undo`
- `bold_selection`
- `italic_selection`
- `underline_selection`
- `code_selection`
- `indent_block`
- `outdent_block`
- `exit_code_block`

推荐在 `Velotype_InitializeControl` 前调用；初始化后调用也会重新安装 DLL 控件的编辑 keymap。

### `Velotype_ResetEditorKeyBindings`

```c
BOOL WINAPI Velotype_ResetEditorKeyBindings(HWND hwnd);
```

清除通过 `Velotype_SetEditorKeyBinding` 设置的自定义编辑热键，并恢复 DLL 默认编辑热键。

### `Velotype_MarkdownToDisplayText`

```c
size_t WINAPI Velotype_MarkdownToDisplayText(
    const wchar_t *markdown,
    wchar_t *buffer,
    size_t capacity
);
```

把 Markdown 转为纯文本显示内容。返回完整输出所需 UTF-16 code unit 数，不含结尾 `NUL`；`capacity` 包含结尾 `NUL`。可先用 `buffer = NULL, capacity = 0` 查询长度。

### `Velotype_RenderMarkdownToHtml`

```c
size_t WINAPI Velotype_RenderMarkdownToHtml(
    const wchar_t *markdown,
    const wchar_t *title,
    const wchar_t *theme_id,
    wchar_t *buffer,
    size_t capacity
);
```

使用 Velotype 的 HTML export renderer 生成完整 HTML 文档（含主题 CSS）。返回完整输出所需 UTF-16 code unit 数，不含结尾 `NUL`；`capacity` 包含结尾 `NUL`。当前只解析内置主题 ID；空 `title` 使用 `Velotype`，空 `theme_id` 使用 `velotype-light`。

### `Velotype_CreateStandaloneWindow`

```c
HWND WINAPI Velotype_CreateStandaloneWindow(void);
```

创建一个用于手动测试的顶级窗口。主要用于 smoke test，不是推荐宿主集成入口。

### `Velotype_RunMessageLoop`

```c
int WINAPI Velotype_RunMessageLoop(void);
```

运行简单 Win32 message loop。宿主程序通常不需要调用；宿主应使用自己的 message loop。

## AHK v2 示例

```autohotkey
params := Buffer(80, 0)
NumPut("UInt", 80, params, 0)
NumPut("Ptr", main_gui.Hwnd, params, 8)
NumPut("Int", 12, params, 16)
NumPut("Int", 12, params, 20)
NumPut("Int", 760, params, 24)
NumPut("Int", 520, params, 28)

control_hwnd := DllCall(
    dll_path "\Velotype_CreateControlEx",
    "Ptr", params,
    "Ptr"
)

DllCall(dll_path "\Velotype_SetControlBackgroundColor", "Ptr", control_hwnd, "UInt", 0x00F0F0F0, "Int")
DllCall(dll_path "\Velotype_SetThemeParameter", "Ptr", control_hwnd, "Str", "editor_background", "Str", "FFFFFF", "Int")
DllCall(dll_path "\Velotype_SetThemeParameter", "Ptr", control_hwnd, "Str", "font_size", "Str", "17", "Int")
DllCall(dll_path "\Velotype_SetThemeParameter", "Ptr", control_hwnd, "Str", "line_height", "Str", "1.6", "Int")
DllCall(dll_path "\Velotype_InitializeControl", "Ptr", control_hwnd, "Str", markdown, "Int")
DllCall(dll_path "\Velotype_ShowControl", "Ptr", control_hwnd, "Int", true, "Int")
DllCall(dll_path "\Velotype_SetTheme", "Ptr", control_hwnd, "Str", "velotype-light", "Int")
DllCall(dll_path "\Velotype_SetLanguage", "Ptr", control_hwnd, "Str", "en-US", "Int")
DllCall(dll_path "\Velotype_SetEditorKeyBinding", "Ptr", control_hwnd, "Str", "bold_selection", "Str", "ctrl-alt-b", "Int")
; 默认不显示 caret；需要时显式显示：
DllCall(dll_path "\Velotype_SetCaretPosition", "Ptr", control_hwnd, "UInt", 0, "UInt", 0, "Int")
```

宿主窗口收到 `WM_SIZE` 时应同步调整控件：

```autohotkey
DllCall("MoveWindow", "Ptr", control_hwnd, "Int", x, "Int", y, "Int", w, "Int", h, "Int", true)
```

查询 HTML 输出长度：

```autohotkey
html_len := DllCall(
    dll_path "\Velotype_RenderMarkdownToHtml",
    "Str", "# Velotype",
    "Str", "Smoke",
    "Str", "velotype-light",
    "Ptr", 0,
    "UPtr", 0,
    "UPtr"
)
```
