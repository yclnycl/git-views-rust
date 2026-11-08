use super::*;

#[cfg(test)]
mod headless_tests;

const MAX_OPEN_TABS: usize = 32;

fn tab_key(tab: &FileTab) -> PreviewKey {
    PreviewKey {
        root: tab.root.clone(),
        path: tab.path.clone(),
        staged: tab.staged,
        revision: tab.revision.clone(),
    }
}

fn remaining_active(active: Option<usize>, closed: usize, count: usize) -> Option<usize> {
    match (active, count) {
        (_, 0) | (None, _) => None,
        (Some(index), _) if index > closed => Some(index - 1),
        (Some(index), _) if index == closed => Some(closed.min(count - 1)),
        (Some(index), _) => Some(index),
    }
}

impl Workspace {
    pub(super) fn active_preview_key(&self) -> Option<PreviewKey> {
        self.active_tab
            .and_then(|index| self.opened_files.get(index))
            .map(tab_key)
    }

    pub(super) fn open_history_file(
        &mut self,
        path: String,
        commit_id: String,
        cx: &mut Context<Self>,
    ) {
        self.open_preview_tab(path, false, Some(commit_id), false, cx);
    }

    pub(super) fn open_preview_tab(
        &mut self,
        path: String,
        staged: bool,
        revision: Option<String>,
        prefer_source: bool,
        cx: &mut Context<Self>,
    ) {
        let repo = self.changes_repository();
        let Some(repo) = repo else {
            return;
        };
        let tab = FileTab {
            root: repo.root.clone(),
            path,
            staged,
            revision,
        };
        let index = if let Some(index) = self
            .opened_files
            .iter()
            .position(|existing| existing == &tab)
        {
            index
        } else {
            if self.opened_files.len() >= MAX_OPEN_TABS {
                self.notice = Some("最多打开 32 个预览标签，请先关闭一个标签（Ctrl W）。".into());
                cx.notify();
                return;
            }
            self.opened_files.push(tab);
            self.opened_files.len() - 1
        };
        self.activate_preview(index, prefer_source, false, cx);
    }

