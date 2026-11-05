//! Top-level editor controller and window state.
//!
//! [`Editor`] owns window-level concerns such as view mode, save/close flow,
//! scroll state, and focus deferral. The runtime block tree itself lives in
//! [`DocumentTree`], which centralizes structural mutations and cached visible
//! order metadata.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::*;

use self::context_menu::{ContextMenuState, TableInsertDialogState};
use self::tree::DocumentTree;
use crate::components::{
    Block, BlockKind, BlockRecord, FootnoteDefinitionBinding, FootnoteReferenceLocation,
    FootnoteRegistry, FootnoteResolvedOccurrence, ImageReferenceDefinitions, ImageRuntime,
    InlineTextTree,
    LinkReferenceDefinitions, parse_image_reference_definitions, parse_link_reference_definitions,
};
use crate::components::{
    TableAxisHighlight, TableAxisKind, TableAxisMarker, TableCellPosition, TableColumnAlignment,
    TableData, TableRuntime, UndoCaptureKind, serialize_table_cell_markdown,
};
mod close;
mod context_menu;
mod document;
mod events;
mod export;
mod file_drop;
mod file_watch;
mod history;
mod persistence;
pub(crate) mod picgo;
mod render;
mod runtime_context;
mod search;
mod selection;
mod source_mapping;
mod status_bar;
mod table_edit;
#[cfg(test)]
mod tests;
mod tree;
mod update;
mod window_state;
mod workspace;

use self::file_watch::FileSignature;
use self::search::SearchBarState;
use self::status_bar::StatusBarState;
use self::workspace::{WorkspaceResizeDrag, WorkspaceState, WorkspaceWatchState};

/// Link navigation request deferred until a `Window` is available.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingOpenLink {
    /// Human-readable target shown in the confirmation prompt.
    pub(crate) prompt_target: String,
    /// Resolved URL passed to the platform opener after confirmation.
    pub(crate) open_target: String,
}

/// Native image-paste prompt deferred until this editor's window renders.
pub(super) enum ImagePastePrompt {
    Storage(String),
    PicGoFallback(String),
}

