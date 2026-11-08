use super::*;
#[cfg(test)]
mod headless_tests;
use gpui_kit::component::{
    Disableable as _,
    button::{Button, ButtonVariants as _},
    input::Textarea,
};

pub(super) enum LocalOperation {
    Write(WriteAction),
    Stash(StashRequest),
    Branch(BranchAction),
    History(HistoryToolsRequest),
    Conflict(ConflictRequest),
}

impl Workspace {
    pub(super) fn local_write_ready(&mut self, cx: &mut Context<Self>) -> bool {
        self.advanced_mode_ready(cx) && self.local_repository_ready(cx)
    }

    pub(super) fn local_repository_ready(&mut self, cx: &mut Context<Self>) -> bool {
        if self.preview
            || self.loading
            || self.submodule_loading
            || self.changes_repository().is_none()
        {
            self.notice = Some("请先打开并等候当前仓库读取完成。".into());
        } else if self.mutation_task.is_some()
            || self.remote_job.is_some()
            || self.remote_pending.is_some()
            || self.worktree_busy
            || self.git_settings_saving
            || self.mutation_refresh
        {
            self.notice = Some("已有 Git 操作正在执行，请等待完成。".into());
        } else {
            return true;
        }
        cx.notify();
        false
    }

    pub(super) fn active_head_id(&self) -> Option<String> {
        let repo = self.changes_repository()?;
        match &repo.head {
            Head::Unborn(_) => None,
            Head::Detached(id) => Some(id.clone()),
            Head::Branch(name) => repo
                .recent_commits
                .first()
                .map(|commit| commit.id.clone())
                .or_else(|| {
                    self.details
                        .as_ref()?
                        .branches
                        .iter()
                        .find(|branch| {
                            !branch.is_remote
                                && branch.is_current
                                && branch.name == *name
                                && self
                                    .repository
                                    .as_ref()
                                    .is_some_and(|parent| paths_equal(&parent.root, &repo.root))
                        })
                        .map(|branch| branch.commit_id.clone())
                }),
        }
    }

    pub(super) fn run_write(
        &mut self,
        action: WriteAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_local_operation(LocalOperation::Write(action), window, cx);
    }

