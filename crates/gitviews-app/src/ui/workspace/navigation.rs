use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NavigationKind {
    Repository,
    Submodule,
}

struct ReadProgress {
    kind: NavigationKind,
    target: PathBuf,
    phase: &'static str,
    started: Instant,
}

#[derive(Default)]
pub(super) struct NavigationRead {
    generation: u64,
    cancel: Arc<AtomicBool>,
    progress: Option<ReadProgress>,
    tick: Option<Task<()>>,
}

impl Drop for NavigationRead {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

impl Workspace {
    pub(super) fn stop_navigation(&mut self) {
        self.navigation.cancel.store(true, Ordering::Release);
        self.navigation.progress = None;
        self.navigation.tick = None;
    }

    pub(super) fn stop_submodule_navigation(&mut self) {
        if self
            .navigation
            .progress
            .as_ref()
            .is_some_and(|progress| progress.kind == NavigationKind::Submodule)
        {
            self.stop_navigation();
        }
    }

    pub(super) fn begin_navigation(
        &mut self,
        kind: NavigationKind,
        target: PathBuf,
        cx: &mut Context<Self>,
    ) -> Arc<AtomicBool> {
        self.navigation.cancel.store(true, Ordering::Release);
        self.navigation.generation = self.navigation.generation.wrapping_add(1);
        let generation = self.navigation.generation;
        tracing::info!(target: "gitviews::navigation", generation, path = %display_path(&target), "navigation_started");
        self.navigation.cancel = Arc::new(AtomicBool::new(false));
        self.navigation.progress = Some(ReadProgress {
            kind,
            target,
            phase: "识别仓库与当前分支",
            started: Instant::now(),
        });
        self.navigation.tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                if !matches!(
                    this.update(cx, |view, cx| {
                        if view.navigation.generation != generation
                            || view.navigation.progress.is_none()
                        {
                            return false;
                        }
                        cx.notify();
                        true
                    }),
                    Ok(true)
                ) {
                    break;
                }
            }
        }));
        cx.notify();
        self.navigation.cancel.clone()
    }

    pub(super) fn navigation_phase(&mut self, phase: &'static str, cx: &mut Context<Self>) {
        if let Some(progress) = &mut self.navigation.progress {
            progress.phase = phase;
            tracing::debug!(target: "gitviews::navigation", phase, elapsed_ms = progress.started.elapsed().as_millis() as u64, "navigation_phase");
            cx.notify();
        }
    }

    pub(super) fn finish_navigation(&mut self, kind: NavigationKind, cx: &mut Context<Self>) {
        if self
            .navigation
            .progress
            .as_ref()
            .is_some_and(|progress| progress.kind == kind)
        {
            self.navigation.cancel.store(true, Ordering::Release);
            self.navigation.progress = None;
            self.navigation.tick = None;
            cx.notify();
        }
    }

    pub(super) fn navigation_loading(&self) -> bool {
        self.navigation.progress.is_some()
    }

    pub(super) fn navigation_progress_panel(
        &self,
        light: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let progress = self
            .navigation
            .progress
            .as_ref()
            .expect("render while loading");
        let text = format!(
            "{} · 已等待 {:.1} 秒",
            progress.phase,
            progress.started.elapsed().as_secs_f32()
        );
        div()
            .id("navigation-progress")
            .test_support()
            .role(Role::Status)
            .aria_label(text.clone())
            .absolute()
            .top(px(106.))
            .right(px(16.))
            .w(px(450.))
            .max_w(relative(0.9))
            .p(px(14.))
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(if light { 0xd0d7de } else { 0x3d444d }))
            .bg(rgb(if light { 0xffffff } else { 0x212830 }))
            .shadow_md()
            .occlude()
            .child(if progress.kind == NavigationKind::Submodule {
                "正在切换子仓库"
            } else {
                "正在打开仓库 / 工作树"
            })
            .child(
                div()
                    .mt(px(7.))
                    .text_size(px(11.))
                    .child(display_path(&progress.target)),
            )
            .child(div().mt(px(7.)).text_size(px(12.)).child(text))
            .child(
                div()
                    .mt(px(7.))
                    .text_size(px(11.))
                    .child("可继续选择其他仓库或工作树，旧请求的后续读取会停止。"),
            )
            .child(
                div()
                    .id("cancel-navigation")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("取消仓库读取")
                    .mt(px(10.))
                    .cursor_pointer()
                    .child("取消读取")
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_navigation(cx))),
            )
    }

    pub(super) fn cancel_navigation(&mut self, cx: &mut Context<Self>) {
        let Some(progress) = self.navigation.progress.take() else {
            return;
        };
        self.navigation.cancel.store(true, Ordering::Release);
        self.navigation.tick = None;
        if progress.kind == NavigationKind::Submodule {
            self.show_parent_changes(cx);
        } else {
            self.request_id = self.request_id.wrapping_add(1);
            self.load_task = None;
            self.loading = false;
            self.repository = None;
            self.details = None;
            self.tree = None;
            self.file_list.update(cx, |list, cx| list.clear(cx));
        }
        self.notice = Some("已取消读取，可重新选择仓库或点击刷新重试。".into());
        cx.notify();
    }

    pub(super) fn read_repository(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let started = Instant::now();
        self.scan_cancel.store(true, Ordering::Release);
        self.scan_epoch = self.scan_epoch.wrapping_add(1);
        self.requested_path = Some(path.clone());
        self.loading = true;
        self.notice = None;
        let cancel = self.begin_navigation(NavigationKind::Repository, path.clone(), cx);
        let service = Arc::clone(&self.service);
        let request_id = self.request_id;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let worker = service.clone();
            let stopped = cancel.clone();
            let metadata = tasks::spawn(
                cx.background_executor(),
                "repository.metadata",
                request_id,
                move || {
                    check_cancel(&stopped)?;
                    worker.open_metadata(&path)
                },
            )
            .await;
            let root = match metadata {
                Ok(repository) => {
                    let root = repository.root.clone();
                    if !matches!(
                        this.update(cx, |view, cx| {
                            if view.request_id != request_id || cancel.load(Ordering::Acquire) {
                                return false;
                            }
                            view.requested_path = Some(repository.root.clone());
                            view.repository = Some(repository);
                            view.navigation_phase("读取工作树与分支", cx);
                            true
                        }),
                        Ok(true)
                    ) {
                        return;
                    }
                    root
                }
                Err(error) => {
                    let _ = this.update(cx, |view, cx| {
                        view.repository_read_failed(request_id, error, cx)
                    });
                    return;
                }
            };

            let worker = service.clone();
            let worker_root = root.clone();
            let stopped = cancel.clone();
            let details = tasks::spawn(
                cx.background_executor(),
                "repository.details",
                request_id,
                move || {
                    check_cancel(&stopped)?;
                    worker.details(&worker_root)
                },
            )
            .await;
            if !matches!(
                this.update(cx, |view, cx| {
                    if view.request_id != request_id || cancel.load(Ordering::Acquire) {
                        return false;
                    }
                    match details {
                        Ok(details) => view.details = Some(details),
                        Err(error) => view.notice = Some(format!("分支和工作树读取失败：{error}")),
                    }
                    view.navigation_phase("读取近期提交并检查暂存区、工作区与未跟踪文件", cx);
                    true
                }),
                Ok(true)
            ) {
                return;
            }

            let worker = service.clone();
            let worker_root = root.clone();
            let stopped = cancel.clone();
            let snapshot = tasks::spawn(
                cx.background_executor(),
                "repository.open",
                request_id,
                move || {
                    check_cancel(&stopped)?;
                    worker.open(&worker_root)
                },
            )
            .await;
            let changes = match snapshot {
                Ok(repository) => {
                    let changes = repository.changes.clone();
                    if !matches!(
                        this.update(cx, |view, cx| {
                            if view.request_id != request_id || cancel.load(Ordering::Acquire) {
                                return false;
                            }
                            view.repository = Some(repository);
                            view.sync_file_list(cx);
                            view.navigation_phase("构建文件树与子模块列表", cx);
                            true
                        }),
                        Ok(true)
                    ) {
                        return;
                    }
                    changes
                }
                Err(error) => {
                    let _ = this.update(cx, |view, cx| {
                        view.repository_read_failed(request_id, error, cx)
                    });
                    return;
                }
            };
            let stopped = cancel.clone();
            let tree = tasks::spawn(
                cx.background_executor(),
                "repository.tree",
                request_id,
                move || {
                    check_cancel(&stopped)?;
                    service.tree_with_changes(&root, &changes)
                },
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.request_id != request_id || cancel.load(Ordering::Acquire) {
                    return;
                }
                view.load_task = None;
                view.loading = false;
                view.finish_navigation(NavigationKind::Repository, cx);
                let tree_loaded = tree.is_ok();
                match tree {
                    Ok(tree) => view.tree = Some(Arc::new(tree)),
                    Err(error) => view.notice = Some(format!("文件树读取失败：{error}")),
                }
                if let Some(repository) = view.repository.take() {
                    view.register_repository(&repository, cx);
                    view.repository = Some(repository);
                }
                view.sync_file_list(cx);
                view.start_watching(cx);
                view.select_first_change(cx);
                tracing::info!(target: "gitviews::performance", request_id,
                    load_ms = started.elapsed().as_millis() as u64,
                    files = view.tree.as_ref().map_or(0, |tree| tree.entries.len()),
                    changes = view.repository.as_ref().map_or(0, |repository| repository.changes.len()),
                    tree_loaded, "repository_ready");
                if std::mem::take(&mut view.scan_pending) {
                    view.queue_scan(cx);
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn read_submodule(
        &mut self,
        parent: PathBuf,
        target: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.file_search.pending_open = None;
        self.pause_repository_reads();
        self.submodule_request_id = self.submodule_request_id.wrapping_add(1);
        self.submodule_cancel.store(true, Ordering::Release);
        let cancel = self.begin_navigation(NavigationKind::Submodule, target.clone(), cx);
        self.submodule_cancel = cancel.clone();
        let scope_id = self.submodule_request_id;
        let repository_id = self.request_id;
        let started = Instant::now();
        tracing::info!(target: "gitviews::submodule", request_id = scope_id, parent = %display_path(&parent), path = %display_path(&target), "submodule_select");
        let service = self.service.clone();
        self.submodule_loading = true;
        self.notice = None;
        self.submodule_task = Some(cx.spawn(async move |this, cx| {
            let worker = service.clone();
            let worker_parent = parent.clone();
            let worker_target = target.clone();
            let stopped = cancel.clone();
            let metadata = tasks::spawn(
                cx.background_executor(),
                "submodule.metadata",
                scope_id,
                move || {
                    worker.submodule_metadata_with_cancel(&worker_parent, &worker_target, &stopped)
                },
            )
            .await;
            let root = match metadata {
                Ok(repository) => {
                    let root = repository.root.clone();
                    if !matches!(
                        this.update(cx, |view, cx| {
                            if view.request_id != repository_id
                                || view.submodule_request_id != scope_id
                                || cancel.load(Ordering::Acquire)
                            {
                                return false;
                            }
                            view.clear_changes_preview(cx);
                            view.submodule_watch_task = None;
                            view.submodule_changes = Some(SubmoduleChanges {
                                parent_root: parent,
                                target,
                                repository,
                                tree: Arc::new(RepositoryTree::default()),
                                watcher: None,
                                watcher_needs_restart: false,
                            });
                            view.refresh_branch_context(cx);
                            view.sync_file_list(cx);
                            view.navigation_phase("读取子仓库近期提交并检查文件变更", cx);
                            true
                        }),
                        Ok(true)
                    ) {
                        return;
                    }
                    root
                }
                Err(error) => {
                    let _ = this.update(cx, |view, cx| {
                        view.submodule_read_failed(repository_id, scope_id, error, cx)
                    });
                    return;
                }
            };
            let worker = service.clone();
            let worker_root = root.clone();
            let stopped = cancel.clone();
            let overview = tasks::spawn(
                cx.background_executor(),
                "submodule.overview",
                scope_id,
                move || {
                    check_cancel(&stopped)?;
                    worker.validate_root(&worker_root)?;
                    let repository = worker.open(&worker_root)?;
                    worker.validate_root(&worker_root)?;
                    check_cancel(&stopped)?;
                    Ok::<_, gitviews_service::ServiceError>(repository)
                },
            )
            .await;
            let changes = match overview {
                Ok(repository) => {
                    let changes = repository.changes.clone();
                    if !matches!(
                        this.update(cx, |view, cx| {
                            if view.request_id != repository_id
                                || view.submodule_request_id != scope_id
                                || cancel.load(Ordering::Acquire)
                            {
                                return false;
                            }
                            if let Some(scope) = &mut view.submodule_changes {
                                scope.repository = repository;
                            }
                            view.sync_file_list(cx);
                            view.navigation_phase("构建子仓库文件树", cx);
                            true
                        }),
                        Ok(true)
                    ) {
                        return;
                    }
                    changes
                }
                Err(error) => {
                    let _ = this.update(cx, |view, cx| {
                        view.submodule_read_failed(repository_id, scope_id, error, cx)
                    });
                    return;
                }
            };
            let stopped = cancel.clone();
            let tree = tasks::spawn(
                cx.background_executor(),
                "submodule.tree",
                scope_id,
                move || {
                    check_cancel(&stopped)?;
                    service.validate_root(&root)?;
                    let tree = service.tree_with_changes(&root, &changes)?;
                    service.validate_root(&root)?;
                    check_cancel(&stopped)?;
                    service.invalidate(&root);
                    Ok::<_, gitviews_service::ServiceError>(tree)
                },
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.request_id != repository_id
                    || view.submodule_request_id != scope_id
                    || cancel.load(Ordering::Acquire)
                {
                    return;
                }
                view.submodule_task = None;
                view.submodule_loading = false;
                view.finish_navigation(NavigationKind::Submodule, cx);
                match tree {
                    Ok(tree) => {
                        let apply_started = Instant::now();
                        let files = tree.entries.len();
                        if let Some(scope) = &mut view.submodule_changes {
                            scope.tree = Arc::new(tree);
                        }
                        view.sync_file_list(cx);
                        view.select_first_change(cx);
                        view.start_submodule_watching(cx);
                        view.complete_search_navigation(cx);
                        let changes = view.submodule_changes.as_ref().map_or(0, |scope| scope.repository.changes.len());
                        tracing::info!(target: "gitviews::submodule", request_id = scope_id, files, changes, total_ms = started.elapsed().as_millis() as u64, ui_apply_ms = apply_started.elapsed().as_millis() as u64, "submodule_ready");
                    }
                    Err(error) => {
                        view.recover_parent_changes(cx);
                        view.notice = Some(format!("无法读取子仓库文件树：{error}"));
                    }
                }
                if std::mem::take(&mut view.scan_pending) {
                    view.queue_scan(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn submodule_read_failed(
        &mut self,
        repository_id: u64,
        scope_id: u64,
        error: gitviews_service::ServiceError,
        cx: &mut Context<Self>,
    ) {
        if self.request_id != repository_id || self.submodule_request_id != scope_id {
            return;
        }
        self.submodule_task = None;
        self.submodule_loading = false;
        self.finish_navigation(NavigationKind::Submodule, cx);
        self.recover_parent_changes(cx);
        self.notice = Some(format!("无法读取子仓库：{error}"));
        cx.notify();
    }

    fn repository_read_failed(
        &mut self,
        request_id: u64,
        error: gitviews_service::ServiceError,
        cx: &mut Context<Self>,
    ) {
        if self.request_id != request_id {
            return;
        }
        self.load_task = None;
        self.loading = false;
        self.repository = None;
        self.details = None;
        self.tree = None;
        self.file_list.update(cx, |list, cx| list.clear(cx));
        self.finish_navigation(NavigationKind::Repository, cx);
        self.notice = Some(format!("仓库读取失败：{error}"));
        cx.notify();
    }
}

pub(super) fn check_cancel(cancel: &AtomicBool) -> Result<(), gitviews_service::ServiceError> {
    if cancel.load(Ordering::Acquire) {
        Err(gitviews_service::ServiceError::Cancelled {
            operation: "read repository",
        })
    } else {
        Ok(())
    }
}
