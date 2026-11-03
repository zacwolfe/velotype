//! Lightweight workspace panel state, file-tree scanning, and outline parsing.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context as _, Result};
use gpui::*;

use super::file_watch::{FileSignature, directory_modified};
use super::{BlockKind, Editor};
use crate::components::InlineTextTree;
use crate::config::EditorSettings;
use crate::config::preferences::WorkspacePreferences;
use crate::i18n::I18nStrings;
use crate::theme::Theme;

const FOLDER_ICON: &str = "icon/workspace/folder.svg";
const MARKDOWN_ICON: &str = "icon/workspace/markdown.svg";
const WORKSPACE_PANEL_TARGET_RATIO: f32 = 0.15;
const WORKSPACE_PANEL_MIN_WIDTH: f32 = 240.0;
const WORKSPACE_PANEL_MAX_WIDTH: f32 = 360.0;
// Manual drag allows a wider range than the automatic viewport-ratio width above.
const WORKSPACE_PANEL_DRAG_MIN_WIDTH: f32 = 180.0;
const WORKSPACE_PANEL_DRAG_MAX_WIDTH: f32 = 720.0;
const WORKSPACE_NODE_HEIGHT: f32 = 28.0;
const WORKSPACE_NODE_INDENT: f32 = 18.0;
const WORKSPACE_PARENT_NODE_ID: &str = "workspace-parent-row";
// Loading is lazy (one directory level at a time), which removes the need
// for a depth or whole-tree cap. A single directory holding tens of
// thousands of entries would still stall the one read that loads it, so
// each level on its own stays capped.
const WORKSPACE_SCAN_MAX_ENTRIES_PER_DIR: usize = 2_000;
// Skipped during scanning: these are huge and never contain the documents a
// user is browsing the sidebar for.
const WORKSPACE_SCAN_SKIP_DIRS: &[&str] = &["node_modules", "target", "build", "dist"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkspaceSection {
    Files,
    Outline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum WorkspaceTreeKind {
    Directory(PathBuf),
    MarkdownFile(PathBuf),
    Heading { line: usize, level: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WorkspaceTreeNode {
    id: String,
    label: String,
    kind: WorkspaceTreeKind,
    children: Vec<WorkspaceTreeNode>,
    /// False only for a directory whose children have not been read yet.
    /// Files and outline headings are always "loaded" — they have no
    /// children to fetch.
    children_loaded: bool,
}

/// One row the sidebar is currently painting, in DFS order. Used by keyboard
/// navigation so up/down walk exactly the rows the user can see.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WorkspaceVisibleRow {
    id: String,
    parent_id: Option<String>,
    kind: WorkspaceTreeKind,
    expanded: bool,
    expandable: bool,
}

pub(super) struct WorkspaceState {
    pub(super) is_open: bool,
    files_section_open: bool,
    outline_section_open: bool,
    root: Option<PathBuf>,
    file_tree: Option<WorkspaceTreeNode>,
    file_error: Option<String>,
    outline_tree: Vec<WorkspaceTreeNode>,
    outline_source: Option<String>,
    /// Fingerprint of the document the cached outline was built from.
    outline_fingerprint: Option<u64>,
    expanded: HashSet<String>,
    selected_file: Option<PathBuf>,
    selected_outline: Option<String>,
    /// Sidebar section that owns the keyboard cursor. Kept separate from the
    /// open-file/heading selections so arrow-key navigation can highlight a
    /// row without opening it or moving the document caret.
    keyboard_section: Option<WorkspaceSection>,
    keyboard_file_node: Option<String>,
    keyboard_outline_node: Option<String>,
    /// `None` means the user has never dragged the resize handle; the width
    /// then tracks the viewport automatically. Deliberately in-memory only.
    width: Option<f32>,
}

/// Filesystem identities for everything the Files sidebar currently shows.
///
/// Directory mtimes tell us when entries were added, removed, or renamed;
/// per-file signatures tell us when a Markdown file listed in the tree was
/// edited outside the app. `changed_files` drives the small dot shown on such
/// rows until the file is opened.
#[derive(Default)]
pub(super) struct WorkspaceWatchState {
    pub(super) dir_signatures: HashMap<PathBuf, Option<SystemTime>>,
    pub(super) file_signatures: HashMap<PathBuf, Option<FileSignature>>,
    pub(super) changed_files: HashSet<PathBuf>,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        // Both sections start open: the sane default layout for a fresh window.
        Self {
            is_open: false,
            files_section_open: true,
            outline_section_open: true,
            root: None,
            file_tree: None,
            file_error: None,
            outline_tree: Vec::new(),
            outline_source: None,
            outline_fingerprint: None,
            expanded: HashSet::new(),
            selected_file: None,
            selected_outline: None,
            keyboard_section: None,
            keyboard_file_node: None,
            keyboard_outline_node: None,
            width: None,
        }
    }
}

impl WorkspaceState {
    /// Seeds the drawer's open state and section layout from the persisted
    /// preference, so a new window opens with the sidebar and sections the
    /// user last left. Reads the mirrored global rather than the file, so
    /// window creation never touches disk and tests fall back to the
    /// defaults instead of the developer's own config.
    pub(super) fn from_settings(cx: &App) -> Self {
        let preferences = EditorSettings::workspace_preferences(cx);
        Self {
            is_open: preferences.drawer_open,
            files_section_open: preferences.files_open,
            outline_section_open: preferences.outline_open,
            ..Self::default()
        }
    }
}

/// In-flight sidebar resize. `max_width` is captured at drag start so the
/// clamp does not shift if the window resizes mid-drag.
#[derive(Clone, Copy)]
pub(super) struct WorkspaceResizeDrag {
    start_pointer_x: f32,
    start_width: f32,
    max_width: f32,
}

impl Editor {
    pub(crate) fn toggle_workspace_drawer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.workspace.is_open {
            self.workspace.is_open = false;
        } else {
            self.close_menu_bar(cx);
            self.dismiss_contextual_overlays(cx);
            self.workspace.is_open = true;
            self.sync_workspace_models(cx);
            window.activate_window();
        }
        EditorSettings::set_workspace_preferences(
            cx,
            WorkspacePreferences {
                drawer_open: self.workspace.is_open,
                files_open: self.workspace.files_section_open,
                outline_open: self.workspace.outline_section_open,
            },
        );
        cx.notify();
    }

    /// Opens the Files/Outline drawer without activating the window. Used by
    /// the launch path, where `foreground_on_launch` (not this call) governs
    /// whether the window should steal focus — so unlike
    /// `toggle_workspace_drawer` this takes no `Window` and never calls
    /// `window.activate_window()`. Idempotent: a no-op when already open.
    ///
    /// Deliberately does not persist `is_open`: this is the automatic
    /// `velotype <dir>` reveal, and a one-off directory launch must not
    /// silently turn the sidebar on for every future single-file launch.
    /// Costs nothing, since a directory launch reveals the drawer every time
    /// regardless of the stored preference.
    pub(crate) fn reveal_workspace_drawer(&mut self, cx: &mut Context<Self>) {
        if self.workspace.is_open {
            return;
        }
        self.close_menu_bar(cx);
        self.dismiss_contextual_overlays(cx);
        self.workspace.is_open = true;
        self.sync_workspace_models(cx);
        cx.notify();
    }

    pub(crate) fn on_toggle_workspace_action(
        &mut self,
        _: &crate::components::ToggleWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_workspace_drawer(window, cx);
    }

    pub(super) fn sync_workspace_after_document_path_change(&mut self, cx: &mut Context<Self>) {
        self.workspace.root = None;
        self.workspace.file_tree = None;
        self.workspace.file_error = None;
        self.workspace.outline_source = None;
        self.workspace.outline_fingerprint = None;
        // Keep an explicit root while the document being opened lives inside
        // it, so browsing to a parent survives opening files from the tree;
        // an unrelated document elsewhere reclaims the root.
        let inside_override = match (
            self.workspace_root_override.as_ref(),
            self.file_path.as_ref(),
        ) {
            (Some(root), Some(path)) => path.starts_with(root),
            _ => false,
        };
        if !inside_override {
            self.workspace_root_override = None;
        }
        if self.workspace.is_open {
            self.sync_workspace_models(cx);
        }
    }

    /// Roots the sidebar at `root` regardless of the open document's path
    /// (e.g. `velotype ~/some-dir/` with no markdown file inside). Clears the
    /// cached tree first so the stale root can't survive the early-return
    /// guard in `sync_workspace_file_tree`.
    pub(crate) fn set_workspace_root_override(
        &mut self,
        root: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.workspace_root_override = Some(root);
        self.workspace.root = None;
        self.workspace.file_tree = None;
        self.workspace.file_error = None;
        self.workspace.outline_source = None;
        self.workspace.outline_fingerprint = None;
        if self.workspace.is_open {
            self.sync_workspace_models(cx);
        }
        cx.notify();
    }

    /// Re-roots the Files tree at the current root's parent. Returns false at
    /// the filesystem root, where there is nowhere to go.
    pub(super) fn navigate_workspace_root_up(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(current_root) = self
            .workspace_root_for_current_file()
            .or_else(|| self.workspace.root.clone())
        else {
            return false;
        };
        let Some(parent) = current_root.parent() else {
            return false;
        };
        self.set_workspace_root_override(parent.to_path_buf(), cx);
        true
    }

    fn sync_workspace_models(&mut self, cx: &mut Context<Self>) {
        self.sync_workspace_file_tree();
        self.sync_workspace_outline(cx);
    }

    fn workspace_root_for_current_file(&self) -> Option<PathBuf> {
        if let Some(root) = self.workspace_root_override.as_ref() {
            return Some(root.clone());
        }
        self.file_path.as_ref()?.parent().map(Path::to_path_buf)
    }

    fn sync_workspace_file_tree(&mut self) {
        let next_root = self.workspace_root_for_current_file();
        if self.workspace.root == next_root && self.workspace.file_tree.is_some() {
            self.workspace.selected_file = self.file_path.clone();
            return;
        }

        self.workspace.root = next_root.clone();
        self.workspace.file_tree = None;
        self.workspace.file_error = None;

        let Some(root) = next_root else {
            self.workspace.selected_file = None;
            self.workspace_watch = WorkspaceWatchState::default();
            return;
        };

        // Validate the root path
        if root.as_os_str().is_empty() {
            self.workspace.file_error = Some("Invalid workspace path: empty path".to_string());
            self.workspace.selected_file = None;
            self.workspace_watch = WorkspaceWatchState::default();
            return;
        }

        match scan_workspace_dir(&root) {
            Ok(mut tree) => {
                self.workspace.expanded.insert(tree.id.clone());
                // A directory further down may still be marked expanded from
                // before this (re-)scan; without this its children would
                // render expanded-yet-empty.
                restore_expanded_descendants(&mut tree, &self.workspace.expanded);
                self.workspace.file_tree = Some(tree);
                self.workspace.selected_file = self.file_path.clone();
                self.seed_workspace_watch_state();
            }
            Err(err) => {
                self.workspace.file_error = Some(err.to_string());
                self.workspace_watch = WorkspaceWatchState::default();
            }
        }
    }

    /// Records the current identity of every directory and Markdown file the
    /// sidebar has loaded. Called after a scan or lazy directory load, never on
    /// a plain render, so the baseline is not reset before a change can be
    /// noticed.
    fn seed_workspace_watch_state(&mut self) {
        let mut dirs = HashSet::new();
        let mut files = HashSet::new();

        if let Some(tree) = self.workspace.file_tree.as_ref() {
            collect_watch_targets(tree, &mut dirs, &mut files);
        } else if let Some(root) = self.workspace.root.as_ref() {
            dirs.insert(root.clone());
        }

        self.workspace_watch
            .dir_signatures
            .retain(|path, _| dirs.contains(path));
        self.workspace_watch
            .file_signatures
            .retain(|path, _| files.contains(path));
        self.workspace_watch
            .changed_files
            .retain(|path| files.contains(path));

        for dir in dirs {
            // Directory mtimes are the trigger for a rescan, so a reseed after
            // that rescan must consume the new value. Otherwise the poll would
            // see the same stale signature and rescan forever.
            self.workspace_watch
                .dir_signatures
                .insert(dir.clone(), directory_modified(&dir));
        }
        for file in files {
            self.workspace_watch
                .file_signatures
                .entry(file.clone())
                .or_insert_with(|| FileSignature::read(&file));
        }
    }

    /// Polls the Files sidebar's visible directories and Markdown files.
    /// A directory mtime change means entries were added, removed, or renamed,
    /// so the tree is rescanned with its expansion state preserved. A file
    /// signature change marks that row as externally updated until it is
    /// opened.
    pub(super) fn poll_workspace_external_changes(&mut self, cx: &mut Context<Self>) {
        if !self.workspace.is_open {
            return;
        }
        let Some(root) = self.workspace.root.clone() else {
            self.workspace_watch = WorkspaceWatchState::default();
            return;
        };

        // Keep watching the root even while a previous scan is in the error
        // state, so a directory that reappears can be picked up again.
        self.workspace_watch
            .dir_signatures
            .entry(root.clone())
            .or_insert_with(|| directory_modified(&root));

        let structure_changed = self
            .workspace_watch
            .dir_signatures
            .iter()
            .any(|(dir, signature)| directory_modified(dir) != *signature);
        if structure_changed {
            self.refresh_workspace_file_tree(cx);
            return;
        }

        let updates: Vec<(PathBuf, Option<FileSignature>)> = self
            .workspace_watch
            .file_signatures
            .iter()
            .filter_map(|(file, previous)| {
                let current = FileSignature::read(file);
                (*previous != current).then(|| (file.clone(), current))
            })
            .collect();

        let mut newly_changed = Vec::new();
        for (file, signature) in updates {
            self.workspace_watch
                .file_signatures
                .insert(file.clone(), signature);
            if signature.is_some() && self.file_path.as_deref() != Some(file.as_path()) {
                newly_changed.push(file);
            }
        }

        let mut needs_repaint = false;
        for file in newly_changed {
            needs_repaint |= self.workspace_watch.changed_files.insert(file);
        }
        if needs_repaint {
            cx.notify();
        }
    }

    /// Rescans the sidebar root after an external structural change, keeping
    /// the user's expanded folders and selection intact.
    fn refresh_workspace_file_tree(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.root.clone() else {
            self.workspace_watch = WorkspaceWatchState::default();
            return;
        };

        match scan_workspace_dir(&root) {
            Ok(mut tree) => {
                restore_expanded_descendants(&mut tree, &self.workspace.expanded);
                self.workspace.file_tree = Some(tree);
                self.workspace.file_error = None;
                self.workspace.selected_file = self.file_path.clone();
                self.seed_workspace_watch_state();
            }
            Err(err) => {
                self.workspace.file_error = Some(err.to_string());
                self.workspace.file_tree = None;
                self.workspace.selected_file = self.file_path.clone();
                self.workspace_watch = WorkspaceWatchState::default();
                self.workspace_watch
                    .dir_signatures
                    .insert(root.clone(), directory_modified(&root));
            }
        }
        cx.notify();
    }

    fn sync_workspace_outline(&mut self, cx: &mut Context<Self>) {
        // The caret blink re-runs render ~30x/second, and serializing the whole
        // document allocates a string per block plus document-sized offset-map
        // vectors per inline tree. Check a cheap allocation-free fingerprint
        // first so the steady state costs a hash walk instead of a full
        // serialize whose result is then usually discarded.
        let fingerprint = self.document.content_fingerprint(cx);
        if self.workspace.outline_fingerprint == Some(fingerprint)
            && self.workspace.outline_source.is_some()
        {
            return;
        }
        self.workspace.outline_fingerprint = Some(fingerprint);

        let source = self.serialized_document_text(cx);
        if self.workspace.outline_source.as_deref() == Some(source.as_str()) {
            return;
        }

        let outline = build_outline_tree(&source);
        prune_outline_state(&mut self.workspace, &outline);
        self.workspace.outline_tree = outline;
        self.workspace.outline_source = Some(source);
    }

    /// Forces the outline to rebuild after the document text was replaced
    /// outside the normal editing path.
    pub(super) fn invalidate_workspace_outline(&mut self) {
        self.workspace.outline_source = None;
        self.workspace.outline_fingerprint = None;
    }

    fn toggle_workspace_section(&mut self, section: WorkspaceSection, cx: &mut Context<Self>) {
        toggle_workspace_section_state(&mut self.workspace, section);
        EditorSettings::set_workspace_preferences(
            cx,
            WorkspacePreferences {
                drawer_open: self.workspace.is_open,
                files_open: self.workspace.files_section_open,
                outline_open: self.workspace.outline_section_open,
            },
        );
        cx.notify();
    }

    pub(super) fn workspace_panel_width(&self, viewport_width: f32) -> f32 {
        resolve_workspace_panel_width(self.workspace.width, viewport_width)
    }

    pub(super) fn start_workspace_resize(
        &mut self,
        start_pointer_x: f32,
        start_width: f32,
        max_width: f32,
        cx: &mut Context<Self>,
    ) {
        self.workspace_resize_drag = Some(WorkspaceResizeDrag {
            start_pointer_x,
            start_width,
            max_width,
        });
        cx.notify();
    }

    pub(super) fn update_workspace_resize(&mut self, pointer_x: f32, cx: &mut Context<Self>) {
        let Some(drag) = self.workspace_resize_drag else {
            return;
        };

        let next = drag.start_width + (pointer_x - drag.start_pointer_x);
        self.workspace.width = Some(next.clamp(WORKSPACE_PANEL_DRAG_MIN_WIDTH, drag.max_width));
        cx.notify();
    }

    pub(super) fn end_workspace_resize(&mut self, cx: &mut Context<Self>) {
        if self.workspace_resize_drag.take().is_some() {
            cx.notify();
        }
    }

    /// `dir` is `Some` for a directory row, `None` otherwise (files/headings
    /// have no children to load). On collapse, only `expanded` membership
    /// changes — already-read children stay cached, so re-expanding is
    /// instant. Consequence: a file created outside the app will not appear
    /// until the root is re-scanned (root change / document path change).
    fn toggle_workspace_node(&mut self, id: &str, dir: Option<&Path>, cx: &mut Context<Self>) {
        if self.workspace.expanded.remove(id) {
            cx.notify();
            return;
        }
        self.workspace.expanded.insert(id.to_string());
        if let Some(dir) = dir {
            self.load_workspace_dir_children(dir);
        }
        cx.notify();
    }

    /// Reads `dir`'s immediate children into the cached tree. No-op when the
    /// node is already loaded or not present.
    fn load_workspace_dir_children(&mut self, dir: &Path) {
        let expanded = &self.workspace.expanded;
        let Some(root) = self.workspace.file_tree.as_mut() else {
            return;
        };
        find_and_load_dir(root, dir, expanded);
        self.seed_workspace_watch_state();
    }

    fn select_outline_node(&mut self, id: String, cx: &mut Context<Self>) {
        self.workspace.selected_outline = Some(id);
        cx.notify();
    }

    fn workspace_section_is_usable(&self, section: WorkspaceSection) -> bool {
        match section {
            WorkspaceSection::Files => {
                self.workspace.files_section_open && self.workspace.file_tree.is_some()
            }
            WorkspaceSection::Outline => {
                self.workspace.outline_section_open && !self.workspace.outline_tree.is_empty()
            }
        }
    }

    fn workspace_active_section(&self) -> Option<WorkspaceSection> {
        self.workspace
            .keyboard_section
            .filter(|section| self.workspace_section_is_usable(*section))
            .or_else(|| {
                [WorkspaceSection::Files, WorkspaceSection::Outline]
                    .into_iter()
                    .find(|section| self.workspace_section_is_usable(*section))
            })
    }

    fn workspace_visible_rows(&self, section: WorkspaceSection) -> Vec<WorkspaceVisibleRow> {
        let mut rows = Vec::new();
        match section {
            WorkspaceSection::Files => {
                if let Some(parent) = self.workspace.root.as_deref().and_then(Path::parent) {
                    rows.push(WorkspaceVisibleRow {
                        id: WORKSPACE_PARENT_NODE_ID.to_string(),
                        parent_id: None,
                        kind: WorkspaceTreeKind::Directory(parent.to_path_buf()),
                        expanded: false,
                        expandable: false,
                    });
                }
                if let Some(root) = self.workspace.file_tree.as_ref() {
                    collect_workspace_visible_rows(
                        std::slice::from_ref(root),
                        &self.workspace.expanded,
                        None,
                        &mut rows,
                    );
                }
            }
            WorkspaceSection::Outline => {
                collect_workspace_visible_rows(
                    &self.workspace.outline_tree,
                    &self.workspace.expanded,
                    None,
                    &mut rows,
                );
            }
        }
        rows
    }

    fn workspace_keyboard_node(&self, section: WorkspaceSection) -> Option<String> {
        match section {
            WorkspaceSection::Files => self.workspace.keyboard_file_node.clone(),
            WorkspaceSection::Outline => self.workspace.keyboard_outline_node.clone(),
        }
    }

    fn set_workspace_keyboard_node(&mut self, section: WorkspaceSection, id: String) {
        match section {
            WorkspaceSection::Files => self.workspace.keyboard_file_node = Some(id),
            WorkspaceSection::Outline => self.workspace.keyboard_outline_node = Some(id),
        }
    }

    /// Initial cursor for a section: the open file / selected heading when it is
    /// visible, otherwise the first row.
    fn workspace_default_cursor(
        &self,
        section: WorkspaceSection,
        rows: &[WorkspaceVisibleRow],
    ) -> Option<String> {
        let selected = match section {
            WorkspaceSection::Files => self.workspace.selected_file.as_ref().and_then(|path| {
                rows.iter()
                    .find(|row| {
                        matches!(&row.kind, WorkspaceTreeKind::MarkdownFile(row_path) if row_path == path)
                    })
                    .map(|row| row.id.clone())
            }),
            WorkspaceSection::Outline => self.workspace.selected_outline.clone(),
        };
        selected
            .filter(|id| rows.iter().any(|row| row.id == *id))
            .or_else(|| {
                rows.iter()
                    .find(|row| row.id != WORKSPACE_PARENT_NODE_ID)
                    .or_else(|| rows.first())
                    .map(|row| row.id.clone())
            })
    }

    fn focus_workspace_node(
        &mut self,
        section: WorkspaceSection,
        node_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace.keyboard_section = Some(section);
        self.set_workspace_keyboard_node(section, node_id);
        window.focus(&self.workspace_focus_handle);
        cx.notify();
    }

    fn focus_workspace_section(
        &mut self,
        section: WorkspaceSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.workspace.keyboard_section = Some(section);
        if self.workspace_keyboard_node(section).is_none()
            || !self
                .workspace_visible_rows(section)
                .iter()
                .any(|row| Some(&row.id) == self.workspace_keyboard_node(section).as_ref())
        {
            let rows = self.workspace_visible_rows(section);
            if let Some(cursor) = self.workspace_default_cursor(section, &rows) {
                self.set_workspace_keyboard_node(section, cursor);
            }
        }
        window.focus(&self.workspace_focus_handle);
        cx.notify();
    }

    /// Keeps the sidebar keyboard cursor focused after an action that would
    /// otherwise hand focus to the document (opening a file, jumping to a
    /// heading, or applying a stale pending focus).
    fn retain_workspace_keyboard_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_drop_replace_dialog {
            return;
        }
        self.pending_focus = None;
        window.focus(&self.workspace_focus_handle);
        cx.notify();
    }

    fn focus_document_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .active_entity_id
            .filter(|id| self.focusable_entity_by_id(*id).is_some())
            .or_else(|| self.first_focusable_entity_id(cx));
        if let Some(entity_id) = target
            && let Some(block) = self.focusable_entity_by_id(entity_id)
        {
            block.read(cx).focus_handle.focus(window);
            self.active_entity_id = Some(entity_id);
            self.pending_focus = None;
            self.pending_scroll_active_block_into_view = true;
        }
        cx.notify();
    }

    /// Cycles focus through the visible sidebar sections and the document.
    pub(super) fn cycle_workspace_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.workspace.is_open {
            self.reveal_workspace_drawer(cx);
        }
        self.sync_workspace_models(cx);

        let mut sections = Vec::new();
        if self.workspace_section_is_usable(WorkspaceSection::Files) {
            sections.push(WorkspaceSection::Files);
        }
        if self.workspace_section_is_usable(WorkspaceSection::Outline) {
            sections.push(WorkspaceSection::Outline);
        }

        let current_section = self
            .workspace_focus_handle
            .is_focused(window)
            .then_some(self.workspace.keyboard_section)
            .flatten()
            .filter(|section| sections.contains(section));

        let next_section = match current_section.and_then(|current| {
            sections
                .iter()
                .position(|section| *section == current)
                .map(|index| index + 1)
        }) {
            Some(index) if index < sections.len() => Some(sections[index]),
            Some(_) => None,
            None => sections.first().copied(),
        };

        if let Some(section) = next_section {
            self.focus_workspace_section(section, window, cx);
        } else {
            self.focus_document_panel(window, cx);
        }
    }

    pub(super) fn on_cycle_workspace_focus(
        &mut self,
        _: &crate::components::CycleWorkspaceFocus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_workspace_focus(window, cx);
    }

    /// Activates the keyboard cursor: opens a file in the Files tree, toggles a
    /// directory, or jumps to a heading in the Outline tree.
    fn workspace_activate_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(section) = self.workspace_active_section() else {
            return false;
        };
        let rows = self.workspace_visible_rows(section);
        let node_id = self
            .workspace_keyboard_node(section)
            .filter(|id| rows.iter().any(|row| row.id == *id))
            .or_else(|| self.workspace_default_cursor(section, &rows));
        let Some(node_id) = node_id else {
            return false;
        };
        let Some(row) = rows.into_iter().find(|row| row.id == node_id) else {
            return false;
        };

        if section == WorkspaceSection::Files && row.id == WORKSPACE_PARENT_NODE_ID {
            if self.navigate_workspace_root_up(cx) {
                self.workspace.keyboard_file_node = None;
                self.focus_workspace_section(WorkspaceSection::Files, window, cx);
            }
            self.retain_workspace_keyboard_focus(window, cx);
            return true;
        }

        match (section, row.kind) {
            (WorkspaceSection::Files, WorkspaceTreeKind::MarkdownFile(path)) => {
                self.set_workspace_keyboard_node(section, row.id.clone());
                self.open_workspace_file(path, window, cx);
                self.retain_workspace_keyboard_focus(window, cx);
                true
            }
            (WorkspaceSection::Files, WorkspaceTreeKind::Directory(path)) => {
                self.set_workspace_keyboard_node(section, row.id.clone());
                if row.expandable {
                    self.toggle_workspace_node(&row.id, Some(&path), cx);
                }
                self.retain_workspace_keyboard_focus(window, cx);
                true
            }
            (WorkspaceSection::Outline, WorkspaceTreeKind::Heading { line, .. }) => {
                self.set_workspace_keyboard_node(section, row.id.clone());
                self.select_outline_node(row.id, cx);
                self.jump_to_outline_heading(line, cx);
                self.retain_workspace_keyboard_focus(window, cx);
                true
            }
            _ => false,
        }
    }

    pub(super) fn on_workspace_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let modifiers = event.keystroke.modifiers;
        if modifiers.shift
            || modifiers.control
            || modifiers.platform
            || modifiers.alt
            || modifiers.function
        {
            return;
        }

        let key = event.keystroke.key.as_str();
        if key == "enter" {
            if self.workspace_activate_cursor(window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        if !matches!(key, "up" | "down" | "left" | "right") {
            return;
        }
        if self.workspace_handle_navigation_key(key, cx) {
            self.retain_workspace_keyboard_focus(window, cx);
            cx.stop_propagation();
        }
    }

    /// Applies one unmodified arrow key to the active sidebar tree. Split from
    /// the GPUI event handler so it can be tested without a window.
    pub(super) fn workspace_handle_navigation_key(
        &mut self,
        key: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(section) = self.workspace_active_section() else {
            return false;
        };
        let rows = self.workspace_visible_rows(section);
        if rows.is_empty() {
            return false;
        }
        self.workspace.keyboard_section = Some(section);

        let current = self
            .workspace_keyboard_node(section)
            .and_then(|id| rows.iter().position(|row| row.id == id));

        match key {
            "up" => {
                match current {
                    Some(0) => {}
                    Some(index) => {
                        self.set_workspace_keyboard_node(section, rows[index - 1].id.clone());
                        cx.notify();
                    }
                    None => {
                        self.set_workspace_keyboard_node(
                            section,
                            rows.last().expect("rows is non-empty").id.clone(),
                        );
                        cx.notify();
                    }
                }
                true
            }
            "down" => {
                match current {
                    Some(index) if index + 1 < rows.len() => {
                        self.set_workspace_keyboard_node(section, rows[index + 1].id.clone());
                        cx.notify();
                    }
                    Some(_) => {}
                    None => {
                        self.set_workspace_keyboard_node(
                            section,
                            rows.first().expect("rows is non-empty").id.clone(),
                        );
                        cx.notify();
                    }
                }
                true
            }
            "right" => {
                let Some(index) = current else {
                    self.set_workspace_keyboard_node(
                        section,
                        rows.first().expect("rows is non-empty").id.clone(),
                    );
                    cx.notify();
                    return true;
                };
                let row = rows[index].clone();
                if section == WorkspaceSection::Files && row.id == WORKSPACE_PARENT_NODE_ID {
                    if self.navigate_workspace_root_up(cx) {
                        self.workspace.keyboard_file_node = None;
                    }
                    return true;
                }
                if row.expandable && !row.expanded {
                    let dir = match &row.kind {
                        WorkspaceTreeKind::Directory(path) => Some(path.as_path()),
                        _ => None,
                    };
                    self.toggle_workspace_node(&row.id, dir, cx);
                }
                // After expanding, or when already expanded, step into the
                // first visible child. This makes Right literally descend one
                // level rather than only toggling the disclosure arrow.
                let expanded_rows = self.workspace_visible_rows(section);
                let first_child = expanded_rows
                    .get(index + 1)
                    .filter(|child| child.parent_id.as_deref() == Some(row.id.as_str()))
                    .map(|child| child.id.clone());
                if let Some(child) = first_child {
                    self.set_workspace_keyboard_node(section, child);
                    cx.notify();
                }
                true
            }
            "left" => {
                let Some(index) = current else {
                    self.set_workspace_keyboard_node(
                        section,
                        rows.first().expect("rows is non-empty").id.clone(),
                    );
                    cx.notify();
                    return true;
                };
                let row = rows[index].clone();
                if row.expanded && row.expandable {
                    let dir = match &row.kind {
                        WorkspaceTreeKind::Directory(path) => Some(path.as_path()),
                        _ => None,
                    };
                    self.toggle_workspace_node(&row.id, dir, cx);
                } else if let Some(parent) = row.parent_id {
                    self.set_workspace_keyboard_node(section, parent);
                    cx.notify();
                }
                true
            }
            _ => false,
        }
    }

    /// Moves the caret to the document position of an outline heading, which
    /// also scrolls it into view via the existing pending-scroll path.
    pub(super) fn jump_to_outline_heading(&mut self, line: usize, cx: &mut Context<Self>) -> bool {
        let source = self.serialized_document_text(cx);
        let Some(offset) = line_start_offset(&source, line) else {
            return false;
        };
        let mappings = self.build_source_target_mappings(cx);
        let Some(endpoint) = self.endpoint_for_source_offset(offset, &mappings, cx) else {
            return false;
        };
        let Some(block) = self.focusable_entity_by_id(endpoint.entity_id) else {
            return false;
        };
        self.focus_block_range(&block, endpoint.offset..endpoint.offset, cx);
        // `focus_block_range` resets the explicit target, and the sidebar keeps
        // keyboard focus afterward, so remember the heading separately. The
        // pending-scroll pass uses this instead of the focused caret.
        self.pending_scroll_target = Some(block.entity_id());
        // The heading is a label; the content under it is the point. Set this
        // after `focus_block_range`, which resets alignment to `Nearest`.
        self.pending_scroll_align = super::ScrollAlign::Top;
        true
    }

    fn open_workspace_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.selected_file = Some(path.clone());
        self.request_dropped_markdown_replace(path, window, cx);
    }

    pub(super) fn render_workspace_panel(
        &mut self,
        theme: &Theme,
        strings: &I18nStrings,
        panel_width: f32,
        viewport_width: f32,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.workspace.is_open {
            return None;
        }

        self.sync_workspace_models(cx);
        let editor = cx.entity().downgrade();
        let c = &theme.colors;
        let d = &theme.dimensions;

        // Body only rendered when its section is open; a collapsed section
        // needs no tree built, since the caller below skips it entirely.
        let files_body = self
            .workspace
            .files_section_open
            .then(|| self.render_workspace_files_tree(theme, strings, &editor));
        let mut section_children = self.render_workspace_section(
            WorkspaceSection::Files,
            strings.workspace_tab_files.clone(),
            self.workspace.files_section_open,
            "workspace-files-scroll",
            files_body,
            theme,
            &editor,
        );

        let outline_body = self
            .workspace
            .outline_section_open
            .then(|| self.render_workspace_outline_tree(theme, strings, &editor));
        section_children.extend(self.render_workspace_section(
            WorkspaceSection::Outline,
            strings.workspace_tab_outline.clone(),
            self.workspace.outline_section_open,
            "workspace-outline-scroll",
            outline_body,
            theme,
            &editor,
        ));

        let resize_max_width = workspace_panel_drag_max_width(viewport_width);
        let resize_editor = editor.clone();
        let workspace_focus_handle = self.workspace_focus_handle.clone();

        Some(
            div()
                .id("workspace-panel")
                .h_full()
                .w(px(panel_width))
                .relative()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .bg(c.dialog_surface)
                .border_r(px(d.dialog_border_width))
                .border_color(c.dialog_border)
                .track_focus(&workspace_focus_handle)
                .key_context("Workspace")
                .on_key_down(cx.listener(Self::on_workspace_key_down))
                .children(section_children)
                .child(
                    div()
                        .id("workspace-resize-handle")
                        .absolute()
                        .top_0()
                        .right_0()
                        .h_full()
                        .w(px(6.0))
                        .cursor_ew_resize()
                        .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
                            let pointer_x = f32::from(event.position.x);
                            let _ = resize_editor.update(cx, |editor, cx| {
                                cx.stop_propagation();
                                editor.start_workspace_resize(
                                    pointer_x,
                                    panel_width,
                                    resize_max_width,
                                    cx,
                                );
                            });
                        })
                        .child(
                            canvas(
                                |_, _, _| (),
                                move |_bounds, _, window, _| {
                                    window.on_mouse_event({
                                        let editor = editor.clone();
                                        move |_event: &MouseUpEvent, phase, _window, cx| {
                                            if !phase.bubble() {
                                                return;
                                            }
                                            let _ = editor.update(cx, |editor, cx| {
                                                editor.end_workspace_resize(cx);
                                            });
                                        }
                                    });

                                    window.on_mouse_event({
                                        let editor = editor.clone();
                                        move |event: &MouseMoveEvent, phase, _window, cx| {
                                            if !phase.bubble() || !event.dragging() {
                                                return;
                                            }

                                            let pointer_x = f32::from(event.position.x);
                                            let _ = editor.update(cx, |editor, cx| {
                                                editor.update_workspace_resize(pointer_x, cx);
                                            });
                                        }
                                    });
                                },
                            )
                            .size_full(),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Renders one collapsible section: an always-present header row, plus
    /// its scrollable body when open. `body` is `None` when the section is
    /// collapsed. Called once per section (Files, Outline); flexbox alone
    /// then splits remaining height between however many bodies are open.
    fn render_workspace_section(
        &self,
        section: WorkspaceSection,
        label: String,
        is_open: bool,
        scroll_id: &'static str,
        body: Option<AnyElement>,
        theme: &Theme,
        editor: &WeakEntity<Editor>,
    ) -> Vec<AnyElement> {
        let c = &theme.colors;
        let d = &theme.dimensions;
        let t = &theme.typography;

        let header_id = match section {
            WorkspaceSection::Files => "workspace-section-files",
            WorkspaceSection::Outline => "workspace-section-outline",
        };
        let arrow = if is_open { "v" } else { ">" };
        let header_editor = editor.clone();

        let header = div()
            .id(header_id)
            .flex_shrink_0()
            .h(px(30.0))
            .w_full()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .hover(|this| this.bg(c.dialog_secondary_button_hover))
            .border_b(px(d.dialog_border_width))
            .border_color(c.dialog_border)
            .text_size(px(t.text_size * 0.88))
            .text_color(c.text_default)
            .child(
                div()
                    .w(px(14.0))
                    .flex_shrink_0()
                    .text_color(c.dialog_muted)
                    .child(arrow),
            )
            .child(label)
            .on_click(move |_event, window, cx| {
                let _ = header_editor.update(cx, |editor, cx| {
                    editor.focus_workspace_section(section, window, cx);
                    editor.toggle_workspace_section(section, cx);
                });
            })
            .into_any_element();

        // Each open body gets its own scroll region (distinct `.id`) so that,
        // with both sections open, scrolling one does not affect the other.
        let mut elements = vec![header];
        if let Some(body) = body {
            elements.push(
                div()
                    .id(scroll_id)
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .px(px(8.0))
                    .py(px(10.0))
                    .child(body)
                    .into_any_element(),
            );
        }
        elements
    }

    fn render_workspace_files_tree(
        &self,
        theme: &Theme,
        strings: &I18nStrings,
        editor: &WeakEntity<Editor>,
    ) -> AnyElement {
        if self.workspace.root.is_none() {
            return self.render_workspace_empty_state(
                &strings.workspace_no_file_title,
                &strings.workspace_no_file_message,
                theme,
            );
        }

        if let Some(error) = self.workspace.file_error.as_ref() {
            return self.render_workspace_empty_state(
                &strings.workspace_scan_failed_title,
                error,
                theme,
            );
        }

        let Some(root) = self.workspace.file_tree.as_ref() else {
            return self.render_workspace_empty_state("", &strings.workspace_empty_files, theme);
        };

        let parent_row = self
            .workspace
            .root
            .as_deref()
            .and_then(Path::parent)
            .map(|_| self.render_workspace_parent_row(theme, editor));

        div()
            .w_full()
            .flex()
            .flex_col()
            .children(parent_row)
            .children(self.render_workspace_nodes(
                std::slice::from_ref(root),
                0,
                WorkspaceSection::Files,
                theme,
                editor,
            ))
            .into_any_element()
    }

    /// The synthetic "navigate to parent directory" row prepended above the
    /// scanned tree. Deliberately not a `WorkspaceTreeKind` variant and not
    /// part of the scanned tree, so it stays out of the `expanded`/
    /// `selected` id namespaces.
    fn render_workspace_parent_row(
        &self,
        theme: &Theme,
        editor: &WeakEntity<Editor>,
    ) -> AnyElement {
        let c = &theme.colors;
        let t = &theme.typography;
        let click_editor = editor.clone();
        let selected = self.workspace.keyboard_section == Some(WorkspaceSection::Files)
            && self.workspace.keyboard_file_node.as_deref() == Some(WORKSPACE_PARENT_NODE_ID);
        let label_color = if selected {
            c.text_default
        } else {
            c.dialog_muted
        };

        div()
            .id("workspace-parent-row")
            .h(px(WORKSPACE_NODE_HEIGHT))
            .w_full()
            .overflow_hidden()
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(8.0))
            .pr(px(8.0))
            .rounded(px(6.0))
            .bg(if selected {
                c.selection
            } else {
                hsla(0.0, 0.0, 0.0, 0.0)
            })
            .hover(|this| this.bg(c.dialog_secondary_button_hover))
            .cursor_pointer()
            .child(div().w(px(14.0)).h(px(18.0)).flex_shrink_0())
            .child(
                svg()
                    .path(FOLDER_ICON)
                    .size(px(16.0))
                    .flex_shrink_0()
                    .text_color(c.dialog_muted)
                    .into_any_element(),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .truncate()
                    .text_size(px(t.text_size * 0.9))
                    .line_height(px(t.text_size * t.text_line_height))
                    .text_color(label_color)
                    .child(".."),
            )
            .on_click(move |_event, window, cx| {
                let _ = click_editor.update(cx, |editor, cx| {
                    editor.focus_workspace_node(
                        WorkspaceSection::Files,
                        WORKSPACE_PARENT_NODE_ID.to_string(),
                        window,
                        cx,
                    );
                    editor.navigate_workspace_root_up(cx);
                    editor.workspace.keyboard_file_node = None;
                    editor.focus_workspace_section(WorkspaceSection::Files, window, cx);
                    editor.retain_workspace_keyboard_focus(window, cx);
                });
            })
            .into_any_element()
    }

    fn render_workspace_outline_tree(
        &self,
        theme: &Theme,
        strings: &I18nStrings,
        editor: &WeakEntity<Editor>,
    ) -> AnyElement {
        if self.workspace.outline_tree.is_empty() {
            return self.render_workspace_empty_state("", &strings.workspace_empty_outline, theme);
        }

        div()
            .w_full()
            .flex()
            .flex_col()
            .children(self.render_workspace_nodes(
                &self.workspace.outline_tree,
                0,
                WorkspaceSection::Outline,
                theme,
                editor,
            ))
            .into_any_element()
    }

    fn render_workspace_empty_state(
        &self,
        title: &str,
        message: &str,
        theme: &Theme,
    ) -> AnyElement {
        let c = &theme.colors;
        let t = &theme.typography;
        let title = (!title.is_empty()).then(|| {
            div()
                .text_size(px(t.text_size))
                .font_weight(FontWeight::MEDIUM)
                .text_color(c.text_default)
                .child(title.to_string())
        });

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .px(px(22.0))
            .text_align(TextAlign::Center)
            .children(title)
            .child(
                div()
                    .text_size(px(t.text_size * 0.9))
                    .line_height(px(t.text_size * t.text_line_height))
                    .text_color(c.dialog_muted)
                    .child(message.to_string()),
            )
            .into_any_element()
    }

    fn render_workspace_nodes(
        &self,
        nodes: &[WorkspaceTreeNode],
        depth: usize,
        section: WorkspaceSection,
        theme: &Theme,
        editor: &WeakEntity<Editor>,
    ) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        for node in nodes {
            elements.push(self.render_workspace_node(node, depth, section, theme, editor));
            if !node.children.is_empty() && self.workspace.expanded.contains(&node.id) {
                elements.extend(self.render_workspace_nodes(
                    &node.children,
                    depth + 1,
                    section,
                    theme,
                    editor,
                ));
            }
        }
        elements
    }

    fn render_workspace_node(
        &self,
        node: &WorkspaceTreeNode,
        depth: usize,
        section: WorkspaceSection,
        theme: &Theme,
        editor: &WeakEntity<Editor>,
    ) -> AnyElement {
        let c = &theme.colors;
        let t = &theme.typography;
        let is_expanded = self.workspace.expanded.contains(&node.id);
        // An unloaded directory has no children yet but must still show an
        // arrow, or it would render permanently unexpandable. A directory
        // confirmed empty after loading correctly loses its arrow.
        let expandable = match &node.kind {
            WorkspaceTreeKind::Directory(_) => !node.children_loaded || !node.children.is_empty(),
            _ => !node.children.is_empty(),
        };
        let keyboard_selected = self.workspace.keyboard_section == Some(section)
            && match section {
                WorkspaceSection::Files => {
                    self.workspace.keyboard_file_node.as_deref() == Some(node.id.as_str())
                }
                WorkspaceSection::Outline => {
                    self.workspace.keyboard_outline_node.as_deref() == Some(node.id.as_str())
                }
            };
        let selected = keyboard_selected
            || match &node.kind {
            WorkspaceTreeKind::MarkdownFile(path) => {
                self.workspace.selected_file.as_deref() == Some(path.as_path())
            }
            WorkspaceTreeKind::Heading { .. } => {
                self.workspace.selected_outline.as_deref() == Some(node.id.as_str())
            }
            WorkspaceTreeKind::Directory(_) => false,
        };
        let externally_changed = match &node.kind {
            WorkspaceTreeKind::MarkdownFile(path) => {
                self.workspace_watch.changed_files.contains(path)
            }
            _ => false,
        };
        let node_id = node.id.clone();
        let click_editor = editor.clone();
        let click_kind = node.kind.clone();
        let context_menu_editor = editor.clone();
        let context_menu_entry = workspace_entry_context_menu_target(&node.kind);
        let arrow_node_id = node.id.clone();
        let arrow_dir = match &node.kind {
            WorkspaceTreeKind::Directory(path) => Some(path.clone()),
            _ => None,
        };
        let arrow_editor = editor.clone();
        let arrow = if expandable {
            if is_expanded { "v" } else { ">" }
        } else {
            ""
        };

        let icon = match &node.kind {
            WorkspaceTreeKind::Directory(_) => Some((FOLDER_ICON, Hsla::from(rgba(0xf59e0bff)))),
            WorkspaceTreeKind::MarkdownFile(_) => {
                Some((MARKDOWN_ICON, Hsla::from(rgba(0x2563ebff))))
            }
            WorkspaceTreeKind::Heading { .. } => None,
        };

        let label_color = if selected {
            c.text_default
        } else {
            c.dialog_muted
        };

        let mut arrow_el = div()
            .w(px(14.0))
            .h(px(18.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.0))
            .text_color(c.dialog_muted)
            .child(arrow);
        if expandable {
            arrow_el = arrow_el.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                move |_event, window, cx| {
                    let _ = arrow_editor.update(cx, |editor, cx| {
                        editor.focus_workspace_node(section, arrow_node_id.clone(), window, cx);
                        editor.toggle_workspace_node(&arrow_node_id, arrow_dir.as_deref(), cx);
                        editor.retain_workspace_keyboard_focus(window, cx);
                    });
                    cx.stop_propagation();
                },
            );
        }

        div()
            .id(("workspace-node", stable_node_hash(&node.id)))
            .h(px(WORKSPACE_NODE_HEIGHT))
            .w_full()
            .overflow_hidden()
            .flex()
            .items_center()
            .gap(px(6.0))
            .pl(px(8.0 + depth as f32 * WORKSPACE_NODE_INDENT))
            .pr(px(8.0))
            .rounded(px(6.0))
            .bg(if selected {
                c.selection
            } else {
                hsla(0.0, 0.0, 0.0, 0.0)
            })
            .hover(|this| this.bg(c.dialog_secondary_button_hover))
            .cursor_pointer()
            .child(arrow_el)
            .children(icon.map(|(path, color)| {
                svg()
                    .path(path)
                    .size(px(16.0))
                    .flex_shrink_0()
                    .text_color(color)
                    .into_any_element()
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .truncate()
                    .text_size(px(t.text_size * 0.9))
                    .line_height(px(t.text_size * t.text_line_height))
                    .text_color(label_color)
                    .child(node.label.clone()),
            )
            .children(externally_changed.then(|| {
                div()
                    .w(px(6.0))
                    .h(px(6.0))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(c.dialog_primary_button_bg)
                    .into_any_element()
            }))
            .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                let Some((path, is_directory)) = context_menu_entry.clone() else {
                    return;
                };
                let _ = context_menu_editor.update(cx, |editor, cx| {
                    editor.on_workspace_entry_context_menu_mouse_down(
                        path,
                        is_directory,
                        event,
                        window,
                        cx,
                    );
                });
            })
            .on_click(move |_event, window, cx| {
                let node_id = node_id.clone();
                let click_kind = click_kind.clone();
                let _ = click_editor.update(cx, |editor, cx| {
                    editor.focus_workspace_node(section, node_id.clone(), window, cx);
                    match click_kind {
                        WorkspaceTreeKind::Directory(path) => {
                            if expandable {
                                editor.toggle_workspace_node(&node_id, Some(&path), cx);
                            }
                        }
                        WorkspaceTreeKind::MarkdownFile(path) => {
                            editor.open_workspace_file(path, window, cx);
                        }
                        WorkspaceTreeKind::Heading { line, .. } => {
                            editor.select_outline_node(node_id, cx);
                            editor.jump_to_outline_heading(line, cx);
                        }
                    }
                    editor.retain_workspace_keyboard_focus(window, cx);
                });
            })
            .into_any_element()
    }
}

