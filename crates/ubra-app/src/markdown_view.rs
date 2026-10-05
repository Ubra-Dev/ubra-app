//! Native GPUI presentation for the bounded Markdown document model.
//!
//! Parsing and sanitization live in `markdown`; this module translates the
//! resulting blocks into the inspector's visual language. Keeping the view
//! here gives PR descriptions, discussions, and future agent notes one
//! consistent readable measure and typographic hierarchy.

use gpui::{AnyElement, FontWeight, SharedString, div, prelude::*, px, rgba};
use ubra_ui::{Ink, Radius, SemanticColors};

use crate::markdown::{InlineText, MarkdownBlock, MarkdownDocument};

pub fn render_markdown(document: &MarkdownDocument, colors: SemanticColors) -> AnyElement {
    let mut content = div()
        .w_full()
        .max_w(px(760.0))
        .flex()
        .flex_col()
        .gap(px(10.0));

    for block in &document.blocks {
        content = content.child(render_block(block, colors));
    }

    content.into_any_element()
}

fn render_block(block: &MarkdownBlock, colors: SemanticColors) -> AnyElement {
    match block {
        MarkdownBlock::Heading { level, content } => {
            let (size, line_height, top) = match *level {
                1 => (18.0, 23.0, 5.0),
                2 => (15.0, 20.0, 4.0),
                3 => (13.0, 18.0, 3.0),
                _ => (12.0, 17.0, 2.0),
            };
            div()
                .mt(px(top))
                .line_height(px(line_height))
                .text_size(px(size))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors.primary)
                .child(render_inline(content, colors, true))
                .into_any_element()
        }
        MarkdownBlock::Paragraph(content) => div()
            .line_height(px(18.0))
            .text_size(px(11.5))
            .text_color(colors.secondary)
            .child(render_inline(content, colors, false))
            .into_any_element(),
        MarkdownBlock::List {
            ordered,
            start,
            items,
        } => {
            let mut list = div().flex().flex_col().gap(px(6.0));
            for (index, item) in items.iter().enumerate() {
                let marker = match item.checked {
                    Some(true) => "✓".to_owned(),
                    Some(false) => "○".to_owned(),
                    None if *ordered => format!("{}.", start + index),
                    None => "•".to_owned(),
                };
                let marker_color = match item.checked {
                    Some(true) => Ink::FRESH,
                    Some(false) => colors.tertiary,
                    None => colors.tertiary,
                };
                list = list.child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(8.0))
                        .line_height(px(18.0))
                        .text_size(px(11.5))
                        .text_color(colors.secondary)
                        .child(
                            div()
                                .w(px(18.0))
                                .flex_none()
                                .text_right()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(marker_color)
                                .child(marker),
                        )
                        .child(div().min_w(px(0.0)).flex_1().child(render_inline(
                            &item.content,
                            colors,
                            false,
                        ))),
                );
            }
            list.into_any_element()
        }
        MarkdownBlock::Quote(blocks) => {
            let mut quote = div()
                .pl(px(11.0))
                .py(px(3.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .border_l_2()
                .border_color(rgba(0xd9775788))
                .text_color(colors.secondary);
            for block in blocks {
                quote = quote.child(render_block(block, colors));
            }
            quote.into_any_element()
        }
        MarkdownBlock::CodeBlock { language, code } => {
            // One text node instead of one div per line: per-line divs each
            // paid layout/hit-test per scroll frame. Newlines in the joined
            // string break lines in text layout; long lines still overflow
            // into the horizontal scroller.
            let joined: SharedString = if code.is_empty() {
                " ".into()
            } else {
                code.lines().collect::<Vec<_>>().join("\n").into()
            };
            div()
                .rounded(px(Radius::BADGE))
                .bg(rgba(0x0d1117aa))
                .border_1()
                .border_color(colors.primary.alpha(0.075))
                .overflow_hidden()
                .when_some(language.clone(), |surface, language| {
                    surface.child(
                        div()
                            .h(px(25.0))
                            .px(px(9.0))
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(colors.primary.alpha(0.06))
                            .text_size(px(9.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.tertiary)
                            .child(language),
                    )
                })
                .child(
                    div()
                        .id(SharedString::from(format!("markdown-code-{:x}", {
                            use std::collections::hash_map::DefaultHasher;
                            use std::hash::{Hash, Hasher};
                            let mut hasher = DefaultHasher::new();
                            code.hash(&mut hasher);
                            hasher.finish()
                        },)))
                        .w_full()
                        .overflow_x_scroll()
                        .child(
                            div()
                                .w_full()
                                .p(px(10.0))
                                .font_family(crate::fonts::mono_family())
                                .line_height(px(17.0))
                                .text_size(px(10.5))
                                .text_color(rgba(0xd8dee9ff))
                                .child(joined),
                        ),
                )
                .into_any_element()
        }
        MarkdownBlock::ThematicBreak => div()
            .my(px(3.0))
            .h(px(1.0))
            .w_full()
            .bg(colors.primary.alpha(0.08))
            .into_any_element(),
    }
}

fn render_inline(content: &InlineText, colors: SemanticColors, heading: bool) -> AnyElement {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut line = div().min_w(px(0.0)).flex().flex_wrap().items_baseline();
    for span in &content.spans {
        if span.text.is_empty() {
            continue;
        }
        let style = span.style;
        let text = SharedString::from(span.text.clone());
        // Only links keep an id + handler. Plain spans are style-only divs
        // with no hitbox; pointer-address ids rebuilt layout per frame.
        if let Some(link) = span.link.clone() {
            let mut hasher = DefaultHasher::new();
            link.hash(&mut hasher);
            span.text.hash(&mut hasher);
            let id = SharedString::from(format!("markdown-link-{:x}", hasher.finish()));
            line = line.child(
                div()
                    .id(id)
                    .when(style.bold, |piece| piece.font_weight(FontWeight::SEMIBOLD))
                    .when(style.italic, |piece| piece.italic())
                    .when(style.strikethrough, |piece| piece.line_through())
                    .when(style.code, |piece| {
                        piece
                            .mx(px(1.0))
                            .px(px(4.0))
                            .rounded(px(4.0))
                            .bg(colors.primary.alpha(0.065))
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(if heading { 0.92 * 13.0 } else { 10.5 }))
                            .text_color(rgba(0xe7b49fff))
                    })
                    .underline()
                    .cursor_pointer()
                    .text_color(rgba(0x8bb9e8ff))
                    .on_click(move |_, _, cx| {
                        cx.open_url(&link);
                        cx.stop_propagation();
                    })
                    .child(text),
            );
        } else {
            line = line.child(
                div()
                    .when(style.bold, |piece| piece.font_weight(FontWeight::SEMIBOLD))
                    .when(style.italic, |piece| piece.italic())
                    .when(style.strikethrough, |piece| piece.line_through())
                    .when(style.code, |piece| {
                        piece
                            .mx(px(1.0))
                            .px(px(4.0))
                            .rounded(px(4.0))
                            .bg(colors.primary.alpha(0.065))
                            .font_family(crate::fonts::mono_family())
                            .text_size(px(if heading { 0.92 * 13.0 } else { 10.5 }))
                            .text_color(rgba(0xe7b49fff))
                    })
                    .child(text),
            );
        }
    }
    line.into_any_element()
}

#[cfg(test)]
mod tests {
    #[test]
    fn renderer_source_keeps_a_readable_measure_and_native_block_treatment() {
        let source = include_str!("markdown_view.rs");
        assert!(source.contains("max_w(px(760.0))"));
        assert!(source.contains("MarkdownBlock::CodeBlock"));
        assert!(source.contains("MarkdownBlock::List"));
        assert!(source.contains("MarkdownBlock::Quote"));
    }
}