/// Top-level controller that owns editor-wide state and delegates tree
/// mutations to [`DocumentTree`].
///
/// The editor subscribes to every [`BlockEvent`](crate::components::BlockEvent)
/// emitted by child blocks. Structural changes are handled centrally so focus,
/// scrolling, dirty tracking, and serialization stay synchronized.
pub struct Editor {
    /// Structural owner of roots and cached visible-order metadata.
    document: DocumentTree,
    /// Cell editors are indexed separately because they are not document-tree nodes.
    table_cells: HashMap<EntityId, TableCellBinding>,
    /// Which view the editor is currently presenting.
    pub(crate) view_mode: ViewMode,
    /// Deferred focus target applied during render when a [`Window`] is
    /// available.
    pending_focus: Option<EntityId>,
    /// Last editing target, which can remain set while a dialog owns window focus.
    active_entity_id: Option<EntityId>,
    /// Queued caret scroll applied once layout bounds are available.
    pending_scroll_active_block_into_view: bool,
    /// How the pending scroll should align its target. Reset to `Nearest` by
    /// `focus_block`, so a jump that wants another alignment sets it afterward.
    pending_scroll_align: ScrollAlign,
    /// Block to scroll to even when keyboard focus belongs to the sidebar.
    /// Outline jumps leave focus in the tree but still need the document to
    /// reveal the selected heading.
    pending_scroll_target: Option<EntityId>,
    /// Requests another caret scroll after a layout pass.
    pending_scroll_recheck_after_layout: bool,
    /// Save request deferred until a render pass has a window.
    /// Focused target plus its caret/selection range as of the last frame, so
    /// the viewport can follow the caret however it moved. Caret motion inside a
    /// single block emits no editor-level event — and in source mode the whole
    /// document *is* one block — so polling this each frame is what keeps arrow
    /// keys, Home/End, word selection, and drag selection scrolling.
    last_caret_position: Option<(EntityId, Range<usize>)>,
    /// Set by operations that restore a caret but must leave the viewport where
    /// the user left it, so the next observed caret becomes the baseline instead
    /// of a scroll target. See `Editor::follow_caret_movement`.
    suppress_caret_scroll_follow: bool,
    pending_save: bool,
    /// Save As request deferred until a render pass has a window.
    pending_save_as: bool,
    /// Link confirmation deferred until a render pass has a window.
    pending_open_link: Option<PendingOpenLink>,
    /// Failed paste batches awaiting a prompt in this editor's own window.
    pending_image_paste_prompts: VecDeque<ImagePastePrompt>,
    /// Prevents overlapping native prompts while earlier failures await dismissal.
    image_paste_prompt_active: bool,
    /// Number of in-flight PicGo batches anchored to each block.
    pending_picgo_uploads: HashMap<EntityId, usize>,
    /// Asset folder chosen for this unsaved document's image pastes.
    unsaved_image_assets_dir: Option<PathBuf>,
    /// Desired native edited indicator, applied when a window is available.
    pending_window_edited: bool,
    /// Set when a reload should clear the platform's edited dot too.
    pending_window_edited_clear: bool,
    /// Whether the native title needs rebuilding from path and dirty state.
    pending_window_title_refresh: bool,
    /// Whether the current document has unsaved edits.
    document_dirty: bool,
    /// Current document path, absent for a new untitled document.
    file_path: Option<PathBuf>,
    /// Last content read from (or written to) `file_path`, normalized. Used to
    /// tell a real external edit apart from a timestamp change caused by our
    /// own save.
    document_disk_text: Option<String>,
    /// Signature observed on the previous external-change poll.
    watched_document_signature: Option<FileSignature>,
    /// New disk contents waiting for the user's Reload / Keep choice because
    /// the buffer has unsaved edits.
    pending_external_reload: Option<String>,
    external_reload_prompt_active: bool,
    external_watch_task: Option<Task<()>>,
    /// Scroll state for the main block column.
    scroll_handle: ScrollHandle,
    /// Viewport size used to detect meaningful resize.
    last_scroll_viewport_size: Option<Size<Pixels>>,
    /// Last frame's visible block IDs; changes prevent reading stale row bounds
    /// by index while retaining block-keyed height estimates.
    prev_visible_block_ids: Vec<EntityId>,
    /// Per-row footprint (height plus trailing gap), keyed by the row's first
    /// block. Scroll-invariant, unlike raw painted positions, so windowing from
    /// their running sum stays correct as the document scrolls. Filled as rows
    /// paint; unknown rows use a minimum-height estimate.
    row_stride_cache: HashMap<EntityId, f32>,
    /// Content column the cached footprints were measured at. Rows rewrap when it
    /// changes, so entries from another width are discarded rather than reused.
    row_stride_width: Option<f32>,
    /// Where last frame's run sat among the scroll container's children.
    prev_mounted_run: Option<MountedRun>,
    /// Half-open visible-block index ranges the previous frame actually
    /// mounted (the run, plus the focus island when it sat outside it).
    /// Recorded in visible-block space rather than row space because a row can
    /// group several blocks. Identifies whose cached bounds are trustworthy.
    prev_painted_visible: Vec<(usize, usize)>,
    /// Whether this window already has the unsaved-changes close callback.
    close_guard_installed: bool,
    /// Whether the guarded-close confirmation is visible.
    show_unsaved_changes_dialog: bool,
    /// Suppresses duplicate save-first prompts while a prepared update waits.
    update_save_prompt_active: bool,
    /// When true, the window will close after the next successful save.
    pending_close_after_save: bool,
    /// Focus target to restore when the close dialog is dismissed.
    close_dialog_restore_focus: Option<EntityId>,
    /// Dropped Markdown file awaiting save/discard confirmation.
    pending_drop_replace_path: Option<PathBuf>,
    /// Whether the dropped-file replacement dialog is visible.
    show_drop_replace_dialog: bool,
    /// Resumes the queued dropped-file replacement after a successful save.
    pending_drop_replace_after_save: bool,
    /// Focus target to restore when dropped-file replacement is canceled.
    drop_replace_restore_focus: Option<EntityId>,
    /// Optional informational dialog shown from the Help menu.
    info_dialog: Option<InfoDialogKind>,
    /// Image the user clicked to enlarge, drawn over the whole application frame.
    expanded_image: Option<ImageRuntime>,
    /// True while an online update check is running in the background.
    update_check_in_progress: bool,
    workspace: WorkspaceState,
    workspace_watch: WorkspaceWatchState,
    /// Focus target for keyboard navigation of the Files/Outline sidebar.
    workspace_focus_handle: FocusHandle,
    /// Scroll state for the Files section viewport, so arrow-key navigation
    /// can keep the highlighted row visible.
    workspace_files_scroll_handle: ScrollHandle,
    /// Scroll state for the Outline section viewport.
    workspace_outline_scroll_handle: ScrollHandle,
    workspace_resize_drag: Option<WorkspaceResizeDrag>,
    /// Sidebar root requested explicitly (e.g. launching with a directory
    /// argument), used when no open document implies one. Takes precedence so
    /// the tree can stay rooted at a directory the user named even while the
    /// buffer itself is untitled.
    workspace_root_override: Option<PathBuf>,
    /// Hover state for status-bar controls.
    status_bar: StatusBarState,
    /// Currently open insert or table-axis context menu.
    context_menu: Option<ContextMenuState>,
    /// Table insertion dialog state, if open.
    table_insert_dialog: Option<TableInsertDialogState>,
    search_bar: Option<SearchBarState>,
    /// Delayed insert-submenu close task, canceled on renewed hover.
    context_menu_submenu_close_task: Option<Task<()>>,
    /// Hover preview is distinct from the committed row/column selection.
    table_axis_preview: Option<TableAxisSelection>,
    /// Committed table row or column selection.
    table_axis_selection: Option<TableAxisSelection>,
    /// Selection spanning visible rendered blocks.
    cross_block_selection: Option<CrossBlockSelection>,
    /// Anchor held during a rendered cross-block pointer drag.
    cross_block_drag: Option<CrossBlockDrag>,
    /// Recent Select All presses for block-to-document selection upgrade.
    rendered_select_all_cycle: Option<RenderedSelectAllCycle>,
    /// Open top-level menu in the in-window fallback menu bar.
    menu_bar_open: Option<usize>,
    /// Open child submenu inside the in-window fallback menu panel.
    menu_submenu_open: Option<usize>,
    /// Pointer presence over the top-level menu bar.
    menu_bar_hovered: bool,
    /// Pointer presence over the active menu panel.
    menu_panel_hovered: bool,
    /// Pointer presence over the active submenu panel.
    menu_submenu_panel_hovered: bool,
    /// Hover state for the invisible bridge spanning the gap between the menu
    /// panel and an open submenu. Tracked separately from
    /// `menu_submenu_panel_hovered` so the handoff between the two regions
    /// cannot clobber a single shared flag and tear the menu down.
    menu_submenu_bridge_hovered: bool,
    /// Delayed close task for the in-window menu bar.
    menu_close_task: Option<Task<()>>,
    /// Whether the pointer is over the editor's scrollbar region.
    scrollbar_hovered: bool,
    /// Deadline until which the custom scrollbar remains visible.
    scrollbar_visible_until: Instant,
    /// Task that repaints after the scrollbar visibility deadline.
    scrollbar_fade_task: Option<Task<()>>,
    /// Forces a repaint shortly after a pending scroll-into-view that could
    /// not be satisfied yet (the target block has no measured bounds), so the
    /// scroll lands on the next frame instead of waiting for the cursor blink.
    scroll_recheck_task: Option<Task<()>>,
    /// Pointer offset and track dimensions for an active thumb drag.
    scrollbar_drag: Option<ScrollbarDragSession>,
    /// Past document states available for undo.
    undo_history: Vec<HistoryEntry>,
    /// Forward document states available for redo.
    redo_history: Vec<HistoryEntry>,
    /// Snapshot awaiting confirmation that an edit changed source text.
    pending_undo_capture: Option<PendingUndoCapture>,
    /// Last stable selection expressed in canonical source byte offsets.
    last_selection_snapshot: UndoSelectionSnapshot,
    /// Inputs the cached `last_selection_snapshot` was computed from, so the
    /// per-frame refresh in `render` can skip rebuilding it while idle.
    last_selection_snapshot_key: Option<SelectionSnapshotKey>,
    /// Source text paired with the stable selection for the next undo capture.
    last_stable_source_text: String,
    /// Suppresses recursive history capture while restoring an entry.
    history_restore_in_progress: bool,
    /// Shared parsed image reference definitions for visible blocks.
    image_reference_definitions: Arc<ImageReferenceDefinitions>,
    /// Shared parsed link reference definitions for visible blocks.
    link_reference_definitions: Arc<LinkReferenceDefinitions>,
    /// Shared definition and occurrence lookup for footnote navigation.
    footnote_registry: Arc<FootnoteRegistry>,
    /// Keeps the editor re-rendering when [`crate::config::EditorSettings`]
    /// changes (e.g. table/reveal preferences applied from the preferences
    /// window), so already-open documents reflect the new values immediately
    /// instead of waiting for a reopen or restart.
    editor_settings_observer: Option<Subscription>,
}