    pub(super) fn commit_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        if self.summary.trim().is_empty() {
            self.notice = Some("请填写提交摘要；提交只包含已经暂存的改动。".into());
            self.inputs[Field::Summary.index()]
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
            cx.notify();
            return;
        }
        self.run_write(
            WriteAction::Commit {
                summary: self.summary.clone(),
                description: self.description.clone(),
            },
            window,
            cx,
        );
    }

    pub(super) fn run_local_operation(
        &mut self,
        operation: LocalOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.local_write_ready(cx) {
            let error = self.notice.clone();
            self.branch_dialog.update(cx, |dialog, cx| {
                if dialog.is_open() {
                    dialog.set_error(error.clone(), cx);
                }
            });
            self.stash_dialog.update(cx, |dialog, cx| {
                if dialog.is_open() {
                    dialog.set_error(error.clone(), cx);
                }
            });
            self.history_tools_dialog.update(cx, |dialog, cx| {
                if dialog.is_open() {
                    dialog.set_error(error.clone(), cx);
                }
            });
            self.conflict_dialog.update(cx, |dialog, cx| {
                if dialog.is_open() {
                    dialog.set_error(error.clone(), cx);
                }
            });
            return;
        }
        let repo = self.changes_repository().unwrap();
        let root = repo.root.clone();
        let expected_head = self.active_head_id();
        if expected_head.is_none() && !matches!(repo.head, Head::Unborn(_)) {
            self.notice = Some("当前 HEAD 尚未读取完整，请刷新后重试。".into());
            cx.notify();
            return;
        }
        let committing = matches!(
            &operation,
            LocalOperation::Write(WriteAction::Commit { .. })
        );
        let history_changed = committing || matches!(&operation, LocalOperation::Conflict(_));
        let service = self.service.clone();
        let request_id = self.request_id;
        let scope_id = self.submodule_request_id;
        self.menu = None;
        self.active_input = None;
        self.pause_repository_reads();
        self.notice = Some(format!("正在处理 Git 操作：{}", display_path(&root)));
        self.branch_dialog.update(cx, |dialog, cx| {
            if dialog.is_open() {
                dialog.set_busy(true, cx);
            }
        });
        self.stash_dialog.update(cx, |dialog, cx| {
            if dialog.is_open() {
                dialog.set_busy(true, cx);
            }
        });
        self.history_tools_dialog.update(cx, |dialog, cx| {
            if dialog.is_open() {
                dialog.set_busy(true, cx);
            }
        });
        self.conflict_dialog.update(cx, |dialog, cx| {
            if dialog.is_open() {
                dialog.set_busy(true, cx);
            }
        });
        self.mutation_task = Some(cx.spawn_in(window, async move |this, cx| {
            let target = root.clone();
            let result = tasks::spawn(
                cx.background_executor(),
                "git.write",
                request_id,
                move || match operation {
                    LocalOperation::Conflict(request) => service
                        .conflict(&target, &request)
                        .map(|outcome| outcome.summary)
                        .map_err(|error| error.to_string()),
                    LocalOperation::History(request) => service
                        .history_tools(
                            &target,
                            &request,
                            &std::sync::atomic::AtomicBool::new(false),
                        )
                        .and_then(|result| match result {
                            HistoryToolsResult::Restored { summary } => Ok(summary),
                            _ => Err(gitviews_service::HistoryToolsError::new(
                                gitviews_service::HistoryToolsErrorKind::InvalidRequest,
                                "历史写入请求无效。",
                            )),
                        })
                        .map_err(|error| error.to_string()),
                    LocalOperation::Write(action) => service
                        .write(
                            &target,
                            &WriteRequest {
                                expected_head,
                                action,
                            },
                        )
                        .map(|outcome| outcome.summary)
                        .map_err(|error| error.to_string()),
                    LocalOperation::Stash(request) => service
                        .stash(&target, &request)
                        .map(|outcome| outcome.summary)
                        .map_err(|error| error.to_string()),
                    LocalOperation::Branch(action) => service
                        .manage_branch(
                            &target,
                            &BranchRequest {
                                expected_head,
                                action,
                            },
                        )
                        .map(|outcome| outcome.summary)
                        .map_err(|error| error.to_string()),
                },
            )
            .await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.mutation_task = None;
                if view.request_id != request_id {
                    cx.notify();
                    return;
                }
                if view.submodule_request_id != scope_id
                    || view
                        .changes_repository()
                        .is_none_or(|repo| !paths_equal(&repo.root, &root))
                {
                    view.pause_repository_reads();
                    view.invalidate_changes();
                    view.preview_deck.invalidate_root(&root);
                    view.mutation_refresh = true;
                    view.notice = Some(format!(
                        "{} · {}",
                        display_path(&root),
                        match result {
                            Ok(message) => message,
                            Err(error) => error,
                        }
                    ));
                    view.scan_pending = false;
                    view.queue_scan(cx);
                    cx.notify();
                    return;
                }
                view.invalidate_changes();
                view.mutation_refresh = true;
                match result {
                    Ok(message) => {
                        if committing {
                            view.summary.clear();
                            view.description.clear();
                            view.inputs[Field::Summary.index()]
                                .update(cx, |input, cx| input.set_value("", window, cx));
                            view.commit_description
                                .update(cx, |input, cx| input.set_value("", window, cx));
                        }
                        if history_changed {
                            view.reset_history(cx);
                            if view.history {
                                view.load_history(true, cx);
                            }
                        }
                        view.notice = Some(format!("{} · {message}", display_path(&root)));
                        view.history_tools_dialog.update(cx, |dialog, cx| {
                            if dialog.is_open() {
                                dialog.set_result(
                                    HistoryToolsResult::Restored {
                                        summary: message.clone(),
                                    },
                                    cx,
                                );
                            }
                        });
                        if view.branch_dialog.read(cx).is_open() {
                            view.reload_branch_manager(cx);
                        }
                        if view.stash_dialog.read(cx).is_open() {
                            view.reload_stashes(cx);
                        }
                        if view.conflict_dialog.read(cx).is_open() {
                            view.reload_conflicts(cx);
                        }
                    }
                    Err(error) => {
                        view.notice = Some(format!("{} · {error}", display_path(&root)));
                        view.branch_dialog.update(cx, |dialog, cx| {
                            if dialog.is_open() {
                                dialog.set_error(Some(error.clone()), cx);
                            }
                        });
                        view.stash_dialog.update(cx, |dialog, cx| {
                            if dialog.is_open() {
                                dialog.set_error(Some(error.clone()), cx);
                            }
                        });
                        view.history_tools_dialog.update(cx, |dialog, cx| {
                            if dialog.is_open() {
                                dialog.set_error(Some(error.clone()), cx);
                            }
                        });
                        view.conflict_dialog.update(cx, |dialog, cx| {
                            if dialog.is_open() {
                                dialog.set_error(Some(error.clone()), cx);
                            }
                        });
                    }
                }
                view.scan_pending = false;
                view.queue_scan(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn open_stashes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open(cx) || !self.local_write_ready(cx) {
            return;
        }
        let repo = self.changes_repository().unwrap();
        let (root, name) = (repo.root.clone(), repo.name.clone());
        self.close_file_search(cx);
        self.menu = None;
        self.active_input = None;
        self.focus.focus(window, cx);
        self.stash_dialog
            .update(cx, |dialog, cx| dialog.show(root, name, cx));
        self.reload_stashes(cx);
    }

    fn reload_stashes(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.changes_repository() else {
            return;
        };
        let root = repo.root.clone();
        let service = self.service.clone();
        let head = self.active_head_id();
        let scope_id = self.submodule_request_id;
        let target_root = root.clone();
        self.operations_read_id = self.operations_read_id.wrapping_add(1);
        let id = self.operations_read_id;
        self.stash_dialog
            .update(cx, |dialog, cx| dialog.set_busy(true, cx));
        self.operations_read_task = Some(cx.spawn(async move |this, cx| {
            let result = tasks::spawn(cx.background_executor(), "stash.list", id, move || {
                service
                    .stashes(&root)
                    .map(|entries| (entries, head))
                    .map_err(|error| error.to_string())
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.operations_read_id != id
                    || !view.stash_dialog.read(cx).is_open()
                    || view.submodule_request_id != scope_id
                    || view
                        .changes_repository()
                        .is_none_or(|repo| !paths_equal(&repo.root, &target_root))
                {
                    return;
                }
                view.operations_read_task = None;
                view.stash_dialog.update(cx, |dialog, cx| match result {
                    Ok((entries, head)) => dialog.set_entries(entries, head, cx),
                    Err(error) => dialog.set_error(Some(error), cx),
                });
            });
        }));
    }

    pub(super) fn handle_stash_dialog(
        &mut self,
        event: &StashDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            StashDialogEvent::Inspect { id } if self.mutation_task.is_none() => {
                let Some(repo) = self.changes_repository() else {
                    return;
                };
                let root = repo.root.clone();
                let head = self.active_head_id();
                self.cancel_operation_reads();
                self.stash_dialog.update(cx, |dialog, cx| dialog.clear(cx));
                self.history_tools_dialog.update(cx, |dialog, cx| {
                    dialog.show_stash(root, head, id.clone(), cx)
                });
            }
            StashDialogEvent::Apply(request) => {
                self.run_local_operation(LocalOperation::Stash(request.clone()), window, cx)
            }
            StashDialogEvent::Reload if self.mutation_task.is_none() => self.reload_stashes(cx),
            StashDialogEvent::Close if self.mutation_task.is_none() => {
                self.operations_read_id = self.operations_read_id.wrapping_add(1);
                self.operations_read_task = None;
                self.stash_dialog.update(cx, |dialog, cx| dialog.clear(cx));
                self.focus.focus(window, cx);
            }
            _ => {}
        }
        cx.notify();
    }

    pub(super) fn open_branch_manager(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open(cx) || !self.local_write_ready(cx) {
            return;
        }
        let root = self.changes_repository().unwrap().root.clone();
        self.close_file_search(cx);
        self.menu = None;
        self.active_input = None;
        self.focus.focus(window, cx);
        self.branch_dialog.update(cx, |dialog, cx| {
            dialog.show(root, RepositoryDetails::default(), cx)
        });
        self.reload_branch_manager(cx);
    }

    fn reload_branch_manager(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.changes_repository() else {
            return;
        };
        let root = repo.root.clone();
        let scope_id = self.submodule_request_id;
        let target_root = root.clone();
        let service = self.service.clone();
        self.operations_read_id = self.operations_read_id.wrapping_add(1);
        let id = self.operations_read_id;
        self.branch_dialog
            .update(cx, |dialog, cx| dialog.set_busy(true, cx));
        self.operations_read_task = Some(cx.spawn(async move |this, cx| {
            let result = tasks::spawn(
                cx.background_executor(),
                "branch.manage.list",
                id,
                move || service.details(&root),
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.operations_read_id != id
                    || !view.branch_dialog.read(cx).is_open()
                    || view.submodule_request_id != scope_id
                    || view
                        .changes_repository()
                        .is_none_or(|repo| !paths_equal(&repo.root, &target_root))
                {
                    return;
                }
                view.operations_read_task = None;
                view.branch_dialog.update(cx, |dialog, cx| match result {
                    Ok(details) => dialog.replace_details(details, cx),
                    Err(error) => dialog.set_error(Some(error.to_string()), cx),
                });
            });
        }));
    }

    pub(super) fn handle_branch_dialog(
        &mut self,
        event: &BranchDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            BranchDialogEvent::Request(action) => {
                self.run_local_operation(LocalOperation::Branch(action.clone()), window, cx)
            }
            BranchDialogEvent::Reload if self.mutation_task.is_none() => {
                self.reload_branch_manager(cx)
            }
            BranchDialogEvent::Closed if self.mutation_task.is_none() => {
                self.operations_read_id = self.operations_read_id.wrapping_add(1);
                self.operations_read_task = None;
                self.branch_dialog.update(cx, |dialog, cx| dialog.clear(cx));
                self.focus.focus(window, cx);
            }
            _ => {}
        }
        cx.notify();
    }

    pub(super) fn render_commit_form(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.mutation_task.is_some()
            || self.mutation_refresh
            || self.remote_job.is_some()
            || self.loading
            || self.submodule_loading;
        let target = self
            .changes_repository()
            .map(|repo| format!("操作仓库：{}", display_path(&repo.root)))
            .unwrap_or_else(|| "请先打开仓库".into());
        let staged = self.changes_repository().map_or(0, |repo| {
            repo.changes
                .iter()
                .filter(|change| !matches!(change.index_status, ' ' | '?'))
                .count()
        });
        div()
            .id("native-commit-form")
            .test_support()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap(px(7.))
            .p(px(12.))
            .w_full()
            .border_t_1()
            .border_color(rgb(if cx.theme().mode == ThemeMode::Light {
                0xd0d7de
            } else {
                0x30363d
            }))
            .child(div().text_size(px(11.)).child(target))
            .child(
                div()
                    .flex()
                    .gap(px(6.))
                    .child(
                        Button::new("stage-all")
                            .label("暂存全部")
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.run_write(WriteAction::StageAll, window, cx)
                            })),
                    )
                    .child(
                        Button::new("unstage-all")
                            .label("全部取消暂存")
                            .disabled(busy)
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.run_write(WriteAction::UnstageAll, window, cx)
                            })),
                    )
                    .child(
                        Button::new("open-stashes")
                            .label("储藏…")
                            .disabled(busy)
                            .on_click(
                                cx.listener(|view, _, window, cx| view.open_stashes(window, cx)),
                            ),
                    ),
            )
            .child(
                Input::new(&self.inputs[Field::Summary.index()])
                    .id("commit-summary")
                    .disabled(busy)
                    .h(px(32.)),
            )
            .child(
                Textarea::new(&self.commit_description)
                    .accessibility_id("commit-description")
                    .aria_label("提交详细描述")
                    .disabled(busy)
                    .h(px(54.)),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .child("本地提交只包含暂存区；此版本不运行 Git hooks。"),
            )
            .child(
                Button::new("commit-staged")
                    .primary()
                    .label(format!("提交 {staged} 个已暂存文件 · Ctrl Enter"))
                    .disabled(busy || staged == 0 || self.summary.trim().is_empty())
                    .on_click(cx.listener(|view, _, window, cx| view.commit_changes(window, cx))),
            )
            .into_any_element()
    }

    pub(super) fn hunk_controls(&self, cx: &mut Context<Self>) -> Div {
        let eligible = !self.settings.read_only
            && !self.history
            && self.selected_file.is_some()
            && self
                .diff
                .as_ref()
                .is_some_and(|diff| diff.patch.contains("@@"));
        div().flex().gap(px(6.)).when(eligible, |this| {
            this.child(
                Button::new("stage-selected-hunk")
                    .label(if self.selected_staged {
                        "取消暂存当前修改块"
                    } else {
                        "暂存当前修改块"
                    })
                    .disabled(self.mutation_task.is_some() || self.mutation_refresh)
                    .on_click(cx.listener(|view, _, window, cx| {
                        let Some(diff) = view.diff.as_ref() else {
                            return;
                        };
                        let Some(patch) = selected_patch_hunk(&diff.patch, view.hunk_index) else {
                            view.notice = Some("没有可操作的文本修改块。".into());
                            cx.notify();
                            return;
                        };
                        let action = if view.selected_staged {
                            WriteAction::UnstageHunk {
                                path: diff.path.clone(),
                                patch,
                            }
                        } else {
                            WriteAction::StageHunk {
                                path: diff.path.clone(),
                                patch,
                            }
                        };
                        view.run_write(action, window, cx);
                    })),
            )
        })
    }
}

