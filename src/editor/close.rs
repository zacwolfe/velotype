//! Unsaved-changes dialog and window-close interception.
//!
//! When the document is dirty, `Editor::on_window_should_close` returns
//! false and shows an overlay offering three choices: save-and-close,
//! discard-and-close, or keep editing.  Focus is restored to the
//! previously active block when the dialog is dismissed without closing.

use gpui::*;

use super::Editor;

impl Editor {
    /// Offers the normal save flow while keeping this window available for a later update click.
    pub(crate) fn allow_update_restart(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.document_dirty {
            return true;
        }
        if self.update_save_prompt_active {
            return false;
        }
        self.update_save_prompt_active = true;
        let strings = cx.global::<crate::i18n::I18nManager>().strings_arc();
        let prompt = window.prompt(
            PromptLevel::Info,
            &strings.unsaved_changes_title,
            Some(&strings.update_save_first_message),
            &[
                strings.menu_save.as_str(),
                strings.unsaved_changes_cancel.as_str(),
            ],
            cx,
        );
        let handle = window.window_handle();
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let save = prompt.await == Ok(0);
            let _ = cx.update_window(handle, |_, window, cx| {
                let _ = this.update(cx, |editor, cx| {
                    editor.update_save_prompt_active = false;
                    if save {
                        editor.pending_close_after_save = false;
                        editor.save_document(window, cx);
                    }
                });
            });
        })
        .detach();
        false
    }

    /// Closes a clean window immediately, or opens the unsaved-changes dialog.
    pub(crate) fn request_close_current_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_menu_bar(cx);
        self.hide_info_dialog(cx);
        self.pending_close_after_save = false;

        if self.on_window_should_close(window, cx) {
            self.close_dialog_restore_focus = None;
            window.remove_window();
        }
    }

    /// Consumes the dialog's saved focus target and queues focus and scrolling.
    pub(crate) fn restore_focus_after_close_dialog(&mut self, cx: &mut Context<Self>) {
        if let Some(focus_id) = self.close_dialog_restore_focus.take() {
            self.pending_focus = Some(focus_id);
            self.scroll_recheck_attempts = 0;
            self.pending_scroll_active_block_into_view = true;
            cx.notify();
        }
    }

    /// Hides the close overlay without changing the pending save or focus target.
    pub(crate) fn hide_unsaved_changes_dialog(&mut self, cx: &mut Context<Self>) {
        if self.show_unsaved_changes_dialog {
            self.show_unsaved_changes_dialog = false;
            cx.notify();
        }
    }

    /// Cancels close-after-save and restores focus when that flow was pending.
    pub(crate) fn abort_pending_close_after_save(&mut self, cx: &mut Context<Self>) {
        let had_pending_close = self.pending_close_after_save;
        self.pending_close_after_save = false;
        self.close_menu_bar(cx);
        self.hide_unsaved_changes_dialog(cx);
        if had_pending_close {
            self.restore_focus_after_close_dialog(cx);
        } else {
            self.close_dialog_restore_focus = None;
        }
    }

    /// Returns whether closing is allowed; dirty documents show an overlay and blur focus.
    pub(crate) fn on_window_should_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.document_dirty {
            return true;
        }

        self.close_menu_bar(cx);
        self.hide_info_dialog(cx);
        if !self.show_unsaved_changes_dialog {
            self.close_dialog_restore_focus = self.document.focused_block_entity_id(window, cx);
            self.show_unsaved_changes_dialog = true;
            window.blur(cx);
            cx.notify();
        }

        false
    }

    /// Keeps the window open and restores the block focused before the close dialog.
    pub(crate) fn on_cancel_close_dialog(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_close_after_save = false;
        self.close_menu_bar(cx);
        self.hide_unsaved_changes_dialog(cx);
        self.restore_focus_after_close_dialog(cx);
    }

    /// Removes the window without saving or restoring its previous focus.
    pub(crate) fn on_discard_and_close(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_close_after_save = false;
        self.close_dialog_restore_focus = None;
        self.close_menu_bar(cx);
        self.hide_unsaved_changes_dialog(cx);
        window.remove_window();
    }

    /// Queues a save whose successful completion will remove the window.
    pub(crate) fn on_save_and_close(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_close_after_save = true;
        self.close_menu_bar(cx);
        self.hide_unsaved_changes_dialog(cx);
        self.pending_save = true;
        cx.notify();
    }
}