/// Maps a Files-tree row to the `(path, is_directory)` its right-click
/// context menu needs. An outline heading has no filesystem path, so its
/// right-click must not open a menu meant for file operations — `None`
/// tells the caller to treat the click as a no-op.
fn workspace_entry_context_menu_target(kind: &WorkspaceTreeKind) -> Option<(PathBuf, bool)> {
    match kind {
        WorkspaceTreeKind::Directory(path) => Some((path.clone(), true)),
        WorkspaceTreeKind::MarkdownFile(path) => Some((path.clone(), false)),
        WorkspaceTreeKind::Heading { .. } => None,
    }
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("md"))
}

/// Scans `path` one level deep for the Files sidebar: the returned root is
/// loaded, each child directory is not (its own children are fetched lazily
/// on expand — see `load_workspace_dir_children`), so a caller must not
/// assume this reflects anything below the immediate children of `path`.
fn scan_workspace_dir(path: &Path) -> Result<WorkspaceTreeNode> {
    let children = read_dir_one_level(path)?;
    Ok(WorkspaceTreeNode {
        id: file_node_id(path),
        label: file_label(path),
        kind: WorkspaceTreeKind::Directory(path.to_path_buf()),
        children,
        children_loaded: true,
    })
}

/// Skip noise directories that are huge and never contain documents the user
/// is looking for: dotdirs (`.git`, `.venv`, ...) plus common build/dep dirs.
fn should_skip_scan_dir(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        name.starts_with('.') || WORKSPACE_SCAN_SKIP_DIRS.contains(&name.as_ref())
    })
}

