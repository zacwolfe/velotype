//! External file-change detection.
//!
//! The editor polls two things on a short timer: the document currently open in
//! the window, and the directories/files that the Files sidebar has loaded.
//! Polling keeps the feature dependency-free and works uniformly across the
//! platforms Velotype targets, while the intervals are short enough to feel
//! live: the open document reacts within a quarter second and the sidebar
//! within about one second.
//!
//! An open document that is clean reloads in place, preserving the caret,
//! focus, and scroll position. A document with unsaved edits is never
//! overwritten silently; the user gets a Reload / Keep prompt instead.

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use gpui::*;

use super::Editor;
use crate::i18n::I18nManager;

/// How often the open document's on-disk signature is checked.
const DOCUMENT_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The Files sidebar only changes when entries are added/removed/renamed, so
/// it can afford to be checked a little less often.
const WORKSPACE_POLL_TICKS: u32 = 4;

/// Cheap identity for a file on disk. Metadata changes for any external write
/// worth reacting to; the length guards against coarse timestamp resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FileSignature {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileSignature {
    pub(super) fn read(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

pub(super) fn directory_modified(path: &Path) -> Option<SystemTime> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_dir() {
        return None;
    }
    metadata.modified().ok()
}

pub(super) fn normalized_markdown(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

impl Editor {
    /// Starts the background poll loop on the first render. It stays alive for
    /// the window's lifetime and stops once the editor entity is gone.
    pub(super) fn ensure_external_file_watch(&mut self, cx: &mut Context<Self>) {
        if self.external_watch_task.is_some() {
            return;
        }

        let mut tick = 0u32;
        self.external_watch_task = Some(cx.spawn(
            async move |this: WeakEntity<Self>, cx: &mut AsyncApp| loop {
                cx.background_executor().timer(DOCUMENT_POLL_INTERVAL).await;

                if this
                    .update(cx, |editor, cx| {
                        editor.poll_open_document_external_change(cx);
                    })
                    .is_err()
                {
                    break;
                }

                tick = tick.wrapping_add(1);
                if tick.is_multiple_of(WORKSPACE_POLL_TICKS)
                    && this
                        .update(cx, |editor, cx| {
                            editor.poll_workspace_external_changes(cx);
                        })
                        .is_err()
                {
                    break;
                }
            },
        ));
    }

    /// Reads the open document's signature and reloads the buffer if the file
    /// changed behind the editor's back.
    pub(super) fn poll_open_document_external_change(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.file_path.clone() else {
            self.watched_document_signature = None;
            return;
        };

        let signature = FileSignature::read(&path);
        if self.watched_document_signature == signature {
            return;
        }
        // Record the observed signature whether or not a read succeeds, so a
        // transient failure does not make every later tick retry in a loop.
        self.watched_document_signature = signature;

        if signature.is_none() {
            // The file was moved or deleted. Keep the buffer untouched: the
            // user may still save it back, and clearing the window on an
            // unrelated filesystem hiccup would be destructive.
            return;
        }

        let Ok(text) = fs::read_to_string(&path) else {
            return;
        };
        let normalized = normalized_markdown(&text);
        if self.document_disk_text.as_deref() == Some(normalized.as_str()) {
            return;
        }

        self.document_disk_text = Some(normalized.clone());
        self.workspace_watch.changed_files.remove(&path);

        if self.document_dirty {
            self.pending_external_reload = Some(normalized);
            cx.notify();
        } else {
            self.reload_document_from_external_markdown(normalized, cx);
        }
    }

    /// Shows the Reload / Keep prompt when a dirty buffer was changed on disk.
    /// Deferred until no other modal owns the window, so an update prompt or
    /// unsaved-changes dialog is never stacked underneath.
    pub(super) fn sync_external_reload_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.external_reload_prompt_active
            || self.pending_external_reload.is_none()
            || self.info_dialog.is_some()
            || self.show_drop_replace_dialog
            || self.show_unsaved_changes_dialog
        {
            return;
        }

        self.external_reload_prompt_active = true;
        let strings = cx.global::<I18nManager>().strings().clone();
        let buttons = [
            strings.external_change_reload.as_str(),
            strings.external_change_keep.as_str(),
        ];
        let prompt = window.prompt(
            PromptLevel::Warning,
            &strings.external_change_title,
            Some(&strings.external_change_message),
            &buttons,
            cx,
        );

        let weak_editor = cx.entity().downgrade();
        cx.spawn(async move |_this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let choice = prompt.await.ok();
            let _ = weak_editor.update(cx, |editor, cx| {
                editor.external_reload_prompt_active = false;
                if choice == Some(0) {
                    if let Some(markdown) = editor.pending_external_reload.take() {
                        editor.reload_document_from_external_markdown(markdown, cx);
                    } else {
                        cx.notify();
                    }
                } else {
                    editor.pending_external_reload = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Replaces the buffer with `markdown` while keeping the view where the
    /// user left it. This is deliberately not the full document-replacement
    /// path: a live external edit should not reset scroll, focus, undo, or the
    /// find bar.
    fn reload_document_from_external_markdown(&mut self, markdown: String, cx: &mut Context<Self>) {
        let normalized = normalized_markdown(&markdown);
        let focus_index = self
            .active_entity_id
            .and_then(|id| self.document.visible_index_for_entity_id(id));
        let focus_range = self
            .active_entity_id
            .and_then(|id| self.document.block_entity_by_id(id))
            .map(|block| block.read(cx).selected_range.clone())
            .unwrap_or(0..0);
        let scroll_offset = self.scroll_handle.offset();

        let roots = if self.view_mode == super::ViewMode::Source {
            let block = Self::new_block(
                cx,
                crate::components::BlockRecord::paragraph(normalized.clone()),
            );
            block.update(cx, |block, _cx| block.set_source_document_mode());
            vec![block]
        } else {
            let mut roots = Self::build_root_blocks_from_markdown(cx, &normalized);
            if roots.is_empty() {
                roots.push(Self::new_block(
                    cx,
                    crate::components::BlockRecord::paragraph(String::new()),
                ));
            }
            roots
        };

        self.document.replace_roots(roots, cx);
        self.table_cells.clear();
        self.rebuild_table_runtimes(cx);
        self.rebuild_image_runtimes(cx);

        self.document_disk_text = Some(normalized.clone());
        self.document_dirty = false;
        self.pending_window_edited = false;
        self.pending_window_edited_clear = true;
        self.pending_window_title_refresh = true;
        self.pending_save = false;
        self.pending_save_as = false;
        self.pending_open_link = None;
        self.pending_close_after_save = false;
        self.close_dialog_restore_focus = None;
        self.show_unsaved_changes_dialog = false;
        self.clear_pending_drop_replace_state(cx);
        self.context_menu = None;
        self.table_insert_dialog = None;
        self.context_menu_submenu_close_task = None;
        self.table_axis_preview = None;
        self.table_axis_selection = None;
        self.sync_table_axis_visuals(cx);
        self.clear_cross_block_selection(cx);
        self.rendered_select_all_cycle = None;

        // Restore the caret where it was, clamped to whatever survives in the
        // new version of the document.
        self.pending_focus = None;
        self.active_entity_id = None;
        if let Some(index) = focus_index {
            let visible = self.document.visible_blocks();
            if !visible.is_empty() {
                let target = visible[index.min(visible.len() - 1)].entity.clone();
                let text_len = target.read(cx).display_text().len();
                let start = focus_range.start.min(text_len);
                let end = focus_range.end.min(text_len).max(start);
                self.focus_block_range(&target, start..end, cx);
            }
        }

        // `focus_block_range` asks for a scroll-into-view; external changes
        // should not yank the viewport, so cancel that and put the old offset
        // back. The next frame clamps it if the document got shorter.
        self.pending_scroll_active_block_into_view = false;
        self.pending_scroll_target = None;
        self.pending_scroll_recheck_after_layout = false;
        self.suppress_caret_scroll_follow = true;
        self.scroll_handle.set_offset(scroll_offset);
        self.last_scroll_viewport_size = None;

        self.last_stable_source_text = normalized;
        self.last_selection_snapshot = Self::empty_selection_snapshot();
        self.last_selection_snapshot_key = None;
        self.refresh_stable_document_snapshot(cx);
        self.invalidate_workspace_outline();
        self.recompute_search_matches(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{Editor, FileSignature};
    use gpui::TestAppContext;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    fn init_editor_test_app(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::i18n::I18nManager::init(cx);
            crate::theme::ThemeManager::init(cx);
            crate::components::init(cx);
        });
    }

    fn temp_markdown_file(name: &str, contents: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "velotype-{name}-{}-{}.md",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::write(&path, contents).expect("write fixture");
        path
    }

    #[test]
    fn file_signature_distinguishes_content_length() {
        let path =
            std::env::temp_dir().join(format!("velotype-signature-test-{}", uuid::Uuid::new_v4()));
        fs::write(&path, "a").expect("write file");
        let first = FileSignature::read(&path);
        fs::write(&path, "ab").expect("append");
        let second = FileSignature::read(&path);
        assert!(first.is_some());
        assert!(second.is_some());
        assert_ne!(first, second);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn file_signature_ignores_directories_and_missing_paths() {
        let dir =
            std::env::temp_dir().join(format!("velotype-signature-dir-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create dir");
        assert!(FileSignature::read(&dir).is_none());
        assert!(FileSignature::read(&PathBuf::from("/definitely/not/here.md")).is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[gpui::test]
    async fn clean_document_reloads_when_the_file_changes_on_disk(cx: &mut TestAppContext) {
        init_editor_test_app(cx);
        let path = temp_markdown_file("external-reload", "old\n");
        let (editor, cx) = cx.add_window_view(|_window, cx| {
            Editor::from_markdown(cx, "old\n".to_string(), Some(path.clone()))
        });

        editor.update(cx, |editor, cx| {
            // First poll records the baseline signature.
            editor.poll_open_document_external_change(cx);
        });

        std::thread::sleep(Duration::from_millis(20));
        fs::write(&path, "new\n").expect("external write");

        editor.update(cx, |editor, cx| {
            editor.poll_open_document_external_change(cx);
        });
        editor.read_with(cx, |editor, _cx| {
            assert_eq!(
                editor.document_disk_text.as_deref(),
                Some("new\n"),
                "disk baseline should follow the external write"
            );
            assert!(!editor.document_dirty);
        });
        // Rendered mode reserializes the paragraph, so compare the visible text
        // of the one block rather than raw source formatting.
        editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.document.raw_source_text(cx), "new");
        });

        let _ = fs::remove_file(path);
    }

    #[gpui::test]
    async fn dirty_document_keeps_edits_and_queues_a_reload_prompt(cx: &mut TestAppContext) {
        init_editor_test_app(cx);
        let path = temp_markdown_file("external-conflict", "old\n");
        let (editor, cx) = cx.add_window_view(|_window, cx| {
            Editor::from_markdown(cx, "old\n".to_string(), Some(path.clone()))
        });

        editor.update(cx, |editor, cx| {
            editor.poll_open_document_external_change(cx);
            editor.mark_dirty(cx);
        });

        std::thread::sleep(Duration::from_millis(20));
        fs::write(&path, "new\n").expect("external write");

        editor.update(cx, |editor, cx| {
            editor.poll_open_document_external_change(cx);
        });
        editor.read_with(cx, |editor, _cx| {
            assert!(editor.document_dirty, "local edits must survive");
            assert_eq!(editor.pending_external_reload.as_deref(), Some("new\n"));
        });

        let _ = fs::remove_file(path);
    }
}
