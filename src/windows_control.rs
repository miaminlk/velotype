use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use gpui::*;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT, TRUE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, PAINTSTRUCT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW,
    DefWindowProcW, DispatchMessageW, GWLP_USERDATA, GetClientRect, GetMessageW, GetParent,
    GetWindowLongPtrW, IDC_ARROW, LoadCursorW, MSG, MoveWindow, PostMessageW, PostQuitMessage,
    RegisterClassExW, SW_HIDE, SW_SHOW, SetWindowLongPtrW, ShowWindow, TranslateMessage,
    UnregisterClassW, WM_CREATE, WM_DESTROY, WM_ERASEBKGND, WM_GETTEXT, WM_GETTEXTLENGTH,
    WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SETTEXT, WM_SIZE, WM_USER, WNDCLASSEXW, WS_CHILD,
    WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows_sys::core::BOOL;

use crate::components::{
    install_block_editor_keybindings_with_config, is_block_editor_shortcut_id,
    normalize_shortcut_keys,
};
use crate::editor::Editor;
use crate::export;
use crate::i18n::I18nManager;
use crate::markdown_display::markdown_to_display_text;
use crate::theme::{Theme, ThemeManager};

const CLASS_NAME: &[u16] = &[
    b'V' as u16,
    b'e' as u16,
    b'l' as u16,
    b'o' as u16,
    b't' as u16,
    b'y' as u16,
    b'p' as u16,
    b'e' as u16,
    0,
];
pub const VTM_SETMARKDOWN: u32 = WM_USER + 1;
pub const VTM_GETMARKDOWNLENGTH: u32 = WM_USER + 2;
pub const VTM_GETMARKDOWN: u32 = WM_USER + 3;
pub const VTM_INITIALIZE: u32 = WM_USER + 4;
pub const VTM_SHOW: u32 = WM_USER + 5;
pub const VTM_SETTHEME: u32 = WM_USER + 6;
pub const VTM_SETLANGUAGE: u32 = WM_USER + 7;
const VTM_CHILD_READY: u32 = WM_USER + 64;

pub const VEL_CREATE_VISIBLE: u32 = 0x0000_0001;
pub const VEL_CREATE_INITIALIZE: u32 = 0x0000_0002;
pub const VEL_CREATE_GPUI_FOCUS: u32 = 0x0000_0004;
pub const VEL_CREATE_GPUI_RESIZABLE: u32 = 0x0000_0008;

#[repr(C)]
pub struct VelotypeControlCreateParams {
    pub cb_size: u32,
    pub parent: HWND,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub ex_style: u32,
    pub style: u32,
    pub control_id: isize,
    pub flags: u32,
    pub markdown: *const u16,
    pub theme_id: *const u16,
    pub language_id: *const u16,
}

static INSTANCE: AtomicIsize = AtomicIsize::new(0);

struct VelotypeControlAssets;

impl AssetSource for VelotypeControlAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        match path {
            "icon/workspace/folder.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/workspace/folder.svg"
            )))),
            "icon/workspace/markdown.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/workspace/markdown.svg"
            )))),
            "icon/titlebar/chrome-close.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/titlebar/chrome-close.svg"
            )))),
            "icon/titlebar/chrome-minimize.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/titlebar/chrome-minimize.svg"
            )))),
            "icon/titlebar/chrome-maximize.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/titlebar/chrome-maximize.svg"
            )))),
            "icon/titlebar/chrome-restore.svg" => Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/titlebar/chrome-restore.svg"
            )))),
            _ => Ok(None),
        }
    }

    fn list(&self, _path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}

enum ControlCommand {
    SetMarkdown(String),
    SetTheme(String),
    SetLanguage(String),
    SetEditorKeyBindings(BTreeMap<String, Vec<String>>),
    GetMarkdown(mpsc::Sender<String>),
    SetThemeParameter(String, String),
    SetCaretPosition(u32, u32),
    HideCaret,
    Close,
}

#[derive(Clone)]
struct ControlOptions {
    focus: bool,
    resizable: bool,
    theme_id: String,
    language_id: String,
    editor_keybindings: BTreeMap<String, Vec<String>>,
    theme_params: BTreeMap<String, String>,
    hide_caret: bool,
}

