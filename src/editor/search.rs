//! Find bar: a small floating, non-modal search box that lets the user
//! locate text across the whole document (including table cells) and jump
//! to matches.
//!
//! Jumping to a match reuses the same focus + selection mechanism as
//! footnote navigation (see `Editor::jump_to_footnote_definition`): the
//! target block's `selected_range` is set and the block is focused, which
//! gets the existing selection painting, scroll-into-view, and (for
//! rendered-mode blocks) inline-projection coordinate remap for free. Search
//! itself only ever reads text from blocks while they are unfocused, so
//! `display_text()` is always the clean, unprojected text no matter which
//! view mode the document is in.

use std::ops::Range;
use std::time::Instant;

use gpui::{prelude::FluentBuilder, *};

use super::{Block, BlockKind, Editor};
use crate::components::{Find, FindNext, FindPrevious};
use crate::i18n::I18nManager;
use crate::theme::Theme;

/// One occurrence of the current query in the document.
#[derive(Clone)]
pub(super) struct SearchMatch {
    pub(super) entity_id: EntityId,
    pub(super) range: Range<usize>,
}

/// State for the open find bar.
pub(super) struct SearchBarState {
    pub(super) focus_handle: FocusHandle,
    pub(super) query: String,
    pub(super) matches: Vec<SearchMatch>,
    pub(super) active_index: Option<usize>,
    /// Whatever had real focus before the bar stole it, so closing the bar
    /// without ever jumping to a match restores it.
    previous_focus: Option<EntityId>,
}

/// Case-insensitive (ASCII-fold) substring search. Folding only ASCII bytes
/// keeps every returned range a valid boundary in the original `haystack`,
/// since folding never changes UTF-8 byte length or byte-boundary
/// placement — unlike a full Unicode `to_lowercase`, which can.
fn find_all_case_insensitive(haystack: &str, needle_lower: &str) -> Vec<Range<usize>> {
    if needle_lower.is_empty() {
        return Vec::new();
    }
    let haystack_lower = haystack.to_ascii_lowercase();
    let mut ranges = Vec::new();
    let mut start = 0;
    while let Some(pos) = haystack_lower[start..].find(needle_lower) {
        let match_start = start + pos;
        let match_end = match_start + needle_lower.len();
        ranges.push(match_start..match_end);
        start = match_end;
    }
    ranges
}

impl Editor {
    fn collect_search_matches(&self, query_lower: &str, cx: &App) -> Vec<SearchMatch> {
        let mut matches = Vec::new();
        for visible in self.document.visible_blocks() {
            let block = visible.entity.read(cx);
            if block.kind() == BlockKind::Table {
                let Some(runtime) = block.table_runtime.as_ref() else {
                    continue;
                };
                for cell in runtime.header.iter().chain(runtime.rows.iter().flatten()) {
                    let cell_ref = cell.read(cx);
                    for range in find_all_case_insensitive(cell_ref.display_text(), query_lower) {
                        matches.push(SearchMatch {
                            entity_id: cell.entity_id(),
                            range,
                        });
                    }
                }
                continue;
            }
            for range in find_all_case_insensitive(block.display_text(), query_lower) {
                matches.push(SearchMatch {
                    entity_id: visible.entity.entity_id(),
                    range,
                });
            }
        }
        matches
    }

    pub(super) fn recompute_search_matches(&mut self, cx: &mut Context<Self>) {
        let Some(query_lower) = self
            .search_bar
            .as_ref()
            .map(|search| search.query.to_ascii_lowercase())
        else {
            return;
        };
        let matches = self.collect_search_matches(&query_lower, cx);
        if let Some(search) = self.search_bar.as_mut() {
            search.matches = matches;
            // Typing never jumps or focuses anything by itself, so there is
            // no "current" match yet — the count label reads 0/N until the
            // user explicitly steps to the first one.
            search.active_index = None;
        }
    }

