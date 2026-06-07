//! PDF generation is disabled in this build.

use std::path::Path;
use crate::theme::Theme;

pub(crate) fn render_pdf(
	_markdown: &str,
	_theme: &Theme,
	_title: &str,
	_base_path: Option<&Path>,
) -> anyhow::Result<Vec<u8>> {
	anyhow::bail!("PDF export is disabled in this build")
}