impl Default for ControlOptions {
    fn default() -> Self {
        Self {
            focus: false,
            resizable: false,
            theme_id: "velotype-light".to_string(),
            language_id: "en-US".to_string(),
            editor_keybindings: BTreeMap::new(),
            theme_params: BTreeMap::new(),
            hide_caret: true,
        }
    }
}

struct CreateContext {
    markdown: String,
    options: ControlOptions,
    initialize: bool,
}

struct ControlState {
    source: String,
    source_wide: Vec<u16>,
    command_sender: Option<mpsc::Sender<ControlCommand>>,
    child_hwnd: isize,
    options: ControlOptions,
    initialize_on_create: bool,
    background_color: u32,
}

impl ControlState {
    fn new(markdown: String, options: ControlOptions, initialize_on_create: bool) -> Self {
        let source_wide = wide_null(&markdown);
        Self {
            source: markdown,
            source_wide,
            command_sender: None,
            child_hwnd: 0,
            options,
            initialize_on_create,
            background_color: 0x00F0F0F0,
        }
    }

    fn default_create_window() -> Self {
        Self::new(String::new(), ControlOptions::default(), true)
    }

    fn initialize(&mut self, hwnd: HWND) -> bool {
        if self.command_sender.is_some() {
            return true;
        }
        self.command_sender = start_gpui_child(hwnd, self.source.clone(), self.options.clone());
        self.command_sender.is_some()
    }

    fn set_markdown(&mut self, markdown: String) {
        self.source_wide = wide_null(&markdown);
        self.source = markdown.clone();
        if let Some(sender) = &self.command_sender {
            let _ = sender.send(ControlCommand::SetMarkdown(markdown));
        }
    }

    fn set_theme(&mut self, theme_id: String) {
        self.options.theme_id = theme_id.clone();
        if let Some(sender) = &self.command_sender {
            let _ = sender.send(ControlCommand::SetTheme(theme_id));
            for (name, value) in &self.options.theme_params {
                let _ = sender.send(ControlCommand::SetThemeParameter(
                    name.clone(),
                    value.clone(),
                ));
            }
        }
    }

    fn set_language(&mut self, language_id: String) {
        self.options.language_id = language_id.clone();
        if let Some(sender) = &self.command_sender {
            let _ = sender.send(ControlCommand::SetLanguage(language_id));
        }
    }

    fn set_editor_key_binding(&mut self, command_id: String, keys: Vec<String>) -> bool {
        if !is_block_editor_shortcut_id(&command_id) || normalize_shortcut_keys(&keys).is_none() {
            return false;
        }
        self.options.editor_keybindings.insert(command_id, keys);
        if let Some(sender) = &self.command_sender {
            let _ = sender.send(ControlCommand::SetEditorKeyBindings(
                self.options.editor_keybindings.clone(),
            ));
        }
        true
    }

    fn reset_editor_key_bindings(&mut self) {
        self.options.editor_keybindings.clear();
        if let Some(sender) = &self.command_sender {
            let _ = sender.send(ControlCommand::SetEditorKeyBindings(
                self.options.editor_keybindings.clone(),
            ));
        }
    }

