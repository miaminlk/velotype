use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::{
    COLORREF, ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, RECT,
    TRUE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DEFAULT_GUI_FONT, DT_LEFT, DT_NOPREFIX, DT_TOP, DT_WORDBREAK,
    DeleteObject, DrawTextW, EndPaint, FillRect, GetStockObject, HGDIOBJ, InvalidateRect,
    PAINTSTRUCT, SetBkMode, SetTextColor, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::SystemServices::{DLL_PROCESS_ATTACH, DLL_PROCESS_DETACH};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_DBLCLKS, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, CreateWindowExW,
    DefWindowProcW, DispatchMessageW, GWLP_USERDATA, GetClientRect, GetMessageW, GetParent,
    GetWindowLongPtrW, IDC_ARROW, LoadCursorW, MSG, PostQuitMessage, RegisterClassExW,
    SetWindowLongPtrW, TranslateMessage, UnregisterClassW, WM_CREATE, WM_DESTROY, WM_ERASEBKGND,
    WM_GETTEXT, WM_GETTEXTLENGTH, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SETTEXT, WM_SIZE,
    WM_USER, WNDCLASSEXW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows_sys::core::BOOL;

use crate::markdown_to_display_text;

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
const BACKGROUND: COLORREF = 0x00ff_fb_f7;
const TEXT_COLOR: COLORREF = 0x0024_211f;

pub const VTM_SETMARKDOWN: u32 = WM_USER + 1;
pub const VTM_GETMARKDOWNLENGTH: u32 = WM_USER + 2;
pub const VTM_GETMARKDOWN: u32 = WM_USER + 3;

static INSTANCE: AtomicIsize = AtomicIsize::new(0);

struct ControlState {
    source: String,
    display: Vec<u16>,
}

impl ControlState {
    fn new() -> Self {
        Self {
            source: String::new(),
            display: wide_null(""),
        }
    }

    fn set_markdown(&mut self, markdown: String) {
        self.display = wide_null(&markdown_to_display_text(&markdown));
        self.source = markdown;
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
            if !create.is_null() {
                let text = unsafe { wide_ptr_to_string((*create).lpszName) };
                if !text.is_empty() {
                    unsafe {
                        with_state(hwnd, |state| state.set_markdown(text));
                    }
                }
            }
            0
        }
        WM_SETTEXT | VTM_SETMARKDOWN => {
            let text = unsafe { wide_ptr_to_string(l_param as *const u16) };
            unsafe {
                with_state(hwnd, |state| state.set_markdown(text));
                InvalidateRect(hwnd, null(), TRUE);
            }
            TRUE as LRESULT
        }
        WM_GETTEXTLENGTH => unsafe { with_state(hwnd, |state| utf16_len(&state.display)) },
        VTM_GETMARKDOWNLENGTH => unsafe {
            with_state(hwnd, |state| state.source.encode_utf16().count() as isize)
        },
        WM_GETTEXT => unsafe {
            with_state(hwnd, |state| copy_utf16(&state.display, w_param, l_param))
        },
        VTM_GETMARKDOWN => unsafe {
            let source = wide_null(&with_state(hwnd, |state| state.source.clone()));
            copy_utf16(&source, w_param, l_param)
        },
        WM_ERASEBKGND => TRUE as LRESULT,
        WM_SIZE => {
            unsafe {
                InvalidateRect(hwnd, null(), TRUE);
            }
            0
        }
        WM_PAINT => {
            unsafe {
                paint_control(hwnd);
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

unsafe fn paint_control(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    let mut rect = RECT::default();
    unsafe {
        GetClientRect(hwnd, &mut rect);
    }
    let brush = unsafe { CreateSolidBrush(BACKGROUND) };
    if !brush.is_null() {
        unsafe {
            FillRect(hdc, &rect, brush);
            DeleteObject(brush as HGDIOBJ);
        }
    }
    unsafe {
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, TEXT_COLOR);
        let font = GetStockObject(DEFAULT_GUI_FONT);
        if !font.is_null() {
            windows_sys::Win32::Graphics::Gdi::SelectObject(hdc, font);
        }
        rect.left += 12;
        rect.top += 12;
        rect.right -= 12;
        rect.bottom -= 12;
        with_state(hwnd, |state| {
            DrawTextW(
                hdc,
                state.display.as_ptr(),
                utf16_len(&state.display) as i32,
                &mut rect,
                DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
            );
        });
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
