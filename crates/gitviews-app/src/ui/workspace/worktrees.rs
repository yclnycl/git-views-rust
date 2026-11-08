use super::*;

#[cfg(test)]
mod headless_tests;

impl Workspace {
    pub(super) fn modal_open(&self, cx: &App) -> bool {
        !self.git_engine_ready
            || self.git_engine_dialog.read(cx).is_open()
            || self.settings_open(cx)
            || self.repository_setup_dialog.read(cx).is_open()
            || self.git_settings_dialog.read(cx).is_open()
            || self.worktree_dialog.read(cx).is_open()
            || self.branch_dialog.read(cx).is_open()
            || self.stash_dialog.read(cx).is_open()
            || self.history_tools_dialog.read(cx).is_open()
            || self.conflict_dialog.read(cx).is_open()
    }

    pub(super) fn open_worktrees(
        &mut self,
        create: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        if self.modal_open(cx) || self.remote_blocks_navigation(cx) {
            return;
        }
        if self.preview || self.loading || self.catalog_import_count > 0 {
            self.notice = Some("请先打开并等候仓库读取完成。".into());
            cx.notify();
            return;
        }
        let Some(repo) = &self.repository else {
            self.notice = Some("请先打开一个本地仓库。".into());
            cx.notify();
            return;
        };
        let root = repo.root.clone();
        let details = self.details.clone().unwrap_or_default();
        self.close_file_search(cx);
        self.menu = None;
        self.show_about = false;
        self.active_input = None;
        self.focus.focus(window, cx);
        self.worktree_dialog.update(cx, |dialog, cx| {
            dialog.show(root, details, cx);
            if create {
                dialog.start_create(cx);
            }
        });
        self.reload_worktrees(cx);
        cx.notify();
    }

    pub(super) fn handle_worktree_event(
        &mut self,
        event: &WorktreeDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            WorktreeDialogEvent::Apply(action) => self.run_worktree(action.clone(), cx),
            WorktreeDialogEvent::Reload => self.reload_worktrees(cx),
            WorktreeDialogEvent::Close if !self.worktree_busy => {
                self.worktree_generation = self.worktree_generation.wrapping_add(1);
                self.worktree_task = None;
                self.worktree_dialog
                    .update(cx, |dialog, cx| dialog.clear(cx));
                self.focus.focus(window, cx);
                cx.notify();
            }
            WorktreeDialogEvent::Close => {}
        }
    }

    fn reload_worktrees(&mut self, cx: &mut Context<Self>) {
        if self.worktree_busy {
            return;
        }
        let Some(root) = self.repository.as_ref().map(|repo| repo.root.clone()) else {
            return;
        };
        self.worktree_busy = true;
        self.worktree_generation = self.worktree_generation.wrapping_add(1);
        let generation = self.worktree_generation;
        let repository_id = self.request_id;
        let service = Arc::clone(&self.service);
        self.worktree_dialog
            .update(cx, |dialog, cx| dialog.set_busy(true, cx));
        self.worktree_task = Some(cx.spawn(async move |this, cx| {
            let result = tasks::spawn(
                cx.background_executor(),
                "worktree.list",
                generation,
                move || service.details(&root),
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.worktree_generation != generation || view.request_id != repository_id {
                    return;
                }
                view.worktree_task = None;
                view.worktree_busy = false;
                match result {
                    Ok(details) => {
                        view.details = Some(details.clone());
                        view.worktree_dialog
                            .update(cx, |dialog, cx| dialog.refresh_details(details, cx));
                    }
                    Err(error) => view.worktree_dialog.update(cx, |dialog, cx| {
                        dialog.set_error(Some(error.to_string()), cx)
                    }),
                }
                view.worktree_dialog
                    .update(cx, |dialog, cx| dialog.set_busy(false, cx));
                if view.scan_pending {
                    view.queue_scan(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn run_worktree(&mut self, action: WorktreeAction, cx: &mut Context<Self>) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        if self.worktree_busy
            || self.remote_job.is_some()
            || self.mutation_task.is_some()
            || self.mutation_refresh
        {
            return;
        }
        let Some(repository) = &self.repository else {
            return;
        };
        let root = repository.root.clone();
        let expected_commit = match &repository.head {
            Head::Detached(id) => Some(id.clone()),
            _ => repository
                .recent_commits
                .first()
                .map(|commit| commit.id.clone()),
        };
        let request = WorktreeRequest {
            expected_head: repository.head.clone(),
            expected_commit,
            action,
        };
        self.pause_repository_reads();
        self.worktree_busy = true;
        self.worktree_generation = self.worktree_generation.wrapping_add(1);
        let generation = self.worktree_generation;
        let repository_id = self.request_id;
        let service = Arc::clone(&self.service);
        self.worktree_dialog
            .update(cx, |dialog, cx| dialog.set_busy(true, cx));
        self.worktree_task = Some(cx.spawn(async move |this, cx| {
            let (result, details) = tasks::spawn(cx.background_executor(), "worktree.manage", generation, move || {
                let result = service.worktree(&root, &request);
                let details = service.details(&root);
                (result, details)
            }).await;
            let _ = this.update(cx, |view, cx| {
                if view.worktree_generation != generation || view.request_id != repository_id { return; }
                view.worktree_task = None;
                view.worktree_busy = false;
                view.pause_repository_reads();
                view.worktree_dialog.update(cx, |dialog, cx| dialog.set_busy(false, cx));
                let refresh_error = match details {
                    Ok(details) => {
                        view.details = Some(details.clone());
                        view.worktree_dialog.update(cx, |dialog, cx| {
                            if result.is_ok() { dialog.replace_details(details, cx); }
                            else { dialog.refresh_details(details, cx); }
                        });
                        None
                    }
                    Err(error) => Some(format!("工作树列表刷新失败：{error}")),
                };
                match result {
                    Ok(outcome) => {
                        if let Some(previous) = &outcome.previous_path {
                            let registered = view.catalog.repositories.iter().any(|entry| paths_equal(&entry.root, previous));
                            if registered {
                                view.catalog.remove(previous);
                                if let Some(path) = &outcome.path {
                                    let name = path.file_name().map_or_else(|| display_path(path), |name| name.to_string_lossy().into_owned());
                                    if let Err(error) = view.catalog.register(path.clone(), name) { view.catalog_error = Some(error.to_string()); }
                                }
                                view.save_catalog(cx);
                            }
                        }
                        view.notice = Some(outcome.message);
                        if let Some(error) = refresh_error { view.worktree_dialog.update(cx, |dialog, cx| dialog.set_error(Some(error), cx)); }
                        tracing::info!(target: "gitviews::worktrees", generation, "worktree_operation_complete");
                    }
                    Err(error) => {
                        view.worktree_dialog.update(cx, |dialog, cx| dialog.set_error(Some(error.to_string()), cx));
                        tracing::warn!(target: "gitviews::worktrees", generation, kind = ?error.kind, "worktree_operation_failed");
                    }
                }
                view.invalidate_changes();
                view.scan_pending = false;
                view.queue_scan(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }
}