/// Runtime binding between a table block and one cell editor.
#[derive(Clone)]
struct TableCellBinding {
    /// Table container owning the cell.
    table_block: Entity<Block>,
    /// Independently focusable cell editor.
    cell: Entity<Block>,
    /// Visual row and column within the table runtime.
    position: TableCellPosition,
}

/// Selected row or column in a rendered native table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct TableAxisSelection {
    /// Table entity whose axis is selected.
    table_block_id: EntityId,
    /// Whether the selection is a row or column.
    kind: TableAxisKind,
    /// Visual index, with row zero denoting the header.
    index: usize,
}

/// Where a pending scroll should place its target within the viewport.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ScrollAlign {
    /// Scroll the minimum distance that brings the target into view. Correct
    /// for caret movement, where yanking the view around while typing would be
    /// disorienting.
    #[default]
    Nearest,
    /// Put the target as near the top of the viewport as the document allows.
    /// Used for outline jumps: the heading itself is just a label, and what the
    /// user actually wants to read is the content beneath it.
    Top,
}

/// Pixel geometry for the custom editor scrollbar.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ScrollbarGeometry {
    /// Track height in pixels.
    track_height: f32,
    /// Thumb height in pixels.
    thumb_height: f32,
    /// Thumb offset from the top of the track.
    thumb_top: f32,
    /// Maximum downward document scroll distance.
    max_scroll_y: f32,
}