    /// Sets the target's selection to `range` and focuses it, mirroring
    /// `focus_block_range`'s footnote-jump convention: this earns the
    /// existing selection paint, scroll-into-view, and (for a rendered-mode
    /// block) inline-projection coordinate remap at no extra cost.
    fn focus_search_match(
        &mut self,
        entity: &Entity<Block>,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) {
        entity.update(cx, move |block, cx| {
            block.selected_range = range;
            block.selection_reversed = false;
            block.marked_range = None;
            block.vertical_motion_x = None;
            block.cursor_blink_epoch = Instant::now();
            cx.notify();
        });
        self.focus_block(entity.entity_id());
    }

    fn go_to_search_match(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(m) = self
            .search_bar
            .as_ref()
            .and_then(|search| search.matches.get(index).cloned())
        else {
            return;
        };
        if let Some(search) = self.search_bar.as_mut() {
            search.active_index = Some(index);
        }
        let Some(entity) = self.focusable_entity_by_id(m.entity_id) else {
            return;
        };
        self.focus_search_match(&entity, m.range, cx);
    }

    fn step_search_match(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(search) = self.search_bar.as_ref() else {
            return;
        };
        let count = search.matches.len();
        if count == 0 {
            return;
        }
        let next_index = match search.active_index {
            Some(current) if forward => (current + 1) % count,
            Some(current) => (current + count - 1) % count,
            None => 0,
        };
        self.go_to_search_match(next_index, cx);
    }

