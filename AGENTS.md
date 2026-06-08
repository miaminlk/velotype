# Project Overview & Development Guidelines

This is a custom branch of Velotype (a multi-platform Markdown editor). The target is to compile it as `Velotype.dll`, serving as a Markdown rendering control similar to how `Scintilla.dll` works.

## Dev Roadmap & Progress

- [x] Analyze codebase structure, tech stack, and control flows.
- [x] De-generalize cross-platform abstractions in favor of native WinAPI (replacing windowing mechanisms with raw Win32 HWND and window messages).
- [x] Configure and verify compilation environment.
- [ ] Replace standard `image` crate with `imgpix.dll` for BGRA decoding and lossless JXL clipboard serialization.
- [ ] Clean up redundant and unused dependencies (0% probability libraries):
  - [x] Remove `chromiumoxide` (Headless browser controller) from root dependencies.
  - [x] Remove `scap` (Screen capture) dependency and feature flags.
  - [x] Remove/Disable optional cross-platform features (`media`, `objc2`, `ashpd`, X11/Wayland unused features).
  - [ ] Refactor network dependencies: isolate or remove `reqwest`/`gpui_http_client` (substitute with host-provided FFI callbacks for fetching remote assets).
- [ ] Implement Markdown rendering control interface in `Velotype.dll` (referencing `scintilla/` and WinAPI patterns).

## Practical Rules

- **Avoid Compilation**: Use `cargo check` to verify code correctness. Avoid compiling the full binary/DLL unless explicitly requested by the user, as the compile process is slow.
- **Git Commit**: Use Chinese for all git commit messages (Git 提交说明必须使用中文).
- **Writing New Code** (do not change existing styles):
  1. **Naming**: Use Snake Case.
  2. **Indent**: Use tabs (`\t`).
  3. **Braces `{`**:
     * Definitions (functions, classes, structs): Keep on the **same line**.
     * Control flow (`if`, `loop`, `match`): Move to a **new line**.