    pub(super) fn activate_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        self.activate_preview(index, false, true, cx);
    }

    fn activate_preview(
        &mut self,
        index: usize,
        prefer_source: bool,
        sync_history: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.opened_files.get(index).cloned() else {
            return;
        };
        self.diff_request_id = self.diff_request_id.wrapping_add(1);
        self.diff_task = None;
        self.diff_loading = false;
        let key = tab_key(&tab);
        self.preview_pane = self.preview_deck.activate(key.clone(), cx);
        self.active_tab = Some(index);
        self.selected_file = Some(tab.path.clone());
        self.selected_staged = tab.staged;
        self.diff = self.preview_pane.read(cx).file_diff();
        self.source_file = self.preview_pane.read(cx).source();
        let retained = self.diff.is_some() || self.source_file.is_some();
        if retained {
            self.source = self.preview_pane.read(cx).mode() == PreviewMode::Source;
            self.unified = self.preview_pane.read(cx).mode() == PreviewMode::Unified;
            self.ignore_whitespace = self.preview_pane.read(cx).ignore_whitespace();
            if prefer_source {
                self.source = true;
                if self.source_file.is_some() {
                    self.sync_preview(cx);
                }
            }
        } else {
            self.source = prefer_source;
        }
        if let Some(revision) = &tab.revision {
            self.commit_file = Some(tab.path.clone());
            if sync_history {
                self.activate_history_file(tab.path.clone(), revision.clone(), cx);
            }
        } else {
            if self.history {
                self.history = false;
                self.pause_history_graph();
                self.history_request_id = self.history_request_id.wrapping_add(1);
                self.commit_request_id = self.commit_request_id.wrapping_add(1);
                self.history_task = None;
                self.commit_task = None;
                self.history_loading = false;
            }
            self.file_list
                .update(cx, |list, cx| list.select(tab.path.clone(), tab.staged, cx));
        }
        // A retained pane displays immediately; the cheap service signature still
        // verifies it in the background in case a watcher event has not arrived.
        if !(retained && tab.revision.is_some() && !self.preview_deck.is_stale(&key)) {
            self.request_preview(tab.path, tab.staged, tab.revision, retained, cx);
        }
        cx.notify();
    }

    pub(super) fn clear_tabs(&mut self, cx: &mut Context<Self>) {
        self.diff_request_id = self.diff_request_id.wrapping_add(1);
        self.diff_task = None;
        self.diff_loading = false;
        self.preview_deck.clear(cx);
        self.opened_files.clear();
        self.active_tab = None;
        self.preview_pane = cx.new(PreviewPane::new_deferred);
        self.diff = None;
        self.source_file = None;
        self.selected_file = None;
    }

    pub(super) fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.opened_files.len() {
            return;
        }
        let was_active = self.active_tab == Some(index);
        let tab = self.opened_files.remove(index);
        self.preview_deck.close(&tab_key(&tab), cx);
        self.active_tab = remaining_active(self.active_tab, index, self.opened_files.len());
        if was_active {
            if let Some(index) = self.active_tab {
                self.activate_tab(index, cx);
            } else {
                self.diff_request_id = self.diff_request_id.wrapping_add(1);
                self.diff_task = None;
                self.diff_loading = false;
                self.selected_file = None;
                self.commit_file = None;
                self.diff = None;
                self.source_file = None;
                self.preview_pane = cx.new(PreviewPane::new_deferred);
            }
        }
        cx.notify();
    }

    pub(super) fn cycle_tab(&mut self, direction: isize, cx: &mut Context<Self>) {
        if self.opened_files.is_empty() {
            return;
        }
        let next = (self.active_tab.unwrap_or(0) as isize + direction)
            .rem_euclid(self.opened_files.len() as isize) as usize;
        self.activate_tab(next, cx);
    }

    pub(super) fn exit_history_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self
            .opened_files
            .iter()
            .rposition(|tab| tab.revision.is_none())
        {
            self.activate_tab(index, cx);
        } else {
            self.diff_request_id = self.diff_request_id.wrapping_add(1);
            self.diff_task = None;
            self.diff_loading = false;
            self.preview_deck.deactivate();
            self.active_tab = None;
            self.selected_file = None;
            self.commit_file = None;
            self.diff = None;
            self.source_file = None;
            self.preview_pane = cx.new(PreviewPane::new_deferred);
        }
    }

    pub(super) fn render_preview_tabs(&self, cx: &mut Context<Self>) -> Div {
        let light = cx.theme().mode == ThemeMode::Light;
        let active = self.active_tab;
        let mut tabs = div()
            .id("history-preview-tabs")
            .test_support()
            .flex()
            .w_full()
            .h(px(35.))
            .overflow_x_scroll();
        for (index, tab) in self.opened_files.iter().enumerate() {
            let suffix = tab
                .revision
                .as_deref()
                .map(|id| id.chars().take(8).collect::<String>())
                .unwrap_or_else(|| {
                    if tab.staged {
                        "暂存区".into()
                    } else {
                        "工作区".into()
                    }
                });
            tabs = tabs.child(
                div()
                    .id(("preview-tab", index))
                    .test_support()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(px(9.))
                    .px(px(10.))
                    .h(px(34.))
                    .max_w(px(250.))
                    .cursor_pointer()
                    .when(active == Some(index), |this| {
                        this.bg(rgb(if light { 0xddf4ff } else { 0x1a3453 }))
                    })
                    .child(div().max_w(px(195.)).truncate().child(format!(
                        "{} · {suffix}",
                        tab.path.rsplit('/').next().unwrap_or(&tab.path)
                    )))
                    .child(
                        div()
                            .id(("close-preview-tab", index))
                            .role(Role::Button)
                            .aria_label(format!("关闭 {}", tab.path))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_tab(index, cx);
                            })),
                    )
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_tab(index, cx);
                        }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.activate_tab(index, cx))),
            );
        }
        div()
            .flex_shrink_0()
            .h(px(35.))
            .w_full()
            .overflow_hidden()
            .child(tabs)
    }
}

#[cfg(test)]
mod tests {
    use super::remaining_active;
    #[test]
    fn closing_tabs_keeps_the_active_identity_or_chooses_an_adjacent_tab() {
        assert_eq!(remaining_active(Some(3), 1, 4), Some(2));
        assert_eq!(remaining_active(Some(1), 1, 4), Some(1));
        assert_eq!(remaining_active(Some(4), 4, 4), Some(3));
        assert_eq!(remaining_active(Some(0), 0, 0), None);
    }
}