    fn current_markdown(&mut self) -> String {
        if let Some(sender) = &self.command_sender {
            let (reply_sender, reply_receiver) = mpsc::channel();
            if sender
                .send(ControlCommand::GetMarkdown(reply_sender))
                .is_ok()
            {
                if let Ok(markdown) = reply_receiver.recv_timeout(Duration::from_millis(500)) {
                    self.source_wide = wide_null(&markdown);
                    self.source = markdown.clone();
                    return markdown;
                }
            }
        }
        self.source.clone()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(
    h_instance: HINSTANCE,
    reason: u32,
    _reserved: *mut c_void,
) -> BOOL {
    match reason {
        DLL_PROCESS_ATTACH => {
            INSTANCE.store(h_instance as isize, Ordering::SeqCst);
            if unsafe { register_class(h_instance) } {
                TRUE
            } else {
                0
            }
        }
        DLL_PROCESS_DETACH => {
            let instance = INSTANCE.load(Ordering::SeqCst) as HINSTANCE;
            if !instance.is_null() {
                unsafe {
                    UnregisterClassW(CLASS_NAME.as_ptr(), instance);
                }
            }
            TRUE
        }
        _ => TRUE,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_RegisterClasses(h_instance: HINSTANCE) -> BOOL {
    let instance = if h_instance.is_null() {
        unsafe { module_instance() }
    } else {
        h_instance
    };
    INSTANCE.store(instance as isize, Ordering::SeqCst);
    unsafe { register_class(instance) as BOOL }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_UnregisterClasses(h_instance: HINSTANCE) -> BOOL {
    let instance = if h_instance.is_null() {
        INSTANCE.load(Ordering::SeqCst) as HINSTANCE
    } else {
        h_instance
    };
    if instance.is_null() {
        return 0;
    }
    unsafe { UnregisterClassW(CLASS_NAME.as_ptr(), instance) as BOOL }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_DirectFunction(
    hwnd: HWND,
    message: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    unsafe { control_wnd_proc(hwnd, message, w_param, l_param) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_CreateControlEx(
    params: *const VelotypeControlCreateParams,
) -> HWND {
    if params.is_null() {
        return null_mut();
    }
    let params = unsafe { &*params };
    let instance = INSTANCE.load(Ordering::SeqCst) as HINSTANCE;
    let instance = if instance.is_null() {
        unsafe { module_instance() }
    } else {
        instance
    };
    if params.parent.is_null() || !unsafe { register_class(instance) } {
        return null_mut();
    }
    let style = if params.style == 0 {
        WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS
    } else {
        params.style
    } | if params.flags & VEL_CREATE_VISIBLE != 0 {
        WS_VISIBLE
    } else {
        0
    };
    let create_context = Box::new(CreateContext {
        markdown: unsafe { wide_ptr_to_string(params.markdown) },
        options: ControlOptions {
            focus: params.flags & VEL_CREATE_GPUI_FOCUS != 0,
            resizable: params.flags & VEL_CREATE_GPUI_RESIZABLE != 0,
            theme_id: unsafe { wide_ptr_to_string_or_default(params.theme_id, "velotype-light") },
            language_id: unsafe { wide_ptr_to_string_or_default(params.language_id, "en-US") },
            editor_keybindings: BTreeMap::new(),
            theme_params: BTreeMap::new(),
            hide_caret: true,
        },
        initialize: params.flags & VEL_CREATE_INITIALIZE != 0,
    });
    unsafe {
        CreateWindowExW(
            params.ex_style,
            CLASS_NAME.as_ptr(),
            null(),
            style,
            params.x,
            params.y,
            params.width.max(1),
            params.height.max(1),
            params.parent,
            params.control_id as *mut c_void,
            instance,
            Box::into_raw(create_context) as *mut c_void,
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_CreateAsChildControl(
    parent: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    markdown: *const u16,
) -> HWND {
    let params = VelotypeControlCreateParams {
        cb_size: size_of::<VelotypeControlCreateParams>() as u32,
        parent,
        x,
        y,
        width,
        height,
        ex_style: 0,
        style: 0,
        control_id: 0,
        flags: VEL_CREATE_VISIBLE | VEL_CREATE_INITIALIZE,
        markdown,
        theme_id: null(),
        language_id: null(),
    };
    unsafe { Velotype_CreateControlEx(&params) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_InitializeControl(hwnd: HWND, markdown: *const u16) -> BOOL {
    if hwnd.is_null() {
        return 0;
    }
    unsafe {
        with_state(hwnd, |state| {
            if !markdown.is_null() {
                state.set_markdown(wide_ptr_to_string(markdown));
            }
            state.initialize(hwnd) as BOOL
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_ShowControl(hwnd: HWND, show: BOOL) -> BOOL {
    if hwnd.is_null() {
        return 0;
    }
    unsafe {
        ShowWindow(hwnd, if show != 0 { SW_SHOW } else { SW_HIDE });
    }
    TRUE
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_SetMarkdown(hwnd: HWND, markdown: *const u16) -> BOOL {
    if hwnd.is_null() {
        return 0;
    }
    let text = unsafe { wide_ptr_to_string(markdown) };
    unsafe {
        with_state(hwnd, |state| state.set_markdown(text));
    }
    TRUE
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_GetMarkdownLength(hwnd: HWND) -> usize {
    if hwnd.is_null() {
        return 0;
    }
    unsafe {
        with_state(hwnd, |state| {
            state.current_markdown().encode_utf16().count()
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_GetMarkdown(
    hwnd: HWND,
    buffer: *mut u16,
    capacity: usize,
) -> usize {
    if hwnd.is_null() {
        return 0;
    }
    unsafe {
        with_state(hwnd, |state| {
            state.current_markdown();
            copy_utf16_required(&state.source_wide, buffer, capacity)
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_SetTheme(hwnd: HWND, theme_id: *const u16) -> BOOL {
    if hwnd.is_null() {
        return 0;
    }
    let theme_id = unsafe { wide_ptr_to_string_or_default(theme_id, "velotype-light") };
    unsafe {
        with_state(hwnd, |state| state.set_theme(theme_id));
    }
    TRUE
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_SetLanguage(hwnd: HWND, language_id: *const u16) -> BOOL {
    if hwnd.is_null() {
        return 0;
    }
    let language_id = unsafe { wide_ptr_to_string_or_default(language_id, "en-US") };
    unsafe {
        with_state(hwnd, |state| state.set_language(language_id));
    }
    TRUE
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_SetEditorKeyBinding(
    hwnd: HWND,
    command_id: *const u16,
    keys: *const u16,
) -> BOOL {
    if hwnd.is_null() {
        return 0;
    }
    let command_id = unsafe { wide_ptr_to_string(command_id) };
    let keys = parse_key_binding_list(&unsafe { wide_ptr_to_string(keys) });
    unsafe {
        with_state(hwnd, |state| {
            state.set_editor_key_binding(command_id, keys) as BOOL
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_ResetEditorKeyBindings(hwnd: HWND) -> BOOL {
    if hwnd.is_null() {
        return 0;
    }
    unsafe {
        with_state(hwnd, |state| {
            state.reset_editor_key_bindings();
            TRUE
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_SetControlBackgroundColor(
    hwnd: HWND,
    color_bgr: u32,
) -> BOOL {
    if hwnd.is_null() {
        return 0;
    }
    unsafe {
        with_state(hwnd, |state| {
            state.background_color = color_bgr;
        });
    }
    TRUE
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_SetThemeParameter(
    hwnd: HWND,
    param_name: *const u16,
    param_value: *const u16,
) -> BOOL {
    if hwnd.is_null() {
        return 0;
    }
    let name = unsafe { wide_ptr_to_string(param_name) };
    let value = unsafe { wide_ptr_to_string(param_value) };
    if name.is_empty() {
        return 0;
    }
    unsafe {
        with_state(hwnd, |state| {
            state
                .options
                .theme_params
                .insert(name.clone(), value.clone());
            if let Some(sender) = &state.command_sender {
                let _ = sender.send(ControlCommand::SetThemeParameter(name, value));
            }
        });
    }
    TRUE
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_SetCaretPosition(
    hwnd: HWND,
    line: u32,
    column: u32,
) -> BOOL {
    if hwnd.is_null() {
        return 0;
    }
    unsafe {
        with_state(hwnd, |state| {
            if let Some(sender) = &state.command_sender {
                let _ = sender.send(ControlCommand::SetCaretPosition(line, column));
            }
        });
    }
    TRUE
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_HideCaret(hwnd: HWND) -> BOOL {
    if hwnd.is_null() {
        return 0;
    }
    unsafe {
        with_state(hwnd, |state| {
            if let Some(sender) = &state.command_sender {
                let _ = sender.send(ControlCommand::HideCaret);
            }
        });
    }
    TRUE
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_MarkdownToDisplayText(
    markdown: *const u16,
    buffer: *mut u16,
    capacity: usize,
) -> usize {
    let display_text = markdown_to_display_text(&unsafe { wide_ptr_to_string(markdown) });
    let source = wide_null(&display_text);
    unsafe { copy_utf16_required(&source, buffer, capacity) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_RenderMarkdownToHtml(
    markdown: *const u16,
    title: *const u16,
    theme_id: *const u16,
    buffer: *mut u16,
    capacity: usize,
) -> usize {
    let markdown = unsafe { wide_ptr_to_string(markdown) };
    let title = unsafe { wide_ptr_to_string_or_default(title, "Velotype") };
    let theme_id = unsafe { wide_ptr_to_string_or_default(theme_id, "velotype-light") };
    let theme = theme_for_id(&theme_id);
    let html = export::render_html_with_base_dir(&markdown, &theme, &title, None);
    let source = wide_null(&html);
    unsafe { copy_utf16_required(&source, buffer, capacity) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_CreateStandaloneWindow() -> HWND {
    let instance = INSTANCE.load(Ordering::SeqCst) as HINSTANCE;
    let instance = if instance.is_null() {
        unsafe { module_instance() }
    } else {
        instance
    };
    if !unsafe { register_class(instance) } {
        return null_mut();
    }
    let title = wide_null("Velotype.dll");
    unsafe {
        CreateWindowExW(
            0,
            CLASS_NAME.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            760,
            520,
            null_mut(),
            null_mut(),
            instance,
            null_mut(),
        )
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Velotype_RunMessageLoop() -> i32 {
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    message.wParam as i32
}

unsafe extern "system" fn control_wnd_proc(
    hwnd: HWND,
    message: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create = l_param as *const CREATESTRUCTW;
            let state = if !create.is_null() && !unsafe { (*create).lpCreateParams }.is_null() {
                let context =
                    unsafe { Box::from_raw((*create).lpCreateParams as *mut CreateContext) };
                Box::new(ControlState::new(
                    context.markdown,
                    context.options,
                    context.initialize,
                ))
            } else {
                Box::new(ControlState::default_create_window())
            };
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
            }
            TRUE as LRESULT
        }
        WM_CREATE => {
            let create = l_param as *const CREATESTRUCTW;
            let mut initial_markdown = String::new();
            if !create.is_null() {
                let text = unsafe { wide_ptr_to_string((*create).lpszName) };
                if !text.is_empty() {
                    initial_markdown = text;
                }
            }
            unsafe {
                with_state(hwnd, |state| {
                    if !initial_markdown.is_empty() && state.source.is_empty() {
                        state.source = initial_markdown.clone();
                        state.source_wide = wide_null(&initial_markdown);
                    }
                    if state.initialize_on_create {
                        state.initialize(hwnd);
                    }
                });
            }
            0
        }
        VTM_CHILD_READY => {
            unsafe {
                with_state(hwnd, |state| {
                    state.child_hwnd = w_param as isize;
                    resize_child(hwnd, state.child_hwnd);
                });
            }
            0
        }
        VTM_INITIALIZE => {
            let text = unsafe { wide_ptr_to_string(l_param as *const u16) };
            unsafe {
                with_state(hwnd, |state| {
                    if !text.is_empty() {
                        state.set_markdown(text);
                    }
                    state.initialize(hwnd) as LRESULT
                })
            }
        }
        VTM_SHOW => {
            unsafe {
                ShowWindow(hwnd, if w_param != 0 { SW_SHOW } else { SW_HIDE });
            }
            TRUE as LRESULT
        }
        VTM_SETTHEME => {
            let theme_id =
                unsafe { wide_ptr_to_string_or_default(l_param as *const u16, "velotype-light") };
            unsafe {
                with_state(hwnd, |state| state.set_theme(theme_id));
            }
            TRUE as LRESULT
        }
        VTM_SETLANGUAGE => {
            let language_id =
                unsafe { wide_ptr_to_string_or_default(l_param as *const u16, "en-US") };
            unsafe {
                with_state(hwnd, |state| state.set_language(language_id));
            }
            TRUE as LRESULT
        }
        WM_SETTEXT | VTM_SETMARKDOWN => {
            let text = unsafe { wide_ptr_to_string(l_param as *const u16) };
            unsafe {
                with_state(hwnd, |state| state.set_markdown(text));
            }
            TRUE as LRESULT
        }
        WM_GETTEXTLENGTH => unsafe { with_state(hwnd, |state| utf16_len(&state.source_wide)) },
        VTM_GETMARKDOWNLENGTH => unsafe {
            with_state(hwnd, |state| state.source.encode_utf16().count() as isize)
        },
        WM_GETTEXT => unsafe {
            with_state(hwnd, |state| {
                copy_utf16(&state.source_wide, w_param, l_param)
            })
        },
        VTM_GETMARKDOWN => unsafe {
            let source = wide_null(&with_state(hwnd, |state| state.source.clone()));
            copy_utf16(&source, w_param, l_param)
        },
        WM_ERASEBKGND => TRUE as LRESULT,
        WM_SIZE => {
            unsafe {
                with_state(hwnd, |state| resize_child(hwnd, state.child_hwnd));
            }
            0
        }
        WM_PAINT => {
            unsafe {
                validate_paint(hwnd);
            }
            0
        }
        WM_DESTROY => {
            if unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } != 0 && is_standalone(hwnd) {
                unsafe {
                    PostQuitMessage(0);
                }
            }
            0
        }
        WM_NCDESTROY => {
            let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ControlState;
            if !ptr.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    if let Some(sender) = &(*ptr).command_sender {
                        let _ = sender.send(ControlCommand::Close);
                    }
                    drop(Box::from_raw(ptr));
                }
            }
            unsafe { DefWindowProcW(hwnd, message, w_param, l_param) }
        }
        _ => unsafe { DefWindowProcW(hwnd, message, w_param, l_param) },
    }
}

unsafe fn register_class(instance: HINSTANCE) -> bool {
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW | CS_DBLCLKS,
        lpfnWndProc: Some(control_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: null_mut(),
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        hbrBackground: unsafe { CreateSolidBrush(0x00F0F0F0) } as *mut c_void,
        lpszMenuName: null(),
        lpszClassName: CLASS_NAME.as_ptr(),
        hIconSm: null_mut(),
    };
    let atom = unsafe { RegisterClassExW(&class) };
    if atom != 0 {
        return true;
    }
    unsafe { GetLastError() == ERROR_CLASS_ALREADY_EXISTS }
}

fn start_gpui_child(
    hwnd: HWND,
    markdown: String,
    options: ControlOptions,
) -> Option<mpsc::Sender<ControlCommand>> {
    let host_hwnd = hwnd as isize;
    let hide_caret = options.hide_caret;
    let (width, height) = unsafe { client_size(hwnd) };
    let (command_sender, command_receiver) = mpsc::channel::<ControlCommand>();
    let builder = std::thread::Builder::new().name("VelotypeGpuiControl".to_string());
    let result = builder.spawn(move || {
        Application::new()
            .with_assets(VelotypeControlAssets)
            .run(move |cx: &mut App| {
                I18nManager::init_with_language_id(cx, &options.language_id);
                ThemeManager::init_with_theme_id(cx, &options.theme_id);
                let theme_params = options.theme_params.clone();
                cx.update_global::<ThemeManager, _>(|manager, _app| {
                    for (name, value) in &theme_params {
                        apply_theme_parameter(manager, name, value);
                    }
                });
                install_block_editor_keybindings_with_config(cx, &options.editor_keybindings);

                let bounds = Bounds::new(
                    point(px(0.0), px(0.0)),
                    size(px(width.max(1) as f32), px(height.max(1) as f32)),
                );
                let options = WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: None,
                    focus: options.focus,
                    is_movable: false,
                    is_resizable: options.resizable,
                    is_minimizable: false,
                    window_background: WindowBackgroundAppearance::Opaque,
                    parent_handle: Some(host_hwnd),
                    ..WindowOptions::default()
                };
                let handle = cx
                    .open_window(options, move |window, cx| {
                        let child_hwnd = raw_hwnd(window);
                        if child_hwnd != 0 {
                            unsafe {
                                PostMessageW(
                                    host_hwnd as HWND,
                                    VTM_CHILD_READY,
                                    child_hwnd as WPARAM,
                                    0,
                                );
                            }
                        }
                        cx.new(move |cx| {
                            Editor::from_markdown_embedded(cx, markdown, None, hide_caret)
                        })
                    })
                    .unwrap();

                cx.spawn(async move |cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(30))
                            .await;
                        while let Ok(command) = command_receiver.try_recv() {
                            match command {
                                ControlCommand::SetMarkdown(markdown) => {
                                    let _ = handle.update(cx, |editor, _window, cx| {
                                        editor.replace_markdown(markdown, cx);
                                    });
                                    let _ = cx.refresh();
                                }
                                ControlCommand::SetTheme(theme_id) => {
                                    let _ = cx.update(|app| {
                                        ThemeManager::init_with_theme_id(app, &theme_id);
                                        app.refresh_windows();
                                    });
                                }
                                ControlCommand::SetLanguage(language_id) => {
                                    let _ = cx.update(|app| {
                                        I18nManager::init_with_language_id(app, &language_id);
                                        app.refresh_windows();
                                    });
                                }
                                ControlCommand::SetEditorKeyBindings(config) => {
                                    let _ = cx.update(|app| {
                                        app.clear_key_bindings();
                                        install_block_editor_keybindings_with_config(app, &config);
                                    });
                                }
                                ControlCommand::GetMarkdown(reply_sender) => {
                                    let markdown = handle
                                        .update(cx, |editor, _window, cx| {
                                            editor.serialized_document_text(cx)
                                        })
                                        .unwrap_or_default();
                                    let _ = reply_sender.send(markdown);
                                }
                                ControlCommand::SetThemeParameter(name, value) => {
                                    let _ = cx.update(|app| {
                                        app.update_global::<ThemeManager, _>(|manager, _app| {
                                            apply_theme_parameter(manager, &name, &value);
                                        });
                                        app.refresh_windows();
                                    });
                                }
                                ControlCommand::SetCaretPosition(line, column) => {
                                    let _ = handle.update(cx, |editor, window, cx| {
                                        editor.set_caret_position(line, column, window, cx);
                                    });
                                    let _ = cx.refresh();
                                }
                                ControlCommand::HideCaret => {
                                    let _ = handle.update(cx, |editor, window, cx| {
                                        editor.hide_caret(window, cx);
                                    });
                                    let _ = cx.refresh();
                                }
                                ControlCommand::Close => {
                                    let _ = cx.update(|app| app.quit());
                                    return;
                                }
                            }
                        }
                    }
                })
                .detach();
            });
    });

    if result.is_err() {
        return None;
    }

    Some(command_sender)
}

fn raw_hwnd(window: &Window) -> isize {
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return 0;
    };
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => handle.hwnd.get(),
        _ => 0,
    }
}

unsafe fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut rect);
    }
    (rect.right - rect.left, rect.bottom - rect.top)
}

unsafe fn resize_child(hwnd: HWND, child_hwnd: isize) {
    if child_hwnd == 0 {
        return;
    }
    let (width, height) = unsafe { client_size(hwnd) };
    unsafe {
        MoveWindow(child_hwnd as HWND, 0, 0, width.max(1), height.max(1), TRUE);
    }
}

unsafe fn validate_paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    unsafe {
        let hdc = BeginPaint(hwnd, &mut ps);
        let color = with_state(hwnd, |state| state.background_color);
        let brush = CreateSolidBrush(color);
        FillRect(hdc, &ps.rcPaint, brush);
        DeleteObject(brush as *mut c_void);
        EndPaint(hwnd, &ps);
    }
}

unsafe fn with_state<T>(hwnd: HWND, f: impl FnOnce(&mut ControlState) -> T) -> T
where
    T: Default,
{
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut ControlState;
    if ptr.is_null() {
        return T::default();
    }
    f(unsafe { &mut *ptr })
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn parse_key_binding_list(value: &str) -> Vec<String> {
    value
        .split([',', ';', '|', '\n', '\r'])
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

unsafe fn wide_ptr_to_string(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(ptr, len) }.as_ref())
}

unsafe fn wide_ptr_to_string_or_default(ptr: *const u16, default: &str) -> String {
    let value = unsafe { wide_ptr_to_string(ptr) };
    if value.is_empty() {
        default.to_string()
    } else {
        value
    }
}

fn utf16_len(value: &[u16]) -> isize {
    value.iter().position(|ch| *ch == 0).unwrap_or(value.len()) as isize
}

unsafe fn copy_utf16(value: &[u16], capacity: WPARAM, target: LPARAM) -> LRESULT {
    if target == 0 || capacity == 0 {
        return 0;
    }
    let target = target as *mut u16;
    let max_chars = capacity.saturating_sub(1);
    let count = utf16_len(value).max(0) as usize;
    let copy_count = count.min(max_chars);
    unsafe {
        std::ptr::copy_nonoverlapping(value.as_ptr(), target, copy_count);
        *target.add(copy_count) = 0;
    }
    copy_count as LRESULT
}

unsafe fn copy_utf16_required(value: &[u16], target: *mut u16, capacity: usize) -> usize {
    let count = utf16_len(value).max(0) as usize;
    if !target.is_null() && capacity > 0 {
        let copy_count = count.min(capacity.saturating_sub(1));
        unsafe {
            std::ptr::copy_nonoverlapping(value.as_ptr(), target, copy_count);
            *target.add(copy_count) = 0;
        }
    }
    count
}

fn apply_theme_parameter(manager: &mut ThemeManager, name: &str, value: &str) {
    let mut theme = manager.current().clone();
    match name {
        "editor_background" => {
            if let Some(color) = parse_hex_color(value) {
                theme.colors.editor_background = color;
            }
        }
        "font_size" | "text_size" => {
            if let Ok(size) = value.parse::<f32>() {
                theme.typography.text_size = size;
            }
        }
        "font_family" | "text_font_family" => {
            if !value.is_empty() {
                theme.typography.text_font_family = value.to_string();
            }
        }
        "text_line_height" | "line_height" | "line_spacing" => {
            if let Ok(height) = value.parse::<f32>() {
                theme.typography.text_line_height = height;
            }
        }
        "h1_size" => {
            if let Ok(size) = value.parse::<f32>() {
                theme.typography.h1_size = size;
            }
        }
        "h2_size" => {
            if let Ok(size) = value.parse::<f32>() {
                theme.typography.h2_size = size;
            }
        }
        "h3_size" => {
            if let Ok(size) = value.parse::<f32>() {
                theme.typography.h3_size = size;
            }
        }
        "code_size" => {
            if let Ok(size) = value.parse::<f32>() {
                theme.typography.code_size = size;
            }
        }
        "block_gap" | "paragraph_spacing" => {
            if let Ok(gap) = value.parse::<f32>() {
                theme.dimensions.block_gap = gap;
            }
        }
        "editor_padding" => {
            if let Ok(padding) = value.parse::<f32>() {
                theme.dimensions.editor_padding = padding;
            }
        }
        "block_padding_x" => {
            if let Ok(padding) = value.parse::<f32>() {
                theme.dimensions.block_padding_x = padding;
            }
        }
        "block_padding_y" => {
            if let Ok(padding) = value.parse::<f32>() {
                theme.dimensions.block_padding_y = padding;
            }
        }
        "cursor_width" => {
            if let Ok(width) = value.parse::<f32>() {
                theme.dimensions.cursor_width = width;
            }
        }
        _ => return,
    }
    manager.set_theme(theme);
}

fn parse_hex_color(value: &str) -> Option<Hsla> {
    let hex_str = value.trim_start_matches("0x").trim_start_matches('#');
    let rgba_val = u32::from_str_radix(hex_str, 16).ok()?;
    let rgba_val = if hex_str.len() <= 6 {
        (rgba_val << 8) | 0xFF
    } else {
        rgba_val
    };
    Some(Hsla::from(rgba(rgba_val)))
}

fn theme_for_id(theme_id: &str) -> Theme {
    if theme_id.eq_ignore_ascii_case("velotype-light") {
        Theme::light_theme()
    } else {
        Theme::default_theme()
    }
}

unsafe fn module_instance() -> HINSTANCE {
    unsafe { GetModuleHandleW(null()) as HINSTANCE }
}

fn is_standalone(hwnd: HWND) -> bool {
    unsafe { GetParent(hwnd).is_null() }
}
