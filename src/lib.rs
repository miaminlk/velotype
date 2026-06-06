//! Velotype embeddable Windows control library.

mod app_identity;
mod app_menu;
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
