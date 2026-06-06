//! Velotype embeddable Windows control library.

mod markdown_display;

#[cfg(windows)]
mod windows_control;

pub use markdown_display::markdown_to_display_text;
