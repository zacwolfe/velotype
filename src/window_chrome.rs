//! Shared window chrome helpers for themed client-side title bars.

use gpui::prelude::*;
use gpui::{
    AnyElement, Bounds, ClickEvent, Context, Decorations, Hsla, MouseButton, Pixels, SharedString,
    TitlebarOptions, Window, WindowBackgroundAppearance, WindowBounds, WindowButton,
    WindowButtonLayout, WindowControlArea, WindowDecorations, WindowOptions, div, point, px, rgba,
    svg,
};

use crate::app_identity::VELOTYPE_APP_ID;
use crate::theme::{Theme, ThemeDimensions};

/// Minimum custom titlebar height in logical pixels.
const TITLEBAR_MIN_HEIGHT: f32 = 32.0;
/// Width of each application-rendered window control in logical pixels.
const TITLEBAR_BUTTON_WIDTH: f32 = 46.0;
/// Side length of titlebar control icons in logical pixels.
const TITLEBAR_ICON_SIZE: f32 = 12.0;
/// Space reserved on each side of the title around macOS traffic lights.
const MAC_TRAFFIC_LIGHT_RESERVED_WIDTH: f32 = 84.0;
/// Embedded asset path for the close control.
const TITLEBAR_CLOSE_ICON: &str = "icon/titlebar/chrome-close.svg";
/// Embedded asset path for the maximize control.
const TITLEBAR_MAXIMIZE_ICON: &str = "icon/titlebar/chrome-maximize.svg";
/// Embedded asset path for the minimize control.
const TITLEBAR_MINIMIZE_ICON: &str = "icon/titlebar/chrome-minimize.svg";
/// Embedded asset path shown when a maximized or fullscreen window can restore.
const TITLEBAR_RESTORE_ICON: &str = "icon/titlebar/chrome-restore.svg";

/// Selects whether Velotype or the platform should render window controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TitlebarControlMode {
    /// Leaves macOS window controls to the platform.
    NativeTrafficLights,
    /// Draws and handles minimize, maximize, and close controls in the app.
    AppControls,
}

/// Layout metadata shared by editor and preferences windows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CustomTitlebarLayout {
    /// Height in logical pixels, including the titlebar's bottom border.
    pub(crate) height: f32,
    /// Owner of the visible window controls.
    pub(crate) controls: TitlebarControlMode,
}

/// Chooses the drag mechanism for the platform titlebar implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TitlebarDragStrategy {
    /// Marks a drag region for GPUI's platform hit testing.
    PlatformHitTest,
    /// Starts a native move on mouse-down and zooms on double-click.
    ExplicitMoveRequest,
}

/// Keeps the existing right-side order when the platform does not supply a layout.
fn titlebar_button_layout(platform_layout: Option<WindowButtonLayout>) -> WindowButtonLayout {
    platform_layout.unwrap_or(WindowButtonLayout {
        left: [None; 3],
        right: [
            Some(WindowButton::Minimize),
            Some(WindowButton::Maximize),
            Some(WindowButton::Close),
        ],
    })
}

/// Sets the native title, transparency, and macOS traffic-light position.
pub(crate) fn titlebar_options_for_target_os(
    target_os: &str,
    title: SharedString,
) -> TitlebarOptions {
    TitlebarOptions {
        title: Some(title),
        appears_transparent: matches!(target_os, "macos" | "windows"),
        traffic_light_position: if target_os == "macos" {
            Some(point(px(14.0), px(10.0)))
        } else {
            None
        },
    }
}

/// Requests client decorations on Linux/FreeBSD, otherwise leaving GPUI defaults.
pub(crate) fn window_decorations_for_target_os(target_os: &str) -> Option<WindowDecorations> {
    match target_os {
        "linux" | "freebsd" => Some(WindowDecorations::Client),
        _ => None,
    }
}

/// Builds opaque, windowed options with shared app identity and platform chrome.
pub(crate) fn velotype_window_options_for_target_os(
    target_os: &str,
    title: SharedString,
    bounds: Bounds<Pixels>,
) -> WindowOptions {
    WindowOptions {
        app_id: Some(VELOTYPE_APP_ID.to_string()),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(titlebar_options_for_target_os(target_os, title)),
        window_background: WindowBackgroundAppearance::Opaque,
        window_decorations: window_decorations_for_target_os(target_os),
        ..WindowOptions::default()
    }
}

/// Builds the shared window options for the host operating system.
pub(crate) fn velotype_window_options(
    title: SharedString,
    bounds: Bounds<Pixels>,
) -> WindowOptions {
    velotype_window_options_for_target_os(std::env::consts::OS, title, bounds)
}

