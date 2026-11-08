use super::writes::LocalOperation;
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
mod conflict_entry_tests;
#[cfg(test)]
mod headless_tests;

impl Workspace {
    pub(super) fn open_changed_file(
        &mut self,
        path: String,
        staged: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let conflicted = self.changes_repository().is_some_and(|repository| {
            repository
                .changes
                .iter()
                .any(|change| change.path == path && change.is_conflicted())
        });
        if !conflicted || self.settings.read_only {
            self.select_file(path, staged, cx);
            return;
        }
        if self.modal_open(cx) || !self.local_write_ready(cx) {
            return;
        }
        let repository = self.changes_repository().unwrap();
        let (root, name) = (repository.root.clone(), repository.name.clone());
        self.close_file_search(cx);
        self.close_submodule_branch_menu(cx);
        self.menu = None;
        self.active_input = None;
        self.selected_file = Some(path.clone());
        self.selected_staged = staged;
        self.file_list
            .update(cx, |list, cx| list.select(path.clone(), staged, cx));
        self.focus.focus(window, cx);
        self.conflict_dialog.update(cx, |dialog, cx| {
            dialog.show_file(root, name, path.clone(), cx)
        });
        self.load_conflict_data(Some(path), cx);
    }

    pub(super) fn open_conflicts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open(cx) || !self.local_write_ready(cx) {
            return;
        }
        let repo = self.changes_repository().unwrap();
        let (root, name) = (repo.root.clone(), repo.name.clone());
        self.close_file_search(cx);
        self.menu = None;
        self.active_input = None;
        self.focus.focus(window, cx);
        self.conflict_dialog
            .update(cx, |dialog, cx| dialog.show(root, name, cx));
        self.reload_conflicts(cx);
    }

    pub(super) fn reload_conflicts(&mut self, cx: &mut Context<Self>) {
        self.load_conflict_data(None, cx);
    }

    fn load_conflict_data(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        let Some(repo) = self.changes_repository() else {
            return;
        };
        let root = repo.root.clone();
        let target = root.clone();
        let service = self.service.clone();
        self.cancel_operation_reads();
        let id = self.operations_read_id;
        let scope_id = self.submodule_request_id;
        if path.is_none() {
            self.conflict_dialog
                .update(cx, |dialog, cx| dialog.set_busy(true, cx));
        }
        self.operations_read_task = Some(cx.spawn(async move |this, cx| {
            let result =
                tasks::spawn(
                    cx.background_executor(),
                    "conflict.read",
                    id,
                    move || match path {
                        Some(path) => service.conflict_state(&target).and_then(|state| {
                            service
                                .conflict_versions(&target, &path)
                                .map(|versions| ConflictData::File(state, versions))
                        }),
                        None => service.conflict_state(&target).map(ConflictData::State),
                    },
                )
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.operations_read_id != id
                    || view.submodule_request_id != scope_id
                    || !view.conflict_dialog.read(cx).is_open()
                    || view
                        .changes_repository()
                        .is_none_or(|repo| !paths_equal(&repo.root, &root))
                {
                    return;
                }
                view.operations_read_task = None;
                view.conflict_dialog.update(cx, |dialog, cx| match result {
                    Ok(ConflictData::State(state)) => dialog.set_state(state, cx),
                    Ok(ConflictData::File(state, versions)) => {
                        dialog.set_state(state, cx);
                        dialog.set_versions(versions, cx);
                    }
                    Err(error) => dialog.set_error(Some(error.to_string()), cx),
                });
            });
        }));
    }

    pub(super) fn handle_conflicts(
        &mut self,
        event: &ConflictDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ConflictDialogEvent::Apply(request) => {
                self.cancel_operation_reads();
                self.run_local_operation(LocalOperation::Conflict(request.clone()), window, cx);
            }
            ConflictDialogEvent::Reload if self.mutation_task.is_none() => {
                self.reload_conflicts(cx)
            }
            ConflictDialogEvent::LoadFile(path) if self.mutation_task.is_none() => {
                self.load_conflict_data(Some(path.clone()), cx)
            }
            ConflictDialogEvent::Close if self.mutation_task.is_none() => {
                self.cancel_operation_reads();
                self.conflict_dialog
                    .update(cx, |dialog, cx| dialog.clear(cx));
                self.focus.focus(window, cx);
            }
            _ => {}
        }
        cx.notify();
    }

    pub(super) fn cancel_operation_reads(&mut self) {
        if let Some(cancel) = self.operations_read_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.operations_read_id = self.operations_read_id.wrapping_add(1);
        self.operations_read_task = None;
    }

    pub(super) fn open_history_tools(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open(cx) || !self.local_write_ready(cx) {
            return;
        }
        let root = self.changes_repository().unwrap().root.clone();
        let head = self.active_head_id();
        let path = self.selected_file.clone();
        self.close_file_search(cx);
        self.menu = None;
        self.active_input = None;
        self.focus.focus(window, cx);
        self.history_tools_dialog.update(cx, |dialog, cx| {
            dialog.show(root, head, cx);
            if let Some(path) = path {
                dialog.set_path(path, cx);
            }
        });
    }

    pub(super) fn handle_history_tools(
        &mut self,
        event: &HistoryToolsDialogEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            HistoryToolsDialogEvent::Closed if self.mutation_task.is_none() => {
                self.cancel_operation_reads();
                self.history_tools_dialog
                    .update(cx, |dialog, cx| dialog.clear(cx));
                self.focus.focus(window, cx);
            }
            HistoryToolsDialogEvent::Request(request) if self.mutation_task.is_none() => {
                self.cancel_operation_reads();
                if request.is_mutation() {
                    if self.changes_repository().is_none_or(|repository| {
                        !paths_equal(&repository.root, self.history_tools_dialog.read(cx).root())
                    }) {
                        self.history_tools_dialog.update(cx, |dialog, cx| {
                            dialog.set_error(
                                Some("当前仓库已改变，请重新打开文件历史后操作。".into()),
                                cx,
                            )
                        });
                        return;
                    }
                    self.run_local_operation(LocalOperation::History(request.clone()), window, cx);
                    return;
                }
                let root = self.history_tools_dialog.read(cx).root().to_path_buf();
                let target = root.clone();
                let service = self.service.clone();
                let id = self.operations_read_id;
                let scope_id = self.submodule_request_id;
                let cancel = Arc::new(AtomicBool::new(false));
                self.operations_read_cancel = Some(cancel.clone());
                let request = request.clone();
                self.operations_read_task = Some(cx.spawn(async move |this, cx| {
                    let result =
                        tasks::spawn(cx.background_executor(), "history.tools", id, move || {
                            service.history_tools(&target, &request, &cancel)
                        })
                        .await;
                    let _ = this.update(cx, |view, cx| {
                        if view.operations_read_id != id
                            || view.submodule_request_id != scope_id
                            || !view.history_tools_dialog.read(cx).is_open()
                            || !paths_equal(view.history_tools_dialog.read(cx).root(), &root)
                        {
                            return;
                        }
                        view.operations_read_task = None;
                        view.operations_read_cancel = None;
                        view.history_tools_dialog
                            .update(cx, |dialog, cx| match result {
                                Ok(result) => dialog.set_result(result, cx),
                                Err(error) => dialog.set_error(Some(error.to_string()), cx),
                            });
                    });
                }));
            }
            _ => {}
        }
        cx.notify();
    }
}

enum ConflictData {
    State(gitviews_service::ConflictState),
    File(
        gitviews_service::ConflictState,
        gitviews_service::ConflictVersions,
    ),
}
