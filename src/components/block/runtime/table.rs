//! Native table cell and axis runtime state.

use super::*;
use crate::components::{parse_table_region, serialize_table_markdown_lines};

impl Block {
    pub(crate) fn is_table_cell(&self) -> bool {
        self.table_cell_position.is_some()
    }

    pub(crate) fn table_cell_position(&self) -> Option<TableCellPosition> {
        self.table_cell_position
    }

    pub(crate) fn table_cell_alignment(&self) -> Option<TableColumnAlignment> {
        self.table_cell_alignment
    }

    pub(crate) fn text_align(&self) -> TextAlign {
        match self
            .table_cell_alignment()
            .unwrap_or(TableColumnAlignment::Default)
        {
            TableColumnAlignment::Default | TableColumnAlignment::Left => TextAlign::Left,
            TableColumnAlignment::Center => TextAlign::Center,
            TableColumnAlignment::Right => TextAlign::Right,
        }
    }

    pub(crate) fn set_table_cell_mode(
        &mut self,
        position: TableCellPosition,
        alignment: TableColumnAlignment,
    ) {
        self.table_cell_position = Some(position);
        self.table_cell_alignment = Some(alignment);
        self.edit_mode = EditMode::RenderedRich;
        self.clear_inline_projection();
        self.sync_render_cache();
    }

    pub(crate) fn set_table_runtime(&mut self, runtime: TableRuntime) {
        self.table_runtime = Some(runtime);
    }

    pub(crate) fn clear_table_runtime(&mut self) {
        self.table_runtime = None;
        self.table_axis_preview = None;
        self.table_axis_selection = None;
        self.table_axis_highlight = TableAxisHighlight::None;
        self.table_append_column_edge_hovered = false;
        self.table_append_column_hovered = false;
        self.table_append_column_zone_hovered = false;
        self.table_append_column_button_hovered = false;
        self.table_append_column_close_task = None;
        self.table_append_row_edge_hovered = false;
        self.table_append_row_hovered = false;
        self.table_append_row_zone_hovered = false;
        self.table_append_row_button_hovered = false;
        self.table_append_row_close_task = None;
    }

    pub(crate) fn set_table_axis_visual_state(
        &mut self,
        preview: Option<TableAxisMarker>,
        selection: Option<TableAxisMarker>,
    ) {
        self.table_axis_preview = preview;
        self.table_axis_selection = selection;
    }

    pub(crate) fn set_table_axis_highlight(&mut self, highlight: TableAxisHighlight) {
        self.table_axis_highlight = highlight;
    }

    /// Whether this table block is currently showing its raw Markdown for
    /// text editing rather than the native grid.
    pub(crate) fn is_table_markdown_editing(&self) -> bool {
        self.record.table_markdown_editing
    }

    /// Keeps raw-Markdown table editing in sync with focus, mirroring
    /// `sync_image_focus_state`. `enabled` is the live `edit_tables_as_markdown`
    /// preference; the caller (`render`) reads it via `cx` since `BlockRecord`
    /// has no access to it. Focusing a table block while enabled loads its
    /// Markdown into the title for editing; losing focus (or the preference
    /// turning off) reparses the title back into `record.table`. Markdown
    /// that fails to parse is left in place instead of dropped: the flag
    /// stays set so the raw text keeps rendering, and reparsing is retried
    /// on the next call rather than discarding the user's content.
    pub(crate) fn sync_table_markdown_focus_state(&mut self, focused: bool, enabled: bool) -> bool {
        if self.kind() != BlockKind::Table {
            return false;
        }
        let should_edit_as_markdown = focused && enabled;
        if should_edit_as_markdown && !self.record.table_markdown_editing {
            self.enter_table_markdown_edit();
            return true;
        }
        if !should_edit_as_markdown && self.record.table_markdown_editing {
            return self.exit_table_markdown_edit();
        }
        false
    }

    fn enter_table_markdown_edit(&mut self) {
        let markdown = self
            .record
            .table
            .as_ref()
            .map(|table| serialize_table_markdown_lines(table).join("\n"))
            .unwrap_or_default();
        self.record.table_markdown_editing = true;
        self.record.set_title(InlineTextTree::plain(markdown));
        self.set_source_raw_mode();
        self.sync_render_cache();
        self.assign_collapsed_selection_offset(0, CollapsedCaretAffinity::Default, None);
        self.marked_range = None;
    }

    /// Reparses the raw Markdown back into `record.table`. Returns whether
    /// the block actually left raw-edit mode: invalid Markdown returns
    /// `false` and leaves everything untouched, so the caller can retry on
    /// the next blur instead of losing what the user typed.
    fn exit_table_markdown_edit(&mut self) -> bool {
        let markdown = self.record.title.visible_text().to_string();
        let lines = markdown.split('\n').map(str::to_string).collect::<Vec<_>>();
        let Some(table) = parse_table_region(&lines) else {
            return false;
        };
        self.record.table = Some(table);
        self.record.table_markdown_editing = false;
        self.record.set_title(InlineTextTree::plain(String::new()));
        self.edit_mode = EditMode::RenderedRich;
        self.sync_render_cache();
        self.assign_collapsed_selection_offset(0, CollapsedCaretAffinity::Default, None);
        self.marked_range = None;
        true
    }
}
