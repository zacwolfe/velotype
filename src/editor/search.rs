//! Find and replace: a floating, non-modal bar that locates text across the
//! whole document (including table cells), highlights every match, and can
//! replace one match or all of them.
//!
//! Jumping to a match reuses the same focus + selection mechanism as
//! footnote navigation (see `Editor::jump_to_footnote_definition`): the
//! target block's `selected_range` is set and the block is focused, which
//! gets the existing selection painting, scroll-into-view, and (for
//! rendered-mode blocks) inline-projection coordinate remap for free. Search
//! itself only ever reads text from blocks while they are unfocused, so
//! `display_text()` is always the clean, unprojected text no matter which
//! view mode the document is in.
//!
//! While the bar is open, though, stepping between matches deliberately keeps
//! keyboard focus in the search field: only the selection, the active
//! highlight, and an explicit scroll target move, so Enter can be pressed
//! repeatedly without the caret jumping into the document. The block is only
//! focused when the bar closes, or when `cmd-g` runs with the bar already
//! closed.
//!
//! Editing always goes through
//! [`Block::replace_text_in_visible_range`](crate::components::Block::replace_text_in_visible_range),
//! never through raw string surgery on the Markdown source: that primitive is
//! what keeps inline links, code spans, math, and link-reference spellings
//! intact.

use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};

use unicode_segmentation::UnicodeSegmentation;

use gpui::{prelude::FluentBuilder, *};

use super::search_match::{Matcher, MatcherError, SearchOptions};
use super::{Block, BlockKind, Editor};
use crate::components::{Find, FindNext, FindPrevious, Replace, ReplaceAll, UndoCaptureKind};
use crate::config::preferences::{AppPreferences, save_app_preferences};
use crate::config::{SearchPreferences, read_app_preferences};
use crate::i18n::I18nManager;
use crate::theme::Theme;

/// Hard cap on collected matches. A one-character query on a big document
/// would otherwise stall every keystroke; the bar reports `10 000+` instead.
pub(super) const MAX_SEARCH_MATCHES: usize = 10_000;

/// Caret stays solid this long after each edit or caret move, so typing never
/// fights a blink. Mirrors the block caret's timing.
const SEARCH_CURSOR_SOLID_AFTER_EDIT: Duration = Duration::from_millis(500);
/// How long the caret stays in each on/off state once blinking.
const SEARCH_CURSOR_BLINK_HALF_PERIOD: Duration = Duration::from_millis(500);
/// Blink poll interval. A fraction of the half-period so a state flip is never
/// more than this late, while still costing ~4 repaints/second instead of ~30.
const SEARCH_CURSOR_BLINK_POLL: Duration = Duration::from_millis(125);

/// One occurrence of the current query in the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SearchMatch {
    pub(super) entity_id: EntityId,
    pub(super) range: Range<usize>,
}

/// Which of the bar's two text fields currently owns keystrokes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SearchField {
    Query,
    Replacement,
}

/// State for the open find bar.
pub(super) struct SearchBarState {
    pub(super) focus_handle: FocusHandle,
    pub(super) query: String,
    pub(super) query_cursor: usize,
    pub(super) replacement: String,
    pub(super) replacement_cursor: usize,
    pub(super) show_replace_row: bool,
    pub(super) focused_field: SearchField,
    pub(super) options: SearchOptions,
    /// Compiled form of `query` + `options`; `None` when the query is empty or
    /// the regex failed to compile.
    pub(super) matcher: Option<Matcher>,
    /// Regex compile error text, shown in the bar and which disables replace.
    pub(super) matcher_error: Option<String>,
    pub(super) matches: Vec<SearchMatch>,
    pub(super) active_index: Option<usize>,
    /// True when collection stopped at [`MAX_SEARCH_MATCHES`].
    pub(super) truncated: bool,
    /// Last Replace All count, shown briefly in place of the match counter.
    pub(super) last_replaced: Option<usize>,
    /// Token to detect whether a newer replace operation has occurred.
    pub(super) last_replaced_token: u64,
    /// Whatever had real focus before the bar stole it, so closing the bar
    /// without ever jumping to a match restores it.
    previous_focus: Option<EntityId>,
    /// When the last edit or caret move happened; the caret stays solid for a
    /// short window after this before it starts blinking.
    cursor_blink_epoch: Instant,
    /// Repaints the caret only when its on/off state flips. Dropped when the
    /// bar closes or no field is focused, so a closed bar costs nothing.
    cursor_blink_task: Option<Task<()>>,
}

impl SearchBarState {
    fn clear_results(&mut self) {
        self.matches.clear();
        self.active_index = None;
        self.truncated = false;
    }

    fn cursor(&self, field: SearchField) -> usize {
        match field {
            SearchField::Query => self.query_cursor,
            SearchField::Replacement => self.replacement_cursor,
        }
    }

    fn set_cursor(&mut self, field: SearchField, cursor: usize) {
        match field {
            SearchField::Query => self.query_cursor = cursor,
            SearchField::Replacement => self.replacement_cursor = cursor,
        }
        // Every caret move is an edit for blink purposes: hold the caret solid
        // briefly so the user sees it land before it starts blinking.
        self.reset_cursor_blink();
    }

    /// Marks the caret as freshly moved/edited so it stays solid for a short
    /// window before resuming its blink.
    fn reset_cursor_blink(&mut self) {
        self.cursor_blink_epoch = Instant::now();
    }

    /// Caret alpha: solid for [`SEARCH_CURSOR_SOLID_AFTER_EDIT`] after each
    /// reset, then a hard on/off step every [`SEARCH_CURSOR_BLINK_HALF_PERIOD`].
    ///
    /// Deliberately a step rather than a fade: GPUI has no partial
    /// invalidation, so a smooth fade would repaint the whole window ~30 times
    /// a second while the caret merely sits there. A step repaints only when
    /// the state flips.
    fn cursor_opacity(&self) -> f32 {
        let elapsed = self.cursor_blink_epoch.elapsed();
        if elapsed < SEARCH_CURSOR_SOLID_AFTER_EDIT {
            return 1.0;
        }
        let phases = (elapsed - SEARCH_CURSOR_SOLID_AFTER_EDIT).as_millis()
            / SEARCH_CURSOR_BLINK_HALF_PERIOD.as_millis();
        if phases % 2 == 0 {
            1.0
        } else {
            0.0
        }
    }

