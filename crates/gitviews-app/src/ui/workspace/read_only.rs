use super::*;

#[cfg(test)]
mod headless_tests;

impl Workspace {
    /// All mutation dispatchers use this guard, including shortcuts and stale UI events.
    pub(super) fn advanced_mode_ready(&mut self, cx: &mut Context<Self>) -> bool {
        if self.settings.read_only {
            self.notice =
                Some("当前为只读模式。关闭工具栏或设置中的只读模式后，可使用高级功能。".into());
        } else if self.settings_saving {
            self.notice = Some("正在保存设置，请稍候。".into());
        } else if !self.git_engine_ready || self.git_engine_dialog.read(cx).is_open() {
            self.notice = Some("请先完成高级功能的 Git 方案选择。".into());
        } else {
            return true;
        }
        cx.notify();
        false
    }

    pub(super) fn mode_change_busy(&self) -> bool {
        self.mutation_task.is_some()
            || self.mutation_refresh
            || self.remote_job.is_some()
            || self.remote_pending.is_some()
            || self.worktree_busy
            || self.git_settings_saving
            || self.git_engine_task.is_some()
            || self.repository_setup_state.is_busy()
    }

    pub(super) fn toggle_read_only(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open(cx) || self.settings_saving {
            return;
        }
        let mut settings = self.settings;
        settings.read_only = !settings.read_only;
        self.menu = None;
        self.save_settings(settings, false, window, cx);
    }
}
