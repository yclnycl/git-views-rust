use super::*;

type PreviewResult = (
    Result<Arc<FileDiff>, gitviews_service::ServiceError>,
    Result<Arc<SourceFile>, gitviews_service::ServiceError>,
);

impl Workspace {
    pub(super) fn pause_repository_reads(&mut self) {
        self.submodule_branch_menu = None;
        self.branch_context_task = None;
        self.scan_cancel
            .store(true, std::sync::atomic::Ordering::Release);
        self.scan_epoch = self.scan_epoch.wrapping_add(1);
        self.scan_task = None;
        self.scan_pending = false;
        self.diff_request_id = self.diff_request_id.wrapping_add(1);
        self.diff_task = None;
        self.diff_loading = false;
        if self.submodule_loading {
            self.stop_submodule_navigation();
            self.submodule_cancel
                .store(true, std::sync::atomic::Ordering::Release);
            self.submodule_request_id = self.submodule_request_id.wrapping_add(1);
            self.submodule_task = None;
            self.submodule_loading = false;
        }
    }

    pub(super) fn sync_file_list(&mut self, cx: &mut Context<Self>) {
        if let Some(repo) = self.changes_repository() {
            let tree = self
                .submodule_changes
                .as_ref()
                .map(|scope| Arc::clone(&scope.tree))
                .or_else(|| self.tree.clone())
                .unwrap_or_default();
            self.file_list.update(cx, |list, cx| {
                list.set_data(repo.root.clone(), tree, repo.changes.clone(), cx)
            });
            if let Some(tree) = self.changes_tree()
                && let Some(warning) = tree.warnings.first()
            {
                self.notice = Some(format!(
                    "文件树已跳过 {} 个无法读取的项目：{} · {}",
                    tree.warnings.len(),
                    warning.path,
                    warning.message
                ));
            }
        }
    }
    pub(super) fn sync_preview(&mut self, cx: &mut Context<Self>) {
        if self.source && self.source_file.is_none() && self.diff.is_some() {
            self.source = false;
            self.notice = Some("此版本没有可读取的源码，显示文件差异。".into());
        } else if !self.source && self.diff.is_none() && self.source_file.is_some() {
            self.source = true;
            self.notice = Some("此文件不能生成文本差异，显示只读源码。".into());
        }
        let source = self.source_file.clone();
        let diff = self.diff.clone();
        let mode = if self.source {
            PreviewMode::Source
        } else if self.unified {
            PreviewMode::Unified
        } else {
            PreviewMode::Split
        };
        self.preview_pane.update(cx, |pane, cx| {
            if self.source {
                if let Some(source) = source {
                    pane.set_source(source, cx);
                }
            } else if let Some(diff) = diff {
                pane.set_diff(diff, cx);
            }
            pane.set_mode(mode, cx);
            pane.set_ignore_whitespace(self.ignore_whitespace, cx);
        });
    }
    pub(super) fn apply_preview_result(&mut self, result: PreviewResult, cx: &mut Context<Self>) {
        let (diff, source) = result;
        let diff_error = diff.as_ref().err().map(preview_error);
        let source_error = source.as_ref().err().map(preview_error);
        let next_diff = diff.ok();
        let next_source = source.ok();
        self.diff = next_diff;
        self.source_file = next_source;
        let accepted_source = self.source_file.clone();
        let accepted_diff = self.diff.clone();
        self.preview_pane.update(cx, |pane, cx| {
            pane.set_payloads(accepted_source, accepted_diff, cx)
        });
        if let Some(key) = self.active_preview_key() {
            self.preview_deck.mark_fresh(&key);
        }
        let _ = self.preview_deck.enforce_limits(cx);
        if self.source && self.source_file.is_none() {
            if self.diff.is_some() {
                self.source = false;
                self.notice = source_error;
            } else {
                self.preview_pane.update(cx, |pane, cx| {
                    pane.set_error(source_error.unwrap_or_else(|| "无法读取文件".into()), cx)
                });
                return;
            }
        }
        if !self.source && self.diff.is_none() {
            if self.source_file.is_some() {
                self.source = true;
                self.notice = diff_error;
            } else {
                self.preview_pane.update(cx, |pane, cx| {
                    pane.set_error(diff_error.unwrap_or_else(|| "无法读取差异".into()), cx)
                });
                return;
            }
        }
        self.sync_preview(cx);
    }
    pub(super) fn request_preview(
        &mut self,
        path: String,
        staged: bool,
        revision: Option<String>,
        quiet: bool,
        cx: &mut Context<Self>,
    ) {
        let repository = self.changes_repository();
        let Some(repo) = repository else {
            return;
        };
        let root = self
            .active_preview_key()
            .filter(|key| key.path == path && key.staged == staged && key.revision == revision)
            .map_or_else(|| repo.root.clone(), |key| key.root);
        let preview_key = self.active_preview_key();
        let submodule = self.submodule_changes.is_some();
        let service = Arc::clone(&self.service);
        self.diff_request_id = self.diff_request_id.wrapping_add(1);
        let request_id = self.diff_request_id;
        self.diff_loading = !quiet;
        if !quiet {
            self.preview_pane
                .update(cx, |pane, cx| pane.set_loading(cx));
        }
        self.diff_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(25))
                .await;
            let result = tasks::spawn(
                cx.background_executor(),
                "preview.read",
                request_id,
                move || {
                    let invalid_root = |error: gitviews_service::ServiceError| {
                        let reason = error.to_string();
                        (
                            Err(gitviews_service::ServiceError::Submodule {
                                path: root.clone(),
                                reason: reason.clone(),
                            }),
                            Err(gitviews_service::ServiceError::Submodule {
                                path: root.clone(),
                                reason,
                            }),
                        )
                    };
                    if submodule && let Err(error) = service.validate_root(&root) {
                        return invalid_root(error);
                    }
                    let diff = if let Some(id) = &revision {
                        service.cached_commit_diff(&root, id, &path)
                    } else {
                        service.cached_diff(&root, &path, staged)
                    };
                    let source = service.cached_source(
                        &root,
                        &path,
                        revision.as_deref().or(staged.then_some(":index")),
                    );
                    if submodule && let Err(error) = service.validate_root(&root) {
                        // A deinitialized submodule can otherwise discover the
                        // parent and return a different file with the same name.
                        return invalid_root(error);
                    }
                    (diff, source)
                },
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.diff_request_id != request_id
                    || view.active_preview_key() != preview_key
                    || view.preview_deck.active_key() != preview_key.as_ref()
                    || preview_key
                        .as_ref()
                        .is_some_and(|key| view.preview_deck.get(key).is_none())
                {
                    return;
                }
                view.diff_loading = false;
                view.apply_preview_result(result, cx);
                cx.notify();
            });
        }));
    }
    pub(super) fn start_watching(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = &self.repository else {
            return;
        };
        let root = repo.root.clone();
        let request_id = self.request_id;
        let debounce = Duration::from_millis(self.settings.refresh.debounce_ms);
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            let watcher = tasks::spawn(
                cx.background_executor(),
                "repository.watcher",
                request_id,
                move || RepositoryWatcher::new_with_debounce(root, debounce),
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.request_id != request_id {
                    return;
                }
                match watcher {
                    Ok(mut watcher) => {
                        watcher
                            .set_debounce(Duration::from_millis(view.settings.refresh.debounce_ms));
                        view.watcher = Some(watcher);
                    }
                    Err(error) => {
                        view.notice = Some(format!("文件监听不可用，将定期扫描：{error}"))
                    }
                }
                view.last_scan = Instant::now();
                cx.notify();
            });
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let alive = this.update(cx, |view, cx| {
                    if view.request_id != request_id {
                        return false;
                    }
                    let parent_batch = view.watcher.as_mut().and_then(RepositoryWatcher::poll);
                    let child_batch = view
                        .submodule_changes
                        .as_mut()
                        .and_then(|scope| scope.watcher.as_mut())
                        .and_then(RepositoryWatcher::poll);
                    if parent_batch.is_some() || child_batch.is_some() {
                        view.invalidate_changes();
                        let batches = parent_batch.iter().chain(child_batch.iter());
                        if let Some(scope) = view.submodule_changes.as_mut() {
                            scope.watcher_needs_restart |= batches.clone().any(|batch| {
                                batch.paths.iter().any(|path| {
                                    path.file_name().is_some_and(|name| {
                                        name == ".git"
                                            || name == ".gitmodules"
                                            || name == "commondir"
                                    })
                                })
                            });
                        }
                        if let Some(error) = batches.flat_map(|batch| &batch.errors).next() {
                            view.notice = Some(format!("监听异常，已安排重扫：{error}"));
                        }
                        view.queue_scan(cx);
                    } else if view.last_scan.elapsed()
                        >= Duration::from_secs(view.settings.refresh.scan_interval_secs)
                        && view.scan_task.is_none()
                        && view.remote_job.is_none()
                        && view.mutation_task.is_none()
                        && !view.worktree_busy
                    {
                        view.invalidate_changes();
                        view.queue_scan(cx);
                    }
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        }));
    }
    pub(super) fn queue_scan(&mut self, cx: &mut Context<Self>) {
        if self.scan_task.is_some()
            || self.loading
            || self.submodule_loading
            || self.remote_job.is_some()
            || self.mutation_task.is_some()
            || self.worktree_busy
        {
            self.scan_pending = true;
            return;
        }
        let Some(repo) = &self.repository else {
            return;
        };
        let root = repo.root.clone();
        self.scan_cancel
            .store(true, std::sync::atomic::Ordering::Release);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.scan_cancel = cancel.clone();
        let service = Arc::clone(&self.service);
        let request_id = self.request_id;
        let scan_epoch = self.scan_epoch;
        let debounce = Duration::from_millis(self.settings.refresh.debounce_ms);
        let scope_id = self.submodule_request_id;
        let previous_tree = self.tree.clone();
        let scope = self
            .submodule_changes
            .as_ref()
            .filter(|_| !self.submodule_loading)
            .map(|scope| {
                (
                    scope.parent_root.clone(),
                    scope.target.clone(),
                    (scope.watcher_needs_restart || scope.watcher.is_none())
                        && self.submodule_watch_task.is_none(),
                    scope.tree.clone(),
                )
            });
        let scope_target = scope.as_ref().map(|(_, target, _, _)| target.clone());
        self.scan_task = Some(cx.spawn(async move |this, cx| {
            let result = tasks::spawn(
                cx.background_executor(),
                "repository.scan",
                request_id,
                move || {
                    let parent = service.scan_with_cancel(&root, &cancel).map(|scan| {
                        let (tree, tree_changed) = reuse_scanned_tree(previous_tree, scan.tree);
                        (scan.snapshot, scan.details, tree, tree_changed)
                    });
                    // A missing submodule must not discard a healthy main scan.
                    let child = scope.map(|(parent, target, restart_watcher, previous_tree)| {
                        service
                            .submodule_with_cancel(&parent, &target, &cancel)
                            .map(|snapshot| {
                                let watcher = restart_watcher.then(|| {
                                    RepositoryWatcher::new_with_debounce(
                                        snapshot.repository.root.clone(),
                                        debounce,
                                    )
                                });
                                let (tree, tree_changed) =
                                    reuse_scanned_tree(Some(previous_tree), snapshot.tree);
                                (snapshot.repository, tree, tree_changed, watcher)
                            })
                    });
                    (parent, child)
                },
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.request_id != request_id || view.scan_epoch != scan_epoch {
                    return;
                }
                view.scan_task = None;
                view.mutation_refresh = false;
                view.last_scan = Instant::now();
                view.scan_count += 1;
                let mut sync_files = false;
                let mut search_scope_scanned = false;
                let mut head_changed = false;
                match result.0 {
                    Ok((snapshot, details, tree, tree_changed)) => {
                        search_scope_scanned = view.submodule_changes.is_none();
                        head_changed = view.repository.as_ref().is_none_or(|old| {
                            old.head != snapshot.head
                                || old.recent_commits.first() != snapshot.recent_commits.first()
                        });
                        let files_changed = tree_changed
                            || view
                                .repository
                                .as_ref()
                                .is_none_or(|old| old.changes != snapshot.changes);
                        view.repository = Some(snapshot);
                        view.details = Some(details);
                        view.tree = Some(tree);
                        sync_files = files_changed && view.submodule_changes.is_none();
                    }
                    Err(error) => view.notice = Some(format!("自动刷新失败：{error}")),
                }
                let mut scope_failed = false;
                if !view.submodule_loading
                    && view.submodule_request_id == scope_id
                    && view.submodule_changes.as_ref().map(|scope| &scope.target)
                        == scope_target.as_ref()
                    && let Some(result) = result.1
                {
                    let pending = view.scan_pending;
                    match result {
                        Ok((repository, tree, tree_changed, watcher)) => {
                            search_scope_scanned = true;
                            if let Some(scope) = view.submodule_changes.as_mut() {
                                sync_files |=
                                    tree_changed || scope.repository.changes != repository.changes;
                                scope.repository = repository;
                                scope.tree = tree;
                                if let Some(watcher) = watcher {
                                    scope.watcher_needs_restart = watcher.is_err() || pending;
                                    if let Err(error) = &watcher {
                                        view.notice =
                                            Some(format!("子模块监听不可用，将定期扫描：{error}"));
                                    }
                                    scope.watcher = watcher.ok().map(|mut watcher| {
                                        watcher.set_debounce(Duration::from_millis(
                                            view.settings.refresh.debounce_ms,
                                        ));
                                        watcher
                                    });
                                }
                            }
                        }
                        Err(error) => {
                            view.recover_parent_changes(cx);
                            view.notice =
                                Some(format!("子模块已不可用，已返回主仓库文件变更：{error}"));
                            scope_failed = true;
                        }
                    }
                }
                if sync_files {
                    view.sync_file_list(cx);
                }
                if view.submodule_changes.is_some() && !view.submodule_loading {
                    view.refresh_branch_context(cx);
                }
                // A file already marked modified can change again without any
                // tree/status difference. Revalidate content fingerprints too.
                if sync_files || (search_scope_scanned && view.content_search_open()) {
                    view.refresh_file_search(cx);
                }
                if !view.history
                    && !scope_failed
                    && let Some(path) = view.selected_file.clone()
                {
                    let revision = view.active_preview_key().and_then(|key| key.revision);
                    view.request_preview(path, view.selected_staged, revision, true, cx);
                }
                if head_changed {
                    view.history_head_changed(cx);
                }
                if std::mem::take(&mut view.scan_pending) {
                    view.queue_scan(cx);
                }
                cx.notify();
            });
        }));
    }
}