    fn open_search_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(search) = self.search_bar.as_ref() {
            let handle = search.focus_handle.clone();
            window.focus(&handle);
            return;
        }
        let previous_focus = self.focused_edit_target_entity_id(window, cx);
        let focus_handle = cx.focus_handle();
        self.search_bar = Some(SearchBarState {
            focus_handle: focus_handle.clone(),
            query: String::new(),
            matches: Vec::new(),
            active_index: None,
            previous_focus,
        });
        window.focus(&focus_handle);
        cx.notify();
    }

    pub(super) fn close_search_bar(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search_bar.take() else {
            return;
        };
        if search.active_index.is_none()
            && let Some(previous) = search.previous_focus
        {
            self.focus_block(previous);
        }
        cx.notify();
    }

    pub(super) fn on_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.open_search_bar(window, cx);
    }

    pub(super) fn on_find_next(
        &mut self,
        _: &FindNext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_search_match(true, cx);
    }

    pub(super) fn on_find_previous(
        &mut self,
        _: &FindPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_search_match(false, cx);
    }

    pub(super) fn on_search_prev_click(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_search_match(false, cx);
    }

    pub(super) fn on_search_next_click(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_search_match(true, cx);
    }

    pub(super) fn on_search_close_click(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_search_bar(cx);
    }

    pub(super) fn on_search_query_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search_bar.is_none() {
            return;
        }
        let keystroke = event.keystroke.clone();
        let mods = keystroke.modifiers;
        let is_shortcut_modifier = mods.platform || mods.control;

        match keystroke.key.as_str() {
            "backspace" => {
                if let Some(search) = self.search_bar.as_mut() {
                    search.query.pop();
                }
                self.recompute_search_matches(cx);
                cx.notify();
                cx.stop_propagation();
                return;
            }
            "enter" => {
                self.step_search_match(!mods.shift, cx);
                cx.stop_propagation();
                return;
            }
            "v" if is_shortcut_modifier => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let sanitized: String =
                        text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
                    if !sanitized.is_empty()
                        && let Some(search) = self.search_bar.as_mut()
                    {
                        search.query.push_str(&sanitized);
                    }
                    self.recompute_search_matches(cx);
                    cx.notify();
                }
                cx.stop_propagation();
                return;
            }
            _ => {}
        }

        if is_shortcut_modifier {
            return;
        }

        if let Some(ch) = keystroke.key_char.as_deref() {
            if let Some(search) = self.search_bar.as_mut() {
                search.query.push_str(ch);
            }
            self.recompute_search_matches(cx);
            cx.notify();
            cx.stop_propagation();
        }
    }

    pub(super) fn render_search_bar_overlay(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let search = self.search_bar.as_ref()?;
        let c = &theme.colors;
        let d = &theme.dimensions;
        let strings = cx.global::<I18nManager>().strings().clone();
        let has_matches = !search.matches.is_empty();

        let count_label: String = if search.query.is_empty() {
            String::new()
        } else if search.matches.is_empty() {
            strings.search_no_matches.clone()
        } else {
            format!(
                "{}/{}",
                search.active_index.map(|i| i + 1).unwrap_or(0),
                search.matches.len()
            )
        };

        let nav_button = |id: &'static str, label: &'static str, enabled: bool| {
            div()
                .id(id)
                .w(px(22.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .text_color(c.dialog_secondary_button_text)
                .when(enabled, |this| {
                    this.cursor_pointer()
                        .hover(|this| this.bg(c.dialog_secondary_button_hover))
                })
                .child(label)
        };

        Some(
            div()
                .id("search-bar")
                .absolute()
                .top(px(12.0))
                .right(px(16.0))
                .track_focus(&search.focus_handle)
                .on_key_down(cx.listener(Self::on_search_query_key_down))
                .flex()
                .items_center()
                .gap(px(6.0))
                .p(px(8.0))
                .bg(c.dialog_surface)
                .border(px(d.dialog_border_width))
                .border_color(c.dialog_border)
                .rounded(px(d.dialog_radius))
                .shadow_lg()
                .child(
                    div()
                        .w(px(180.0))
                        .h(px(d.code_language_input_height.max(20.0)))
                        .px(px(6.0))
                        .flex()
                        .items_center()
                        .gap(px(2.0))
                        .rounded(px(6.0))
                        .bg(c.code_language_input_bg)
                        .border(px(1.0))
                        .border_color(c.code_language_input_border)
                        .child(if search.query.is_empty() {
                            div()
                                .text_color(c.code_language_input_placeholder)
                                .child(strings.search_placeholder.clone())
                        } else {
                            div()
                                .text_color(c.code_language_input_text)
                                .child(search.query.clone())
                        })
                        .when(!search.query.is_empty(), |this| {
                            this.child(div().w(px(1.0)).h(px(14.0)).bg(c.cursor))
                        }),
                )
                .child(
                    div()
                        .min_w(px(56.0))
                        .text_color(c.dialog_muted)
                        .child(count_label),
                )
                .child(
                    nav_button("search-prev", "‹", has_matches).on_click(cx.listener(
                        |editor, event, window, cx| editor.on_search_prev_click(event, window, cx),
                    )),
                )
                .child(
                    nav_button("search-next", "›", has_matches).on_click(cx.listener(
                        |editor, event, window, cx| editor.on_search_next_click(event, window, cx),
                    )),
                )
                .child(nav_button("search-close", "×", true).on_click(cx.listener(
                    |editor, event, window, cx| editor.on_search_close_click(event, window, cx),
                )))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::find_all_case_insensitive;

    #[test]
    fn finds_case_insensitive_non_overlapping_matches() {
        let ranges = find_all_case_insensitive("Foo foo FOO bar", "foo");
        assert_eq!(ranges, vec![0..3, 4..7, 8..11]);
    }

    #[test]
    fn preserves_byte_offsets_around_multibyte_text() {
        // "café" has a 2-byte 'é'; the trailing "TEXT" must still resolve to
        // valid, correct byte offsets despite the earlier multibyte content.
        let ranges = find_all_case_insensitive("café TEXT", "text");
        let haystack = "café TEXT";
        assert_eq!(ranges.len(), 1);
        assert_eq!(&haystack[ranges[0].clone()], "TEXT");
    }

    #[test]
    fn empty_needle_yields_no_matches() {
        assert!(find_all_case_insensitive("anything", "").is_empty());
    }
}