/// Reads `path`'s immediate children — subdirectories (unloaded) and
/// Markdown files — sorted directories-first then case-insensitively by
/// label. A failure reading `path` itself propagates; a per-entry
/// `file_type()` failure just skips that entry.
fn read_dir_one_level(path: &Path) -> Result<Vec<WorkspaceTreeNode>> {
    let entries =
        fs::read_dir(path).with_context(|| format!("failed to read '{}'", path.display()))?;

    let mut children = Vec::new();
    for entry in entries {
        if children.len() >= WORKSPACE_SCAN_MAX_ENTRIES_PER_DIR {
            break;
        }
        let Ok(entry) = entry else { continue };
        let entry_path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            if should_skip_scan_dir(&entry_path) {
                continue;
            }
            children.push(WorkspaceTreeNode {
                id: file_node_id(&entry_path),
                label: file_label(&entry_path),
                kind: WorkspaceTreeKind::Directory(entry_path),
                children: Vec::new(),
                children_loaded: false,
            });
        } else if file_type.is_file() && is_markdown_file(&entry_path) {
            children.push(WorkspaceTreeNode {
                id: file_node_id(&entry_path),
                label: file_label(&entry_path),
                kind: WorkspaceTreeKind::MarkdownFile(entry_path),
                children: Vec::new(),
                children_loaded: true,
            });
        }
    }

    children.sort_by(|left, right| {
        let left_dir = matches!(left.kind, WorkspaceTreeKind::Directory(_));
        let right_dir = matches!(right.kind, WorkspaceTreeKind::Directory(_));
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.label.to_lowercase().cmp(&right.label.to_lowercase()))
    });

    Ok(children)
}

