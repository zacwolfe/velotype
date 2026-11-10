//! Viewport anchoring across rendered/source view-mode switches.
//!
//! The two view modes lay the document out completely differently: rendered
//! mode has one block per semantic element, source mode is a single raw block.
//! A raw pixel scroll offset therefore means nothing after a switch. Instead we
//! anchor on the SOURCE byte offset of the text at the top of the viewport, a
//! coordinate both modes share, and restore the viewport to that offset once the
//! new layout has been measured.

use std::ops::Range;

use gpui::*;

use super::{Editor, SourceTargetMapping, ViewMode};
use crate::components::Block;

/// A viewport position expressed in the coordinates both view modes share: the
/// canonical Markdown line at the top of the viewport, plus how many pixels of
/// that line's top were scrolled above the viewport.
///
/// The clip matters because rendered mode parks an outline-jumped heading
/// `ScrollAlign::Top`'s padding below the viewport top. The line that actually
/// crosses the viewport top is then the tail of the previous paragraph, and
/// anchoring only its start would shift the whole document down by the clipped
/// amount on every switch. Carrying the clip keeps that line where it was and
/// leaves the heading at the same height.
#[derive(Clone, Copy, Debug)]
pub(super) struct ViewportAnchor {
    pub(super) source_offset: usize,
    pub(super) clip: Pixels,
}

impl ViewportAnchor {
    fn new(source_offset: usize, clip: Pixels) -> Self {
        Self {
            source_offset,
            clip: clip.max(px(0.0)),
        }
    }
}

/// Byte ranges of each hard-line (`\n`-separated) segment of `text`.
///
/// Mirrors `components::block::element::hard_line_ranges`. The editor cannot
/// reach that module (it is private to the block component), and a `WrappedLine`
/// is produced per hard line, so index `i` here corresponds to line `i`.
fn hard_line_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (idx, _) in text.match_indices('\n') {
        ranges.push(start..idx);
        start = idx + 1;
    }
    ranges.push(start..text.len());
    ranges
}

/// Index of the hard line containing `offset`.
fn line_index_for_offset(ranges: &[Range<usize>], offset: usize) -> usize {
    let clamped = offset.min(ranges.last().map(|range| range.end).unwrap_or(0));
    for (index, range) in ranges.iter().enumerate() {
        if clamped <= range.end {
            return index;
        }
    }
    ranges.len().saturating_sub(1)
}

/// Height of the lines above `line_idx`.
fn wrapped_line_top(lines: &[WrappedLine], line_height: Pixels, line_idx: usize) -> Pixels {
    lines.iter().take(line_idx).fold(px(0.0), |height, line| {
        height + line.size(line_height).height
    })
}

/// Index of the wrapped line covering `relative_y` within a block's layout.
fn wrapped_line_index_for_y(
    lines: &[WrappedLine],
    line_height: Pixels,
    relative_y: Pixels,
) -> Option<usize> {
    if lines.is_empty() {
        return None;
    }

    let mut top = px(0.0);
    for (index, line) in lines.iter().enumerate() {
        let height = line.size(line_height).height;
        if relative_y < top + height || index + 1 == lines.len() {
            return Some(index);
        }
        top += height;
    }

    Some(lines.len() - 1)
}

impl Editor {
    /// Whether `entity_id`'s row (or block, when not windowed) painted in the
    /// previous frame, and so whether its cached `last_bounds` reflect the
    /// current scroll offset.
    fn painted_in_previous_frame(&self, entity_id: EntityId) -> bool {
        let Some(visible_index) = self.document.visible_index_for_entity_id(entity_id) else {
            // Not a windowed row (so never culled); its bounds are as fresh as
            // its last paint.
            return true;
        };
        self.prev_painted_visible
            .iter()
            .any(|&(start, end)| visible_index >= start && visible_index < end)
    }

    /// Hard line (index, character offset) at `relative_y` below the block's
    /// laid-out top.
    fn block_line_at_relative_y(block: &Block, relative_y: Pixels) -> Option<(usize, usize)> {
        let lines = block.last_layout.as_ref()?;
        let line_index = wrapped_line_index_for_y(lines, block.last_line_height, relative_y)?;
        let ranges = hard_line_ranges(block.display_text());
        ranges
            .get(line_index)
            .map(|range| (line_index, range.start))
    }

    /// Maps a visible-text character offset in `mapping`'s block to a canonical
    /// source byte offset.
    fn source_offset_for_mapping_char(
        mapping: &SourceTargetMapping,
        char_offset: usize,
        cx: &App,
    ) -> usize {
        let block = mapping.entity.read(cx);
        let visible_len = block.visible_len();
        if char_offset == 0 {
            return mapping.full_source_range.start;
        }
        if char_offset >= visible_len {
            return mapping.full_source_range.end;
        }
        let markdown_offset = block
            .current_range_to_markdown_range(char_offset..char_offset)
            .start;
        let max_content = mapping.content_to_source.len().saturating_sub(1);
        mapping.full_source_range.start
            + mapping.content_to_source[markdown_offset.min(max_content)]
    }

