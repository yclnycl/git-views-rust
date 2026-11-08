use super::*;

impl Workspace {
    pub(super) fn restore_repositories(
        &mut self,
        initial_paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        self.pending_repositories.extend(initial_paths);
        self.loading = true;
        let store = Arc::clone(&self.catalog_store);
        self.startup_task = Some(cx.spawn(async move |this, cx| {
            let result = tasks::spawn(cx.background_executor(), "catalog.load", 0, move || {
                store.load()
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                view.catalog_ready = true;
                view.loading = false;
                match result {
                    Ok(catalog) => view.catalog = catalog,
                    Err(error) => {
                        // Keep a damaged catalog intact while allowing this session to open repos.
                        view.catalog_write_allowed = false;
                        view.catalog_error = Some(format!("仓库列表无法恢复：{error}"));
                        view.notice = view.catalog_error.clone();
                    }
                }
                let pending = std::mem::take(&mut view.pending_repositories);
                if !pending.is_empty() {
                    view.add_repositories(pending, cx);
                } else if let Some(path) = view.catalog.active.clone().or_else(|| {
                    view.catalog
                        .repositories
                        .first()
                        .map(|entry| entry.root.clone())
                }) {
                    view.load(path, cx);
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn add_repositories(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.remote_blocks_navigation(cx) {
            return;
        }
        if paths.is_empty() {
            return;
        }
        self.preview = false;
        if !self.catalog_ready {
            // Restore existing entries before accepting imports, including when
            // a user opens a repository directly from the static design preview.
            self.pending_repositories.splice(0..0, paths);
            if self.startup_task.is_none() {
                self.restore_repositories(Vec::new(), cx);
            }
            cx.notify();
            return;
        }
        if self.navigation_loading() {
            self.cancel_navigation(cx);
        }
        self.loading = true;
        self.menu = None;
        self.notice = None;
        self.scan_cancel
            .store(true, std::sync::atomic::Ordering::Release);
        self.request_id = self.request_id.wrapping_add(1);
        let request_id = self.request_id;
        self.load_task = None;
        self.diff_request_id = self.diff_request_id.wrapping_add(1);
        self.diff_task = None;
        self.diff_loading = false;
        self.catalog_import_count += 1;
        let service = Arc::clone(&self.service);
        let previous = self.catalog_import_task.take();
        self.catalog_import_task = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let results = tasks::spawn(
                cx.background_executor(),
                "catalog.import",
                request_id,
                move || {
                    paths
                        .into_iter()
                        .map(|path| {
                            let result = service.open_metadata(&path);
                            (path, result)
                        })
                        .collect::<Vec<_>>()
                },
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                let activate = view.request_id == request_id;
                view.catalog_import_count -= 1;
                if activate {
                    view.loading = false;
                }
                let mut first = None;
                let mut errors = Vec::new();
                for (path, result) in results {
                    match result {
                        Ok(repository) => {
                            match view
                                .catalog
                                .register(repository.root.clone(), repository.name)
                            {
                                Ok(_) => {
                                    first.get_or_insert(repository.root);
                                }
                                Err(error) => errors.push(error.to_string()),
                            }
                        }
                        Err(error) => errors.push(format!("{}：{error}", display_path(&path))),
                    }
                }
                if let Some(path) = first {
                    if activate {
                        view.catalog.set_active(&path);
                    }
                    view.save_catalog(cx);
                    if activate {
                        view.load(path, cx);
                    }
                }
                if !errors.is_empty() {
                    view.notice = Some(errors.join("\n"));
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn register_repository(
        &mut self,
        repository: &RepositorySnapshot,
        cx: &mut Context<Self>,
    ) {
        let before = self.catalog.clone();
        match self
            .catalog
            .register(repository.root.clone(), repository.name.clone())
        {
            Ok(_) => {
                self.catalog.set_active(&repository.root);
                if self.catalog != before {
                    self.save_catalog(cx);
                }
            }
            Err(error) => self.notice = Some(format!("仓库列表未更新：{error}")),
        }
    }

    pub(super) fn save_catalog(&mut self, cx: &mut Context<Self>) {
        if !self.catalog_write_allowed {
            return;
        }
        let catalog = self.catalog.clone();
        let store = Arc::clone(&self.catalog_store);
        let previous = self.catalog_save_task.take();
        self.catalog_save_task = Some(cx.spawn(async move |this, cx| {
            // Serialize snapshots so an older save cannot replace the latest selection.
            if let Some(previous) = previous {
                previous.await;
            }
            let result = tasks::spawn(cx.background_executor(), "catalog.save", 0, move || {
                store.save(&catalog)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                if let Err(error) = result {
                    view.catalog_error = Some(format!("仓库列表未保存：{error}"));
                    view.notice = view.catalog_error.clone();
                } else {
                    view.catalog_error = None;
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn remove_repository(&mut self, root: &std::path::Path, cx: &mut Context<Self>) {
        if self.remote_blocks_navigation(cx) {
            return;
        }
        let removing_current = self
            .repository
            .as_ref()
            .is_some_and(|repo| paths_equal(&repo.root, root))
            || self
                .requested_path
                .as_deref()
                .is_some_and(|path| paths_equal(path, root));
        if !self.catalog.remove(root) {
            return;
        }
        self.save_catalog(cx);
        if removing_current {
            self.stop_navigation();
            self.scan_cancel
                .store(true, std::sync::atomic::Ordering::Release);
            self.reset_history(cx);
            self.clear_tabs(cx);
            self.close_file_search(cx);
            self.discard_submodule();
            self.request_id = self.request_id.wrapping_add(1);
            self.diff_request_id = self.diff_request_id.wrapping_add(1);
            self.load_task = None;
            self.diff_task = None;
            self.loading = false;
            self.diff_loading = false;
            self.repository = None;
            self.details = None;
            self.tree = None;
            self.diff = None;
            self.source_file = None;
            self.watcher = None;
            self.watch_task = None;
            self.scan_task = None;
            self.scan_pending = false;
            self.history_request_id = self.history_request_id.wrapping_add(1);
            self.history_task = None;
            self.reset_history(cx);
            self.commit_task = None;
            self.history_commits.clear();
            self.selected_commit = None;
            self.preview_pane.update(cx, |pane, cx| pane.clear(cx));
            self.file_list.update(cx, |list, cx| list.clear(cx));
            self.requested_path = None;
            self.selected_file = None;
            self.opened_files.clear();
            self.collapsed_paths.clear();
            if let Some(next) = self.catalog.active.clone() {
                self.load(next, cx);
            }
        }
        cx.notify();
    }
}