/// Locates the directory node for `dir` inside `node`'s subtree and loads
/// its children if not already loaded, then restores any previously-
/// expanded descendants underneath it. Pruned to an O(depth) walk: only
/// descends into a directory whose path is a prefix of `dir`.
fn find_and_load_dir(node: &mut WorkspaceTreeNode, dir: &Path, expanded: &HashSet<String>) {
    let WorkspaceTreeKind::Directory(path) = &node.kind else {
        return;
    };
    if path == dir {
        if !node.children_loaded {
            // A read failure leaves the node loaded-but-empty rather than
            // propagating, so a permanently unreadable directory does not
            // retry on every expand. This is a nested failure, not the root,
            // so `workspace.file_error` is untouched.
            node.children = read_dir_one_level(dir).unwrap_or_default();
            node.children_loaded = true;
        }
        restore_expanded_descendants(node, expanded);
        return;
    }
    if !dir.starts_with(path) {
        return;
    }
    for child in &mut node.children {
        find_and_load_dir(child, dir, expanded);
    }
}

/// `expanded` persists across root changes and re-scans, so a directory
/// whose id is still in `expanded` but whose children were never (re-)read
/// would render expanded-yet-empty. After reading any level, walk its child
/// directories and load the ones still marked expanded, recursively.
/// Symlinked directories are already excluded upstream (`file_type()` does
/// not follow symlinks), so this cannot loop.
fn restore_expanded_descendants(node: &mut WorkspaceTreeNode, expanded: &HashSet<String>) {
    for child in &mut node.children {
        if !expanded.contains(&child.id) {
            continue;
        }
        let WorkspaceTreeKind::Directory(path) = &child.kind else {
            continue;
        };
        if !child.children_loaded {
            child.children = read_dir_one_level(path).unwrap_or_default();
            child.children_loaded = true;
        }
        restore_expanded_descendants(child, expanded);
    }
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn file_node_id(path: &Path) -> String {
    format!("file:{}", path.to_string_lossy())
}

fn stable_node_hash(id: &str) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    hasher.finish()
}

