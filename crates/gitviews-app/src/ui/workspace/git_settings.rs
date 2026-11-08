use super::*;

impl Workspace {
    pub(super) fn open_git_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let Some(root) = self.changes_repository().map(|repo| repo.root.clone()) else {
            self.notice = Some("请先打开一个本地仓库。".into());
            cx.notify();
            return;
        };
        self.close_file_search(cx);
        self.menu = None;
        self.show_about = false;
        self.active_input = None;
        self.focus.focus(window, cx);
        self.git_settings_dialog
            .update(cx, |dialog, cx| dialog.show(root.clone(), cx));
        self.git_settings_generation = self.git_settings_generation.wrapping_add(1);
        let generation = self.git_settings_generation;
        let repository_id = self.request_id;
        let service = Arc::clone(&self.service);
        self.git_settings_task = Some(cx.spawn(async move |this, cx| {
            let result = tasks::spawn(
                cx.background_executor(),
                "git_settings.load",
                generation,
                move || service.git_settings(&root),
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if generation != view.git_settings_generation || repository_id != view.request_id {
                    return;
                }
                view.git_settings_task = None;
                view.git_settings_dialog
                    .update(cx, |dialog, cx| match result {
                        Ok(settings) => dialog.set_settings(settings, cx),
                        Err(error) => dialog.set_error(Some(error.to_string()), cx),
                    });
            });
        }));
        cx.notify();
    }

    pub(super) fn handle_git_settings_event(
        &mut self,
        event: &GitSettingsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            GitSettingsEvent::Apply(settings) if !self.git_settings_saving => {
                if !self.advanced_mode_ready(cx) {
                    return;
                }
                let Some(root) = self.changes_repository().map(|repo| repo.root.clone()) else {
                    return;
                };
                self.cancel_ssh_discovery();
                self.git_settings_generation = self.git_settings_generation.wrapping_add(1);
                let generation = self.git_settings_generation;
                let repository_id = self.request_id;
                let settings = settings.clone();
                let service = Arc::clone(&self.service);
                self.git_settings_saving = true;
                self.git_settings_dialog
                    .update(cx, |dialog, cx| dialog.set_saving(true, cx));
                self.git_settings_task = Some(cx.spawn_in(window, async move |this, cx| {
                    let result = tasks::spawn(
                        cx.background_executor(),
                        "git_settings.save",
                        generation,
                        move || service.save_git_settings(&root, &settings),
                    )
                    .await;
                    let _ = this.update_in(cx, |view, window, cx| {
                        if generation != view.git_settings_generation
                            || repository_id != view.request_id
                        {
                            return;
                        }
                        view.git_settings_task = None;
                        view.git_settings_saving = false;
                        match result {
                            Ok(()) => {
                                view.git_settings_dialog
                                    .update(cx, |dialog, cx| dialog.clear(cx));
                                view.restore_after_git_settings(window, cx);
                                view.notice =
                                    Some("已保存当前仓库的 Git 设置，下次连接立即使用。".into());
                                view.queue_scan(cx);
                            }
                            Err(error) => view.git_settings_dialog.update(cx, |dialog, cx| {
                                dialog.set_error(Some(error.to_string()), cx);
                            }),
                        }
                        cx.notify();
                    });
                }));
            }
            GitSettingsEvent::DiscoverSshKeys if !self.git_settings_saving => {
                self.discover_ssh_keys(true, cx);
            }
            GitSettingsEvent::Close if !self.git_settings_saving => {
                self.cancel_ssh_discovery();
                self.git_settings_generation = self.git_settings_generation.wrapping_add(1);
                self.git_settings_task = None;
                self.git_settings_dialog
                    .update(cx, |dialog, cx| dialog.clear(cx));
                self.restore_after_git_settings(window, cx);
                cx.notify();
            }
            _ => {}
        }
    }

    fn restore_after_git_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.git_settings_return_to_app) {
            self.settings_dialog
                .update(cx, |dialog, cx| dialog.resume(window, cx));
        } else {
            self.focus.focus(window, cx);
        }
    }

    pub(super) fn cancel_ssh_discovery(&mut self) {
        self.ssh_discovery_id = self.ssh_discovery_id.wrapping_add(1);
        self.ssh_discovery_task = None;
    }

    /// Invoked only by the two dialogs' explicit consent events. Merely opening
    /// a repository, settings, or an authentication prompt performs no scan.
    pub(super) fn discover_ssh_keys(&mut self, settings: bool, cx: &mut Context<Self>) {
        if self.ssh_discovery_task.is_some() {
            return;
        }
        if settings {
            if !self.git_settings_dialog.read(cx).is_open() {
                return;
            }
            self.git_settings_dialog
                .update(cx, |dialog, cx| dialog.set_discovering(true, cx));
        } else {
            if !self.remote_dialog.read(cx).allows_ssh_discovery() {
                return;
            }
            self.remote_dialog
                .update(cx, |dialog, cx| dialog.set_discovering(true, cx));
        }
        self.ssh_discovery_id = self.ssh_discovery_id.wrapping_add(1);
        let id = self.ssh_discovery_id;
        let repository_id = self.request_id;
        self.ssh_discovery_task = Some(cx.spawn(async move |this, cx| {
            let result = tasks::spawn(
                cx.background_executor(),
                "ssh_keys.discover",
                id,
                gitviews_service::discover_ssh_keys,
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.ssh_discovery_id != id || view.request_id != repository_id {
                    return;
                }
                view.ssh_discovery_task = None;
                if settings {
                    if view.git_settings_dialog.read(cx).is_open() {
                        view.git_settings_dialog
                            .update(cx, |dialog, cx| match result {
                                Ok(keys) => dialog.set_discovered_keys(keys, cx),
                                Err(error) => dialog.set_error(Some(error), cx),
                            });
                    }
                } else if view.remote_dialog.read(cx).allows_ssh_discovery() {
                    view.remote_dialog
                        .update(cx, |dialog, cx| dialog.set_discovered_keys(result, cx));
                }
            });
        }));
    }
}