    /// Canonical Markdown byte offset of the text currently at the top of the
    /// viewport, clamped to the document's source length.
    pub(super) fn top_of_viewport_source_offset(&self, cx: &App) -> usize {
        self.top_of_viewport_anchor(cx).source_offset
    }

    /// The line crossing the top of the viewport plus how much of it is above
    /// the viewport. This is the coordinate that survives a view-mode relayout.
    pub(super) fn top_of_viewport_anchor(&self, cx: &App) -> ViewportAnchor {
        let viewport_top = self.scroll_handle.bounds().top();

        match self.view_mode {
            ViewMode::Source => {
                let Some(block) = self.document.first_root() else {
                    return ViewportAnchor::new(0, px(0.0));
                };
                let block = block.read(cx);
                let Some(bounds) = block.last_bounds else {
                    return ViewportAnchor::new(0, px(0.0));
                };
                if viewport_top <= bounds.top() {
                    return ViewportAnchor::new(0, px(0.0));
                }
                let source_len = block.display_text().len();
                let relative_y = viewport_top - bounds.top();
                let Some((line_index, char_offset)) =
                    Self::block_line_at_relative_y(block, relative_y)
                else {
                    return ViewportAnchor::new(0, px(0.0));
                };
                let line_top = block
                    .last_layout
                    .as_ref()
                    .map(|lines| wrapped_line_top(lines, block.last_line_height, line_index))
                    .unwrap_or(px(0.0));
                let clip = viewport_top - (bounds.top() + line_top);
                ViewportAnchor::new(char_offset.min(source_len), clip)
            }
            ViewMode::Rendered => {
                let source_len = self.current_document_source(cx).len();
                // The block painted at the viewport top, or the first one whose
                // top sits below it (the viewport top is then in the gap above
                // that block, so its first line is the nearest content).
                let mut anchor: Option<(EntityId, usize, Pixels)> = None;
                for visible in self.document.visible_blocks() {
                    // A windowed row that was culled keeps the `last_bounds`
                    // it had when it last painted, which can be from a
                    // completely different scroll offset. Trusting those made
                    // the first block of the document "win" the search after
                    // an outline jump, because its stale bounds still straddled
                    // the viewport top. Only rows painted in the previous frame
                    // are on screen now, so only they can anchor the viewport.
                    if !self.painted_in_previous_frame(visible.entity.entity_id()) {
                        continue;
                    }
                    let block = visible.entity.read(cx);
                    let Some(bounds) = block.last_bounds else {
                        continue;
                    };
                    if viewport_top < bounds.top() {
                        // The viewport top is in the gap above this block:
                        // nothing of it is clipped.
                        anchor = Some((visible.entity.entity_id(), 0, px(0.0)));
                        break;
                    }
                    if viewport_top <= bounds.bottom() {
                        let relative_y = viewport_top - bounds.top();
                        let Some((line_index, char_offset)) =
                            Self::block_line_at_relative_y(block, relative_y)
                        else {
                            anchor = Some((visible.entity.entity_id(), 0, px(0.0)));
                            break;
                        };
                        let line_top = block
                            .last_layout
                            .as_ref()
                            .map(|lines| {
                                wrapped_line_top(lines, block.last_line_height, line_index)
                            })
                            .unwrap_or(px(0.0));
                        let clip = viewport_top - (bounds.top() + line_top);
                        anchor = Some((visible.entity.entity_id(), char_offset, clip));
                        break;
                    }
                }

                let Some((entity_id, char_offset, clip)) = anchor else {
                    return ViewportAnchor::new(0, px(0.0));
                };
                let (mappings, block_ranges) =
                    self.build_source_target_mappings_with_block_ranges(cx);
                let source_offset = if let Some(mapping) = mappings
                    .iter()
                    .find(|mapping| mapping.entity.entity_id() == entity_id)
                {
                    Self::source_offset_for_mapping_char(mapping, char_offset, cx).min(source_len)
                } else {
                    // Atomic blocks (tables, images) carry no text mapping; their
                    // whole source span starts here.
                    block_ranges
                        .get(&entity_id)
                        .map(|range| range.start)
                        .unwrap_or(0)
                        .min(source_len)
                };
                ViewportAnchor::new(source_offset, clip)
            }
        }
    }

    /// Queues a viewport restore that puts `anchor`'s line (with the same
    /// amount clipped above the top) back at the top of the viewport once the
    /// new layout has been measured.
    pub(super) fn restore_viewport_anchor(
        &mut self,
        anchor: ViewportAnchor,
        cx: &mut Context<Self>,
    ) {
        self.pending_viewport_restore = Some(anchor);
        self.schedule_scroll_recheck(cx);
        cx.notify();
    }

