use super::*;
use gitviews_service::{GitEnginePreferences, detect_system_git};

#[cfg(test)]
mod headless_tests;

impl Workspace {
    pub(super) fn initialize_git_engine(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.read_only {
            self.git_engine_ready = true;
            self.restore_repositories(paths, cx);
            return;
        }
        self.git_engine_initial_paths = Some(paths);
        let store = Arc::clone(&self.git_engine_store);
        let service = Arc::clone(&self.service);
        self.git_engine_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = tasks::spawn(cx.background_executor(), "git_engine.load", 0, move || {
                let preferences = store
                    .load()
                    .map_err(|error| (error, true, GitEnginePreferences::default()))?;
                if preferences.engine.is_some() {
                    service
                        .configure_git_engine(&preferences)
                        .map_err(|error| (error, false, preferences.clone()))?;
                }
                Ok::<_, (gitviews_service::GitEngineError, bool, GitEnginePreferences)>(preferences)
            })
            .await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.git_engine_task = None;
                match result {
                    Ok(preferences) if preferences.engine.is_some() => {
                        view.finish_git_engine_startup(window, cx)
                    }
                    result => {
                        let (preferences, failure) = match result {
                            Ok(preferences) => (preferences, None),
                            Err((error, recoverable, preferences)) => {
                                (preferences, Some((error, recoverable)))
                            }
                        };
                        let path = display_path(view.git_engine_store.path());
                        view.git_engine_dialog.update(cx, |dialog, cx| {
                            dialog.show(preferences, true, cx);
                            if let Some((error, recoverable)) = failure {
                                dialog.set_error(format!("{error} 配置文件：{path}"), cx);
                                dialog.set_recovery_available(recoverable, cx);
                            }
                        });
                        cx.notify();
                    }
                }
            });
        }));
    }

    fn finish_git_engine_startup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.git_engine_ready = true;
        self.git_engine_dialog
            .update(cx, |dialog, cx| dialog.clear(cx));
        self.focus.focus(window, cx);
        if let Some(paths) = self.git_engine_initial_paths.take()
            && !self.catalog_ready
            && self.startup_task.is_none()
        {
            self.restore_repositories(paths, cx);
        }
        cx.notify();
    }

    pub(super) fn open_git_engine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        if self.modal_open(cx)
            || self.remote_blocks_navigation(cx)
            || self.git_engine_task.is_some()
        {
            return;
        }
        self.close_file_search(cx);
        self.menu = None;
        self.active_input = None;
        self.focus.focus(window, cx);
        let preferences = self.service.git_engine_preferences();
        self.git_engine_dialog
            .update(cx, |dialog, cx| dialog.show(preferences, false, cx));
        cx.notify();
    }

    pub(super) fn handle_git_engine_event(
        &mut self,
        event: &GitEngineEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.git_engine_task.is_some() {
            return;
        }
        match event {
            GitEngineEvent::Recover => {
                let store = Arc::clone(&self.git_engine_store);
                self.git_engine_dialog
                    .update(cx, |dialog, cx| dialog.set_busy(true, cx));
                self.git_engine_task = Some(cx.spawn_in(window, async move |this, cx| {
                    let result = tasks::spawn(
                        cx.background_executor(),
                        "git_engine.recover",
                        0,
                        move || store.backup_invalid(),
                    )
                    .await;
                    let _ = this.update_in(cx, |view, _, cx| {
                        view.git_engine_task = None;
                        let required = !view.git_engine_ready;
                        let path = display_path(view.git_engine_store.path());
                        view.git_engine_dialog
                            .update(cx, |dialog, cx| match result {
                                Ok(backup) => {
                                    dialog.show(GitEnginePreferences::default(), required, cx);
                                    dialog.set_recovered(backup, cx);
                                }
                                Err(error) => {
                                    dialog.set_error(format!("{error} 配置文件：{path}"), cx);
                                    dialog.set_recovery_available(true, cx);
                                }
                            });
                        cx.notify();
                    });
                }));
            }
            GitEngineEvent::Detect => {
                self.git_engine_task = Some(cx.spawn(async move |this, cx| {
                    let candidates = tasks::spawn(
                        cx.background_executor(),
                        "git_engine.detect",
                        0,
                        detect_system_git,
                    )
                    .await;
                    let _ = this.update(cx, |view, cx| {
                        view.git_engine_task = None;
                        view.git_engine_dialog
                            .update(cx, |dialog, cx| dialog.set_candidates(candidates, cx));
                    });
                }));
            }
            GitEngineEvent::Browse => {
                let receiver = cx.prompt_for_paths(PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: false,
                    prompt: Some("选择 Git 可执行文件 git.exe".into()),
                });
                self.git_engine_dialog
                    .update(cx, |dialog, cx| dialog.set_busy(true, cx));
                self.git_engine_task = Some(cx.spawn_in(window, async move |this, cx| {
                    let result = receiver.await;
                    let _ = this.update_in(cx, |view, window, cx| {
                        view.git_engine_task = None;
                        view.git_engine_dialog.update(cx, |dialog, cx| {
                            dialog.set_busy(false, cx);
                            match result {
                                Ok(Ok(Some(paths))) => {
                                    if let Some(path) = paths.into_iter().next() {
                                        dialog.select_executable(path, window, cx);
                                    }
                                }
                                Ok(Ok(None)) => {}
                                _ => dialog.set_error(
                                    "无法打开 Git 文件选择器，请直接填写路径。".into(),
                                    cx,
                                ),
                            }
                        });
                    });
                }));
            }
            GitEngineEvent::Apply(preferences) => {
                let preferences = preferences.clone();
                let store = Arc::clone(&self.git_engine_store);
                let service = Arc::clone(&self.service);
                self.git_engine_task = Some(cx.spawn_in(window, async move |this, cx| {
                    let (result, recovery_available) =
                        tasks::spawn(cx.background_executor(), "git_engine.save", 0, move || {
                            let result = service.save_git_engine(&store, &preferences);
                            let recovery_available = result.is_err() && store.load().is_err();
                            (result, recovery_available)
                        })
                        .await;
                    let _ = this.update_in(cx, |view, window, cx| {
                        view.git_engine_task = None;
                        match result {
                            Ok(()) => {
                                view.finish_git_engine_startup(window, cx);
                                view.notice = Some(
                                    "Git 方案已保存并启用。可在“仓库 → Git 方案”修改。".into(),
                                );
                            }
                            Err(error) => view.git_engine_dialog.update(cx, |dialog, cx| {
                                dialog.set_error(
                                    format!(
                                        "{error}\n配置文件：{}",
                                        display_path(view.git_engine_store.path())
                                    ),
                                    cx,
                                );
                                dialog.set_recovery_available(recovery_available, cx);
                            }),
                        }
                        cx.notify();
                    });
                }));
            }
            GitEngineEvent::Close => {
                self.focus.focus(window, cx);
                cx.notify();
            }
        }
    }
}
