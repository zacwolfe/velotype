//! A pill-shaped toggle switch component with slide animation.

use std::rc::Rc;
use std::time::Duration;

use gpui::{prelude::FluentBuilder, *};

use crate::theme::ThemeManager;

/// A controlled toggle whose owner updates the checked state after clicks.
#[derive(IntoElement)]
pub(crate) struct Switch {
    /// Stable element identity used to retain the previous animation state.
    id: ElementId,
    /// Checked state supplied by the owner for this render.
    checked: bool,
    /// Visual disabled state; does not suppress the attached click handler.
    disabled: bool,
    /// Optional click callback; the switch does not mutate its own value.
    on_click: Option<Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
}

impl Switch {
    /// Creates an unchecked, enabled switch with no click handler.
    pub fn new(id: impl Into<ElementId>) -> Self {
        Self {
            id: id.into(),
            checked: false,
            disabled: false,
            on_click: None,
        }
    }

    /// Set the checked state.
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }

    /// Set the click handler.
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for Switch {
    /// Renders themed track and thumb, animating changes to the supplied value.
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.global::<ThemeManager>().current().clone();
        let c = &theme.colors;

        let checked = self.checked;
        let disabled = self.disabled;

        let track_color = if disabled {
            c.dialog_secondary_button_bg
        } else if checked {
            c.dialog_primary_button_bg
        } else {
            c.dialog_secondary_button_bg
        };
        let thumb_color = if disabled {
            c.dialog_secondary_button_text
        } else if checked {
            c.dialog_primary_button_text
        } else {
            c.dialog_secondary_button_text
        };

        // Keep the visual position across renders so we can detect changes.
        let toggle_state = window.use_keyed_state::<bool>(self.id.clone(), cx, |_, _| checked);
        let prev_checked = *toggle_state.read(cx);
        let target: f32 = if checked { 16.0 } else { 0.0 };
        let origin: f32 = if prev_checked { 16.0 } else { 0.0 };
        let needs_animation = prev_checked != checked;
        let duration = Duration::from_secs_f64(0.18);

        if needs_animation {
            cx.spawn({
                let toggle_state = toggle_state.clone();
                async move |cx| {
                    cx.background_executor().timer(duration).await;
                    _ = toggle_state.update(cx, |state, _| *state = checked);
                }
            })
            .detach();
        }

        let thumb = div()
            .w(px(16.0))
            .h(px(16.0))
            .rounded(px(8.0))
            .bg(thumb_color)
            .map(|mut this| {
                if needs_animation {
                    this.with_animation(
                        ElementId::NamedInteger("switch-move".into(), checked as u64),
                        Animation::new(duration),
                        move |mut this, delta| {
                            let margin = origin + (target - origin) * delta;
                            this.style().margin.left =
                                Some(Length::Definite(DefiniteLength::from(px(margin))));
                            this
                        },
                    )
                    .into_any_element()
                } else {
                    this.style().margin.left =
                        Some(Length::Definite(DefiniteLength::from(px(target))));
                    this.into_any_element()
                }
            });

        div()
            .id(self.id)
            .w(px(36.0))
            .h(px(20.0))
            .px(px(2.0))
            .flex()
            .items_center()
            .rounded(px(10.0))
            .bg(track_color)
            .when(!disabled, |this| this.cursor_pointer())
            .child(thumb)
            .when_some(self.on_click, |this, on_click| this.on_click(on_click))
    }
}

/// Callback invoked when a switch row is toggled by click or keyboard.
type ToggleHandler = dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static;

/// A row containing a label and a toggle switch, supporting keyboard activation.
#[derive(IntoElement)]
pub(crate) struct SwitchRow {
    id: ElementId,
    label: SharedString,
    checked: bool,
    focus_handle: Option<FocusHandle>,
    label_size: Pixels,
    label_color: Hsla,
    show_label: bool,
    on_toggle: Option<Box<ToggleHandler>>,
}

impl SwitchRow {
    /// Creates a new switch row with the given label and checked state.
    pub fn new(
        label: impl Into<SharedString>,
        checked: bool,
        focus_handle: Option<FocusHandle>,
    ) -> Self {
        let label = label.into();
        Self {
            id: ElementId::Name(format!("preferences-row-{}", label).into()),
            label,
            checked,
            focus_handle,
            label_size: px(13.0),
            label_color: Hsla::black(),
            show_label: true,
            on_toggle: None,
        }
    }

    /// Set the label style.
    pub fn label_style(mut self, size: Pixels, color: Hsla) -> Self {
        self.label_size = size;
        self.label_color = color;
        self
    }

    /// Hides the label, leaving only the switch as the row content.
    #[allow(dead_code)]
    pub fn without_label(mut self) -> Self {
        self.show_label = false;
        self
    }

    /// Set the toggle callback for both click and keyboard events.
    pub fn on_toggle(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_toggle = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for SwitchRow {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let toggle: Rc<ToggleHandler> =
            Rc::from(self.on_toggle.unwrap_or_else(|| Box::new(|_, _, _| {})));

        let label_el = div()
            .text_size(self.label_size)
            .text_color(self.label_color)
            .child(self.label.clone());

        let switch = Switch::new(ElementId::Name(
            format!("preferences-toggle-{}", self.label).into(),
        ))
        .checked(self.checked)
        .on_click({
            let t = toggle.clone();
            move |event, window, cx| t(event, window, cx)
        });

        div()
            .id(self.id)
            .when_some(self.focus_handle, |this, handle| this.track_focus(&handle))
            .flex()
            .items_center()
            .when(self.show_label, |this| this.w(px(280.0)).justify_between())
            .on_key_down({
                let t = toggle.clone();
                move |event, window, cx| {
                    if event.keystroke.modifiers.modified() {
                        return;
                    }
                    if !matches!(event.keystroke.key.as_str(), "space" | "enter") {
                        return;
                    }
                    t(&ClickEvent::default(), window, cx);
                    cx.stop_propagation();
                }
            })
            .when(self.show_label, |this| this.child(label_el))
            .child(switch)
    }
}
