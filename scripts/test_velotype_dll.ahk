#Requires AutoHotkey v2.0
#SingleInstance Force

dll_path := A_ScriptDir "\..\target\release\velotype.dll"
if !FileExist(dll_path) {
	MsgBox "Missing DLL: " dll_path
	ExitApp 1
}

module := DllCall("LoadLibraryW", "Str", dll_path, "Ptr")
if !module {
	MsgBox "LoadLibraryW failed for " dll_path
	ExitApp 1
}

main_gui := Gui("+Resize", "Velotype.dll smoke test")
main_gui.MarginX := 0
main_gui.MarginY := 0

readme_path := A_ScriptDir "\..\README.md"
if !FileExist(readme_path) {
	MsgBox "Missing README.md: " readme_path
	ExitApp 1
}
markdown := FileRead(readme_path, "UTF-8")
params := Buffer(80, 0)
NumPut("UInt", 80, params, 0)
NumPut("Ptr", main_gui.Hwnd, params, 8)
NumPut("Int", 12, params, 16)
NumPut("Int", 12, params, 20)
NumPut("Int", 760, params, 24)
NumPut("Int", 520, params, 28)
global control_hwnd := DllCall(
	dll_path "\Velotype_CreateControlEx",
	"Ptr", params,
	"Ptr"
)

if !control_hwnd {
	MsgBox "Velotype_CreateControlEx failed"
	ExitApp 1
}

if !DllCall(dll_path "\Velotype_SetEditorKeyBinding", "Ptr", control_hwnd, "Str", "bold_selection", "Str", "ctrl-alt-b", "Int") {
	MsgBox "Velotype_SetEditorKeyBinding failed for bold_selection"
	ExitApp 1
}

if DllCall(dll_path "\Velotype_SetEditorKeyBinding", "Ptr", control_hwnd, "Str", "open_file", "Str", "ctrl-o", "Int") {
	MsgBox "Velotype_SetEditorKeyBinding unexpectedly accepted file shortcut"
	ExitApp 1
}

if !DllCall(dll_path "\Velotype_SetControlBackgroundColor", "Ptr", control_hwnd, "UInt", 0x00F0F0F0, "Int") {
	MsgBox "Velotype_SetControlBackgroundColor failed"
	ExitApp 1
}

if !DllCall(dll_path "\Velotype_SetThemeParameter", "Ptr", control_hwnd, "Str", "editor_background", "Str", "FFFFFF", "Int") {
	MsgBox "Velotype_SetThemeParameter failed for editor_background"
	ExitApp 1
}

if !DllCall(dll_path "\Velotype_SetThemeParameter", "Ptr", control_hwnd, "Str", "font_size", "Str", "17", "Int") {
	MsgBox "Velotype_SetThemeParameter failed for font_size"
	ExitApp 1
}

if !DllCall(dll_path "\Velotype_SetThemeParameter", "Ptr", control_hwnd, "Str", "line_height", "Str", "1.6", "Int") {
	MsgBox "Velotype_SetThemeParameter failed for line_height"
	ExitApp 1
}

if !DllCall(dll_path "\Velotype_InitializeControl", "Ptr", control_hwnd, "Str", markdown, "Int") {
	MsgBox "Velotype_InitializeControl failed"
	ExitApp 1
}

if !DllCall(dll_path "\Velotype_ShowControl", "Ptr", control_hwnd, "Int", true, "Int") {
	MsgBox "Velotype_ShowControl failed"
	ExitApp 1
}

DllCall(dll_path "\Velotype_SetTheme", "Ptr", control_hwnd, "Str", "velotype-light", "Int")
DllCall(dll_path "\Velotype_SetLanguage", "Ptr", control_hwnd, "Str", "en-US", "Int")
DllCall(dll_path "\Velotype_HideCaret", "Ptr", control_hwnd, "Int")
if !DllCall(dll_path "\Velotype_SetEditorKeyBinding", "Ptr", control_hwnd, "Str", "italic_selection", "Str", "ctrl-alt-i", "Int") {
	MsgBox "Velotype_SetEditorKeyBinding failed after initialize"
	ExitApp 1
}

display_len := DllCall(dll_path "\Velotype_MarkdownToDisplayText", "Str", "# Velotype`n`n- smoke", "Ptr", 0, "UPtr", 0, "UPtr")
if display_len = 0 {
	MsgBox "Velotype_MarkdownToDisplayText failed"
	ExitApp 1
}

html_len := DllCall(dll_path "\Velotype_RenderMarkdownToHtml", "Str", "# Velotype", "Str", "Smoke", "Str", "velotype-light", "Ptr", 0, "UPtr", 0, "UPtr")
if html_len = 0 {
	MsgBox "Velotype_RenderMarkdownToHtml failed"
	ExitApp 1
}

OnMessage(0x0005, resize_control)
main_gui.OnEvent("Close", (*) => ExitApp(0))
main_gui.Show("w800 h560")

auto_close := false
test_input := false
for arg in A_Args {
	if arg = "--auto-close" {
		auto_close := true
	} else if arg = "--test-input" {
		test_input := true
	}
}

if test_input {
	SetTimer test_keyboard_input, -2000
}

if auto_close {
	SetTimer (() => ExitApp(0)), -10000
}

resize_control(w_param, l_param, msg, hwnd) {
	global main_gui, control_hwnd
	if hwnd != main_gui.Hwnd || !control_hwnd {
		return
	}
	width := l_param & 0xffff
	height := (l_param >> 16) & 0xffff
	DllCall("MoveWindow", "Ptr", control_hwnd, "Int", 12, "Int", 12, "Int", width - 24, "Int", height - 24, "Int", true)
}

test_keyboard_input() {
	global main_gui, control_hwnd, dll_path
	main_gui.GetPos(&x, &y, &w, &h)
	WinActivate "ahk_id " main_gui.Hwnd
	if !DllCall(dll_path "\Velotype_SetCaretPosition", "Ptr", control_hwnd, "UInt", 0, "UInt", 0, "Int") {
		MsgBox "Velotype_SetCaretPosition failed"
		ExitApp 1
	}
	Sleep 300
	Click x + 80, y + 80
	Sleep 500
	SendText "__DLL_INPUT_SMOKE__"
	Sleep 2000

	; Retry readback up to 3 times with increasing delays
	md_len := 0
	loop 3 {
		md_len := DllCall(dll_path "\Velotype_GetMarkdownLength", "Ptr", control_hwnd, "UPtr")
		if md_len > 0 {
			break
		}
		Sleep 1000
	}
	if md_len = 0 {
		MsgBox "Velotype_GetMarkdownLength returned 0 after retries"
		ExitApp 1
	}
	md_buf := Buffer((md_len + 1) * 2, 0)
	DllCall(dll_path "\Velotype_GetMarkdown", "Ptr", control_hwnd, "Ptr", md_buf, "UPtr", md_len + 1, "UPtr")
	markdown_after_input := StrGet(md_buf, "UTF-16")
	if !InStr(markdown_after_input, "__DLL_INPUT_SMOKE__") {
		MsgBox "Keyboard input did not update Markdown`nGot: " SubStr(markdown_after_input, 1, 200)
		ExitApp 1
	}
}
