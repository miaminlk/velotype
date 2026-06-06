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

if !DllCall(dll_path "\Velotype_InitializeControl", "Ptr", control_hwnd, "Str", markdown, "Int") {
	MsgBox "Velotype_InitializeControl failed"
	ExitApp 1
}

if !DllCall(dll_path "\Velotype_ShowControl", "Ptr", control_hwnd, "Int", true, "Int") {
	MsgBox "Velotype_ShowControl failed"
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