pub(super) fn workspace_panel_width_for_viewport(viewport_width: f32) -> f32 {
    let target = viewport_width * WORKSPACE_PANEL_TARGET_RATIO;
    target.clamp(WORKSPACE_PANEL_MIN_WIDTH, WORKSPACE_PANEL_MAX_WIDTH)
}

/// Upper bound for a manually dragged width: never more than 80% of the
/// viewport, so the sidebar cannot crowd out the editor on a small window.
pub(super) fn workspace_panel_drag_max_width(viewport_width: f32) -> f32 {
    (viewport_width * 0.8)
        .min(WORKSPACE_PANEL_DRAG_MAX_WIDTH)
        .max(WORKSPACE_PANEL_DRAG_MIN_WIDTH)
}

/// Effective panel width. A dragged width is re-clamped against the current
/// viewport on every read, so shrinking the window cannot leave a previously
/// dragged sidebar wider than the window allows.
fn resolve_workspace_panel_width(stored_width: Option<f32>, viewport_width: f32) -> f32 {
    stored_width
        .unwrap_or_else(|| workspace_panel_width_for_viewport(viewport_width))
        .clamp(
            WORKSPACE_PANEL_DRAG_MIN_WIDTH,
            workspace_panel_drag_max_width(viewport_width),
        )
}

fn prune_outline_state(workspace: &mut WorkspaceState, outline: &[WorkspaceTreeNode]) {
    let mut current_ids = HashSet::new();
    collect_node_ids(outline, &mut current_ids);
    workspace
        .expanded
        .retain(|id| !is_outline_node_id(id) || current_ids.contains(id));

    // Only the outline half of selection can go stale here; a stale outline
    // heading must not clear an unrelated selected file.
    if workspace
        .selected_outline
        .as_deref()
        .is_some_and(|id| !current_ids.contains(id))
    {
        workspace.selected_outline = None;
    }
}

fn toggle_workspace_section_state(workspace: &mut WorkspaceState, section: WorkspaceSection) {
    match section {
        WorkspaceSection::Files => {
            workspace.files_section_open = !workspace.files_section_open;
        }
        WorkspaceSection::Outline => {
            workspace.outline_section_open = !workspace.outline_section_open;
        }
    }
}

fn collect_node_ids(nodes: &[WorkspaceTreeNode], ids: &mut HashSet<String>) {
    for node in nodes {
        ids.insert(node.id.clone());
        collect_node_ids(&node.children, ids);
    }
}

fn collect_watch_targets(
    node: &WorkspaceTreeNode,
    dirs: &mut HashSet<PathBuf>,
    files: &mut HashSet<PathBuf>,
) {
    match &node.kind {
        WorkspaceTreeKind::Directory(path) => {
            dirs.insert(path.clone());
            if node.children_loaded {
                for child in &node.children {
                    collect_watch_targets(child, dirs, files);
                }
            }
        }
        WorkspaceTreeKind::MarkdownFile(path) => {
            files.insert(path.clone());
        }
        WorkspaceTreeKind::Heading { .. } => {}
    }
}

fn collect_workspace_visible_rows(
    nodes: &[WorkspaceTreeNode],
    expanded: &HashSet<String>,
    parent_id: Option<&str>,
    rows: &mut Vec<WorkspaceVisibleRow>,
) {
    for node in nodes {
        let is_expanded = expanded.contains(&node.id);
        let expandable = match &node.kind {
            WorkspaceTreeKind::Directory(_) => !node.children_loaded || !node.children.is_empty(),
            _ => !node.children.is_empty(),
        };
        rows.push(WorkspaceVisibleRow {
            id: node.id.clone(),
            parent_id: parent_id.map(str::to_string),
            kind: node.kind.clone(),
            expanded: is_expanded,
            expandable,
        });
        if is_expanded && !node.children.is_empty() {
            collect_workspace_visible_rows(
                &node.children,
                expanded,
                Some(node.id.as_str()),
                rows,
            );
        }
    }
}

fn is_outline_node_id(id: &str) -> bool {
    id.starts_with("outline:")
}

fn build_outline_tree(markdown: &str) -> Vec<WorkspaceTreeNode> {
    let mut roots = Vec::new();
    let mut stack: Vec<(u8, Vec<usize>)> = Vec::new();
    let mut fence: Option<(char, usize)> = None;

    for (line_index, line) in markdown.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some((marker, len)) = fence {
            if is_closing_fence(trimmed, marker, len) {
                fence = None;
            }
            continue;
        }

        if let Some(next_fence) = opening_fence(trimmed) {
            fence = Some(next_fence);
            continue;
        }

        let Some((level, title)) = BlockKind::parse_atx_heading_line(line) else {
            continue;
        };

        while stack
            .last()
            .is_some_and(|(parent_level, _)| *parent_level >= level)
        {
            stack.pop();
        }

        let node = WorkspaceTreeNode {
            id: format!("outline:{line_index}"),
            label: InlineTextTree::from_markdown(&title).plain_label(),
            kind: WorkspaceTreeKind::Heading {
                line: line_index,
                level,
            },
            children: Vec::new(),
            children_loaded: true,
        };

        let siblings = if let Some((_, parent_path)) = stack.last() {
            children_at_path_mut(&mut roots, parent_path)
        } else {
            &mut roots
        };
        siblings.push(node);

        let mut node_path = stack
            .last()
            .map(|(_, path)| path.clone())
            .unwrap_or_default();
        node_path.push(siblings.len() - 1);
        stack.push((level, node_path));
    }

    roots
}

fn children_at_path_mut<'a>(
    nodes: &'a mut Vec<WorkspaceTreeNode>,
    path: &[usize],
) -> &'a mut Vec<WorkspaceTreeNode> {
    let mut current = nodes;
    for &index in path {
        current = &mut current[index].children;
    }
    current
}

fn opening_fence(trimmed: &str) -> Option<(char, usize)> {
    let marker = trimmed.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let len = trimmed.chars().take_while(|ch| *ch == marker).count();
    (len >= 3).then_some((marker, len))
}

fn is_closing_fence(trimmed: &str, marker: char, len: usize) -> bool {
    let count = trimmed.chars().take_while(|ch| *ch == marker).count();
    count >= len && trimmed[count..].trim().is_empty()
}