    fn text(&self, field: SearchField) -> &str {
        match field {
            SearchField::Query => &self.query,
            SearchField::Replacement => &self.replacement,
        }
    }

    fn text_mut(&mut self, field: SearchField) -> &mut String {
        match field {
            SearchField::Query => &mut self.query,
            SearchField::Replacement => &mut self.replacement,
        }
    }
}

impl Editor {
    /// Finds every occurrence of the current query, in document order.
    fn collect_search_matches(&self, matcher: &Matcher, cx: &App) -> (Vec<SearchMatch>, bool) {
        let mut matches = Vec::new();
        let mut truncated = false;

        let mut push_ranges = |entity_id: EntityId, text: &str| {
            for range in matcher.find_all(text) {
                if matches.len() >= MAX_SEARCH_MATCHES {
                    truncated = true;
                    return;
                }
                matches.push(SearchMatch { entity_id, range });
            }
        };

        for visible in self.document.visible_blocks() {
            let block = visible.entity.read(cx);
            if block.kind() == BlockKind::Table {
                let Some(runtime) = block.table_runtime.as_ref() else {
                    continue;
                };
                for cell in runtime.header.iter().chain(runtime.rows.iter().flatten()) {
                    let cell_ref = cell.read(cx);
                    push_ranges(cell.entity_id(), cell_ref.display_text());
                }
                continue;
            }
            if !searchable_kind(block.kind()) {
                continue;
            }
            push_ranges(visible.entity.entity_id(), block.display_text());
        }

        (matches, truncated)
    }

    /// Re-runs the query after a document edit so highlights and the counter
    /// stay in sync with the text.
    ///
    /// Skipped while Replace All is mid-batch: the batch emits one `Changed`
    /// per edited block, and recomputing each time would be quadratic. The
    /// batch recomputes once when it finishes.
    pub(super) fn refresh_search_after_edit(&mut self, cx: &mut Context<Self>) {
        if self.search_replace_batch_in_progress || self.search_bar.is_none() {
            return;
        }
        self.recompute_search_matches(cx);
    }

    /// Rebuilds the match list and repaints highlights. This is the single
    /// funnel for every trigger: query edits, option toggles, opening the bar,
    /// document edits, view-mode switches, and replacements.
    pub(super) fn recompute_search_matches(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search_bar.as_mut() else {
            self.clear_search_highlights(cx);
            return;
        };

        // Resolve the previously active match so a re-query after an edit (or
        // after a replacement) keeps the user where they were instead of
        // silently jumping to match 0.
        let previous_anchor = search
            .active_index
            .and_then(|index| search.matches.get(index))
            .map(|found| (found.entity_id, found.range.start));

        match Matcher::build(&search.query, &search.options) {
            Ok(matcher) => {
                search.matcher = Some(matcher);
                search.matcher_error = None;
            }
            Err(MatcherError::Empty) => {
                search.matcher = None;
                search.matcher_error = None;
                search.clear_results();
            }
            Err(error) => {
                search.matcher = None;
                search.matcher_error = Some(error.to_string());
                search.clear_results();
            }
        }

        // Compile once, then walk the document.
        if let Some(matcher) = self.search_bar.as_ref().and_then(|s| s.matcher.clone()) {
            let (matches, truncated) = self.collect_search_matches(&matcher, cx);
            if let Some(search) = self.search_bar.as_mut() {
                search.matches = matches;
                search.truncated = truncated;
                search.active_index = previous_anchor.and_then(|(entity_id, offset)| {
                    nearest_match_index(&search.matches, entity_id, offset)
                });
            }
        }

        self.apply_search_highlights(cx);
        cx.notify();
    }

    /// Clears highlight ranges on every block that could have carried one.
    fn clear_search_highlights(&mut self, cx: &mut Context<Self>) {
        let entities = self.highlightable_entities(cx);
        for entity in entities {
            entity.update(cx, |block, _cx| {
                if !block.search_highlights.is_empty() || block.search_active_highlight.is_some() {
                    block.search_highlights.clear();
                    block.search_active_highlight = None;
                }
            });
        }
    }

    /// Every entity search can produce a match for, in document order.
    fn highlightable_entities(&self, cx: &App) -> Vec<Entity<Block>> {
        let mut entities = Vec::new();
        for visible in self.document.visible_blocks() {
            let block = visible.entity.read(cx);
            if block.kind() == BlockKind::Table {
                if let Some(runtime) = block.table_runtime.as_ref() {
                    for cell in runtime.header.iter().chain(runtime.rows.iter().flatten()) {
                        entities.push(cell.clone());
                    }
                }
                continue;
            }
            if searchable_kind(block.kind()) {
                entities.push(visible.entity.clone());
            }
        }
        entities
    }

