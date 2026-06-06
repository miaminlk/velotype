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
use windows_sys::Win32::Graphics::Gdi::{BeginPaint, EndPaint, PAINTSTRUCT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW,
    DefWindowProcW, DispatchMessageW, GWLP_USERDATA, GetClientRect, GetMessageW, GetParent,
    GetWindowLongPtrW, IDC_ARROW, LoadCursorW, MSG, MoveWindow, PostMessageW, PostQuitMessage,
    RegisterClassExW, SetWindowLongPtrW, TranslateMessage, UnregisterClassW, WM_CREATE, WM_DESTROY,
    WM_ERASEBKGND, WM_GETTEXT, WM_GETTEXTLENGTH, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SETTEXT,
    WM_SIZE, WM_USER, WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_OVERLAPPEDWINDOW,
    WS_VISIBLE,
};
use windows_sys::core::BOOL;

use crate::app_menu::init as init_app_menu;
use crate::components::init_with_keybindings as init_editor;
use crate::editor::Editor;
use crate::i18n::I18nManager;
use crate::theme::ThemeManager;

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
const VTM_CHILD_READY: u32 = WM_USER + 64;

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
    Close,
}

struct ControlState {
    source: String,
    source_wide: Vec<u16>,
    command_sender: Option<mpsc::Sender<ControlCommand>>,
    child_hwnd: isize,
}

impl ControlState {
    fn new() -> Self {
        Self {
            source: String::new(),
            source_wide: wide_null(""),
            command_sender: None,
            child_hwnd: 0,
        }
    }

    fn set_markdown(&mut self, markdown: String) {
        self.source_wide = wide_null(&markdown);
        self.source = markdown.clone();
        if let Some(sender) = &self.command_sender {
            let _ = sender.send(ControlCommand::SetMarkdown(markdown));
        }
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
pub unsafe extern "system" fn Velotype_CreateAsChildControl(
    parent: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    markdown: *const u16,
) -> HWND {
    let instance = INSTANCE.load(Ordering::SeqCst) as HINSTANCE;
    let instance = if instance.is_null() {
        unsafe { module_instance() }
    } else {
        instance
    };
    if parent.is_null() || !unsafe { register_class(instance) } {
        return null_mut();
    }
    unsafe {
        CreateWindowExW(
            0,
            CLASS_NAME.as_ptr(),
            markdown,
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            x,
            y,
            width.max(1),
            height.max(1),
            parent,
            null_mut(),
            instance,
            null_mut(),
        )
    }
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
            let state = Box::new(ControlState::new());
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
                    state.source = initial_markdown.clone();
                    state.source_wide = wide_null(&initial_markdown);
                    state.command_sender = start_gpui_child(hwnd, initial_markdown);
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
        hbrBackground: null_mut(),
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

fn start_gpui_child(hwnd: HWND, markdown: String) -> Option<mpsc::Sender<ControlCommand>> {
    let host_hwnd = hwnd as isize;
    let (width, height) = unsafe { client_size(hwnd) };
    let (command_sender, command_receiver) = mpsc::channel::<ControlCommand>();
    let builder = std::thread::Builder::new().name("VelotypeGpuiControl".to_string());
    let result = builder.spawn(move || {
        Application::new()
            .with_assets(VelotypeControlAssets)
            .run(move |cx: &mut App| {
                I18nManager::init_with_language_id(cx, "en-US");
                ThemeManager::init_with_theme_id(cx, "velotype-light");
                crate::net::install_http_client(cx);
                init_editor(cx, &BTreeMap::new());
                init_app_menu(cx);

                let bounds = Bounds::new(
                    point(px(0.0), px(0.0)),
                    size(px(width.max(1) as f32), px(height.max(1) as f32)),
                );
                let options = WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: None,
                    focus: false,
                    is_movable: false,
                    is_resizable: false,
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
                        cx.new(move |cx| Editor::from_markdown(cx, markdown, None))
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
        EndPaint(hwnd, &ps);
        let _ = hdc;
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

unsafe fn module_instance() -> HINSTANCE {
    unsafe { GetModuleHandleW(null()) as HINSTANCE }
}

fn is_standalone(hwnd: HWND) -> bool {
    unsafe { GetParent(hwnd).is_null() }
}
