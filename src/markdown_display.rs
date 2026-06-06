use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

pub fn markdown_to_display_text(markdown: &str) -> String {
    let mut out = String::new();
    let parser = Parser::new_ext(markdown, Options::all());
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut pending_line_start = true;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    ensure_blank_line(&mut out);
                    push_heading_prefix(&mut out, level);
                    pending_line_start = false;
                }
                Tag::Paragraph | Tag::BlockQuote(_) | Tag::CodeBlock(_) => {
                    ensure_blank_line(&mut out);
                    pending_line_start = true;
                }
                Tag::List(start) => {
                    ensure_blank_line(&mut out);
                    list_stack.push(start);
                    pending_line_start = true;
                }
                Tag::Item => {
                    ensure_line_start(&mut out);
                    if let Some(current) = list_stack.last_mut() {
                        match current {
                            Some(number) => {
                                out.push_str(&format!("{number}. "));
                                *number += 1;
                            }
                            None => out.push_str("• "),
                        }
                    }
                    pending_line_start = false;
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_)
                | TagEnd::Paragraph
                | TagEnd::BlockQuote(_)
                | TagEnd::CodeBlock
                | TagEnd::Item => {
                    ensure_line_end(&mut out);
                    pending_line_start = true;
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                    ensure_line_end(&mut out);
                    pending_line_start = true;
                }
                _ => {}
            },
            Event::Text(text) | Event::Code(text) => {
                if pending_line_start {
                    ensure_line_start(&mut out);
                }
                out.push_str(&text);
                pending_line_start = false;
            }
            Event::SoftBreak | Event::HardBreak => {
                ensure_line_end(&mut out);
                pending_line_start = true;
            }
            Event::Rule => {
                ensure_blank_line(&mut out);
                out.push_str("────────");
                ensure_line_end(&mut out);
                pending_line_start = true;
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                if pending_line_start {
                    ensure_line_start(&mut out);
                }
                out.push_str(&html);
                pending_line_start = false;
            }
            Event::FootnoteReference(reference) => {
                out.push('[');
                out.push_str(&reference);
                out.push(']');
                pending_line_start = false;
            }
            _ => {}
        }
    }

    out.trim_matches(['\r', '\n']).to_string()
}

fn push_heading_prefix(out: &mut String, level: HeadingLevel) {
    let count = match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    };
    out.push_str(&"#".repeat(count));
    out.push(' ');
}

fn ensure_line_start(out: &mut String) {
    if out.is_empty() || out.ends_with('\n') {
        return;
    }
    out.push('\n');
}

fn ensure_line_end(out: &mut String) {
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

fn ensure_blank_line(out: &mut String) {
    if out.is_empty() {
        return;
    }
    if out.ends_with("\n\n") {
        return;
    }
    ensure_line_end(out);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::markdown_to_display_text;

    #[test]
    fn renders_basic_markdown_as_display_text() {
        let rendered = markdown_to_display_text("# Title\n\n- one\n- **two**");
        assert_eq!(rendered, "# Title\n\n• one\n• two");
    }

    #[test]
    fn renders_ordered_lists_and_code_text() {
        let rendered = markdown_to_display_text("1. `alpha`\n2. beta");
        assert_eq!(rendered, "1. alpha\n2. beta");
    }
}
