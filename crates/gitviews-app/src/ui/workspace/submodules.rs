use super::*;
use std::path::Path;

/// A file-browser context. The selected main repository and catalog stay owned
/// by Workspace; a submodule never replaces either of them.
pub(super) struct SubmoduleChanges {
    pub parent_root: PathBuf,
    pub target: PathBuf,
    pub repository: RepositorySnapshot,
    pub tree: Arc<RepositoryTree>,
    pub watcher: Option<RepositoryWatcher>,
    pub watcher_needs_restart: bool,
}

impl Workspace {
    pub(super) fn changes_repository(&self) -> Option<&RepositorySnapshot> {
        self.submodule_changes
            .as_ref()
            .map(|scope| &scope.repository)
            .or(self.repository.as_ref())
    }

    pub(super) fn changes_tree(&self) -> Option<&RepositoryTree> {
        self.submodule_changes
            .as_ref()
            .map(|scope| scope.tree.as_ref())
            .or(self.tree.as_deref())
    }

    pub(super) fn discard_submodule(&mut self) {
        self.submodule_branch_menu = None;
        self.file_search.pending_open = None;
        self.stop_submodule_navigation();
        self.submodule_cancel
            .store(true, std::sync::atomic::Ordering::Release);
        self.submodule_request_id = self.submodule_request_id.wrapping_add(1);
        self.submodule_task = None;
        self.submodule_watch_task = None;
        self.submodule_loading = false;
        self.submodule_changes = None;
        self.branch_context_task = None;
        self.branch_context = None;
    }

    pub(super) fn invalidate_changes(&mut self) {
        if let Some(repo) = &self.repository {
            self.service.invalidate(&repo.root);
            self.preview_deck.invalidate_root(&repo.root);
        }
        if let Some(scope) = &self.submodule_changes {
            self.service.invalidate(&scope.repository.root);
            self.preview_deck.invalidate_root(&scope.repository.root);
        }
    }

    pub(super) fn clear_changes_preview(&mut self, cx: &mut Context<Self>) {
        self.close_file_search(cx);
        self.diff_request_id = self.diff_request_id.wrapping_add(1);
        self.diff_task = None;
        self.diff_loading = false;
        self.history = false;
        self.reset_history(cx);
        self.selected_file = None;
        self.clear_tabs(cx);
        self.diff = None;
        self.source_file = None;
        self.source = false;
        self.hunk_index = 0;
        self.code_scroll.set_offset(point(px(0.), px(0.)));
        self.collapsed_paths.clear();
        self.preview_pane.update(cx, |pane, cx| pane.clear(cx));
    }

    pub(super) fn select_first_change(&mut self, cx: &mut Context<Self>) {
        let first = self.changes_repository().and_then(|repo| {
            repo.changes
                .iter()
                .find(|change| {
                    !self.changes_tree().is_some_and(|tree| {
                        tree.submodules
                            .iter()
                            .any(|module| module.path == change.path)
                    })
                })
                .map(|change| {
                    (
                        change.path.clone(),
                        change.worktree_status == ' ' && change.index_status != ' ',
                    )
                })
        });
        if let Some((path, staged)) = first {
            self.select_file(path, staged, cx);
        }
    }

    pub(super) fn show_parent_changes(&mut self, cx: &mut Context<Self>) {
        self.finish_navigation(navigation::NavigationKind::Submodule, cx);
        self.discard_submodule();
        self.clear_changes_preview(cx);
        self.notice = None;
        self.sync_file_list(cx);
        self.select_first_change(cx);
        cx.notify();
    }

    pub(super) fn recover_parent_changes(&mut self, cx: &mut Context<Self>) {
        self.show_parent_changes(cx);
    }