/// Windowing result: the run of rows to mount, plus the spacer heights standing
/// in for the culled rows. `top_h` is the spacer directly above the run and
/// `bottom_h` the one closing out the document.
#[derive(Clone, Copy, Debug, PartialEq)]
struct RenderWindow {
    /// Inclusive start and exclusive end in the rendered-row sequence.
    run_start: usize,
    run_end: usize,
    /// Spacer height before the mounted run or focus island.
    top_h: f32,
    /// Spacer height after the mounted run or focus island.
    bottom_h: f32,
    /// Separate focused row mounted outside the visible run.
    focus_island: Option<FocusIsland>,
}

/// Where a frame's mounted run sat among the scroll container's children, so the
/// next frame can read its recorded bounds back by index. `child_count` is what
/// makes that mapping checkable rather than assumed.
#[derive(Clone, Copy, Debug, PartialEq)]
struct MountedRun {
    /// Half-open row interval represented by consecutive mounted children.
    row_start: usize,
    row_end: usize,
    /// Index of the first mounted row in the scroll container's children.
    child_base: usize,
    /// Number of scroll-container children from the measured frame.
    child_count: usize,
}

/// Focused row mounted on its own, away from the run. Its position relative to
/// the run follows from `row`, and `lead_h` is the spacer directly above it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FocusIsland {
    /// Index of the separately mounted focused row.
    row: usize,
    /// Spacer directly before that row.
    lead_h: f32,
}

/// Active drag session for the custom scrollbar thumb.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ScrollbarDragSession {
    /// Pointer's vertical distance from the thumb top at drag start.
    pointer_offset_y: f32,
    /// Track height captured at drag start.
    track_height: f32,
    /// Thumb height captured at drag start.
    thumb_height: f32,
    /// Maximum scroll distance captured at drag start.
    max_scroll_y: f32,
}

/// Source-byte selection snapshot stored with undo history across both modes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct UndoSelectionSnapshot {
    /// UTF-8 byte range in the saved source text, not a block-local display range.
    range: std::ops::Range<usize>,
    /// Whether the caret is at the range start rather than its end.
    reversed: bool,
}

