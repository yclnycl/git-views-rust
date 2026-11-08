//! File history is scoped to the row or preview that opened its menu.
use super::*;

#[cfg(test)]
mod headless_tests;

impl Workspace {
    pub(super) fn open_file_history(
        &mut self,
        root: PathBuf,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open(cx)
            || !self.local_repository_ready(cx)
            || self
                .changes_repository()
                .is_none_or(|repo| !paths_equal(&repo.root, &root))
            || path.is_empty()
        {
            return;
        }
        let head = self.active_head_id();
        self.close_file_search(cx);
        self.close_submodule_branch_menu(cx);
        self.menu = None;
        self.active_input = None;
        self.focus.focus(window, cx);
        self.history_tools_dialog.update(cx, |dialog, cx| {
            dialog.show_file_history(root, head, path, cx)
        });
    }

    pub(super) fn bind_preview_file_history(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self.active_preview_key();
        if let Some(key) = key {
            let pane = self.preview_pane.clone();
            if self
                .preview_history_subscription
                .as_ref()
                .is_some_and(|(bound, id, _)| bound == &key && *id == pane.entity_id())
            {
                return;
            }
            pane.update(cx, |pane, cx| pane.set_history_available(true, cx));
            let target = key.clone();
            let subscription =
                cx.subscribe_in(&pane, window, move |view, pane, event, window, cx| {
                    if matches!(event, PreviewPaneEvent::FileHistory)
                        && view.preview_pane == *pane
                        && view.active_preview_key().as_ref() == Some(&target)
                    {
                        view.open_file_history(
                            target.root.clone(),
                            target.path.clone(),
                            window,
                            cx,
                        );
                    }
                });
            self.preview_history_subscription = Some((key, pane.entity_id(), subscription));
        } else {
            self.preview_history_subscription = None;
            self.preview_pane
                .update(cx, |pane, cx| pane.set_history_available(false, cx));
        }
    }
}