fn selected_patch_hunk(patch: &str, index: usize) -> Option<String> {
    let starts = patch
        .match_indices("@@")
        .filter_map(|(offset, _)| {
            (offset == 0 || patch.as_bytes().get(offset - 1) == Some(&b'\n')).then_some(offset)
        })
        .collect::<Vec<_>>();
    let start = *starts.get(index)?;
    let end = starts.get(index + 1).copied().unwrap_or(patch.len());
    Some(format!("{}{}", &patch[..starts[0]], &patch[start..end]))
}

#[cfg(test)]
mod tests {
    use super::selected_patch_hunk;
    #[test]
    fn selects_one_real_patch_hunk_and_preserves_header_and_utf8() {
        let header = "diff --git a/中文.rs b/中文.rs\n--- a/中文.rs\n+++ b/中文.rs\n";
        let first = "@@ -1 +1 @@\n-旧\n+新\n";
        let second = "@@ -20 +20 @@\n-old\n+new\n";
        let patch = format!("{header}{first}{second}");
        assert_eq!(
            selected_patch_hunk(&patch, 1),
            Some(format!("{header}{second}"))
        );
        assert!(selected_patch_hunk(&patch, 2).is_none());
        assert!(selected_patch_hunk("Binary files differ\n", 0).is_none());
    }
}