fn preview_error(error: &gitviews_service::ServiceError) -> String {
    use gitviews_service::{GitError, ServiceError};
    match error {
        ServiceError::Git(GitError::BinaryFile { path }) => format!(
            "二进制文件：{path}\n无法用文本视图比较内容。文件的 Git 状态、暂存及历史查询仍可使用。"
        ),
        ServiceError::Git(GitError::InvalidEncoding { path, encoding }) => {
            format!("无法将 {path} 作为 {encoding} 文本显示。文件的 Git 状态仍会保留。")
        }
        ServiceError::Git(GitError::FileTooLarge { path, limit_bytes }) => {
            format!(
                "{path} 超过文本预览的 {} MiB 限制，Git 状态不受影响。",
                limit_bytes / (1024 * 1024)
            )
        }
        _ => error.to_string(),
    }
}

/// Compare potentially large trees on the scan worker and preserve identity for
/// unchanged results, including consumers that cache indexes by Arc identity.
fn reuse_scanned_tree(
    previous: Option<Arc<RepositoryTree>>,
    next: RepositoryTree,
) -> (Arc<RepositoryTree>, bool) {
    match previous {
        Some(previous) if previous.as_ref() == &next => (previous, false),
        _ => (Arc::new(next), true),
    }
}

#[cfg(test)]
mod tests {
    use super::reuse_scanned_tree;
    use gitviews_service::{FileTreeEntry, FileTreeKind, RepositoryTree};
    use std::sync::Arc;

    #[test]
    fn unchanged_scan_preserves_tree_identity_and_changed_scan_replaces_it() {
        let previous = Arc::new(RepositoryTree::default());
        let (unchanged, changed) =
            reuse_scanned_tree(Some(previous.clone()), RepositoryTree::default());
        assert!(!changed);
        assert!(Arc::ptr_eq(&previous, &unchanged));

        let mut next = RepositoryTree::default();
        next.entries.push(FileTreeEntry {
            path: "new.rs".into(),
            kind: FileTreeKind::File,
            index_status: ' ',
            worktree_status: '?',
        });
        let (replacement, changed) = reuse_scanned_tree(Some(previous.clone()), next);
        assert!(changed);
        assert!(!Arc::ptr_eq(&previous, &replacement));
        assert_eq!(replacement.entries[0].path, "new.rs");
    }
}
