//! Non-modal feedback banner: a single-line strip for transient error/info
//! text that lives outside the main panels, so long messages (like a regex
//! compile error) can never grow or wrap the panel that triggered them.

use gpui::*;

use crate::theme::Theme;

/// Severity of a feedback banner's message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FeedbackKind {
    Error,
    Info,
}

/// Single-line feedback banner that never wraps or grows: the inner div is
/// `overflow_hidden().truncate()` under a `max_w`, mirroring the truncated
/// breadcrumb label in `workspace.rs`.
pub(super) fn render_feedback_banner(
    message: &str,
    kind: FeedbackKind,
    theme: &Theme,
) -> AnyElement {
    let c = &theme.colors;
    let d = &theme.dimensions;
    let (text_color, bg, border) = match kind {
        FeedbackKind::Error => (
            c.dialog_danger_button_bg,
            c.dialog_surface,
            c.dialog_border,
        ),
        FeedbackKind::Info => (c.dialog_body, c.dialog_surface, c.dialog_border),
    };
    div()
        .id("feedback-banner")
        .absolute()
        .bottom(px(d.status_bar_height + 8.0))
        .left(px(16.0))
        .right(px(16.0))
        .flex()
        .justify_center()
        .child(
            div()
                .max_w(px(600.0))
                .px(px(12.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .bg(bg)
                .border(px(1.0))
                .border_color(border)
                .overflow_hidden()
                .truncate()
                .text_color(text_color)
                .child(message.to_string()),
        )
        .into_any_element()
}
