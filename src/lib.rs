//! Velotype embeddable Windows control library.

#![allow(dead_code)]

mod app_identity;
#[cfg(test)]
mod app_menu;
#[cfg(not(test))]
mod app_menu {
    use std::path::Path;

    use gpui::*;

    use crate::editor::Editor;

    pub(crate) fn dispatch_menu_action_for_editor(
        _action: &dyn Action,
        _target: &WeakEntity<Editor>,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    pub(crate) fn install_cli_tool(_cx: &mut App) {}

    pub(crate) fn uninstall_cli_tool(_cx: &mut App) {}

    pub(crate) fn request_quit_application(cx: &mut App) {
        cx.quit();
    }

    pub(crate) fn record_recent_file_from_editor(_path: &Path, _cx: &mut App) {}

    pub(crate) fn install_menus(_cx: &mut App) {}
}

mod components;
#[allow(dead_code, unused_imports)]
mod config;
mod editor;
mod export;
mod i18n;
mod markdown_display;
mod net;
mod theme;
mod window_chrome;

#[cfg(windows)]
mod windows_control;

pub use markdown_display::markdown_to_display_text;