/// Byte offset where 0-based `line` starts in `source`. Returns `None` when
/// the line is past the end.
///
/// `split_inclusive('\n')` keeps line terminators, so the running sum lands
/// on real byte boundaries; the index sequence matches the `lines()`
/// enumeration `build_outline_tree` used to record the heading's line.
fn line_start_offset(source: &str, line: usize) -> Option<usize> {
    if line == 0 {
        return Some(0);
    }
    let mut offset = 0;
    for (index, text) in source.split_inclusive('\n').enumerate() {
        if index == line {
            return Some(offset);
        }
        offset += text.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        Editor, WORKSPACE_PARENT_NODE_ID, WORKSPACE_SCAN_MAX_ENTRIES_PER_DIR, WorkspaceSection,
        WorkspaceState, WorkspaceTreeKind, build_outline_tree, find_and_load_dir,
        line_start_offset, prune_outline_state, read_dir_one_level,
        resolve_workspace_panel_width, restore_expanded_descendants, scan_workspace_dir,
        toggle_workspace_section_state, workspace_entry_context_menu_target,
        workspace_panel_drag_max_width, workspace_panel_width_for_viewport,
    };
    use gpui::{AppContext, TestAppContext};
    use std::collections::HashSet;
    use std::fs;
    use std::path::PathBuf;

    #[gpui::test]
    async fn outline_cache_refreshes_when_a_heading_changes(cx: &mut TestAppContext) {
        let editor = cx.new(|cx| Editor::from_markdown(cx, "# First\n\nbody".to_string(), None));

        editor.update(cx, |editor, cx| {
            editor.sync_workspace_models(cx);
            assert_eq!(
                editor.workspace.outline_tree[0].label, "First",
                "outline should build on first sync"
            );

            // A second sync with nothing changed must reuse the cache, which is
            // the whole point of the fingerprint guard.
            let fingerprint = editor.workspace.outline_fingerprint;
            editor.sync_workspace_models(cx);
            assert_eq!(editor.workspace.outline_fingerprint, fingerprint);

            // Editing the heading must invalidate it: a stale outline is the
            // failure mode a content fingerprint can introduce.
            let heading = editor.document.root_blocks()[0].clone();
            heading.update(cx, |block, _| {
                block
                    .record
                    .set_title(crate::components::InlineTextTree::plain("Renamed"));
            });
            editor.sync_workspace_models(cx);
            assert_eq!(editor.workspace.outline_tree[0].label, "Renamed");
            assert_ne!(editor.workspace.outline_fingerprint, fingerprint);
        });
    }

    /// Mirrors `editor::tests::init_editor_test_app`, which lives in a file
    /// this task may not edit: installs the globals `Editor::from_markdown`
    /// needs before a window can be created in a test.
    fn init_editor_test_app(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            crate::i18n::I18nManager::init(cx);
            crate::theme::ThemeManager::init(cx);
            crate::components::init(cx);
        });
    }

    #[test]
    fn workspace_entry_context_menu_target_maps_files_and_dirs_but_not_headings() {
        let file = PathBuf::from("/workspace/notes.md");
        let dir = PathBuf::from("/workspace/nested");

        assert_eq!(
            workspace_entry_context_menu_target(&WorkspaceTreeKind::MarkdownFile(file.clone())),
            Some((file, false))
        );
        assert_eq!(
            workspace_entry_context_menu_target(&WorkspaceTreeKind::Directory(dir.clone())),
            Some((dir, true))
        );
        // The outline tree reuses `render_workspace_node`; a heading row must
        // not open a menu meant for filesystem operations.
        assert_eq!(
            workspace_entry_context_menu_target(&WorkspaceTreeKind::Heading { line: 0, level: 1 }),
            None
        );
    }

    #[test]
    fn workspace_scan_keeps_dirs_and_md_files_only() {
        let root =
            std::env::temp_dir().join(format!("velotype-workspace-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("nested")).expect("create dirs");
        fs::write(root.join("a.md"), "a").expect("write md");
        fs::write(root.join("a.txt"), "ignored").expect("write txt");
        fs::write(root.join("nested").join("b.md"), "b").expect("write nested md");

        let tree = scan_workspace_dir(&root).expect("scan tree");
        let labels = tree
            .children
            .iter()
            .map(|node| node.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["nested", "a.md"]);
        assert!(matches!(
            tree.children[0].kind,
            WorkspaceTreeKind::Directory(_)
        ));
        assert!(matches!(
            tree.children[1].kind,
            WorkspaceTreeKind::MarkdownFile(_)
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_level_read_never_exceeds_the_per_level_entry_cap() {
        // Lazy loading removes the whole-tree budget that used to underflow
        // on a descent spending its last unit (the regression this test
        // used to guard). What replaces it is a per-level cap; a fixture big
        // enough to hit `WORKSPACE_SCAN_MAX_ENTRIES_PER_DIR` for real would
        // be far too slow for a unit test, so this only exercises the
        // invariant that a level's child count is bounded by the constant,
        // not the boundary itself.
        let root =
            std::env::temp_dir().join(format!("velotype-scan-level-cap-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("create dir");
        for i in 0..10 {
            fs::write(root.join(format!("f{i}.md")), "x").expect("write md");
        }

        let children = read_dir_one_level(&root).expect("read level");
        assert_eq!(children.len(), 10);
        assert!(children.len() <= WORKSPACE_SCAN_MAX_ENTRIES_PER_DIR);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_workspace_dir_loads_only_the_requested_level() {
        let root =
            std::env::temp_dir().join(format!("velotype-scan-one-level-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("nested")).expect("create dirs");
        fs::write(root.join("nested").join("deep.md"), "x").expect("write nested md");
        fs::write(root.join("a.md"), "x").expect("write md");

        let tree = scan_workspace_dir(&root).expect("scan tree");
        assert!(tree.children_loaded);

        let nested = tree
            .children
            .iter()
            .find(|node| node.label == "nested")
            .expect("nested dir present");
        assert!(matches!(nested.kind, WorkspaceTreeKind::Directory(_)));
        assert!(!nested.children_loaded);
        assert!(nested.children.is_empty());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn find_and_load_dir_populates_only_the_target_directory() {
        let root =
            std::env::temp_dir().join(format!("velotype-find-load-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("nested").join("deeper")).expect("create dirs");
        fs::write(root.join("nested").join("deep.md"), "x").expect("write nested md");
        fs::write(root.join("a.md"), "x").expect("write md");

        let mut tree = scan_workspace_dir(&root).expect("scan tree");
        let nested_path = root.join("nested");
        let expanded = HashSet::new();

        find_and_load_dir(&mut tree, &nested_path, &expanded);

        let nested = tree
            .children
            .iter()
            .find(|node| node.label == "nested")
            .expect("nested dir present");
        assert!(nested.children_loaded);
        let labels: Vec<&str> = nested
            .children
            .iter()
            .map(|node| node.label.as_str())
            .collect();
        assert_eq!(labels, vec!["deeper", "deep.md"]);

        // Loading "nested" must not have loaded its own child directory.
        let deeper = nested
            .children
            .iter()
            .find(|node| node.label == "deeper")
            .expect("deeper dir present");
        assert!(!deeper.children_loaded);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_expanded_descendants_loads_a_child_whose_id_was_already_expanded() {
        // Simulates a re-scan: `expanded` still names a directory from
        // before the scan, but the fresh tree has not read it yet. Without
        // this restore step it would render expanded-yet-empty.
        let root = std::env::temp_dir().join(format!(
            "velotype-restore-expanded-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(root.join("nested")).expect("create dirs");
        fs::write(root.join("nested").join("deep.md"), "x").expect("write nested md");

        let mut tree = scan_workspace_dir(&root).expect("scan tree");
        let nested_id = tree
            .children
            .iter()
            .find(|node| node.label == "nested")
            .expect("nested dir present")
            .id
            .clone();
        assert!(
            !tree.children[0].children_loaded,
            "fresh scan should not have loaded nested yet"
        );

        let mut expanded = HashSet::new();
        expanded.insert(nested_id.clone());
        restore_expanded_descendants(&mut tree, &expanded);

        let nested = tree
            .children
            .iter()
            .find(|node| node.id == nested_id)
            .expect("nested dir present");
        assert!(nested.children_loaded);
        assert_eq!(nested.children.len(), 1);
        assert_eq!(nested.children[0].label, "deep.md");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn workspace_scan_skips_noise_dirs_but_keeps_real_files() {
        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-skip-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(root.join("node_modules")).expect("create dir");
        fs::write(root.join("node_modules").join("pkg.md"), "x").expect("write md");
        fs::create_dir_all(root.join(".git")).expect("create dir");
        fs::write(root.join(".git").join("hidden.md"), "x").expect("write md");
        fs::write(root.join("notes.md"), "keep").expect("write md");

        let tree = scan_workspace_dir(&root).expect("scan tree");
        let labels: Vec<&str> = tree
            .children
            .iter()
            .map(|node| node.label.as_str())
            .collect();

        assert!(!labels.contains(&"node_modules"));
        assert!(!labels.contains(&".git"));
        assert!(labels.contains(&"notes.md"));

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_scan_skips_unreadable_subdirectories_without_failing() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-perm-test-{}",
            uuid::Uuid::new_v4()
        ));
        let denied = root.join("denied");
        fs::create_dir_all(&denied).expect("create dirs");
        fs::write(root.join("notes.md"), "keep").expect("write md");

        fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).expect("chmod");

        let result = scan_workspace_dir(&root);

        // Restore before cleanup regardless of outcome, or `remove_dir_all`
        // cannot descend into `denied`.
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o755)).expect("restore perms");

        let tree = result.expect("an unreadable subdir must not abort the scan");
        let labels: Vec<&str> = tree
            .children
            .iter()
            .map(|node| node.label.as_str())
            .collect();
        assert!(labels.contains(&"notes.md"));
        assert!(labels.contains(&"denied"));

        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn load_workspace_dir_children_on_an_unreadable_dir_marks_it_loaded_without_erroring(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::os::unix::fs::PermissionsExt;

        init_editor_test_app(cx);
        let root = std::env::temp_dir().join(format!(
            "velotype-load-denied-test-{}",
            uuid::Uuid::new_v4()
        ));
        let denied = root.join("denied");
        fs::create_dir_all(&denied).expect("create dirs");
        fs::write(root.join("notes.md"), "keep").expect("write md");

        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".to_string(), None));
        editor.update_in(cx, |editor, window, cx| {
            editor.toggle_workspace_drawer(window, cx);
        });
        editor.update(cx, |editor, cx| {
            editor.set_workspace_root_override(root.clone(), cx);
        });

        fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).expect("chmod");
        editor.update(cx, |editor, _cx| {
            editor.load_workspace_dir_children(&denied);
        });
        // Restore before cleanup regardless of outcome, or `remove_dir_all`
        // cannot descend into `denied`.
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o755)).expect("restore perms");

        editor.read_with(cx, |editor, _cx| {
            // A nested failure is not the root's failure.
            assert_eq!(editor.workspace.file_error, None);
            let tree = editor.workspace.file_tree.as_ref().expect("tree synced");
            let denied_node = tree
                .children
                .iter()
                .find(|node| node.label == "denied")
                .expect("denied dir present");
            assert!(denied_node.children_loaded);
            assert!(denied_node.children.is_empty());
        });

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn outline_tree_skips_headings_inside_fenced_code() {
        let outline = build_outline_tree(
            "# Root\n\n```md\n# ignored\n```\n\n## Child\n\n### Grandchild\n\n# Next",
        );

        assert_eq!(outline.len(), 2);
        assert_eq!(outline[0].label, "Root");
        assert_eq!(outline[0].children[0].label, "Child");
        assert_eq!(outline[0].children[0].children[0].label, "Grandchild");
        assert_eq!(outline[1].label, "Next");
    }

    #[test]
    fn outline_tree_renders_a_leading_icon_heading_as_plain_text() {
        // A common README pattern: an inline `<img>` icon in front of a link
        // title. The raw heading text is not readable chrome; the outline
        // label must flatten it down to plain words.
        let outline = build_outline_tree(
            "# <img alt=\"Smithy\" src=\"./anvil.png\" width=\"32\"> [Smithy Plugin](https://example.com/p)",
        );

        assert_eq!(outline.len(), 1);
        assert_eq!(outline[0].label, "Smithy Plugin");
    }

    #[test]
    fn outline_expansion_state_is_not_auto_populated_and_prunes_stale_ids() {
        let outline = build_outline_tree("# Root\n\n## Child\n\n# Next");
        let mut fresh = WorkspaceState::default();
        prune_outline_state(&mut fresh, &outline);
        assert!(fresh.expanded.is_empty());

        let mut existing = WorkspaceState::default();
        existing.expanded.insert("outline:0".to_string());
        existing.expanded.insert("outline:999".to_string());
        existing
            .expanded
            .insert("workspace-dir:C:/docs".to_string());
        existing.selected_outline = Some("outline:999".to_string());

        prune_outline_state(&mut existing, &outline);

        assert!(existing.expanded.contains("outline:0"));
        assert!(existing.expanded.contains("workspace-dir:C:/docs"));
        assert!(!existing.expanded.contains("outline:999"));
        assert_eq!(existing.selected_outline, None);
    }

    #[test]
    fn prune_outline_state_clears_stale_outline_selection_but_keeps_file_selection() {
        let outline = build_outline_tree("# Root\n\n## Child\n\n# Next");
        let state = &mut WorkspaceState {
            selected_outline: Some("outline:999".to_string()),
            selected_file: Some(PathBuf::from("/tmp/keep-me.md")),
            ..WorkspaceState::default()
        };

        prune_outline_state(state, &outline);

        assert_eq!(state.selected_outline, None);
        assert_eq!(state.selected_file, Some(PathBuf::from("/tmp/keep-me.md")));
    }

    #[test]
    fn workspace_state_default_has_both_sections_open() {
        let state = WorkspaceState::default();
        assert!(state.files_section_open);
        assert!(state.outline_section_open);
    }

    #[test]
    fn toggling_a_section_flips_only_that_one() {
        let mut state = WorkspaceState::default();

        toggle_workspace_section_state(&mut state, WorkspaceSection::Files);

        assert!(!state.files_section_open);
        assert!(state.outline_section_open);
    }

    #[test]
    fn both_sections_can_be_collapsed_simultaneously() {
        let mut state = WorkspaceState::default();

        toggle_workspace_section_state(&mut state, WorkspaceSection::Files);
        toggle_workspace_section_state(&mut state, WorkspaceSection::Outline);

        assert!(!state.files_section_open);
        assert!(!state.outline_section_open);
    }

    #[test]
    fn workspace_panel_width_uses_ratio_with_bounds() {
        assert_eq!(workspace_panel_width_for_viewport(1000.0), 240.0);
        assert_eq!(workspace_panel_width_for_viewport(2000.0), 300.0);
        assert_eq!(workspace_panel_width_for_viewport(4000.0), 360.0);
    }

    #[test]
    fn drag_max_width_is_bounded_by_viewport_share_and_constant() {
        // 80% of the viewport while that is the smallest bound.
        assert_eq!(workspace_panel_drag_max_width(500.0), 400.0);
        // The absolute cap wins on a wide window.
        assert_eq!(workspace_panel_drag_max_width(4000.0), 720.0);
        // The floor wins on a very narrow window, so the max never falls
        // below the min and `clamp` cannot panic.
        assert_eq!(workspace_panel_drag_max_width(100.0), 180.0);
    }

    #[test]
    fn undragged_width_tracks_the_viewport() {
        assert_eq!(
            resolve_workspace_panel_width(None, 2000.0),
            workspace_panel_width_for_viewport(2000.0)
        );
    }

    #[test]
    fn dragged_width_is_kept_verbatim_when_it_fits() {
        assert_eq!(resolve_workspace_panel_width(Some(480.0), 2000.0), 480.0);
    }

    #[test]
    fn dragged_width_is_reclamped_when_the_window_shrinks() {
        // Dragged wide on a large window, then the window shrank: the stored
        // width must yield to the smaller viewport instead of crowding out
        // the editor.
        assert_eq!(resolve_workspace_panel_width(Some(700.0), 600.0), 480.0);
        // And it is never squeezed below the drag floor.
        assert_eq!(resolve_workspace_panel_width(Some(200.0), 100.0), 180.0);
    }

    #[gpui::test]
    async fn section_state_seeds_from_defaults_without_the_settings_global(
        cx: &mut gpui::TestAppContext,
    ) {
        // No EditorSettings global is installed here, which is exactly the
        // situation every test runs in. Seeding must fall back to the defaults
        // rather than reaching for the real config file on disk, otherwise
        // tests would depend on the developer's own saved layout.
        let state = cx.update(|cx| WorkspaceState::from_settings(cx));

        assert!(!state.is_open);
        assert!(state.files_section_open);
        assert!(state.outline_section_open);
    }

    #[gpui::test]
    async fn toggling_the_drawer_flips_is_open_without_disturbing_sections(
        cx: &mut gpui::TestAppContext,
    ) {
        init_editor_test_app(cx);
        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".to_string(), None));

        editor.read_with(cx, |editor, _cx| {
            assert!(!editor.workspace.is_open);
        });

        editor.update_in(cx, |editor, window, cx| {
            editor.toggle_workspace_drawer(window, cx);
        });
        editor.read_with(cx, |editor, _cx| {
            assert!(editor.workspace.is_open);
            assert!(editor.workspace.files_section_open);
            assert!(editor.workspace.outline_section_open);
        });

        editor.update_in(cx, |editor, window, cx| {
            editor.toggle_workspace_drawer(window, cx);
        });
        editor.read_with(cx, |editor, _cx| {
            assert!(!editor.workspace.is_open);
            assert!(editor.workspace.files_section_open);
            assert!(editor.workspace.outline_section_open);
        });
    }

    #[gpui::test]
    async fn workspace_root_is_none_without_override_or_open_document(
        cx: &mut gpui::TestAppContext,
    ) {
        init_editor_test_app(cx);
        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".to_string(), None));

        editor.read_with(cx, |editor, _cx| {
            assert_eq!(editor.workspace_root_for_current_file(), None);
        });
    }

    #[gpui::test]
    async fn set_workspace_root_override_points_the_sidebar_at_the_requested_dir(
        cx: &mut gpui::TestAppContext,
    ) {
        init_editor_test_app(cx);
        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-override-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create dir");
        fs::write(root.join("note.md"), "hello").expect("write md");

        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".to_string(), None));
        // The drawer must be open, otherwise `sync_workspace_models` is a
        // no-op and the tree never gets scanned.
        editor.update_in(cx, |editor, window, cx| {
            editor.toggle_workspace_drawer(window, cx);
        });

        let override_root = root.clone();
        editor.update(cx, |editor, cx| {
            editor.set_workspace_root_override(override_root, cx);
        });

        editor.read_with(cx, |editor, _cx| {
            assert_eq!(editor.workspace.root, Some(root.clone()));
            let tree = editor.workspace.file_tree.as_ref().expect("tree synced");
            assert_eq!(tree.children.len(), 1);
            assert_eq!(tree.children[0].label, "note.md");
        });

        let _ = fs::remove_dir_all(&root);
    }

    #[gpui::test]
    async fn workspace_poll_picks_up_a_file_created_outside_the_app(cx: &mut gpui::TestAppContext) {
        init_editor_test_app(cx);
        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-live-create-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create dir");
        fs::write(root.join("a.md"), "a").expect("write md");

        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".into(), None));
        editor.update_in(cx, |editor, window, cx| {
            editor.set_workspace_root_override(root.clone(), cx);
            editor.toggle_workspace_drawer(window, cx);
        });
        editor.update(cx, |editor, cx| {
            // Seed the baseline the same way the poll loop sees it.
            editor.poll_workspace_external_changes(cx);
        });

        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(root.join("b.md"), "b").expect("write new md");

        editor.update(cx, |editor, cx| {
            editor.poll_workspace_external_changes(cx);
        });
        editor.read_with(cx, |editor, _cx| {
            let tree = editor.workspace.file_tree.as_ref().expect("tree present");
            let labels: Vec<&str> = tree
                .children
                .iter()
                .map(|node| node.label.as_str())
                .collect();
            assert_eq!(labels, vec!["a.md", "b.md"]);
        });

        let _ = fs::remove_dir_all(&root);
    }

    #[gpui::test]
    async fn workspace_poll_marks_an_externally_edited_file(cx: &mut gpui::TestAppContext) {
        init_editor_test_app(cx);
        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-live-edit-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create dir");
        let note = root.join("note.md");
        fs::write(&note, "old").expect("write md");

        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".into(), None));
        editor.update_in(cx, |editor, window, cx| {
            editor.set_workspace_root_override(root.clone(), cx);
            editor.toggle_workspace_drawer(window, cx);
            editor.poll_workspace_external_changes(cx);
        });

        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&note, "new content").expect("external write");

        editor.update(cx, |editor, cx| {
            editor.poll_workspace_external_changes(cx);
        });
        editor.read_with(cx, |editor, _cx| {
            assert!(
                editor.workspace_watch.changed_files.contains(&note),
                "an externally edited row should be marked changed"
            );
        });

        let _ = fs::remove_dir_all(&root);
    }

    #[gpui::test]
    async fn files_keyboard_navigation_walks_rows_and_descends_levels(
        cx: &mut gpui::TestAppContext,
    ) {
        init_editor_test_app(cx);
        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-keyboard-files-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(root.join("nested")).expect("create dirs");
        fs::write(root.join("a.md"), "a").expect("write md");
        fs::write(root.join("nested").join("b.md"), "b").expect("write nested md");

        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".into(), None));
        editor.update_in(cx, |editor, window, cx| {
            editor.set_workspace_root_override(root.clone(), cx);
            editor.toggle_workspace_drawer(window, cx);
        });

        let root_id = format!("file:{}", root.to_string_lossy());
        let nested_id = format!("file:{}", root.join("nested").to_string_lossy());
        let nested_file_id = format!("file:{}", root.join("nested").join("b.md").to_string_lossy());

        editor.update(cx, |editor, cx| {
            editor.workspace.keyboard_section = Some(WorkspaceSection::Files);
            editor.workspace.keyboard_file_node = Some(root_id.clone());

            assert!(editor.workspace_handle_navigation_key("down", cx));
            assert_eq!(
                editor.workspace.keyboard_file_node.as_deref(),
                Some(nested_id.as_str())
            );

            // Right expands the collapsed directory and lands on its first
            // child, so one press descends a level.
            assert!(editor.workspace_handle_navigation_key("right", cx));
            assert_eq!(
                editor.workspace.keyboard_file_node.as_deref(),
                Some(nested_file_id.as_str())
            );

            // Left on a leaf ascends to its parent.
            assert!(editor.workspace_handle_navigation_key("left", cx));
            assert_eq!(
                editor.workspace.keyboard_file_node.as_deref(),
                Some(nested_id.as_str())
            );

            // Left on the expanded directory collapses it without moving.
            assert!(editor.workspace_handle_navigation_key("left", cx));
            assert_eq!(
                editor.workspace.keyboard_file_node.as_deref(),
                Some(nested_id.as_str())
            );

            // Collapsed now, so Left ascends to the root.
            assert!(editor.workspace_handle_navigation_key("left", cx));
            assert_eq!(
                editor.workspace.keyboard_file_node.as_deref(),
                Some(root_id.as_str())
            );

            // Up moves from the root to the `..` parent row.
            assert!(editor.workspace_handle_navigation_key("up", cx));
            assert_eq!(
                editor.workspace.keyboard_file_node.as_deref(),
                Some(WORKSPACE_PARENT_NODE_ID)
            );

            // Up on the first row is a handled no-op, not a wrap.
            assert!(editor.workspace_handle_navigation_key("up", cx));
            assert_eq!(
                editor.workspace.keyboard_file_node.as_deref(),
                Some(WORKSPACE_PARENT_NODE_ID)
            );
        });

        let _ = fs::remove_dir_all(&root);
    }

    #[gpui::test]
    async fn outline_keyboard_navigation_walks_rows_and_descends_levels(
        cx: &mut gpui::TestAppContext,
    ) {
        init_editor_test_app(cx);
        let (editor, cx) = cx.add_window_view(|_window, cx| {
            Editor::from_markdown(cx, "# A\n## B\n### C\n# D".to_string(), None)
        });

        editor.update(cx, |editor, cx| {
            editor.sync_workspace_models(cx);
            editor.workspace.expanded.insert("outline:0".to_string());
            // Document serialization inserts blank lines between blocks, so
            // the headings land on lines 0, 2, 4, and 6.
            editor.workspace.expanded.insert("outline:2".to_string());
            editor.workspace.keyboard_section = Some(WorkspaceSection::Outline);
            editor.workspace.keyboard_outline_node = Some("outline:0".to_string());

            assert!(editor.workspace_handle_navigation_key("down", cx));
            assert_eq!(
                editor.workspace.keyboard_outline_node.as_deref(),
                Some("outline:2")
            );
            assert!(editor.workspace_handle_navigation_key("down", cx));
            assert_eq!(
                editor.workspace.keyboard_outline_node.as_deref(),
                Some("outline:4")
            );
            assert!(editor.workspace_handle_navigation_key("down", cx));
            assert_eq!(
                editor.workspace.keyboard_outline_node.as_deref(),
                Some("outline:6")
            );

            assert!(editor.workspace_handle_navigation_key("up", cx));
            assert_eq!(
                editor.workspace.keyboard_outline_node.as_deref(),
                Some("outline:4")
            );

            // Left from C ascends to B.
            assert!(editor.workspace_handle_navigation_key("left", cx));
            assert_eq!(
                editor.workspace.keyboard_outline_node.as_deref(),
                Some("outline:2")
            );

            // B is expanded, so Left collapses it and stays put.
            assert!(editor.workspace_handle_navigation_key("left", cx));
            assert_eq!(
                editor.workspace.keyboard_outline_node.as_deref(),
                Some("outline:2")
            );

            // Collapsed B now ascends to A.
            assert!(editor.workspace_handle_navigation_key("left", cx));
            assert_eq!(
                editor.workspace.keyboard_outline_node.as_deref(),
                Some("outline:0")
            );
        });
    }

    #[gpui::test]
    async fn cycle_focus_visits_files_then_outline_then_document(
        cx: &mut gpui::TestAppContext,
    ) {
        init_editor_test_app(cx);
        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-focus-cycle-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create dir");
        let path = root.join("note.md");
        fs::write(&path, "# Heading\n\nbody").expect("write md");

        let (editor, cx) = cx.add_window_view(|_window, cx| {
            Editor::from_markdown(cx, "# Heading\n\nbody".to_string(), Some(path.clone()))
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_workspace_root_override(root.clone(), cx);
            editor.toggle_workspace_drawer(window, cx);
            editor.focus_workspace_section(WorkspaceSection::Files, window, cx);
            assert!(editor.workspace_focus_handle.is_focused(window));

            editor.cycle_workspace_focus(window, cx);
            assert_eq!(
                editor.workspace.keyboard_section,
                Some(WorkspaceSection::Outline)
            );
            assert!(editor.workspace_focus_handle.is_focused(window));

            editor.cycle_workspace_focus(window, cx);
            assert!(!editor.workspace_focus_handle.is_focused(window));
            let first = editor.document.visible_blocks()[0].entity.clone();
            assert!(first.read(cx).focus_handle.is_focused(window));

            editor.cycle_workspace_focus(window, cx);
            assert_eq!(
                editor.workspace.keyboard_section,
                Some(WorkspaceSection::Files)
            );
            assert!(editor.workspace_focus_handle.is_focused(window));
        });

        let _ = fs::remove_dir_all(&root);
    }

    #[gpui::test]
    async fn enter_on_outline_heading_jumps_and_keeps_sidebar_focus(
        cx: &mut gpui::TestAppContext,
    ) {
        init_editor_test_app(cx);
        let (editor, cx) = cx.add_window_view(|_window, cx| {
            Editor::from_markdown(cx, "# A\n\n## B\n\nbody".to_string(), None)
        });

        editor.update_in(cx, |editor, window, cx| {
            editor.sync_workspace_models(cx);
            editor.workspace.keyboard_section = Some(WorkspaceSection::Outline);
            editor.workspace.keyboard_outline_node = Some("outline:0".to_string());
            editor.focus_workspace_section(WorkspaceSection::Outline, window, cx);

            assert!(editor.workspace_activate_cursor(window, cx));
            assert!(editor.pending_focus.is_none());
            assert!(editor.workspace_focus_handle.is_focused(window));
            assert_eq!(editor.workspace.selected_outline.as_deref(), Some("outline:0"));
            assert!(editor.active_entity_id.is_some());
            assert_eq!(editor.pending_scroll_target, editor.active_entity_id);
        });
    }

    #[gpui::test]
    async fn files_keyboard_reaches_and_activates_the_parent_row(
        cx: &mut gpui::TestAppContext,
    ) {
        init_editor_test_app(cx);
        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-parent-row-{}",
            uuid::Uuid::new_v4()
        ));
        let child = root.join("child");
        fs::create_dir_all(&child).expect("create dirs");
        fs::write(child.join("note.md"), "note").expect("write md");

        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".into(), None));
        editor.update_in(cx, |editor, window, cx| {
            editor.set_workspace_root_override(child.clone(), cx);
            editor.toggle_workspace_drawer(window, cx);
            editor.workspace.keyboard_section = Some(WorkspaceSection::Files);
            editor.focus_workspace_section(WorkspaceSection::Files, window, cx);

            let child_id = format!("file:{}", child.to_string_lossy());
            assert_eq!(
                editor.workspace.keyboard_file_node.as_deref(),
                Some(child_id.as_str())
            );

            assert!(editor.workspace_handle_navigation_key("up", cx));
            assert_eq!(
                editor.workspace.keyboard_file_node.as_deref(),
                Some(WORKSPACE_PARENT_NODE_ID)
            );

            assert!(editor.workspace_activate_cursor(window, cx));
            assert_eq!(editor.workspace.root.as_deref(), Some(root.as_path()));
            assert!(editor.workspace_focus_handle.is_focused(window));
        });

        let _ = fs::remove_dir_all(&root);
    }

    #[gpui::test]
    async fn enter_on_file_opens_it_and_keeps_sidebar_focus(cx: &mut gpui::TestAppContext) {
        init_editor_test_app(cx);
        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-enter-file-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create dir");
        let path = root.join("note.md");
        fs::write(&path, "# Opened\n\nbody").expect("write md");

        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".into(), None));
        editor.update_in(cx, |editor, window, cx| {
            editor.set_workspace_root_override(root.clone(), cx);
            editor.toggle_workspace_drawer(window, cx);
            let file_id = format!("file:{}", path.to_string_lossy());
            editor.workspace.keyboard_section = Some(WorkspaceSection::Files);
            editor.workspace.keyboard_file_node = Some(file_id);
            editor.focus_workspace_section(WorkspaceSection::Files, window, cx);

            assert!(editor.workspace_activate_cursor(window, cx));
            assert_eq!(editor.file_path.as_deref(), Some(path.as_path()));
            assert!(editor.pending_focus.is_none());
            assert!(editor.workspace_focus_handle.is_focused(window));
        });

        let _ = fs::remove_dir_all(&root);
    }

    #[gpui::test]
    async fn opening_a_document_inside_the_override_root_keeps_it(cx: &mut gpui::TestAppContext) {
        init_editor_test_app(cx);
        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-override-keep-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create dir");

        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".to_string(), None));
        editor.update_in(cx, |editor, window, cx| {
            editor.toggle_workspace_drawer(window, cx);
        });

        let override_root = root.clone();
        let opened_path = root.join("opened.md");
        editor.update(cx, |editor, cx| {
            editor.set_workspace_root_override(override_root, cx);
            editor.file_path = Some(opened_path.clone());
            // Opening a file that lives inside the override root must not
            // reclaim it, or navigating up would be undone by the very next
            // click on a file in the tree.
            editor.sync_workspace_after_document_path_change(cx);
        });

        editor.read_with(cx, |editor, _cx| {
            assert_eq!(editor.workspace_root_override, Some(root.clone()));
            assert_eq!(editor.workspace_root_for_current_file(), Some(root.clone()));
        });

        let _ = fs::remove_dir_all(&root);
    }

    #[gpui::test]
    async fn opening_a_document_outside_the_override_root_clears_it(cx: &mut gpui::TestAppContext) {
        init_editor_test_app(cx);
        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-override-clear-test-{}",
            uuid::Uuid::new_v4()
        ));
        let elsewhere = std::env::temp_dir().join(format!(
            "velotype-workspace-elsewhere-test-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create dir");
        fs::create_dir_all(&elsewhere).expect("create dir");

        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".to_string(), None));
        editor.update_in(cx, |editor, window, cx| {
            editor.toggle_workspace_drawer(window, cx);
        });

        let override_root = root.clone();
        let opened_path = elsewhere.join("unrelated.md");
        editor.update(cx, |editor, cx| {
            editor.set_workspace_root_override(override_root, cx);
            editor.file_path = Some(opened_path.clone());
            // An unrelated document elsewhere reclaims the root, otherwise
            // the sidebar would stay pinned to a directory the user has
            // navigated away from.
            editor.sync_workspace_after_document_path_change(cx);
        });

        editor.read_with(cx, |editor, _cx| {
            assert_eq!(editor.workspace_root_override, None);
            assert_eq!(
                editor.workspace_root_for_current_file(),
                Some(elsewhere.clone())
            );
        });

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    #[gpui::test]
    async fn navigate_workspace_root_up_reroots_to_the_parent(cx: &mut gpui::TestAppContext) {
        init_editor_test_app(cx);
        let root = std::env::temp_dir().join(format!(
            "velotype-workspace-nav-up-test-{}",
            uuid::Uuid::new_v4()
        ));
        let child = root.join("child");
        fs::create_dir_all(&child).expect("create dirs");

        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".to_string(), None));

        editor.update(cx, |editor, cx| {
            editor.set_workspace_root_override(child, cx);
        });

        let moved = editor.update(cx, |editor, cx| editor.navigate_workspace_root_up(cx));
        assert!(moved);

        editor.read_with(cx, |editor, _cx| {
            assert_eq!(editor.workspace_root_override, Some(root.clone()));
        });

        let _ = fs::remove_dir_all(&root);
    }

    #[gpui::test]
    async fn navigate_workspace_root_up_returns_false_at_the_filesystem_root(
        cx: &mut gpui::TestAppContext,
    ) {
        init_editor_test_app(cx);
        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".to_string(), None));

        editor.update(cx, |editor, cx| {
            editor.set_workspace_root_override(PathBuf::from("/"), cx);
        });

        let moved = editor.update(cx, |editor, cx| editor.navigate_workspace_root_up(cx));
        assert!(!moved);
    }

    #[gpui::test]
    async fn reveal_workspace_drawer_opens_a_closed_drawer(cx: &mut gpui::TestAppContext) {
        init_editor_test_app(cx);
        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".to_string(), None));

        editor.read_with(cx, |editor, _cx| {
            assert!(!editor.workspace.is_open);
        });

        editor.update(cx, |editor, cx| {
            editor.reveal_workspace_drawer(cx);
        });

        editor.read_with(cx, |editor, _cx| {
            assert!(editor.workspace.is_open);
        });
    }

    #[gpui::test]
    async fn reveal_workspace_drawer_is_a_noop_when_already_open(cx: &mut gpui::TestAppContext) {
        init_editor_test_app(cx);
        let (editor, cx) =
            cx.add_window_view(|_window, cx| Editor::from_markdown(cx, "draft".to_string(), None));

        editor.update_in(cx, |editor, window, cx| {
            editor.toggle_workspace_drawer(window, cx);
        });
        editor.update(cx, |editor, cx| {
            editor.reveal_workspace_drawer(cx);
        });

        editor.read_with(cx, |editor, _cx| {
            assert!(editor.workspace.is_open);
        });
    }

    #[test]
    fn line_start_offset_of_line_zero_is_zero() {
        assert_eq!(line_start_offset("# One\n\nAlpha", 0), Some(0));
    }

    #[test]
    fn line_start_offset_finds_a_middle_line() {
        let source = "# One\n\nAlpha\n\n## Two\n\nBravo";
        let offset = line_start_offset(source, 4).expect("line 4 exists");
        assert!(source[offset..].starts_with("## Two"));
    }

    #[test]
    fn line_start_offset_past_the_end_is_none() {
        let source = "# One\n\nAlpha";
        assert_eq!(line_start_offset(source, 100), None);
    }

    #[test]
    fn line_start_offset_lands_on_a_byte_boundary_past_multibyte_chars() {
        // "café" has a multibyte 'é', so a char-count-based offset would land
        // one byte short of "Bravo" below.
        let source = "# café\n\nAlpha\n\n## Two\n\nBravo";
        let offset = line_start_offset(source, 6).expect("line 6 exists");
        assert!(source[offset..].starts_with("Bravo"));
    }
}