    /// Applies a queued viewport restore once the new mode's blocks have been
    /// measured. Called from `render` after the caret-scroll pass so this pass
    /// has the final say on the offset.
    pub(super) fn apply_pending_viewport_restore(&mut self, cx: &mut Context<Self>) {
        let Some(anchor) = self.pending_viewport_restore else {
            return;
        };

        // A queued caret scroll runs first and may move the viewport. Let it
        // settle before anchoring, then take over.
        if self.pending_scroll_active_block_into_view || self.scrollbar_drag.is_some() {
            self.schedule_scroll_recheck(cx);
            return;
        }

        let Some(desired_y) = self.scroll_offset_for_anchor(anchor, cx) else {
            // The target block has not painted its layout yet.
            self.schedule_scroll_recheck(cx);
            return;
        };

        // Rendered-mode blocks above the target may still carry estimated
        // heights on the first pass; measuring them shifts the target after we
        // scroll. Keep the anchor queued until the computed offset stops
        // moving, so the heading lands exactly where it was rather than a few
        // hundred pixels off.
        let settled = (desired_y - self.scroll_handle.offset().y).abs() <= px(1.0)
            || self.pending_viewport_restore_passes >= 4;
        self.pending_viewport_restore_passes =
            self.pending_viewport_restore_passes.saturating_add(1);
        if settled {
            self.pending_viewport_restore = None;
        } else {
            self.schedule_scroll_recheck(cx);
        }
        self.set_vertical_scroll_offset(desired_y, cx);
    }

    /// Scroll offset (negative, as `ScrollHandle` expects) that puts `anchor`'s
    /// line back where it was: `anchor.clip` pixels of its top above the
    /// viewport top.
    fn scroll_offset_for_anchor(&self, anchor: ViewportAnchor, cx: &App) -> Option<Pixels> {
        let source_offset = anchor.source_offset;
        // At the document head there is nothing to anchor past the top padding.
        // Restoring to zero keeps a switch at the very top perfectly still.
        if source_offset == 0 {
            return Some(px(0.0));
        }

        match self.view_mode {
            ViewMode::Source => {
                let block = self.document.first_root()?.read(cx);
                let bounds = block.last_bounds?;
                let lines = block.last_layout.as_ref()?;
                let text = block.display_text();
                let ranges = hard_line_ranges(text);
                let line_index = line_index_for_offset(&ranges, source_offset.min(text.len()));
                let line_top = wrapped_line_top(lines, block.last_line_height, line_index);
                let viewport_top = self.scroll_handle.bounds().top();
                Some(
                    self.scroll_handle.offset().y
                        + (viewport_top - (bounds.top() + line_top))
                        + anchor.clip,
                )
            }
            ViewMode::Rendered => {
                let mappings = self.build_source_target_mappings(cx);
                let endpoint = self.endpoint_for_source_offset(source_offset, &mappings, cx)?;
                let visible = self.document.visible_blocks();
                let index = self
                    .document
                    .visible_index_for_entity_id(endpoint.entity_id)?;
                let viewport_top = self.scroll_handle.bounds().top();
                let current_y = self.scroll_handle.offset().y;

                // The mapped block may not be painted (virtualization), so fall
                // back to the nearest measured neighbour. Being one blank line
                // off is fine; losing the anchor entirely is not.
                let mut candidates = Vec::with_capacity(visible.len());
                candidates.push(index);
                for delta in 1..visible.len() {
                    if index + delta < visible.len() {
                        candidates.push(index + delta);
                    }
                    if let Some(previous) = index.checked_sub(delta) {
                        candidates.push(previous);
                    }
                }

                for candidate in candidates {
                    // A culled row's `last_bounds` are from an older scroll
                    // offset; scrolling relative to them would land somewhere
                    // unrelated.
                    if !self.painted_in_previous_frame(visible[candidate].entity.entity_id()) {
                        continue;
                    }
                    let block = visible[candidate].entity.read(cx);
                    let Some(bounds) = block.last_bounds else {
                        continue;
                    };
                    let char_offset = if candidate == index {
                        endpoint.offset
                    } else if candidate > index {
                        0
                    } else {
                        block.visible_len()
                    };
                    let line_top = block
                        .last_layout
                        .as_ref()
                        .and_then(|lines| {
                            let text = block.display_text();
                            let ranges = hard_line_ranges(text);
                            let line_index =
                                line_index_for_offset(&ranges, char_offset.min(text.len()));
                            Some(wrapped_line_top(lines, block.last_line_height, line_index))
                        })
                        .unwrap_or(px(0.0));
                    // The clip only describes the originally anchored line, so
                    // it applies to the exact block only. A neighbouring
                    // fallback block just aligns its own start.
                    let clip = if candidate == index {
                        anchor.clip
                    } else {
                        px(0.0)
                    };
                    return Some(current_y + (viewport_top - (bounds.top() + line_top)) + clip);
                }
                None
            }
        }
    }
}