    /// Pushes the current match list into block-local highlight ranges.
    ///
    /// Ranges are only valid while the block is unfocused, which is the state
    /// they were collected in; a block that gains focus clears them, so the
    /// two never disagree.
    fn apply_search_highlights(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search_bar.as_ref() else {
            self.clear_search_highlights(cx);
            return;
        };

        let mut grouped: HashMap<EntityId, Vec<Range<usize>>> = HashMap::new();
        for found in &search.matches {
            grouped
                .entry(found.entity_id)
                .or_default()
                .push(found.range.clone());
        }
        let active = search
            .active_index
            .and_then(|index| search.matches.get(index))
            .map(|found| (found.entity_id, found.range.clone()));

        let entities = self.highlightable_entities(cx);
        for entity in entities {
            let entity_id = entity.entity_id();
            let ranges = grouped.remove(&entity_id).unwrap_or_default();
            let active_range = active
                .as_ref()
                .filter(|(active_id, _)| *active_id == entity_id)
                .map(|(_, range)| range.clone());
            entity.update(cx, |block, _cx| {
                if block.search_highlights != ranges {
                    block.search_highlights = ranges;
                }
                if block.search_active_highlight != active_range {
                    block.search_active_highlight = active_range;
                }
            });
        }
        // Entities that are no longer visible (deleted blocks) keep stale
        // ranges harmlessly; they are not rendered.
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

    /// Marks `range` as the active match without taking keyboard focus away
    /// from the search field: the selection and the active highlight move and
    /// the view scrolls, but the user keeps typing (and keeps pressing Enter)
    /// in the bar. `focus_search_match` is what `cmd-g` uses when the bar is
    /// closed and the caret really should move.
    fn reveal_search_match(
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
        // Caret scrolling follows the focused block and the block is left
        // unfocused here, so aim the scroll at the match explicitly.
        self.pending_scroll_target = Some(entity.entity_id());
        self.pending_scroll_active_block_into_view = true;
        self.pending_scroll_align = super::ScrollAlign::Nearest;
    }

    fn go_to_search_match(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(found) = self
            .search_bar
            .as_ref()
            .and_then(|search| search.matches.get(index).cloned())
        else {
            return;
        };
        if let Some(search) = self.search_bar.as_mut() {
            search.active_index = Some(index);
        }
        let Some(entity) = self.focusable_entity_by_id(found.entity_id) else {
            return;
        };
        // With the bar open focus stays in the query field, so Enter can step
        // again; with it closed the caret moves to the match the way `cmd-f`
        // followed by `cmd-g` has always done.
        if self.search_bar.is_some() {
            self.reveal_search_match(&entity, found.range, cx);
        } else {
            self.focus_search_match(&entity, found.range, cx);
        }
        self.apply_search_highlights(cx);
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

    /// Seeds `active_index` with the match nearest the caret, so the first
    /// Enter steps forward from where the user actually was.
    fn seed_active_index_from_caret(&mut self, cx: &App) {
        let Some(anchor) = self.caret_search_anchor(cx) else {
            return;
        };
        let Some(search) = self.search_bar.as_mut() else {
            return;
        };
        search.active_index = nearest_match_index(&search.matches, anchor.0, anchor.1);
    }

    /// `(entity_id, offset)` of the caret, from the editor's active block.
    /// While the bar is open no block is focused, so this is the block the
    /// user was editing before the bar took focus.
    fn caret_search_anchor(&self, cx: &App) -> Option<(EntityId, usize)> {
        let entity_id = self.active_entity_id?;
        let entity = self.focusable_entity_by_id(entity_id)?;
        let block = entity.read(cx);
        Some((entity_id, block.selected_range.start))
    }

    /// Text of the current single-block selection, used to prefill `cmd-f`.
    fn selected_text_for_find(&self, cx: &App) -> Option<String> {
        let (entity_id, _) = self.caret_search_anchor(cx)?;
        let entity = self.focusable_entity_by_id(entity_id)?;
        let block = entity.read(cx);
        if block.selected_range.is_empty() {
            return None;
        }
        let text = block
            .display_text()
            .get(block.selected_range.start..block.selected_range.end)?
            .to_string();
        if text.is_empty() || text.contains('\n') {
            return None;
        }
        Some(text)
    }

    fn open_search_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_search_bar_with_replace_row(false, window, cx);
    }

    fn open_search_bar_with_replace_row(
        &mut self,
        show_replace_row: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let preferences = load_search_preferences();
        if let Some(search) = self.search_bar.as_mut() {
            let handle = search.focus_handle.clone();
            search.show_replace_row |= show_replace_row;
            if show_replace_row {
                search.focused_field = SearchField::Replacement;
            }
            window.focus(&handle);
            cx.notify();
            return;
        }

        let previous_focus = self.focused_edit_target_entity_id(window, cx);
        let selection_prefill = self.selected_text_for_find(cx);

        let mut query = if let Some(selection) = selection_prefill {
            selection
        } else {
            std::mem::take(&mut self.last_search_query)
        };
        // Multi-line selections cannot be a useful query.
        query = query.replace(['\n', '\r'], " ");

        let focus_handle = cx.focus_handle();
        let query_cursor = query.len();
        let replacement = std::mem::take(&mut self.last_search_replacement);
        let replacement_cursor = replacement.len();
        let mut state = SearchBarState {
            focus_handle: focus_handle.clone(),
            query,
            query_cursor,
            replacement,
            replacement_cursor,
            show_replace_row: show_replace_row || preferences.show_replace_row,
            focused_field: if show_replace_row {
                SearchField::Replacement
            } else {
                SearchField::Query
            },
            options: preferences.options(),
            matcher: None,
            matcher_error: None,
            matches: Vec::new(),
            active_index: None,
            truncated: false,
            last_replaced: None,
            last_replaced_token: 0,
            previous_focus,
            cursor_blink_epoch: Instant::now(),
            cursor_blink_task: None,
        };
        state.show_replace_row = state.show_replace_row || show_replace_row;
        self.search_bar = Some(state);

        window.focus(&focus_handle);
        self.recompute_search_matches(cx);
        self.seed_active_index_from_caret(cx);
        self.apply_search_highlights(cx);
        cx.notify();
    }

    pub(super) fn close_search_bar(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search_bar.take() else {
            return;
        };
        // Keep the typed text so reopening remembers it.
        self.last_search_query = search.query;
        self.last_search_replacement = search.replacement;
        // Stepping leaves focus in the bar, so closing it is where the caret
        // finally lands on the active match. Without a jump, restore whatever
        // had focus before the bar opened.
        let active = search
            .active_index
            .and_then(|index| search.matches.get(index))
            .cloned();
        if let Some(found) = active {
            if let Some(entity) = self.focusable_entity_by_id(found.entity_id) {
                self.focus_search_match(&entity, found.range, cx);
            }
        } else if let Some(previous) = search.previous_focus {
            self.focus_block(previous);
        }
        self.clear_search_highlights(cx);
        cx.notify();
    }

    /// Multiplies the search state's options by flipping one toggle and
    /// persists the result, then re-runs the query.
    pub(super) fn toggle_search_option(
        &mut self,
        toggle: SearchOptionToggle,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = self.search_bar.as_mut() else {
            return;
        };
        match toggle {
            SearchOptionToggle::CaseSensitive => {
                search.options.case_sensitive = !search.options.case_sensitive;
            }
            SearchOptionToggle::WholeWord => {
                search.options.whole_word = !search.options.whole_word;
            }
            SearchOptionToggle::Regex => {
                search.options.regex = !search.options.regex;
            }
            SearchOptionToggle::ReplaceRow => {
                search.show_replace_row = !search.show_replace_row;
                search.focused_field = if search.show_replace_row {
                    SearchField::Replacement
                } else {
                    SearchField::Query
                };
                cx.notify();
                return;
            }
        }
        let options = search.options;
        persist_search_options(options);
        if let Some(search) = self.search_bar.as_mut() {
            search.last_replaced = None;
        }
        self.recompute_search_matches(cx);
        self.seed_active_index_from_caret(cx);
        cx.notify();
    }

    pub(crate) fn on_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.open_search_bar(window, cx);
    }

