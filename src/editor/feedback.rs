//! Non-modal feedback banner: a single-line footer strip for transient
//! error/info text that lives outside the main panels, so long messages (like
//! a regex compile error) can never grow or wrap the panel that triggered
//! them. It is composed in normal flow (not absolutely positioned), so it
//! reserves its own vertical space and never overlaps document content.

use gpui::*;

use crate::theme::Theme;

/// Severity of a feedback banner's message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FeedbackKind {
    Error,
    /// Kept for the reusable API even though only `Error` is constructed
    /// today.
    #[allow(dead_code)]
    Info,
}

/// Single-line footer strip that never wraps or grows: the inner div is
/// `overflow_hidden().truncate()` under a `max_w`, mirroring the status bar
/// text. Because it participates in normal flex layout (`flex_shrink_0()`
/// with a fixed height), it reserves vertical space instead of floating over
/// the document.
pub(super) fn render_feedback_banner(
    message: &str,
    kind: FeedbackKind,
    theme: &Theme,
) -> AnyElement {
    let c = &theme.colors;
    let d = &theme.dimensions;
    let text_color = match kind {
        FeedbackKind::Error => c.dialog_danger_button_bg,
        FeedbackKind::Info => c.dialog_body,
    };
    div()
        .id("feedback-banner")
        .w_full()
        .flex_shrink_0()
        .h(px(d.status_bar_height))
        .flex()
        .items_center()
        .justify_center()
        .px(px(d.status_bar_padding_x))
        .bg(c.status_bar_background)
        .border_t(px(1.0))
        .border_color(c.dialog_border)
        .child(
            div()
                .max_w(px(600.0))
                .overflow_hidden()
                .truncate()
                .text_size(px(d.status_bar_text_size))
                .text_color(text_color)
                .child(message.to_string()),
        )
        .into_any_element()
}