/// Everything `capture_source_selection_snapshot` reads, reduced to a cheap
/// comparable key: the document's content hash plus the caret state of the
/// block being edited.
#[derive(Clone, PartialEq, Eq)]
struct SelectionSnapshotKey {
    content_fingerprint: u64,
    target: Option<(EntityId, std::ops::Range<usize>, bool)>,
    view_mode: ViewMode,
    /// A cross-block selection takes priority inside
    /// `capture_source_selection_snapshot`, so it has to participate in the key
    /// or dragging across blocks would keep serving a stale snapshot.
    cross_block_selection: Option<CrossBlockSelection>,
}

/// One undo history entry containing source text and selection state.
#[derive(Clone, Debug)]
struct HistoryEntry {
    /// Serialized document text at capture time.
    source_text: String,
    /// Selection in source-byte coordinates at capture time.
    selection: UndoSelectionSnapshot,
    /// Capture time used for edit coalescing.
    timestamp: Instant,
    /// Whether this edit may coalesce with adjacent typing.
    kind: UndoCaptureKind,
}

/// Deferred undo capture used to coalesce adjacent typing edits.
#[derive(Clone, Debug)]
struct PendingUndoCapture {
    /// Pre-edit state to commit when the document actually changes.
    snapshot: HistoryEntry,
}

/// Cross-block selection endpoint in visible block order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CrossBlockSelectionEndpoint {
    /// Visible block containing this endpoint.
    pub(super) entity_id: EntityId,
    /// Block-local visible-text byte offset, not a canonical Markdown offset.
    pub(super) offset: usize,
}

/// Editor-level selection spanning two visible block endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CrossBlockSelection {
    /// Endpoint where the selection began.
    pub(super) anchor: CrossBlockSelectionEndpoint,
    /// Current movable endpoint of the selection.
    pub(super) focus: CrossBlockSelectionEndpoint,
}

/// Drag state while creating or extending a cross-block selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CrossBlockDrag {
    /// Pointer-down endpoint from which a drag extends.
    pub(super) anchor: CrossBlockSelectionEndpoint,
}

/// Short-lived Ctrl/Cmd+A press counter for rendered-mode selection upgrade.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RenderedSelectAllCycle {
    /// Block that received the repeated command.
    entity_id: EntityId,
    /// Press count, capped at the document-selection step.
    count: u8,
    /// Time of the most recent Select All press.
    last_pressed_at: Instant,
}

/// Mapping from one visible block's text range to canonical Markdown offsets.
#[derive(Clone)]
pub(super) struct SourceTargetMapping {
    /// Editable block represented by this mapping.
    entity: Entity<Block>,
    /// Absolute canonical-source byte span, including the block's Markdown prefixes.
    full_source_range: std::ops::Range<usize>,
    /// Content-byte cursor positions mapped to block-relative source positions.
    content_to_source: Vec<usize>,
    /// Block-relative source positions mapped back to content-byte positions.
    source_to_content: Vec<usize>,
}

/// The two editing views the editor can present.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// Rich rendered view where each block is styled by its semantic kind.
    Rendered,
    /// Plain source view where the full Markdown document is edited as a
    /// single raw buffer.
    Source,
}

/// The informational dialogs that can be shown from the Help menu.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum InfoDialogKind {
    /// In-progress overlay displayed while the background update check runs.
    CheckForUpdates,
    /// Dialog with app name and version information.
    About,
}

impl Editor {
    /// Supplies saved document paths when the update helper restarts the application.
    pub(crate) fn restart_file_path(&self) -> Option<PathBuf> {
        self.file_path.clone()
    }

    /// Maximum number of retained undo entries.
    const HISTORY_LIMIT: usize = 200;
    /// Maximum gap between compatible typing edits that can share one undo entry.
    const HISTORY_COALESCE_WINDOW: Duration = Duration::from_millis(1_000);
    /// Maximum gap between select-all presses in the rendered selection-upgrade cycle.
    const RENDERED_SELECT_ALL_CYCLE_WINDOW: Duration = Duration::from_millis(750);