    pub(crate) fn on_replace(&mut self, _: &Replace, window: &mut Window, cx: &mut Context<Self>) {
        self.open_search_bar_with_replace_row(true, window, cx);
    }

    pub(crate) fn on_replace_all_action(
        &mut self,
        _: &ReplaceAll,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_all_search_matches(cx);
    }

    pub(crate) fn on_find_next(
        &mut self,
        _: &FindNext,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_search_match(true, cx);
    }

    pub(crate) fn on_find_previous(
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

    pub(super) fn on_search_case_toggle_click(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search_option(SearchOptionToggle::CaseSensitive, cx);
    }

    pub(super) fn on_search_whole_word_toggle_click(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search_option(SearchOptionToggle::WholeWord, cx);
    }

    pub(super) fn on_search_regex_toggle_click(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search_option(SearchOptionToggle::Regex, cx);
    }

    pub(super) fn on_search_replace_toggle_click(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search_option(SearchOptionToggle::ReplaceRow, cx);
    }

    pub(super) fn on_search_replace_one_click(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_active_search_match(cx);
    }

    pub(super) fn on_search_replace_all_click(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_all_search_matches(cx);
    }

    /// Gives the given field the caret and moves keyboard focus to the bar's
    /// focus handle, so keystrokes land in the field instead of the document
    /// behind the bar. Also resets the caret blink so a freshly focused field
    /// starts solid.
    fn focus_search_field(
        &mut self,
        field: SearchField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(search) = self.search_bar.as_mut() {
            search.focused_field = field;
            if field == SearchField::Replacement {
                search.show_replace_row = true;
            }
            search.reset_cursor_blink();
            window.focus(&search.focus_handle);
            cx.notify();
        }
    }

    pub(super) fn on_search_query_field_click(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_search_field(SearchField::Query, window, cx);
    }

    pub(super) fn on_search_replace_field_click(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_search_field(SearchField::Replacement, window, cx);
    }

    /// Mouse-down on the query field: take the caret and keyboard focus, and
    /// swallow the event so it never reaches the source text block underneath,
    /// which would otherwise steal focus and swallow the next keystroke.
    pub(super) fn on_search_query_field_mouse_down(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_search_field(SearchField::Query, window, cx);
        cx.stop_propagation();
    }

    /// Mouse-down on the replacement field: same as the query field, but for
    /// the replace row.
    pub(super) fn on_search_replace_field_mouse_down(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_search_field(SearchField::Replacement, window, cx);
        cx.stop_propagation();
    }

    pub(super) fn on_search_query_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = self.search_bar.as_ref() else {
            return;
        };
        let field = search.focused_field;

        let keystroke = event.keystroke.clone();
        let mods = keystroke.modifiers;
        let is_shortcut_modifier = mods.platform || mods.control;

        match keystroke.key.as_str() {
            "escape" => {
                self.close_search_bar(cx);
                cx.stop_propagation();
                return;
            }
            "tab" => {
                if let Some(search) = self.search_bar.as_mut() {
                    search.show_replace_row = true;
                    search.focused_field = match search.focused_field {
                        SearchField::Query => SearchField::Replacement,
                        SearchField::Replacement => SearchField::Query,
                    };
                }
                cx.notify();
                cx.stop_propagation();
                return;
            }
            "enter" if is_shortcut_modifier && mods.alt => {
                self.replace_all_search_matches(cx);
                cx.stop_propagation();
                return;
            }
            "enter" => {
                match field {
                    SearchField::Replacement => self.replace_active_search_match(cx),
                    SearchField::Query => self.step_search_match(!mods.shift, cx),
                }
                cx.stop_propagation();
                return;
            }
            "backspace" => {
                if let Some(search) = self.search_bar.as_mut() {
                    let cursor = search.cursor(field);
                    let new_cursor = grapheme_left(search.text(field), cursor);
                    if new_cursor < cursor {
                        search.text_mut(field).replace_range(new_cursor..cursor, "");
                        search.set_cursor(field, new_cursor);
                    }
                    if field == SearchField::Query {
                        search.last_replaced = None;
                    }
                }
                if field == SearchField::Query {
                    self.recompute_search_matches(cx);
                } else {
                    cx.notify();
                }
                cx.stop_propagation();
                return;
            }
            "delete" => {
                if let Some(search) = self.search_bar.as_mut() {
                    let cursor = search.cursor(field);
                    let end = grapheme_right(search.text(field), cursor);
                    if end > cursor {
                        search.text_mut(field).replace_range(cursor..end, "");
                        search.reset_cursor_blink();
                    }
                    if field == SearchField::Query {
                        search.last_replaced = None;
                    }
                }
                if field == SearchField::Query {
                    self.recompute_search_matches(cx);
                } else {
                    cx.notify();
                }
                cx.stop_propagation();
                return;
            }
            "left" => {
                if let Some(search) = self.search_bar.as_mut() {
                    let cursor = search.cursor(field);
                    let new_cursor = if mods.alt {
                        word_left(search.text(field), cursor)
                    } else if is_shortcut_modifier {
                        0
                    } else {
                        grapheme_left(search.text(field), cursor)
                    };
                    search.set_cursor(field, new_cursor);
                }
                cx.notify();
                cx.stop_propagation();
                return;
            }
            "right" => {
                if let Some(search) = self.search_bar.as_mut() {
                    let cursor = search.cursor(field);
                    let new_cursor = if mods.alt {
                        word_right(search.text(field), cursor)
                    } else if is_shortcut_modifier {
                        search.text(field).len()
                    } else {
                        grapheme_right(search.text(field), cursor)
                    };
                    search.set_cursor(field, new_cursor);
                }
                cx.notify();
                cx.stop_propagation();
                return;
            }
            "home" => {
                if let Some(search) = self.search_bar.as_mut() {
                    search.set_cursor(field, 0);
                }
                cx.notify();
                cx.stop_propagation();
                return;
            }
            "end" => {
                if let Some(search) = self.search_bar.as_mut() {
                    let len = search.text(field).len();
                    search.set_cursor(field, len);
                }
                cx.notify();
                cx.stop_propagation();
                return;
            }
            "v" if is_shortcut_modifier => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    let sanitized: String =
                        text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
                    if !sanitized.is_empty() {
                        if let Some(search) = self.search_bar.as_mut() {
                            let cursor = search.cursor(field);
                            search.text_mut(field).insert_str(cursor, &sanitized);
                            search.set_cursor(field, cursor + sanitized.len());
                            if field == SearchField::Query {
                                search.last_replaced = None;
                            }
                        }
                        if field == SearchField::Query {
                            self.recompute_search_matches(cx);
                        } else {
                            cx.notify();
                        }
                    }
                }
                cx.stop_propagation();
                return;
            }
            "c" if mods.alt => {
                self.toggle_search_option(SearchOptionToggle::CaseSensitive, cx);
                cx.stop_propagation();
                return;
            }
            "w" if mods.alt => {
                self.toggle_search_option(SearchOptionToggle::WholeWord, cx);
                cx.stop_propagation();
                return;
            }
            "r" if mods.alt => {
                self.toggle_search_option(SearchOptionToggle::Regex, cx);
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
                let cursor = search.cursor(field);
                search.text_mut(field).insert_str(cursor, ch);
                search.set_cursor(field, cursor + ch.len());
                if field == SearchField::Query {
                    search.last_replaced = None;
                }
            }
            if field == SearchField::Query {
                self.recompute_search_matches(cx);
            } else {
                cx.notify();
            }
            cx.stop_propagation();
        }
    }

    /// Starts or stops the caret-blink repaint task for the open bar.
    ///
    /// The blink repaints the whole window, so it must not run for a window the
    /// user is not looking at, and it must stop as soon as the bar loses
    /// keyboard focus (e.g. the user clicked into the document). The task
    /// repaints only when the on/off state flips, never on every poll.
    fn update_search_cursor_blink(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.search_bar.as_mut() else {
            return;
        };
        let blink_active =
            search.focus_handle.is_focused(window) && window.is_window_active();
        if blink_active && search.cursor_blink_task.is_none() {
            search.cursor_blink_epoch = Instant::now();
            search.cursor_blink_task = Some(cx.spawn(
                async |this: WeakEntity<Editor>, cx: &mut AsyncApp| {
                    let mut last_visible: Option<bool> = None;
                    loop {
                        cx.background_executor().timer(SEARCH_CURSOR_BLINK_POLL).await;
                        let still_running = this
                            .update(cx, |this: &mut Editor, cx: &mut Context<Editor>| {
                                let Some(search) = this.search_bar.as_ref() else {
                                    return false;
                                };
                                if search
                                    .cursor_blink_epoch
                                    .elapsed()
                                    < SEARCH_CURSOR_SOLID_AFTER_EDIT
                                {
                                    last_visible = None;
                                    return true;
                                }
                                let visible = search.cursor_opacity() > 0.5;
                                if last_visible != Some(visible) {
                                    last_visible = Some(visible);
                                    cx.notify();
                                }
                                true
                            })
                            .unwrap_or(false);
                        if !still_running {
                            break;
                        }
                    }
                },
            ));
        } else if !blink_active && search.cursor_blink_task.is_some() {
            search.cursor_blink_task = None;
        }
    }

    pub(super) fn render_search_bar_overlay(
        &mut self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.update_search_cursor_blink(window, cx);
        let search = self.search_bar.as_ref()?;
        let c = &theme.colors;
        let d = &theme.dimensions;
        let strings = cx.global::<I18nManager>().strings().clone();
        let has_matches = !search.matches.is_empty();
        let replace_enabled = has_matches && search.matcher_error.is_none();

        let count_label: String = if let Some(replaced) = search.last_replaced {
            strings
                .search_replaced_count
                .replace("{count}", &replaced.to_string())
        } else if search.query.is_empty() {
            String::new()
        } else if let Some(error) = search.matcher_error.as_ref() {
            format!("{}: {}", strings.search_invalid_regex, error)
        } else if search.matches.is_empty() {
            strings.search_no_matches.clone()
        } else if search.truncated {
            strings
                .search_matches_truncated
                .replace("{count}", &search.matches.len().to_string())
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

        let toggle_button =
            |id: &'static str, label: &'static str, active: bool, tooltip: String| {
                div()
                    .id(id)
                    .w(px(22.0))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .text_color(if active {
                        c.dialog_primary_button_text
                    } else {
                        c.dialog_secondary_button_text
                    })
                    .when(active, |this| this.bg(c.dialog_primary_button_bg))
                    .when(!active, |this| {
                        this.hover(|this| this.bg(c.dialog_secondary_button_hover))
                    })
                    .tooltip({
                        let bg = c.dialog_surface;
                        let fg = c.dialog_body;
                        let border = c.dialog_border;
                        move |_window, cx| {
                            let label = tooltip.clone();
                            cx.new(|_| SearchTooltip {
                                label: label.into(),
                                bg,
                                fg,
                                border,
                            })
                            .into()
                        }
                    })
                    .child(label)
            };

        let field = |placeholder: String, value: String, active: bool, cursor: usize| {
            let cursor = cursor.min(value.len());
            div()
                .flex_1()
                .min_w(px(220.0))
                .h(px(d.code_language_input_height.max(20.0)))
                .px(px(6.0))
                .flex()
                .items_center()
                .gap(px(2.0))
                .rounded(px(6.0))
                .bg(c.code_language_input_bg)
                .border(px(1.0))
                .border_color(if active {
                    c.code_language_input_border
                } else {
                    c.dialog_border
                })
                .child(if value.is_empty() {
                    if active {
                        div()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .w(px(1.0))
                                    .h(px(14.0))
                                    .bg(c.cursor)
                                    .opacity(search.cursor_opacity()),
                            )
                            .child(
                                div()
                                    .text_color(c.code_language_input_placeholder)
                                    .child(placeholder),
                            )
                    } else {
                        div()
                            .text_color(c.code_language_input_placeholder)
                            .child(placeholder)
                    }
                } else if active {
                    let (prefix, suffix) = value.split_at(cursor);
                    div()
                        .flex()
                        .items_center()
                        .child(
                            div()
                                .text_color(c.code_language_input_text)
                                .child(prefix.to_string()),
                        )
                        .child(
                            div()
                                .w(px(1.0))
                                .h(px(14.0))
                                .bg(c.cursor)
                                .opacity(search.cursor_opacity()),
                        )
                        .child(
                            div()
                                .text_color(c.code_language_input_text)
                                .child(suffix.to_string()),
                        )
                } else {
                    div().text_color(c.code_language_input_text).child(value)
                })
        };

        let query_field = field(
            strings.search_placeholder.clone(),
            search.query.clone(),
            search.focused_field == SearchField::Query,
            search.query_cursor,
        )
        .id("search-query-field")
        .debug_selector(|| "search-query-field".to_string())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(Self::on_search_query_field_mouse_down),
        )
        .on_click(cx.listener(Self::on_search_query_field_click));

        let replace_field = field(
            strings.search_replace_placeholder.clone(),
            search.replacement.clone(),
            search.focused_field == SearchField::Replacement,
            search.replacement_cursor,
        )
        .id("search-replace-field")
        .debug_selector(|| "search-replace-field".to_string())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(Self::on_search_replace_field_mouse_down),
        )
        .on_click(cx.listener(Self::on_search_replace_field_click));

        let replace_row = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(div().w(px(22.0)).h(px(22.0)).child(""))
            .child(replace_field)
            .child(
                div()
                    .id("search-replace-one")
                    .px(px(6.0))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .text_color(if replace_enabled {
                        c.dialog_secondary_button_text
                    } else {
                        c.dialog_muted
                    })
                    .when(replace_enabled, |this| {
                        this.cursor_pointer()
                            .hover(|this| this.bg(c.dialog_secondary_button_hover))
                    })
                    .when(!replace_enabled, |this| this.opacity(0.5))
                    .child(strings.search_replace_label.clone())
                    .on_click(cx.listener(Self::on_search_replace_one_click)),
            )
            .child(
                div()
                    .id("search-replace-all")
                    .px(px(6.0))
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .rounded(px(4.0))
                    .text_color(if replace_enabled {
                        c.dialog_secondary_button_text
                    } else {
                        c.dialog_muted
                    })
                    .when(replace_enabled, |this| {
                        this.cursor_pointer()
                            .hover(|this| this.bg(c.dialog_secondary_button_hover))
                    })
                    .when(!replace_enabled, |this| this.opacity(0.5))
                    .child(strings.search_replace_all_label.clone())
                    .on_click(cx.listener(Self::on_search_replace_all_click)),
            );

        Some(
            div()
                .id("search-bar")
                .absolute()
                .top(px(12.0))
                .right(px(16.0))
                .track_focus(&search.focus_handle)
                .on_key_down(cx.listener(Self::on_search_query_key_down))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .p(px(8.0))
                .bg(c.dialog_surface)
                .border(px(d.dialog_border_width))
                .border_color(c.dialog_border)
                .rounded(px(d.dialog_radius))
                .shadow_lg()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            toggle_button(
                                "search-toggle-case",
                                "Aa",
                                search.options.case_sensitive,
                                strings.search_case_sensitive_tooltip.clone(),
                            )
                            .on_click(cx.listener(Self::on_search_case_toggle_click)),
                        )
                        .child(
                            toggle_button(
                                "search-toggle-whole-word",
                                "ab|",
                                search.options.whole_word,
                                strings.search_whole_word_tooltip.clone(),
                            )
                            .on_click(cx.listener(Self::on_search_whole_word_toggle_click)),
                        )
                        .child(
                            toggle_button(
                                "search-toggle-regex",
                                ".*",
                                search.options.regex,
                                strings.search_regex_tooltip.clone(),
                            )
                            .on_click(cx.listener(Self::on_search_regex_toggle_click)),
                        )
                        .child(query_field)
                        .child(
                            div()
                                .min_w(px(56.0))
                                .max_w(px(220.0))
                                .overflow_hidden()
                                .text_color(c.dialog_muted)
                                .child(count_label),
                        )
                        .child(
                            nav_button("search-prev", "‹", has_matches).on_click(cx.listener(
                                |editor, event, window, cx| {
                                    editor.on_search_prev_click(event, window, cx)
                                },
                            )),
                        )
                        .child(
                            nav_button("search-next", "›", has_matches).on_click(cx.listener(
                                |editor, event, window, cx| {
                                    editor.on_search_next_click(event, window, cx)
                                },
                            )),
                        )
                        .child(
                            // Toggles the replace row and doubles as its
                            // "active" indicator.
                            toggle_button(
                                "search-toggle-replace",
                                "⇄",
                                search.show_replace_row,
                                strings.search_replace_label.clone(),
                            )
                            .on_click(cx.listener(Self::on_search_replace_toggle_click)),
                        )
                        .child(nav_button("search-close", "×", true).on_click(cx.listener(
                            |editor, event, window, cx| {
                                editor.on_search_close_click(event, window, cx)
                            },
                        ))),
                )
                .when(search.show_replace_row, |this| this.child(replace_row))
                .into_any_element(),
        )
    }
}

