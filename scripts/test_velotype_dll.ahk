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
global control_hwnd := DllCall(
	dll_path "\Velotype_CreateAsChildControl",
	"Ptr", main_gui.Hwnd,
	"Int", 12,
	"Int", 12,
	"Int", 760,
	"Int", 520,
	"Str", markdown,
	"Ptr"
)

if !control_hwnd {
	MsgBox "Velotype_CreateAsChildControl failed"
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