    fn submodule_parent(&self, target: &Path) -> Option<PathBuf> {
        let contains = |repo: &RepositorySnapshot, tree: &RepositoryTree| {
            tree.submodules
                .iter()
                .any(|module| paths_equal(&repo.root.join(&module.path), target))
        };
        if let (Some(repo), Some(tree)) = (&self.repository, &self.tree)
            && contains(repo, tree)
        {
            return Some(repo.root.clone());
        }
        self.submodule_changes.as_ref().and_then(|scope| {
            contains(&scope.repository, &scope.tree).then(|| scope.repository.root.clone())
        })
    }

    pub(super) fn select_submodule(&mut self, target: PathBuf, cx: &mut Context<Self>) {
        if self.remote_blocks_navigation(cx) {
            return;
        }
        if self
            .submodule_changes
            .as_ref()
            .is_some_and(|scope| paths_equal(&scope.target, &target))
            && !self.submodule_loading
        {
            // Reselecting A while B is loading cancels B as well.
            self.submodule_cancel
                .store(true, std::sync::atomic::Ordering::Release);
            self.submodule_request_id = self.submodule_request_id.wrapping_add(1);
            self.submodule_task = None;
            self.submodule_loading = false;
            self.finish_navigation(navigation::NavigationKind::Submodule, cx);
            self.notice = None;
            self.refresh_branch_context(cx);
            tracing::info!(target: "gitviews::submodule", path = %display_path(&target), "submodule_already_active");
            cx.notify();
            return;
        }
        let Some(parent_root) = self.submodule_parent(&target) else {
            self.notice = Some("该路径不是当前仓库中已登记的子模块。".into());
            cx.notify();
            return;
        };
        self.read_submodule(parent_root, target, cx);
    }

    pub(super) fn start_submodule_watching(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = &self.submodule_changes else {
            return;
        };
        let root = scope.repository.root.clone();
        let repository_id = self.request_id;
        let request_id = self.submodule_request_id;
        let debounce = Duration::from_millis(self.settings.refresh.debounce_ms);
        self.submodule_watch_task = Some(cx.spawn(async move |this, cx| {
            let worker_root = root.clone();
            let watcher = tasks::spawn(cx.background_executor(), "submodule.watcher", request_id, move || RepositoryWatcher::new_with_debounce(worker_root, debounce)).await;
            let _ = this.update(cx, |view, cx| {
                if view.request_id != repository_id || view.submodule_changes.as_ref().is_none_or(|scope| !paths_equal(&scope.repository.root, &root)) {
                    return;
                }
                view.submodule_watch_task = None;
                if let Some(scope) = view.submodule_changes.as_mut() {
                    scope.watcher_needs_restart = watcher.is_err();
                    match watcher {
                        Ok(mut watcher) => {
                            watcher.set_debounce(Duration::from_millis(view.settings.refresh.debounce_ms));
                            scope.watcher = Some(watcher);
                        }
                        Err(_) => {
                            view.notice = Some("子模块监听不可用，将定期扫描。".into());
                            tracing::warn!(target: "gitviews::submodule", request_id, "submodule_watcher_failed");
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn submodule_context_bar(&self, light: bool, cx: &mut Context<Self>) -> Div {
        let label = self
            .submodule_changes
            .as_ref()
            .map_or_else(String::new, |scope| {
                let path = self
                    .repository
                    .as_ref()
                    .and_then(|repo| scope.target.strip_prefix(&repo.root).ok());
                display_path(path.unwrap_or(&scope.target))
            });
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .flex_shrink_0()
            .h(px(34.))
            .px(px(12.))
            .border_b_1()
            .border_color(rgb(if light { 0xd0d7de } else { 0x3d444d }))
            .bg(rgb(if light { 0xddf4ff } else { 0x1a3453 }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(label),
            )
            .child(
                div()
                    .id("parent-changes")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("返回主仓库文件变更")
                    .cursor_pointer()
                    .text_size(px(11.))
                    .child("返回主仓库")
                    .on_click(cx.listener(|this, _, _, cx| this.show_parent_changes(cx))),
            )
    }
}