impl Editor {
    /// Replaces the active match, then leaves the bar open and moves the
    /// active index to the next match in document order so a second press
    /// replaces the next occurrence. When the replaced match was the last
    /// one, wraps to the first; when no matches remain, clears it.
    pub(super) fn replace_active_search_match(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search_bar.as_ref() else {
            return;
        };
        let Some(matcher) = search.matcher.clone() else {
            return;
        };
        let Some(found) = search
            .active_index
            .and_then(|index| search.matches.get(index).cloned())
        else {
            return;
        };
        let old_index = search
            .active_index
            .expect("active match was checked above");
        let replacement = search.replacement.clone();
        let Some(entity) = self.focusable_entity_by_id(found.entity_id) else {
            return;
        };

        // Expand captures against the target text *before* editing it.
        let text = entity.read(cx).display_text().to_string();
        let expanded = matcher.expand(&replacement, &text, found.range.clone());

        self.prepare_undo_capture(UndoCaptureKind::NonCoalescible, cx);
        entity.update(cx, move |block, cx| {
            block.replace_text_in_visible_range(
                found.range.clone(),
                &expanded,
                Some(expanded.len()..expanded.len()),
                false,
                cx,
            );
        });
        self.finalize_pending_undo_capture(cx);

        if let Some(search) = self.search_bar.as_mut() {
            search.last_replaced = None;
        }
        self.recompute_search_matches(cx);

        // Advance deterministically from the old index instead of re-seeding
        // from the caret: the caret sits in the replaced block, and a
        // block-local nearest-match lookup would wrap to index 0 even when
        // later blocks still hold matches. Every match after the replaced
        // one shifted down by one, so the match that used to be at
        // `old_index + 1` now sits at `old_index`.
        if let Some(search) = self.search_bar.as_mut() {
            let remaining = search.matches.len();
            search.active_index = if remaining == 0 {
                None
            } else if old_index < remaining {
                Some(old_index)
            } else {
                Some(0)
            };
        }
        self.apply_search_highlights(cx);
    }

