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

返回当前 Markdown 源文本的 UTF-16 code unit 数，不含结尾 `NUL`。

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

复制当前 Markdown 源文本到 `buffer`。返回完整内容所需 UTF-16 code unit 数，不含结尾 `NUL`；`capacity` 包含结尾 `NUL`。可先用 `buffer = NULL, capacity = 0` 查询长度。

### `Velotype_SetTheme`

```c
BOOL WINAPI Velotype_SetTheme(HWND hwnd, const wchar_t *theme_id);
```

设置控件主题。当前内置 ID：

- `velotype-light`
- `velotype`

DLL 默认使用 `velotype-light`。

### `Velotype_SetLanguage`

```c
BOOL WINAPI Velotype_SetLanguage(HWND hwnd, const wchar_t *language_id);
```

设置控件语言。默认 `en-US`。

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

DllCall(dll_path "\Velotype_InitializeControl", "Ptr", control_hwnd, "Str", markdown, "Int")
DllCall(dll_path "\Velotype_ShowControl", "Ptr", control_hwnd, "Int", true, "Int")
DllCall(dll_path "\Velotype_SetTheme", "Ptr", control_hwnd, "Str", "velotype-light", "Int")
DllCall(dll_path "\Velotype_SetLanguage", "Ptr", control_hwnd, "Str", "en-US", "Int")
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
