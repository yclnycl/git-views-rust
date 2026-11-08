use super::*;

impl Workspace {
    pub(super) fn settings_open(&self, cx: &App) -> bool {
        self.settings_dialog.read(cx).is_open()
    }

    pub(super) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open(cx) || self.settings_saving || self.remote_blocks_navigation(cx) {
            return;
        }
        self.close_file_search(cx);
        self.menu = None;
        self.show_about = false;
        self.active_input = None;
        let path = self.settings_store.path().to_owned();
        self.settings_dialog.update(cx, |dialog, cx| {
            dialog.show(self.settings, path, cx);
            dialog.set_error(self.settings_error.clone(), cx);
        });
        // The dialog records/restores this focus when its deferred inputs render.
        self.focus.focus(window, cx);
        cx.notify();
    }

    pub(super) fn handle_settings_event(
        &mut self,
        event: &SettingsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SettingsEvent::GitSettings if !self.settings_saving => {
                if self.settings.read_only {
                    self.settings_dialog.update(cx, |dialog, cx| {
                        dialog.set_error(
                            Some("请先关闭只读模式并保存，再配置当前仓库 Git。".into()),
                            cx,
                        );
                    });
                    return;
                }
                if self.repository.is_none() || self.preview || self.loading {
                    self.settings_dialog.update(cx, |dialog, cx| {
                        dialog.set_error(
                            Some("请先打开并等候仓库读取完成，再配置 Git 认证。".into()),
                            cx,
                        );
                    });
                    return;
                }
                self.settings_generation = self.settings_generation.wrapping_add(1);
                self.settings_task = None;
                self.settings_dialog
                    .update(cx, |dialog, cx| dialog.suspend(cx));
                self.open_git_settings(window, cx);
                self.git_settings_return_to_app = self.git_settings_dialog.read(cx).is_open();
                if !self.git_settings_return_to_app {
                    self.settings_dialog
                        .update(cx, |dialog, cx| dialog.resume(window, cx));
                }
            }
            SettingsEvent::GitSettings => {}
            SettingsEvent::Apply(settings) => self.save_settings(*settings, true, window, cx),
            SettingsEvent::Reload => self.reload_settings(window, cx),
            SettingsEvent::Close if !self.settings_saving => {
                self.settings_generation = self.settings_generation.wrapping_add(1);
                self.settings_task = None;
                self.settings_dialog
                    .update(cx, |dialog, cx| dialog.clear(cx));
                self.focus.focus(window, cx);
                cx.notify();
            }
            SettingsEvent::Close => {}
        }
    }

    pub(super) fn apply_settings(
        &mut self,
        settings: AppSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let enabling_advanced = self.settings.read_only && !settings.read_only;
        self.settings = settings;
        self.file_list
            .update(cx, |list, cx| list.set_read_only(settings.read_only, cx));
        self.history_tools_dialog.update(cx, |dialog, cx| {
            dialog.set_read_only(settings.read_only, cx)
        });
        self.menu = None;
        self.active_input = None;
        self.close_submodule_branch_menu(cx);
        self.appearance = Appearance::from_preference(settings.appearance.theme);
        self.appearance.apply(window, cx);
        cx.set_global(super::super::preview::PreviewPreferences {
            font_size: settings.appearance.editor_font_size,
        });
        self.service
            .set_cache_limits(settings.preview.cache_limits());
        self.preview_deck.set_limits(
            settings.preview.retained_panes,
            settings.preview.retained_bytes(),
            cx,
        );
        let debounce = Duration::from_millis(settings.refresh.debounce_ms);
        if let Some(watcher) = &mut self.watcher {
            watcher.set_debounce(debounce);
        }
        if let Some(scope) = &mut self.submodule_changes
            && let Some(watcher) = &mut scope.watcher
        {
            watcher.set_debounce(debounce);
        }
        if enabling_advanced && !self.preview {
            self.git_engine_ready = false;
            self.initialize_git_engine(Vec::new(), window, cx);
        }
        cx.refresh_windows();
        cx.notify();
    }

    pub(super) fn save_theme(
        &mut self,
        appearance: Appearance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        let mut settings = self.settings;
        settings.appearance.theme = appearance.preference();
        self.save_settings(settings, false, window, cx);
    }

    pub(super) fn save_settings(
        &mut self,
        settings: AppSettings,
        from_dialog: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_saving {
            return;
        }
        if settings.read_only != self.settings.read_only && self.mode_change_busy() {
            let message = "请等待当前 Git 操作完成后再切换只读模式。";
            self.notice = Some(message.into());
            if from_dialog {
                self.settings_dialog
                    .update(cx, |dialog, cx| dialog.set_error(Some(message.into()), cx));
            }
            cx.notify();
            return;
        }
        if let Err(error) = settings.validate() {
            self.settings_dialog.update(cx, |dialog, cx| {
                dialog.set_error(Some(error.to_string()), cx)
            });
            return;
        }
        self.settings_generation = self.settings_generation.wrapping_add(1);
        let generation = self.settings_generation;
        let store = Arc::clone(&self.settings_store);
        self.settings_saving = true;
        if from_dialog {
            self.settings_dialog
                .update(cx, |dialog, cx| dialog.set_saving(true, cx));
        }
        self.settings_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = tasks::spawn(cx.background_executor(), "settings.save", generation, move || store.save(&settings)).await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.settings_generation != generation { return; }
                view.settings_task = None;
                view.settings_saving = false;
                match result {
                    Ok(()) => {
                        let mode_changed = view.settings.read_only != settings.read_only;
                        view.settings_error = None;
                        view.apply_settings(settings, window, cx);
                        if from_dialog {
                            view.settings_dialog.update(cx, |dialog, cx| dialog.clear(cx));
                            view.focus.focus(window, cx);
                        }
                        view.notice = Some(if mode_changed {
                            if settings.read_only { "已开启只读模式，仅浏览仓库状态与代码。" }
                            else { "已关闭只读模式，高级功能已启用。" }
                        } else if settings.logging.level != view.startup_log_level {
                            "配置已保存并应用，日志等级将在下次启动时生效。"
                        } else { "配置已保存并应用。" }.into());
                        tracing::info!(target: "gitviews::settings", generation, "settings_saved");
                    }
                    Err(error) => {
                        view.settings_error = Some(error.to_string());
                        if from_dialog { view.settings_dialog.update(cx, |dialog, cx| dialog.set_error(view.settings_error.clone(), cx)); }
                        else { view.notice = Some("配置未保存，请打开设置查看配置文件错误。".into()); }
                        tracing::warn!(target: "gitviews::settings", generation, "settings_save_failed");
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn reload_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_saving {
            return;
        }
        self.settings_generation = self.settings_generation.wrapping_add(1);
        let generation = self.settings_generation;
        let store = Arc::clone(&self.settings_store);
        self.settings_dialog
            .update(cx, |dialog, cx| dialog.set_loading(true, cx));
        self.settings_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = tasks::spawn(cx.background_executor(), "settings.load", generation, move || store.load()).await;
            let _ = this.update_in(cx, |view, window, cx| {
                if view.settings_generation != generation || !view.settings_open(cx) { return; }
                view.settings_task = None;
                match result {
                    Ok(settings) => {
                        if settings.read_only != view.settings.read_only && view.mode_change_busy() {
                            view.settings_dialog.update(cx, |dialog, cx| {
                                dialog.set_error(Some("请等待当前 Git 操作完成后再切换只读模式。".into()), cx)
                            });
                            return;
                        }
                        let enabling_advanced = view.settings.read_only && !settings.read_only && !view.preview;
                        view.settings_error = None;
                        view.apply_settings(settings, window, cx);
                        let path = view.settings_store.path().to_owned();
                        view.settings_dialog.update(cx, |dialog, cx| {
                            if enabling_advanced { dialog.clear(cx); }
                            else { dialog.show(settings, path, cx); }
                        });
                        tracing::info!(target: "gitviews::settings", generation, "settings_reloaded");
                    }
                    Err(error) => {
                        view.settings_error = Some(error.to_string());
                        view.settings_dialog.update(cx, |dialog, cx| dialog.set_error(view.settings_error.clone(), cx));
                        tracing::warn!(target: "gitviews::settings", generation, "settings_reload_failed");
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}