    /// Replaces every match of the current query in one undoable step.
    pub(super) fn replace_all_search_matches(&mut self, cx: &mut Context<Self>) {
        let Some(search) = self.search_bar.as_ref() else {
            return;
        };
        let Some(matcher) = search.matcher.clone() else {
            return;
        };
        if search.matches.is_empty() {
            return;
        }
        let replacement = search.replacement.clone();

        // Group by block in document order. Each block's edits are planned
        // against a single pre-edit snapshot of its text.
        let mut order: Vec<EntityId> = Vec::new();
        let mut grouped: HashMap<EntityId, Vec<Range<usize>>> = HashMap::new();
        for found in &search.matches {
            if !grouped.contains_key(&found.entity_id) {
                order.push(found.entity_id);
            }
            grouped
                .entry(found.entity_id)
                .or_default()
                .push(found.range.clone());
        }

        let mut plan: Vec<(Entity<Block>, Vec<(Range<usize>, String)>)> = Vec::new();
        for entity_id in order {
            let Some(entity) = self.focusable_entity_by_id(entity_id) else {
                continue;
            };
            let text = entity.read(cx).display_text().to_string();
            let edits = grouped
                .remove(&entity_id)
                .unwrap_or_default()
                .into_iter()
                .map(|range| {
                    let expanded = matcher.expand(&replacement, &text, range.clone());
                    (range, expanded)
                })
                .collect();
            plan.push((entity, edits));
        }
        if plan.is_empty() {
            return;
        }

        let mut replaced = 0usize;
        self.prepare_undo_capture(UndoCaptureKind::NonCoalescible, cx);
        // Suppress the per-block undo finalize that each Changed event would
        // otherwise run, so the whole batch collapses into one entry.
        self.search_replace_batch_in_progress = true;
        for (entity, edits) in plan {
            replaced += edits.len();
            entity.update(cx, move |block, cx| {
                // Back to front so earlier ranges keep their offsets.
                for (range, expanded) in edits.into_iter().rev() {
                    block.replace_text_in_visible_range(range, &expanded, None, false, cx);
                }
            });
        }
        self.search_replace_batch_in_progress = false;
        self.finalize_pending_undo_capture(cx);

        self.recompute_search_matches(cx);
        if let Some(search) = self.search_bar.as_mut() {
            search.active_index = None;
            search.last_replaced = Some(replaced);
            // Increment token and spawn timer to clear after 2 seconds
            search.last_replaced_token = search.last_replaced_token.wrapping_add(1);
            let token = search.last_replaced_token;
            cx.spawn(async move |this: WeakEntity<Editor>, cx: &mut AsyncApp| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let _ = this.update(cx, |editor, cx| {
                    if let Some(search) = editor.search_bar.as_mut()
                        && search.last_replaced_token == token
                        && search.last_replaced.is_some()
                    {
                        search.last_replaced = None;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        self.apply_search_highlights(cx);
        cx.notify();
    }
}

/// Which option a toggle flips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SearchOptionToggle {
    CaseSensitive,
    WholeWord,
    Regex,
    ReplaceRow,
}

/// Kinds whose `display_text()` is real, editable text.
fn searchable_kind(kind: BlockKind) -> bool {
    !matches!(
        kind,
        BlockKind::Separator | BlockKind::Table | BlockKind::MathBlock | BlockKind::MermaidBlock
    )
}

/// Index of the first match at or after `offset` in `entity_id`, wrapping to
/// the closest earlier match when nothing follows.
fn nearest_match_index(
    matches: &[SearchMatch],
    entity_id: EntityId,
    offset: usize,
) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    matches
        .iter()
        .position(|found| found.entity_id == entity_id && found.range.start >= offset)
        .or_else(|| {
            matches
                .iter()
                .position(|found| found.entity_id == entity_id && found.range.end >= offset)
        })
        .or(Some(0))
}

/// Byte offset of the grapheme boundary immediately before `cursor`.
fn grapheme_left(s: &str, cursor: usize) -> usize {
    let cursor = cursor.min(s.len());
    if cursor == 0 {
        return 0;
    }
    s.grapheme_indices(true)
        .rev()
        .find_map(|(i, _)| (i < cursor).then_some(i))
        .unwrap_or(0)
}

/// Byte offset of the grapheme boundary immediately after `cursor`.
fn grapheme_right(s: &str, cursor: usize) -> usize {
    let cursor = cursor.min(s.len());
    s.grapheme_indices(true)
        .find_map(|(i, _)| (i > cursor).then_some(i))
        .unwrap_or(s.len())
}

/// Start of the word immediately before `cursor`.
fn word_left(s: &str, cursor: usize) -> usize {
    let cursor = cursor.min(s.len());
    s.unicode_word_indices()
        .map(|(i, _)| i)
        .filter(|&i| i < cursor)
        .last()
        .unwrap_or(0)
}

/// End of the word immediately after `cursor`.
fn word_right(s: &str, cursor: usize) -> usize {
    let cursor = cursor.min(s.len());
    s.unicode_word_indices()
        .map(|(i, w)| i + w.len())
        .find(|&end| end > cursor)
        .unwrap_or(s.len())
}

/// Search options as stored on disk, projected into the matcher's options.
pub(super) fn load_search_preferences() -> SearchPreferences {
    read_app_preferences()
        .map(|preferences| preferences.search)
        .unwrap_or_default()
}

fn persist_search_options(options: SearchOptions) {
    // Tests must never rewrite the developer's real config.toml.
    if cfg!(test) {
        return;
    }
    let preferences: AppPreferences = match read_app_preferences() {
        Ok(preferences) => preferences,
        Err(error) => {
            eprintln!("failed to read search preferences: {error}");
            return;
        }
    };
    let mut preferences = preferences;
    preferences.search.case_sensitive = options.case_sensitive;
    preferences.search.whole_word = options.whole_word;
    preferences.search.regex = options.regex;
    if let Err(error) = save_app_preferences(&preferences) {
        eprintln!("failed to save search preferences: {error}");
    }
}

/// Minimal hover tooltip used by the find-bar option toggles.
///
/// gpui only accepts an `AnyView` tooltip, so the label and its resolved
/// colours are baked in when the bar is built instead of looking up the theme
/// at hover time.
struct SearchTooltip {
    label: SharedString,
    bg: Hsla,
    fg: Hsla,
    border: Hsla,
}

impl Render for SearchTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(6.0))
            .py(px(3.0))
            .rounded(px(4.0))
            .bg(self.bg)
            .border(px(1.0))
            .border_color(self.border)
            .text_color(self.fg)
            .child(self.label.clone())
    }
}