/// Selects custom titlebar geometry, or `None` when native decorations suffice.
pub(crate) fn custom_titlebar_layout_for_target_os(
    target_os: &str,
    decorations: Decorations,
    dimensions: &ThemeDimensions,
) -> Option<CustomTitlebarLayout> {
    let height = dimensions.menu_bar_height.max(TITLEBAR_MIN_HEIGHT);
    match target_os {
        "macos" => Some(CustomTitlebarLayout {
            height,
            controls: TitlebarControlMode::NativeTrafficLights,
        }),
        "windows" => Some(CustomTitlebarLayout {
            height,
            controls: TitlebarControlMode::AppControls,
        }),
        "linux" | "freebsd" if matches!(decorations, Decorations::Client { .. }) => {
            Some(CustomTitlebarLayout {
                height,
                controls: TitlebarControlMode::AppControls,
            })
        }
        _ => None,
    }
}

/// Windows/macOS use hit-test drag areas; Linux client decorations need an explicit move request.
pub(crate) fn titlebar_drag_strategy_for_target_os(
    target_os: &str,
    decorations: Decorations,
) -> TitlebarDragStrategy {
    match target_os {
        "linux" | "freebsd" if matches!(decorations, Decorations::Client { .. }) => {
            TitlebarDragStrategy::ExplicitMoveRequest
        }
        _ => TitlebarDragStrategy::PlatformHitTest,
    }
}

/// Returns the custom titlebar height, or zero when no custom layout is needed.
pub(crate) fn custom_titlebar_height_for_target_os(
    target_os: &str,
    decorations: Decorations,
    dimensions: &ThemeDimensions,
) -> f32 {
    custom_titlebar_layout_for_target_os(target_os, decorations, dimensions)
        .map(|layout| layout.height)
        .unwrap_or(0.0)
}

/// Returns the host titlebar height, hiding custom chrome in macOS fullscreen.
pub(crate) fn custom_titlebar_height(window: &Window, dimensions: &ThemeDimensions) -> f32 {
    if cfg!(target_os = "macos") && window.is_fullscreen() {
        return 0.0;
    }

    custom_titlebar_height_for_target_os(
        std::env::consts::OS,
        window.window_decorations(),
        dimensions,
    )
}

/// Uses the theme's dialog surface as the custom titlebar background.
pub(crate) fn custom_titlebar_background(theme: &Theme) -> Hsla {
    theme.colors.dialog_surface
}

/// Chooses light or dark control icons from the titlebar background's lightness.
pub(crate) fn custom_titlebar_icon_color(theme: &Theme) -> Hsla {
    if custom_titlebar_background(theme).l < 0.5 {
        Hsla::from(rgba(0xf4f4f5ff))
    } else {
        Hsla::from(rgba(0x18181bff))
    }
}

/// Selects restore for maximized/fullscreen windows and maximize otherwise.
pub(crate) fn titlebar_maximize_icon(is_maximized: bool, is_fullscreen: bool) -> &'static str {
    if is_maximized || is_fullscreen {
        TITLEBAR_RESTORE_ICON
    } else {
        TITLEBAR_MAXIMIZE_ICON
    }
}