    /// Normalizes line endings and creates a rendered editor with subscribed block entities.
    /// Empty input receives a paragraph; tables, references, focus, and history are initialized.
    pub fn from_markdown(
        cx: &mut Context<Self>,
        markdown: String,
        file_path: Option<PathBuf>,
    ) -> Self {
        let normalized = markdown.replace("\r\n", "\n").replace('\r', "\n");
        let document_disk_text = file_path.as_ref().map(|_| normalized.clone());
        let mut roots = Self::build_root_blocks_from_markdown(cx, &normalized);
        if roots.is_empty() {
            roots.push(Self::new_block(cx, BlockRecord::paragraph(String::new())));
        }

        let mut document = DocumentTree::new(roots);
        document.rebuild_metadata_and_snapshot(cx);
        let pending_focus = document.first_root().map(|block| block.entity_id());

        let mut editor = Self {
            document,
            table_cells: HashMap::new(),
            view_mode: ViewMode::Rendered,
            pending_focus,
            active_entity_id: pending_focus,
            pending_scroll_active_block_into_view: true,
            pending_scroll_align: ScrollAlign::Nearest,
            pending_scroll_target: None,
            pending_scroll_recheck_after_layout: true,
            last_caret_position: None,
            suppress_caret_scroll_follow: false,
            pending_save: false,
            pending_save_as: false,
            pending_open_link: None,
            pending_image_paste_prompts: VecDeque::new(),
            image_paste_prompt_active: false,
            pending_picgo_uploads: HashMap::new(),
            unsaved_image_assets_dir: None,
            pending_window_edited: false,
            pending_window_edited_clear: false,
            pending_window_title_refresh: false,
            document_dirty: false,
            file_path,
            document_disk_text,
            watched_document_signature: None,
            pending_external_reload: None,
            external_reload_prompt_active: false,
            external_watch_task: None,
            scroll_handle: ScrollHandle::new(),
            last_scroll_viewport_size: None,
            prev_visible_block_ids: Vec::new(),
            row_stride_cache: HashMap::new(),
            row_stride_width: None,
            prev_mounted_run: None,
            prev_painted_visible: Vec::new(),
            close_guard_installed: false,
            show_unsaved_changes_dialog: false,
            update_save_prompt_active: false,
            pending_close_after_save: false,
            close_dialog_restore_focus: None,
            pending_drop_replace_path: None,
            show_drop_replace_dialog: false,
            pending_drop_replace_after_save: false,
            drop_replace_restore_focus: None,
            info_dialog: None,
            expanded_image: None,
            update_check_in_progress: false,
            workspace: WorkspaceState::from_settings(cx),
            workspace_watch: WorkspaceWatchState::default(),
            workspace_focus_handle: cx.focus_handle(),
            workspace_files_scroll_handle: ScrollHandle::new(),
            workspace_outline_scroll_handle: ScrollHandle::new(),
            workspace_resize_drag: None,
            workspace_root_override: None,
            status_bar: StatusBarState::default(),
            context_menu: None,
            table_insert_dialog: None,
            search_bar: None,
            context_menu_submenu_close_task: None,
            table_axis_preview: None,
            table_axis_selection: None,
            cross_block_selection: None,
            cross_block_drag: None,
            rendered_select_all_cycle: None,
            menu_bar_open: None,
            menu_submenu_open: None,
            menu_bar_hovered: false,
            menu_panel_hovered: false,
            menu_submenu_panel_hovered: false,
            menu_submenu_bridge_hovered: false,
            menu_close_task: None,
            scrollbar_hovered: false,
            scrollbar_visible_until: Instant::now(),
            scrollbar_fade_task: None,
            scroll_recheck_task: None,
            scrollbar_drag: None,
            undo_history: Vec::new(),
            redo_history: Vec::new(),
            pending_undo_capture: None,
            last_selection_snapshot: Self::empty_selection_snapshot(),
            last_selection_snapshot_key: None,
            last_stable_source_text: normalized,
            history_restore_in_progress: false,
            image_reference_definitions: Arc::default(),
            link_reference_definitions: Arc::default(),
            footnote_registry: Arc::default(),
            editor_settings_observer: None,
        };
        editor.rebuild_table_runtimes(cx);
        editor.rebuild_image_runtimes(cx);
        editor.pending_focus = editor.first_focusable_entity_id(cx);
        editor.active_entity_id = editor.pending_focus;
        editor.refresh_stable_document_snapshot(cx);
        editor.editor_settings_observer =
            Some(cx.observe_global::<crate::config::EditorSettings>(|_, cx| cx.notify()));
        editor
    }
}