/// Test-visible helper: the document-order positions of every match of a
/// query, used by the search tests below.
#[cfg(test)]
fn collect_positions(matches: &[SearchMatch]) -> Vec<(EntityId, Range<usize>)> {
    matches
        .iter()
        .map(|found| (found.entity_id, found.range.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        BlockKind, SearchMatch, SearchOptions, collect_positions, grapheme_left, grapheme_right,
        nearest_match_index, searchable_kind, word_left, word_right,
    };
    use crate::config::SearchPreferences;
    use gpui::EntityId;
    use unicode_segmentation::UnicodeSegmentation;

    fn options(case_sensitive: bool, whole_word: bool, regex: bool) -> SearchOptions {
        SearchOptions {
            case_sensitive,
            whole_word,
            regex,
        }
    }

    #[test]
    fn nearest_match_index_prefers_the_match_at_or_after_the_caret() {
        let entity = EntityId::from(7u64);
        let matches = vec![
            SearchMatch {
                entity_id: entity,
                range: 0..3,
            },
            SearchMatch {
                entity_id: entity,
                range: 10..13,
            },
            SearchMatch {
                entity_id: entity,
                range: 20..23,
            },
        ];
        assert_eq!(nearest_match_index(&matches, entity, 0), Some(0));
        assert_eq!(nearest_match_index(&matches, entity, 5), Some(1));
        assert_eq!(nearest_match_index(&matches, entity, 21), Some(2));
        assert_eq!(nearest_match_index(&matches, entity, 99), Some(0));
        assert_eq!(nearest_match_index(&[], entity, 0), None);
    }

    #[test]
    fn searchable_kind_excludes_non_text_blocks() {
        assert!(searchable_kind(BlockKind::Paragraph));
        assert!(searchable_kind(BlockKind::CodeBlock { language: None }));
        assert!(!searchable_kind(BlockKind::Separator));
        assert!(!searchable_kind(BlockKind::Table));
        assert!(!searchable_kind(BlockKind::MathBlock));
        assert!(!searchable_kind(BlockKind::MermaidBlock));
    }

    #[test]
    fn matcher_options_project_onto_search_preferences() {
        let preferences = SearchPreferences {
            case_sensitive: true,
            whole_word: true,
            regex: true,
            show_replace_row: true,
        };
        let projected = preferences.options();
        assert_eq!(projected, options(true, true, true));
        assert!(preferences.show_replace_row);
    }

    #[test]
    fn collect_positions_preserves_document_order() {
        let first = EntityId::from(1u64);
        let second = EntityId::from(2u64);
        let matches = vec![
            SearchMatch {
                entity_id: first,
                range: 0..1,
            },
            SearchMatch {
                entity_id: second,
                range: 2..3,
            },
        ];
        assert_eq!(
            collect_positions(&matches),
            vec![(first, 0..1), (second, 2..3)]
        );
    }

    #[test]
    fn grapheme_and_word_navigation() {
        let s = "héllo wörld";
        // Derive the expected boundaries from the string itself so the
        // assertions stay correct for multi-byte UTF-8 without hardcoding
        // byte indices. `grapheme_indices` yields grapheme *start* offsets;
        // the end of the string is the final boundary a caret can rest on.
        let mut boundaries: Vec<usize> = s.grapheme_indices(true).map(|(i, _)| i).collect();
        boundaries.push(s.len());
        assert_eq!(boundaries.first(), Some(&0));
        assert_eq!(boundaries.last(), Some(&s.len()));
        // "h" is one byte and "é" is two, so the first two steps land on 1
        // and 3.
        assert_eq!((boundaries[1], boundaries[2]), (1, 3));

        // grapheme_right steps to the next boundary.
        assert_eq!(grapheme_right(s, 0), boundaries[1]);
        assert_eq!(grapheme_right(s, boundaries[1]), boundaries[2]);
        // grapheme_left steps back across the two-byte "é".
        assert_eq!(grapheme_left(s, boundaries[2]), boundaries[1]);
        assert_eq!(grapheme_left(s, s.len()), boundaries[boundaries.len() - 2]);
        // Edge and out-of-range cursors clamp instead of panicking.
        assert_eq!(grapheme_left(s, 0), 0);
        assert_eq!(grapheme_right(s, s.len()), s.len());

        // Word jumps land on word starts and ends.
        assert_eq!(word_left(s, s.len()), s.find("wörld").unwrap());
        assert_eq!(word_right(s, 0), "héllo".len());
        assert_eq!(word_left(s, 0), 0);
        assert_eq!(word_right(s, s.len()), s.len());
        assert_eq!(word_left(s, boundaries[1]), 0);
        assert_eq!(word_right(s, s.find("wörld").unwrap()), s.len());
    }
}
