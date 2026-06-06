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

markdown := "# Velotype.dll`r`n`r`n- Loaded by AutoHotkey v2`r`n- Created as a Win32 child control`r`n- Markdown text is rendered by the DLL"
style := 0x40000000 | 0x10000000 | 0x00800000 | 0x00010000
global control_hwnd := DllCall(
	"CreateWindowExW",
	"UInt", 0,
	"Str", "Velotype",
	"Str", markdown,
	"UInt", style,
	"Int", 12,
	"Int", 12,
	"Int", 760,
	"Int", 520,
	"Ptr", main_gui.Hwnd,
	"Ptr", 0,
	"Ptr", module,
	"Ptr", 0,
	"Ptr"
)

if !control_hwnd {
	MsgBox "CreateWindowExW failed for class Velotype"
	ExitApp 1
}

OnMessage(0x0005, resize_control)
main_gui.OnEvent("Close", (*) => ExitApp(0))
main_gui.Show("w800 h560")

if A_Args.Length && A_Args[1] = "--auto-close" {
	SetTimer (() => ExitApp(0)), -2000
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