/// Renders platform chrome with drag handling and the owner's close callback.
///
/// Returns `None` for unsupported decorations and macOS fullscreen windows.
pub(crate) fn render_custom_titlebar<T: 'static>(
    id: &'static str,
    title: SharedString,
    theme: &Theme,
    window: &Window,
    cx: &mut Context<T>,
    on_close: fn(&mut T, &ClickEvent, &mut Window, &mut Context<T>),
) -> Option<AnyElement> {
    if cfg!(target_os = "macos") && window.is_fullscreen() {
        return None;
    }

    let layout = custom_titlebar_layout_for_target_os(
        std::env::consts::OS,
        window.window_decorations(),
        &theme.dimensions,
    )?;
    let drag_strategy =
        titlebar_drag_strategy_for_target_os(std::env::consts::OS, window.window_decorations());
    let c = &theme.colors;
    let t = &theme.typography;
    let controls = window.window_controls();
    let icon_color = custom_titlebar_icon_color(theme);
    let entity = cx.entity().downgrade();

    let drag_title = div()
        .id("window-titlebar-drag-title")
        .h_full()
        .flex_1()
        .min_w(px(0.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .window_control_area(WindowControlArea::Drag)
        .child(
            div()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(theme.dimensions.menu_text_size))
                .font_weight(t.dialog_button_weight.to_font_weight())
                .text_color(c.dialog_secondary_button_text)
                .child(title),
        );

    let drag_title = match drag_strategy {
        TitlebarDragStrategy::PlatformHitTest => drag_title,
        TitlebarDragStrategy::ExplicitMoveRequest => {
            drag_title.on_mouse_down(MouseButton::Left, |event, window, cx| {
                if event.click_count >= 2 {
                    window.zoom_window();
                } else {
                    window.start_window_move();
                }
                cx.stop_propagation();
            })
        }
    }
    .on_click(|event, window, _cx| {
        if event.is_right_click() {
            window.show_window_menu(event.position());
        } else if event.click_count() >= 2 {
            // Platform-hit-test drag (macOS/Windows) does not zoom on its
            // own here: the region is only marked as a drag area, not a
            // real system title bar, so the OS never sees the double-click
            // gesture. A single click or a drag never reaches `on_click`
            // (no completed click at the same position), so this cannot
            // fire mid-drag.
            window.zoom_window();
        }
    });

    let root = div()
        .id(id)
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .h(px(layout.height))
        .occlude()
        .flex()
        .items_center()
        .bg(custom_titlebar_background(theme))
        .border_b(px(theme.dimensions.dialog_border_width))
        .border_color(c.dialog_border);

    let root = match layout.controls {
        TitlebarControlMode::NativeTrafficLights => root
            .child(div().w(px(MAC_TRAFFIC_LIGHT_RESERVED_WIDTH)).h_full())
            .child(drag_title)
            .child(div().w(px(MAC_TRAFFIC_LIGHT_RESERVED_WIDTH)).h_full()),
        TitlebarControlMode::AppControls => {
            let close_entity = entity.clone();
            let layout_buttons = titlebar_button_layout(cx.button_layout());

            // Build each button once as an owned element, then place it
            // according to the desktop layout.
            let mut minimize_btn: Option<AnyElement> = if controls.minimize {
                Some(
                    div()
                        .id("window-titlebar-minimize")
                        .w(px(TITLEBAR_BUTTON_WIDTH))
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .window_control_area(WindowControlArea::Min)
                        .hover(|this| this.bg(c.dialog_secondary_button_hover))
                        .cursor_pointer()
                        .child(
                            svg()
                                .path(TITLEBAR_MINIMIZE_ICON)
                                .size(px(TITLEBAR_ICON_SIZE))
                                .text_color(icon_color),
                        )
                        .on_click(|event, window, _cx| {
                            if event.standard_click() {
                                window.minimize_window();
                            }
                        })
                        .into_any_element(),
                )
            } else {
                None
            };

            let mut maximize_btn: Option<AnyElement> = if controls.maximize {
                Some(
                    div()
                        .id("window-titlebar-maximize")
                        .w(px(TITLEBAR_BUTTON_WIDTH))
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .window_control_area(WindowControlArea::Max)
                        .hover(|this| this.bg(c.dialog_secondary_button_hover))
                        .cursor_pointer()
                        .child(
                            svg()
                                .path(titlebar_maximize_icon(
                                    window.is_maximized(),
                                    window.is_fullscreen(),
                                ))
                                .size(px(TITLEBAR_ICON_SIZE))
                                .text_color(icon_color),
                        )
                        .on_click(|event, window, _cx| {
                            if event.standard_click() {
                                window.zoom_window();
                            }
                        })
                        .into_any_element(),
                )
            } else {
                None
            };

            let mut close_btn: Option<AnyElement> = Some(
                div()
                    .id("window-titlebar-close")
                    .w(px(TITLEBAR_BUTTON_WIDTH))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .window_control_area(WindowControlArea::Close)
                    .hover(|this| this.bg(c.dialog_danger_button_bg))
                    .cursor_pointer()
                    .child(
                        svg()
                            .path(TITLEBAR_CLOSE_ICON)
                            .size(px(TITLEBAR_ICON_SIZE))
                            .text_color(icon_color),
                    )
                    .on_click(move |event, window, app| {
                        if event.standard_click() {
                            let _ = close_entity.update(app, |view, cx| {
                                on_close(view, event, window, cx);
                            });
                        }
                    })
                    .into_any_element(),
            );

            let mut left_row = div().h_full().flex().items_center().flex_shrink_0();
            for kind in layout_buttons.left.into_iter().flatten() {
                let btn = match kind {
                    WindowButton::Minimize => minimize_btn.take(),
                    WindowButton::Maximize => maximize_btn.take(),
                    WindowButton::Close => close_btn.take(),
                };
                if let Some(b) = btn {
                    left_row = left_row.child(b);
                }
            }

            let mut right_row = div().h_full().flex().items_center().flex_shrink_0();
            for kind in layout_buttons.right.into_iter().flatten() {
                let btn = match kind {
                    WindowButton::Minimize => minimize_btn.take(),
                    WindowButton::Maximize => maximize_btn.take(),
                    WindowButton::Close => close_btn.take(),
                };
                if let Some(b) = btn {
                    right_row = right_row.child(b);
                }
            }

            // Any buttons not referenced by the layout fall back to the right.
            for btn in [minimize_btn.take(), maximize_btn.take(), close_btn.take()]
                .into_iter()
                .flatten()
            {
                right_row = right_row.child(btn);
            }

            root.child(left_row).child(drag_title).child(right_row)
        }
    };

    Some(root.into_any_element())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Tiling;

    #[test]
    fn titlebar_options_enable_transparency_on_mac_and_windows() {
        assert!(titlebar_options_for_target_os("windows", "Velotype".into()).appears_transparent);
        assert!(titlebar_options_for_target_os("macos", "Velotype".into()).appears_transparent);
        assert!(!titlebar_options_for_target_os("linux", "Velotype".into()).appears_transparent);
    }

    #[test]
    fn linux_and_freebsd_request_client_decorations() {
        assert_eq!(
            window_decorations_for_target_os("linux"),
            Some(WindowDecorations::Client)
        );
        assert_eq!(
            window_decorations_for_target_os("freebsd"),
            Some(WindowDecorations::Client)
        );
        assert_eq!(window_decorations_for_target_os("unknown"), None);
    }

    #[test]
    fn custom_titlebar_height_respects_platform_and_decorations() {
        let dimensions = Theme::default_theme().dimensions;
        assert_eq!(
            custom_titlebar_height_for_target_os("windows", Decorations::Server, &dimensions),
            dimensions.menu_bar_height.max(TITLEBAR_MIN_HEIGHT)
        );
        assert_eq!(
            custom_titlebar_height_for_target_os(
                "linux",
                Decorations::Client {
                    tiling: Tiling::default()
                },
                &dimensions,
            ),
            dimensions.menu_bar_height.max(TITLEBAR_MIN_HEIGHT)
        );
        assert_eq!(
            custom_titlebar_height_for_target_os("linux", Decorations::Server, &dimensions),
            0.0
        );
        assert_eq!(
            custom_titlebar_height_for_target_os("unknown", Decorations::Server, &dimensions),
            0.0
        );
    }

    #[test]
    fn titlebar_drag_strategy_matches_platform_window_api() {
        assert_eq!(
            titlebar_drag_strategy_for_target_os("windows", Decorations::Server),
            TitlebarDragStrategy::PlatformHitTest
        );
        assert_eq!(
            titlebar_drag_strategy_for_target_os("macos", Decorations::Server),
            TitlebarDragStrategy::PlatformHitTest
        );
        assert_eq!(
            titlebar_drag_strategy_for_target_os(
                "linux",
                Decorations::Client {
                    tiling: Tiling::default()
                },
            ),
            TitlebarDragStrategy::ExplicitMoveRequest
        );
        assert_eq!(
            titlebar_drag_strategy_for_target_os("linux", Decorations::Server),
            TitlebarDragStrategy::PlatformHitTest
        );
    }

    #[test]
    fn custom_titlebar_background_uses_dialog_surface_token() {
        let theme = Theme::light_theme();
        assert_eq!(
            custom_titlebar_background(&theme),
            theme.colors.dialog_surface
        );
    }

    #[test]
    fn custom_titlebar_icon_color_contrasts_with_theme_surface() {
        assert_eq!(
            custom_titlebar_icon_color(&Theme::default_theme()),
            Hsla::from(rgba(0xf4f4f5ff))
        );
        assert_eq!(
            custom_titlebar_icon_color(&Theme::light_theme()),
            Hsla::from(rgba(0x18181bff))
        );
    }

    #[test]
    fn titlebar_maximize_icon_tracks_window_state() {
        assert_eq!(titlebar_maximize_icon(false, false), TITLEBAR_MAXIMIZE_ICON);
        assert_eq!(titlebar_maximize_icon(true, false), TITLEBAR_RESTORE_ICON);
        assert_eq!(titlebar_maximize_icon(false, true), TITLEBAR_RESTORE_ICON);
    }

    #[test]
    fn button_layout_preserves_platform_order() {
        let platform_layout = WindowButtonLayout {
            left: [Some(WindowButton::Close), None, None],
            right: [
                Some(WindowButton::Minimize),
                Some(WindowButton::Maximize),
                None,
            ],
        };
        assert_eq!(
            titlebar_button_layout(Some(platform_layout)),
            platform_layout
        );
    }

    #[test]
    fn button_layout_without_platform_setting_falls_back_to_all_right() {
        let layout = titlebar_button_layout(None);
        assert!(layout.left.iter().all(Option::is_none));
        assert_eq!(
            layout.right,
            [
                Some(WindowButton::Minimize),
                Some(WindowButton::Maximize),
                Some(WindowButton::Close),
            ]
        );
    }
}
