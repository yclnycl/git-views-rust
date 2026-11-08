//! Per-row submodule branch selection keeps its own immutable repository scope.

use super::*;
use crate::ui::copy_menu::{self, CopyMenuExt as _};
use gitviews_service::{BranchInfo, CheckoutError, CheckoutErrorKind};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

#[cfg(test)]
#[path = "submodule_branches/headless_tests.rs"]
mod headless_tests;

static NEXT_MENU: AtomicU64 = AtomicU64::new(1);

pub(super) struct SubmoduleBranchMenu {
    id: u64,
    repository_id: u64,
    parent: PathBuf,
    target: PathBuf,
    repository: Option<RepositorySnapshot>,
    details: Option<RepositoryDetails>,
    query: String,
    search: Entity<InputState>,
    error: Option<String>,
    read_task: Option<Task<()>>,
    cancel: Arc<AtomicBool>,
    _subscription: Subscription,
}

impl Drop for SubmoduleBranchMenu {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

fn branches(menu: &SubmoduleBranchMenu) -> Vec<BranchInfo> {
    let query = menu.query.trim().to_lowercase();
    let mut branches = menu
        .details
        .as_ref()
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

impl Workspace {
    fn branch_menu_parent(&self, target: &Path) -> Option<PathBuf> {
        let registered = |repository: &RepositorySnapshot, tree: &RepositoryTree| {
            tree.submodules.iter().any(|module| {
                module.initialized && paths_equal(&repository.root.join(&module.path), target)
            })
        };
        if let (Some(repository), Some(tree)) = (&self.repository, &self.tree)
            && registered(repository, tree)
        {
            return Some(repository.root.clone());
        }
        self.submodule_changes.as_ref().and_then(|scope| {
            registered(&scope.repository, &scope.tree).then(|| scope.repository.root.clone())
        })
    }

    pub(super) fn close_submodule_branch_menu(&mut self, cx: &mut Context<Self>) {
        if self.submodule_branch_menu.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn open_submodule_branch_menu(
        &mut self,
        target: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        self.close_submodule_branch_menu(cx);
        if self.remote_blocks_navigation(cx) {
            return;
        }
        if self.preview || self.loading || self.submodule_loading {
            self.notice = Some("请等候仓库读取完成后切换子模块分支。".into());
            cx.notify();
            return;
        }
        let Some(parent) = self.branch_menu_parent(&target) else {
            self.notice = Some("该路径不是已初始化并登记的子模块，请先初始化或刷新仓库。".into());
            cx.notify();
            return;
        };
        let id = NEXT_MENU.fetch_add(1, Ordering::Relaxed);
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("搜索本地或远程分支…"));
        let subscription =
            cx.subscribe_in(&search, window, move |view, input, event, window, cx| {
                if view
                    .submodule_branch_menu
                    .as_ref()
                    .is_none_or(|menu| menu.id != id)
                {
                    return;
                }
                match event {
                    InputEvent::Change => {
                        if let Some(menu) = &mut view.submodule_branch_menu {
                            menu.query = input.read(cx).value().to_string();
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        view.submit_submodule_branch_search(window, cx)
                    }
                    _ => {}
                }
            });
        search.read(cx).focus_handle(cx).focus(window, cx);
        let cancel = Arc::new(AtomicBool::new(false));
        self.close_file_search(cx);
        self.menu = None;
        self.show_about = false;
        self.active_input = None;
        self.submodule_branch_menu = Some(SubmoduleBranchMenu {
            id,
            repository_id: self.request_id,
            parent: parent.clone(),
            target: target.clone(),
            repository: None,
            details: None,
            query: String::new(),
            search,
            error: None,
            read_task: None,
            cancel: Arc::clone(&cancel),
            _subscription: subscription,
        });
        let service = Arc::clone(&self.service);
        let repository_id = self.request_id;
        let task = cx.spawn(async move |this, cx| {
            let result = tasks::spawn(
                cx.background_executor(),
                "submodule.branch.details",
                id,
                move || {
                    let repository = service
                        .submodule_metadata_with_cancel(&parent, &target, &cancel)
                        .map_err(|error| error.to_string())?;
                    let details = service
                        .details(&repository.root)
                        .map_err(|error| error.to_string())?;
                    let verified = service
                        .submodule_metadata_with_cancel(&parent, &target, &cancel)
                        .map_err(|error| error.to_string())?;
                    if !paths_equal(&repository.root, &verified.root)
                        || repository.head != verified.head
                    {
                        return Err(
                            "读取期间子模块的 HEAD 已改变，请关闭后重新打开分支菜单。".to_owned()
                        );
                    }
                    Ok((verified, details))
                },
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.request_id != repository_id {
                    return;
                }
                let Some(menu) = &mut view.submodule_branch_menu else {
                    return;
                };
                if menu.id != id || menu.cancel.load(Ordering::Acquire) {
                    return;
                }
                menu.read_task = None;
                match result {
                    Ok((repository, details)) => {
                        menu.repository = Some(repository);
                        menu.details = Some(details);
                    }
                    Err(error) => menu.error = Some(format!("子模块分支读取失败：{error}")),
                }
                cx.notify();
            });
        });
        if let Some(menu) = &mut self.submodule_branch_menu {
            menu.read_task = Some(task);
        }
        cx.notify();
    }

    fn submit_submodule_branch_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = &self.submodule_branch_menu else {
            return;
        };
        let matches = branches(menu);
        let selected = matches
            .iter()
            .find(|branch| branch.name == menu.query.trim())
            .or_else(|| (matches.len() == 1).then(|| &matches[0]));
        if let Some(branch) = selected {
            let (id, name, remote) = (menu.id, branch.name.clone(), branch.is_remote);
            self.focus.focus(window, cx);
            self.checkout_submodule_menu_branch(id, name, remote, cx);
        }
    }

    fn checkout_submodule_menu_branch(
        &mut self,
        menu_id: u64,
        branch: String,
        remote: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        let Some(menu) = self
            .submodule_branch_menu
            .as_ref()
            .filter(|menu| menu.id == menu_id)
        else {
            return;
        };
        if menu.read_task.is_some() || menu.repository_id != self.request_id {
            return;
        }
        let Some(repository) = &menu.repository else {
            return;
        };
        // Select only a row from this menu's captured branch list. A stale row
        // from a previous module can never be retargeted through the active scope.
        if menu.details.as_ref().is_none_or(|details| {
            !details
                .branches
                .iter()
                .any(|candidate| candidate.name == branch && candidate.is_remote == remote)
        }) {
            return;
        }
        if !remote && repository.head == Head::Branch(branch.clone()) {
            self.close_submodule_branch_menu(cx);
            return;
        }
        let expected_head = match &repository.head {
            Head::Detached(id) => Some(id.clone()),
            Head::Branch(name) => menu.details.as_ref().and_then(|details| {
                details
                    .branches
                    .iter()
                    .find(|branch| !branch.is_remote && branch.is_current && &branch.name == name)
                    .map(|branch| branch.commit_id.clone())
            }),
            Head::Unborn(_) => None,
        }
        .filter(|id| id.len() == 40 && id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        if expected_head.is_none() {
            if let Some(menu) = &mut self.submodule_branch_menu {
                menu.error = Some("当前子模块尚无有效提交，或 HEAD 尚未读取完整。".into());
            }
            cx.notify();
            return;
        }
        let request = CheckoutRequest {
            branch,
            remote,
            expected_branch: match &repository.head {
                Head::Branch(name) | Head::Unborn(name) => Some(name.clone()),
                Head::Detached(_) => None,
            },
            expected_head,
        };
        let root = repository.root.clone();
        let parent = menu.parent.clone();
        self.close_submodule_branch_menu(cx);
        if self.remote_blocks_navigation(cx)
            || self.menu.is_some()
            || self.file_search.open
            || self.show_about
        {
            return;
        }
        let request_id = self.request_id;
        let service = Arc::clone(&self.service);
        self.pause_repository_reads();
        self.notice = Some(format!("正在切换子模块分支：{}…", display_path(&root)));
        self.mutation_task = Some(cx.spawn(async move |this, cx| {
            let selected_root = root.clone();
            let result = tasks::spawn(
                cx.background_executor(),
                "submodule.branch.checkout",
                request_id,
                move || {
                    let cancel = AtomicBool::new(false);
                    let map_validation = |error: gitviews_service::ServiceError| {
                        CheckoutError::new(
                            CheckoutErrorKind::InvalidRepository,
                            format!("子模块身份验证失败：{error}"),
                        )
                    };
                    let verified = service
                        .submodule_metadata_with_cancel(&parent, &root, &cancel)
                        .map_err(map_validation)?;
                    if !paths_equal(&verified.root, &root) {
                        return Err(CheckoutError::new(
                            CheckoutErrorKind::InvalidRepository,
                            "子模块根目录已改变。",
                        ));
                    }
                    service.validate_root(&root).map_err(map_validation)?;
                    let outcome = service.checkout(&root, &request)?;
                    service.validate_root(&root).map_err(map_validation)?;
                    service
                        .submodule_metadata_with_cancel(&parent, &root, &cancel)
                        .map_err(map_validation)?;
                    Ok(outcome)
                },
            )
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.request_id != request_id {
                    return;
                }
                view.mutation_task = None;
                view.pause_repository_reads();
                view.service.invalidate(&selected_root);
                view.preview_deck.invalidate_root(&selected_root);
                view.invalidate_changes();
                view.mutation_refresh = true;
                match result {
                    Ok(outcome) => {
                        // Keep the main repo and unrelated file-browser scope.
                        // Only previews of the module just changed become stale.
                        if view
                            .changes_repository()
                            .is_some_and(|repository| paths_equal(&repository.root, &selected_root))
                        {
                            view.clear_changes_preview(cx);
                            view.branch_context = None;
                        }
                        view.notice = Some(format!(
                            "{} {}",
                            display_path(&selected_root),
                            outcome.summary
                        ));
                    }
                    Err(error) => {
                        view.notice = Some(format!(
                            "{} 切换分支未完成：{}",
                            display_path(&selected_root),
                            error.message
                        ))
                    }
                }
                view.scan_pending = false;
                view.queue_scan(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn submodule_branch_popup(
        &mut self,
        light: bool,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        // Keyboard actions can open another surface while this overlay owns the
        // pointer. Drop, rather than merely hide, the menu so its asynchronous
        // read cannot reappear when that surface closes.
        if self.modal_open(cx)
            || self.remote_job.is_some()
            || self.remote_pending.is_some()
            || self.mutation_task.is_some()
            || self.worktree_busy
            || self.loading
            || self.submodule_loading
            || self.menu.is_some()
            || self.file_search.open
            || self.show_about
            || self.submodule_branch_menu.as_ref().is_some_and(|menu| {
                menu.repository_id != self.request_id
                    || self
                        .branch_menu_parent(&menu.target)
                        .is_none_or(|parent| !paths_equal(&parent, &menu.parent))
            })
        {
            self.submodule_branch_menu = None;
            return None;
        }
        let menu = self.submodule_branch_menu.as_ref()?;
        let matches = Arc::new(branches(menu));
        let count = matches.len();
        let menu_id = menu.id;
        let entity = cx.entity();
        let muted = rgb(if light { 0x59636e } else { 0x9198a1 });
        let panel =
            div()
                .id("submodule-branch-picker")
                .test_support()
                .occlude()
                .absolute()
                .top(px(110.))
                .left(px(250.))
                .w(px(420.))
                .p(px(8.))
                .rounded(px(6.))
                .border_1()
                .border_color(rgb(if light { 0xd0d7de } else { 0x3d444d }))
                .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
                .shadow_lg()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        view.close_submodule_branch_menu(cx);
                        view.focus.focus(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(div().px(px(4.)).pb(px(8.)).child("切换子模块分支"))
                .child(
                    div()
                        .px(px(4.))
                        .pb(px(8.))
                        .text_size(px(11.))
                        .text_color(muted)
                        .truncate()
                        .child(display_path(&menu.target)),
                )
                .child(
                    Input::new(&menu.search)
                        .id("submodule-branch-search")
                        .appearance(false)
                        .h(px(32.)),
                )
                .child(
                    div()
                        .px(px(4.))
                        .py(px(8.))
                        .text_size(px(11.))
                        .text_color(muted)
                        .child("仅切换此子模块 · 远程分支创建本地跟踪分支"),
                )
                .when(menu.error.is_some(), |this| {
                    this.child(
                        div()
                            .p(px(8.))
                            .text_color(rgb(0xf85149))
                            .child(menu.error.clone().unwrap_or_default()),
                    )
                })
                .when(count == 0 && menu.error.is_none(), |this| {
                    this.child(div().p(px(12.)).text_color(muted).child(
                        if menu.read_task.is_some() {
                            "正在读取该子模块的分支…"
                        } else {
                            "没有匹配的分支"
                        },
                    ))
                })
                .when(count > 0, |this| {
                    this.child(
                        uniform_list("submodule-branch-list", count, move |range, _, _| {
                            range
                                .map(|index| {
                                    let branch = matches[index].clone();
                                    let target = entity.clone();
                                    let copy_items = copy_menu::branch_info(&branch);
                                    div()
                                        .id(("submodule-branch-row", index))
                                        .test_support()
                                        .role(Role::Button)
                                        .aria_label(format!(
                                            "子模块{} {}",
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
                                                view.focus.focus(window, cx);
                                                view.checkout_submodule_menu_branch(
                                                    menu_id,
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
                })
                .child(
                    div()
                        .id("submodule-branch-close")
                        .test_support()
                        .role(Role::Button)
                        .aria_label("取消切换子模块分支")
                        .px(px(8.))
                        .py(px(8.))
                        .cursor_pointer()
                        .text_color(muted)
                        .child("取消 · Esc")
                        .on_click(cx.listener(|view, _, window, cx| {
                            cx.stop_propagation();
                            view.close_submodule_branch_menu(cx);
                            view.focus.focus(window, cx);
                        })),
                );
        Some(
            div()
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .size_full()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|view, _, window, cx| {
                        view.close_submodule_branch_menu(cx);
                        view.focus.focus(window, cx);
                    }),
                )
                .child(panel),
        )
    }
}
