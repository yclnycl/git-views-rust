use super::*;
use crate::ui::copy_menu::{self, CopyMenuExt as _};
use gitviews_service::BranchInfo;

#[cfg(test)]
#[path = "branches/headless_tests.rs"]
mod headless_tests;

pub(super) struct BranchContext {
    root: PathBuf,
    scope_id: u64,
    details: Option<RepositoryDetails>,
    error: Option<String>,
}

fn filtered_branches(details: Option<&RepositoryDetails>, query: &str) -> Vec<BranchInfo> {
    let query = query.trim().to_lowercase();
    let mut branches = details
        .map(|details| {
            details
                .branches
                .iter()
                .filter(|branch| branch.name.to_lowercase().contains(&query))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    branches.sort_by(|left, right| {
        left.is_remote
            .cmp(&right.is_remote)
            .then_with(|| right.is_current.cmp(&left.is_current))
            .then_with(|| left.name.cmp(&right.name))
    });
    branches
}

fn checkout_head(
    repository: &RepositorySnapshot,
    details: Option<&RepositoryDetails>,
) -> Option<String> {
    match &repository.head {
        Head::Detached(id) => Some(id.clone()),
        Head::Unborn(_) => None,
        Head::Branch(name) => repository
            .recent_commits
            .first()
            .map(|commit| commit.id.clone())
            .or_else(|| {
                details?
                    .branches
                    .iter()
                    .find(|branch| !branch.is_remote && branch.is_current && branch.name == *name)
                    .map(|branch| branch.commit_id.clone())
            }),
    }
    .filter(|id| id.len() == 40 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

impl Workspace {
    pub(super) fn branch_picker_details(&self) -> Option<&RepositoryDetails> {
        let repository = self.changes_repository()?;
        if self
            .repository
            .as_ref()
            .is_some_and(|parent| paths_equal(&parent.root, &repository.root))
        {
            self.details.as_ref()
        } else {
            self.branch_context
                .as_ref()
                .filter(|context| {
                    paths_equal(&context.root, &repository.root)
                        && context.scope_id == self.submodule_request_id
                })
                .and_then(|context| context.details.as_ref())
        }
    }

    pub(super) fn submit_branch_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let matches = filtered_branches(self.branch_picker_details(), &self.branch_query);
        let exact = matches
            .iter()
            .find(|branch| branch.name == self.branch_query.trim());
        if let Some(branch) = exact.or_else(|| (matches.len() == 1).then(|| &matches[0])) {
            self.focus.focus(window, cx);
            self.checkout_active_branch(branch.name.clone(), branch.is_remote, cx);
        } else {
            self.notice = Some("请点击要切换的分支，或继续输入以精确匹配。".into());
            cx.notify();
        }
    }

    pub(super) fn toggle_branch_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        self.close_submodule_branch_menu(cx);
        if self.remote_blocks_navigation(cx) {
            return;
        }
        if self.menu == Some("分支") {
            self.menu = None;
            self.focus.focus(window, cx);
        } else {
            self.menu = Some("分支");
            self.active_input = None;
            self.branch_query.clear();
            self.branch_search
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.branch_search
                .read(cx)
                .focus_handle(cx)
                .focus(window, cx);
            self.refresh_branch_context(cx);
        }
        cx.notify();
    }

    pub(super) fn refresh_branch_context(&mut self, cx: &mut Context<Self>) {
        let Some(scope) = &self.submodule_changes else {
            self.branch_context_task = None;
            self.branch_context = None;
            return;
        };
        let root = scope.repository.root.clone();
        let same_root = self.branch_context.as_ref().is_some_and(|context| {
            paths_equal(&context.root, &root) && context.scope_id == self.submodule_request_id
        });
        if same_root && self.branch_context_task.is_some() {
            return;
        }
        self.branch_context_task = None;
        if !same_root {
            self.branch_context = Some(BranchContext {
                root: root.clone(),
                scope_id: self.submodule_request_id,
                details: None,
                error: None,
            });
        }
        let service = self.service.clone();
        let request_id = self.request_id;
        let scope_id = self.submodule_request_id;
        self.branch_context_task = Some(cx.spawn(async move |this, cx| {
            let selected_root = root.clone();
            let result = tasks::spawn(
                cx.background_executor(),
                "branch.details",
                scope_id,
                move || {
                    service.validate_root(&root)?;
                    let details = service.details(&root)?;
                    service.validate_root(&root)?;
                    Ok::<_, gitviews_service::ServiceError>(details)
                },
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.request_id != request_id
                    || view.submodule_request_id != scope_id
                    || view
                        .changes_repository()
                        .is_none_or(|repo| !paths_equal(&repo.root, &selected_root))
                {
                    return;
                }
                view.branch_context_task = None;
                if let Some(context) = &mut view.branch_context {
                    match result {
                        Ok(details) => {
                            context.details = Some(details);
                            context.error = None;
                        }
                        Err(error) => {
                            let message = format!("子仓库分支读取失败：{error}");
                            context.details = None;
                            context.error = Some(message.clone());
                            view.notice = Some(message);
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn checkout_branch(&mut self, branch: String, remote: bool, cx: &mut Context<Self>) {
        self.checkout_scoped_branch(branch, remote, true, cx);
    }

    fn checkout_active_branch(&mut self, branch: String, remote: bool, cx: &mut Context<Self>) {
        self.checkout_scoped_branch(branch, remote, true, cx);
    }

    fn checkout_scoped_branch(
        &mut self,
        branch: String,
        remote: bool,
        active_scope: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        if self.remote_blocks_navigation(cx) {
            return;
        }
        if self.preview || self.loading || (active_scope && self.submodule_loading) {
            self.notice = Some("请先打开并等待仓库读取完成。".into());
            cx.notify();
            return;
        }
        let repository = if active_scope {
            self.changes_repository()
        } else {
            self.repository.as_ref()
        };
        let Some(repository) = repository else {
            return;
        };
        if !remote && repository.head == Head::Branch(branch.clone()) {
            self.menu = None;
            cx.notify();
            return;
        }
        let expected_branch = match &repository.head {
            Head::Branch(name) | Head::Unborn(name) => Some(name.clone()),
            Head::Detached(_) => None,
        };
        let child_scope = self
            .submodule_changes
            .as_ref()
            .is_some_and(|scope| paths_equal(&scope.repository.root, &repository.root));
        let expected_head = checkout_head(
            repository,
            if child_scope {
                self.branch_picker_details()
            } else {
                self.details.as_ref()
            },
        );
        if expected_head.is_none() {
            self.notice = Some("当前 HEAD 尚未读取完整；请等候仓库读取完成或刷新后重试。".into());
            cx.notify();
            return;
        }
        let root = repository.root.clone();
        let scope_id = self.submodule_request_id;
        let request = CheckoutRequest {
            branch,
            remote,
            expected_branch,
            expected_head,
        };
        let service = Arc::clone(&self.service);
        let request_id = self.request_id;
        self.menu = None;
        self.notice = Some("正在切换分支…".into());
        self.pause_repository_reads();
        self.mutation_task = Some(cx.spawn(async move |this, cx| {
            let selected_root = root.clone();
            let result = tasks::spawn(
                cx.background_executor(),
                "branch.checkout",
                request_id,
                move || service.checkout(&root, &request),
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.request_id != request_id {
                    return;
                }
                view.mutation_task = None;
                let same_scope = if child_scope {
                    view.submodule_request_id == scope_id
                        && view
                            .changes_repository()
                            .is_some_and(|repo| paths_equal(&repo.root, &selected_root))
                } else {
                    view.repository
                        .as_ref()
                        .is_some_and(|repo| paths_equal(&repo.root, &selected_root))
                };
                view.pause_repository_reads();
                view.invalidate_changes();
                view.mutation_refresh = true;
                match result {
                    Ok(outcome) => {
                        view.reset_history(cx);
                        if same_scope && !child_scope {
                            view.discard_submodule();
                        }
                        if same_scope {
                            view.clear_changes_preview(cx);
                            view.sync_file_list(cx);
                        }
                        view.notice = Some(format!(
                            "{} {}",
                            display_path(&selected_root),
                            outcome.summary
                        ));
                    }
                    Err(error) => view.notice = Some(format!("切换分支未完成：{}", error.message)),
                }
                view.scan_pending = false;
                view.queue_scan(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn branch_popup(&self, light: bool, cx: &mut Context<Self>) -> Div {
        let branches = Arc::new(filtered_branches(
            self.branch_picker_details(),
            &self.branch_query,
        ));
        let count = branches.len();
        let entity = cx.entity();
        let expected_root = self
            .changes_repository()
            .map(|repository| repository.root.clone());
        let scope_id = self.submodule_request_id;
        let muted = rgb(if light { 0x59636e } else { 0x9198a1 });
        let panel =
            div()
                .id("branch-picker")
                .test_support()
                .occlude()
                .absolute()
                .top(px(91.))
                .left(px(244.))
                .w(px(370.))
                .p(px(8.))
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(if light { 0xd0d7de } else { 0x3d444d }))
                .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
                .shadow_lg()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        this.menu = None;
                        this.focus.focus(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .child(div().px(px(4.)).pb(px(8.)).child("切换分支"))
                .when_some(self.changes_repository(), |this, repository| {
                    this.child(
                        div()
                            .px(px(4.))
                            .pb(px(8.))
                            .text_size(px(11.))
                            .text_color(muted)
                            .truncate()
                            .child(display_path(&repository.root)),
                    )
                })
                .child(
                    Input::new(&self.branch_search)
                        .id("branch-picker-search")
                        .appearance(false)
                        .h(px(32.)),
                )
                .child(
                    div()
                        .px(px(4.))
                        .py(px(8.))
                        .text_size(px(11.))
                        .text_color(muted)
                        .child("保留不冲突的改动 · 远程分支检出为本地跟踪分支"),
                )
                .when(count == 0, |this| {
                    this.child(
                        div().p(px(12.)).text_color(muted).child(
                            if self.changes_repository().is_none() {
                                "请先打开仓库"
                            } else if self.branch_context_task.is_some() {
                                "正在读取当前子仓库的分支…"
                            } else {
                                self.branch_context
                                    .as_ref()
                                    .and_then(|context| context.error.as_deref())
                                    .unwrap_or("没有匹配的分支")
                            }
                            .to_owned(),
                        ),
                    )
                })
                .when(count > 0, |this| {
                    this.child(
                        uniform_list("branch-picker-list", count, move |range, _, _| {
                            range
                                .map(|index| {
                                    let branch = branches[index].clone();
                                    let target = entity.clone();
                                    let expected_root = expected_root.clone();
                                    let copy_items = copy_menu::branch_info(&branch);
                                    div()
                                        .id(("branch-picker-row", index))
                                        .role(Role::Button)
                                        .aria_label(format!(
                                            "{} {}",
                                            if branch.is_remote {
                                                "检出远程分支"
                                            } else {
                                                "切换本地分支"
                                            },
                                            branch.name
                                        ))
                                        .h(px(32.))
                                        .px(px(8.))
                                        .flex()
                                        .items_center()
                                        .gap(px(8.))
                                        .rounded(px(4.))
                                        .cursor_pointer()
                                        .hover(|this| {
                                            this.bg(rgb(if light { 0xf6f8fa } else { 0x212830 }))
                                        })
                                        .when(branch.is_current, |this| {
                                            this.bg(rgb(if light { 0xddf4ff } else { 0x1a3453 }))
                                        })
                                        .child(div().w(px(14.)).child(if branch.is_current {
                                            "✓"
                                        } else {
                                            ""
                                        }))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .child(branch.name.clone()),
                                        )
                                        .child(div().text_size(px(11.)).text_color(muted).child(
                                            if branch.is_remote { "远程" } else { "本地" },
                                        ))
                                        .on_click(move |_, window, cx| {
                                            cx.stop_propagation();
                                            target.update(cx, |view, cx| {
                                                if view.submodule_request_id != scope_id
                                                    || expected_root.as_ref().is_none_or(|root| {
                                                        view.changes_repository().is_none_or(
                                                            |repo| !paths_equal(&repo.root, root),
                                                        )
                                                    })
                                                {
                                                    return;
                                                }
                                                view.focus.focus(window, cx);
                                                view.checkout_active_branch(
                                                    branch.name.clone(),
                                                    branch.is_remote,
                                                    cx,
                                                );
                                            });
                                        })
                                        .copy_menu(copy_items)
                                })
                                .collect::<Vec<_>>()
                        })
                        .h(px((count.min(10) * 32) as f32))
                        .w_full(),
                    )
                });
        div()
            .absolute()
            .top(px(0.))
            .left(px(0.))
            .size_full()
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.menu = None;
                    this.focus.focus(window, cx);
                    cx.notify();
                }),
            )
            .child(panel)
    }
}

#[cfg(test)]
mod tests {
    use super::{checkout_head, filtered_branches};
    use gitviews_service::{BranchInfo, Head, RepositoryDetails, RepositorySnapshot};

    #[test]
    fn captured_head_can_use_current_branch_metadata_before_history_arrives() {
        let repository = RepositorySnapshot {
            root: "repository".into(),
            name: "repository".into(),
            head: Head::Branch("main".into()),
            changes: vec![],
            recent_commits: vec![],
        };
        let mut details = RepositoryDetails {
            worktrees: vec![],
            branches: vec![BranchInfo {
                name: "main".into(),
                commit_id: "abcdef0123456789abcdef0123456789abcdef01".into(),
                subject: String::new(),
                is_current: true,
                upstream: None,
                is_remote: false,
            }],
        };
        assert_eq!(
            checkout_head(&repository, Some(&details)),
            Some(details.branches[0].commit_id.clone())
        );
        details.branches[0].is_current = false;
        assert!(checkout_head(&repository, Some(&details)).is_none());
        assert!(checkout_head(&repository, None).is_none());
    }

    #[test]
    fn branch_search_matches_full_remote_name_and_chinese_without_case_sensitivity() {
        let details = RepositoryDetails {
            worktrees: vec![],
            branches: vec![
                BranchInfo {
                    name: "origin/Feature/中文".into(),
                    commit_id: String::new(),
                    subject: String::new(),
                    is_current: false,
                    upstream: None,
                    is_remote: true,
                },
                BranchInfo {
                    name: "main".into(),
                    commit_id: String::new(),
                    subject: String::new(),
                    is_current: true,
                    upstream: None,
                    is_remote: false,
                },
            ],
        };
        assert_eq!(
            filtered_branches(Some(&details), " FEATURE/中文 ")[0].name,
            "origin/Feature/中文"
        );
        assert_eq!(filtered_branches(Some(&details), "")[0].name, "main");
        assert!(filtered_branches(Some(&details), "missing").is_empty());
        assert!(filtered_branches(None, "").is_empty());
    }
}
