use super::*;
use crate::ui::copy_menu::{self, CopyMenuExt as _};

impl Workspace {
    pub(super) fn hunk_offsets(&self, light: bool) -> Vec<f32> {
        let mut tree = self.make_tree(light, &mut HashMap::new());
        let Some(body) = find_code_mut(&mut tree) else {
            return Vec::new();
        };
        let mut offset = 0.;
        body.children
            .iter()
            .filter_map(|row| {
                let position = offset;
                offset += row
                    .height
                    .as_ref()
                    .and_then(DesignSize::pixels)
                    .unwrap_or(22.);
                (row.name.starts_with("修改块 ")
                    || row.name.starts_with("单栏 · 修改块 ")
                    || row.name == "实际差异区块")
                    .then_some(position)
            })
            .collect()
    }

    pub(super) fn make_tree(
        &self,
        light: bool,
        actions: &mut HashMap<String, WorkbenchAction>,
    ) -> DesignNode {
        let mut tree = if light {
            self.designs.light.clone()
        } else {
            self.designs.dark.clone()
        };
        if self.unified {
            let replacement = if light {
                &self.designs.unified_light
            } else {
                &self.designs.unified_dark
            };
            if let Some(viewer) = find_name_mut(&mut tree, "文件查看器 · 多标签 Diff") {
                *viewer = replacement.clone();
                viewer.name = "文件查看器 · 多标签 Diff".into();
                viewer.width = Some(DesignSize::Sizing("fill_container".into()));
                viewer.height = Some(DesignSize::Sizing("fill_container".into()));
            }
        }
        if !self.preview {
            workbench::populate(
                &mut tree,
                &WorkbenchData {
                    native_preview: true,
                    repository: self.repository.as_ref(),
                    details: self.details.as_ref(),
                    tree: self.tree.as_deref(),
                    selected_submodule: self
                        .submodule_changes
                        .as_ref()
                        .map(|scope| scope.target.as_path()),
                    changes_repository: self
                        .submodule_changes
                        .as_ref()
                        .map(|scope| &scope.repository),
                    changes_details: self.branch_picker_details(),
                    changes_tree: self
                        .submodule_changes
                        .as_ref()
                        .map(|scope| scope.tree.as_ref()),
                    tree_mode: self.tree_mode,
                    collapsed_paths: &self.collapsed_paths,
                    diff: self.diff.as_deref(),
                    selected_file: self.selected_file.as_deref(),
                    selected_staged: self.selected_staged,
                    opened_files: &self.opened_files,
                    active_tab: self.active_tab,
                    file_query: &self.file_query,
                    repo_query: &self.repo_query,
                    expanded_remotes: &self.expanded_remotes,
                    loading: self.loading
                        || self.diff_loading
                        || self.submodule_loading
                        || self.mutation_task.is_some()
                        || self.mutation_refresh,
                    history: self.history,
                    unified: self.unified,
                    ignore_whitespace: self.ignore_whitespace,
                },
                actions,
            );
        }
        move_hunk_indicator(&mut tree, self.hunk_index);
        if !self.preview && self.source {
            if let Some(header) = find_name_mut(&mut tree, "比较版本标头") {
                header.height = Some(0.0.into());
                header.children.clear();
            }
            if let Some(file) = &self.source_file {
                set_named_text(
                    &mut tree,
                    "编辑器状态",
                    format!("{:?} · 只读 · {} 行", file.encoding, file.line_count),
                );
            }
        }
        if let Some(brand) = find_name_mut(&mut tree, "应用品牌与菜单") {
            brand.children.push(DesignNode {
                kind: "frame".into(),
                id: "caption-drag-space".into(),
                name: "标题栏拖动空白".into(),
                width: Some(DesignSize::Sizing("fill_container".into())),
                height: Some(DesignSize::Sizing("fill_container".into())),
                ..Default::default()
            });
        }
        if self.ignore_whitespace
            && let Some(checkbox) = find_name_mut(&mut tree, "空白字符复选框")
        {
            checkbox.icon = Some("square-check".into());
        }
        let counts = if self.preview {
            3
        } else {
            self.diff
                .as_ref()
                .map(|diff| {
                    diff.lines
                        .iter()
                        .filter(|line| {
                            line.kind == gitviews_service::DiffLineKind::Section
                                && line.text.starts_with("@@")
                        })
                        .count()
                })
                .unwrap_or(0)
        };
        set_named_text(
            &mut tree,
            "当前修改定位计数",
            format!(
                "修改 {} / {counts}",
                if counts == 0 { 0 } else { self.hunk_index + 1 }
            ),
        );
        if self.preview
            && self.source
            && let Some(diff) = &self.diff
            && let Some(body) = find_code_mut(&mut tree)
        {
            body.children = diff
                .new_text
                .lines()
                .take(3000)
                .enumerate()
                .map(|(index, line)| {
                    let mut row = simple_text(
                        format!("source-{index}"),
                        format!("{:>5}    {line}", index + 1),
                        light,
                        true,
                    );
                    row.height = Some(22.0.into());
                    row
                })
                .collect();
        }
        apply_collapsed(&mut tree, &self.collapsed);
        if !self.summary.is_empty() {
            set_named_text(&mut tree, "提交摘要内容", self.summary.clone());
        }
        if !self.description.is_empty() {
            set_named_text(&mut tree, "可选提交描述提示", self.description.clone());
        }
        if !self.repo_query.is_empty() {
            set_named_text(&mut tree, "筛选提示", self.repo_query.clone());
        }
        self.decorate_remote_controls(&mut tree);
        if let Some(header) = find_name_mut(&mut tree, "工作树标题") {
            header.children.push(DesignNode {
                kind: "frame".into(),
                id: "manage-worktrees".into(),
                name: "管理工作树".into(),
                width: Some(24.0.into()),
                height: Some(24.0.into()),
                align_items: Some("center".into()),
                justify_content: Some("center".into()),
                children: vec![DesignNode {
                    kind: "icon".into(),
                    id: "manage-worktrees-icon".into(),
                    icon: Some("settings".into()),
                    width: Some(14.0.into()),
                    height: Some(14.0.into()),
                    fill: Some(if light { "#59636e" } else { "#9198a1" }.into()),
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
        if let Some(template) = find_name_mut(&mut tree, "获取远端").cloned() {
            let mut history = template;
            history.name = "查看提交历史".into();
            history.id = "commit-history-toolbar".into();
            for (index, child) in history.children.iter_mut().enumerate() {
                child.id = format!("commit-history-toolbar-{index}");
                if child.kind == "text" {
                    child.content = Some("历史".into());
                }
                if child.kind == "icon" {
                    child.icon = Some("history".into());
                }
            }
            if let Some(toolbar) = find_name_mut(&mut tree, "仓库全局工具栏") {
                let at = toolbar
                    .children
                    .iter()
                    .position(|node| node.name == "获取远端")
                    .unwrap_or(toolbar.children.len());
                toolbar.children.insert(at, history);
            }
        }
        if let Some(mut settings) = find_name_mut(&mut tree, "获取远端").cloned() {
            settings.name = "打开应用设置".into();
            settings.id = "settings-toolbar".into();
            for (index, child) in settings.children.iter_mut().enumerate() {
                child.id = format!("settings-toolbar-{index}");
                if child.kind == "text" {
                    child.content = Some(
                        if self.settings_error.is_some() {
                            "设置 !"
                        } else {
                            "设置"
                        }
                        .into(),
                    );
                }
                if child.kind == "icon" {
                    child.icon = Some("settings".into());
                }
            }
            if let Some(toolbar) = find_name_mut(&mut tree, "仓库全局工具栏") {
                let at = toolbar
                    .children
                    .iter()
                    .position(|node| node.name == "查看提交历史")
                    .unwrap_or(toolbar.children.len());
                toolbar.children.insert(at, settings);
            }
        }
        if let Some(mut mode) = find_name_mut(&mut tree, "获取远端").cloned() {
            mode.name = "切换只读模式".into();
            mode.id = "read-only-mode-toolbar".into();
            mode.width = None;
            for (index, child) in mode.children.iter_mut().enumerate() {
                child.id = format!("read-only-mode-toolbar-{index}");
                if child.kind == "text" {
                    child.content = Some(
                        if self.settings.read_only {
                            "只读模式 · 关闭"
                        } else {
                            "高级模式 · 切为只读"
                        }
                        .into(),
                    );
                }
                if child.kind == "icon" {
                    child.icon = Some(
                        if self.settings.read_only {
                            "eye"
                        } else {
                            "settings"
                        }
                        .into(),
                    );
                }
            }
            if let Some(toolbar) = find_name_mut(&mut tree, "仓库全局工具栏") {
                let at = toolbar
                    .children
                    .iter()
                    .position(|node| node.name == "打开应用设置")
                    .unwrap_or(toolbar.children.len());
                toolbar.children.insert(at, mode);
            }
        }
        if let Some(search) = find_name_mut(&mut tree, "搜索命令或文件") {
            for child in &mut search.children {
                match child.name.as_str() {
                    "搜索提示" => child.content = Some("搜索文件或内容…".into()),
                    "命令快捷键" => child.content = Some("Ctrl P".into()),
                    _ => {}
                }
            }
        }
        if let Some(repository) = self.changes_repository() {
            set_named_text(&mut tree, "仓库名称", repository.name.clone());
            let head = match &repository.head {
                Head::Branch(name) | Head::Unborn(name) => name.clone(),
                Head::Detached(id) => format!("{}（分离）", id.chars().take(8).collect::<String>()),
            };
            set_named_text(&mut tree, "分支名称", head);
        }
        if self.settings.read_only {
            prune_advanced_controls(&mut tree);
            actions.retain(|_, action| {
                !matches!(
                    action,
                    WorkbenchAction::CheckoutBranch { .. }
                        | WorkbenchAction::OpenBranchMenu
                        | WorkbenchAction::SubmoduleBranches(_)
                )
            });
        }
        tree
    }

    fn popup(&self, light: bool, cx: &mut Context<Self>) -> Div {
        if self.menu == Some("分支") {
            return self.branch_popup(light, cx);
        }
        if self.menu == Some("仓库列表") {
            return self.repository_popup(light, cx);
        }
        let bg = if light { 0xffffff } else { 0x161b22 };
        let border = if light { 0xd0d7de } else { 0x3d444d };
        let left = match self.menu {
            Some("查看") => 236.,
            Some("帮助") => 334.,
            Some("仓库") => 285.,
            Some("编辑") => 187.,
            _ => 138.,
        };
        let mut popup = div()
            .occlude()
            .absolute()
            .top(px(39.))
            .left(px(left))
            .w(px(220.))
            .p(px(5.))
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(border))
            .bg(rgb(bg))
            .shadow_lg();
        if self.menu == Some("仓库") && !self.settings.read_only {
            popup = popup.child(
                div()
                    .id("open-git-engine-menu")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("Git 方案")
                    .px(px(10.))
                    .py(px(9.))
                    .cursor_pointer()
                    .child("Git 方案：内置 / 本地…")
                    .on_click(cx.listener(|this, _, window, cx| this.open_git_engine(window, cx))),
            );
            popup = popup.child(
                div()
                    .id("manage-conflicts-menu")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("合并变基与冲突处理")
                    .px(px(10.))
                    .py(px(9.))
                    .cursor_pointer()
                    .child("合并、变基与冲突…    Ctrl Alt M")
                    .on_click(cx.listener(|view, _, window, cx| view.open_conflicts(window, cx))),
            );
            popup = popup.child(
                div()
                    .id("history-tools-menu")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("历史分析与分支恢复")
                    .px(px(10.))
                    .py(px(9.))
                    .cursor_pointer()
                    .child("历史分析与恢复…     Ctrl Alt H")
                    .on_click(
                        cx.listener(|view, _, window, cx| view.open_history_tools(window, cx)),
                    ),
            );
            popup = popup.child(
                div()
                    .id("manage-branches-menu")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("管理分支")
                    .px(px(10.))
                    .py(px(9.))
                    .cursor_pointer()
                    .child("管理当前仓库分支…     Ctrl Alt B")
                    .on_click(
                        cx.listener(|view, _, window, cx| view.open_branch_manager(window, cx)),
                    ),
            );
            popup = popup.child(
                div()
                    .id("manage-stashes-menu")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("管理储藏")
                    .px(px(10.))
                    .py(px(9.))
                    .cursor_pointer()
                    .child("管理当前仓库储藏…     Ctrl Alt S")
                    .on_click(cx.listener(|view, _, window, cx| view.open_stashes(window, cx))),
            );
            popup = popup.child(
                div()
                    .id("open-git-settings-menu")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("Git 设置")
                    .px(px(10.))
                    .py(px(9.))
                    .cursor_pointer()
                    .child("Git 设置…     Ctrl Alt ,")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_git_settings(window, cx)),
                    ),
            );
            popup = popup.child(
                div()
                    .id("manage-worktrees-menu")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("管理工作树")
                    .px(px(10.))
                    .py(px(9.))
                    .cursor_pointer()
                    .child("管理工作树…     Ctrl Alt W")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_worktrees(false, window, cx)),
                    ),
            );
        }
        if self.menu == Some("帮助") {
            popup = popup.child(
                div()
                    .id("about-gitviews")
                    .role(Role::Button)
                    .aria_label("关于 GitViews 与第三方许可")
                    .px(px(10.))
                    .py(px(9.))
                    .cursor_pointer()
                    .child("关于 GitViews / 第三方许可")
                    .on_click(cx.listener(|this, _, window, cx| this.open_about(window, cx))),
            );
        } else if self.menu == Some("查看") {
            popup = popup.child(
                div()
                    .id("toggle-read-only-menu")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("切换只读模式")
                    .px(px(10.))
                    .py(px(7.))
                    .cursor_pointer()
                    .child(if self.settings.read_only {
                        "✓  只读简单模式（点击关闭）"
                    } else {
                        "开启只读简单模式"
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_read_only(window, cx))),
            );
            popup = popup.child(
                div()
                    .id("open-settings-menu")
                    .role(Role::Button)
                    .aria_label("应用设置")
                    .px(px(10.))
                    .py(px(7.))
                    .cursor_pointer()
                    .child("应用设置…              Ctrl ,")
                    .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
            );
            for appearance in Appearance::ALL {
                popup = popup.child(
                    div()
                        .id(("theme", appearance as usize))
                        .px(px(10.))
                        .py(px(7.))
                        .cursor_pointer()
                        .child(format!(
                            "{}{}",
                            if self.appearance == appearance {
                                "✓  "
                            } else {
                                "    "
                            },
                            appearance.label()
                        ))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.save_theme(appearance, window, cx);
                        })),
                );
            }
            popup = popup.child(
                div()
                    .id("history-item")
                    .px(px(10.))
                    .py(px(7.))
                    .cursor_pointer()
                    .child("提交历史                 Ctrl H")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_history(cx);
                        this.menu = None;
                        cx.notify();
                    })),
            );
            popup = popup.child(
                div()
                    .id("reset-columns")
                    .role(Role::Button)
                    .aria_label("恢复默认三栏宽度")
                    .px(px(10.))
                    .py(px(7.))
                    .cursor_pointer()
                    .child("恢复默认三栏宽度")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.column_sizes.update(cx, |sizes, cx| {
                            sizes.clear();
                            cx.notify();
                        });
                        this.menu = None;
                        cx.notify();
                    })),
            );
        } else {
            popup = popup.child(
                div()
                    .id("open-item")
                    .px(px(10.))
                    .py(px(7.))
                    .cursor_pointer()
                    .child("打开仓库…               Ctrl O")
                    .on_click(cx.listener(|this, _, _, cx| this.choose_repository(cx))),
            );
            if !self.settings.read_only {
                popup = popup
                    .child(
                        div()
                            .id("clone-repository-menu")
                            .test_support()
                            .role(Role::Button)
                            .aria_label("克隆仓库")
                            .px(px(10.))
                            .py(px(7.))
                            .cursor_pointer()
                            .child("克隆仓库…         Ctrl Shift O")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_repository_setup(
                                    gitviews_service::RepositorySetupKind::Clone,
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .child(
                        div()
                            .id("new-repository-menu")
                            .test_support()
                            .role(Role::Button)
                            .aria_label("新建仓库")
                            .px(px(10.))
                            .py(px(7.))
                            .cursor_pointer()
                            .child("新建仓库…                 Ctrl N")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_repository_setup(
                                    gitviews_service::RepositorySetupKind::Init,
                                    window,
                                    cx,
                                )
                            })),
                    );
            }
            popup = popup.child(
                div()
                    .id("refresh-item")
                    .px(px(10.))
                    .py(px(7.))
                    .cursor_pointer()
                    .child("刷新工作区              Ctrl R")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        this.refresh(cx);
                    })),
            );
        }
        popup
    }

    fn repository_popup(&self, light: bool, cx: &mut Context<Self>) -> Div {
        let muted = if light { 0x59636e } else { 0x9198a1 };
        let border = if light { 0xd0d7de } else { 0x3d444d };
        let mut list = div()
            .id("repository-catalog-list")
            .max_h(px(390.))
            .overflow_y_scroll();
        for (index, entry) in self.catalog.repositories.iter().enumerate() {
            let current = self
                .repository
                .as_ref()
                .is_some_and(|repo| paths_equal(&repo.root, &entry.root));
            let selected_path = entry.root.clone();
            let removed_path = entry.root.clone();
            let copy_items = copy_menu::named_path(&entry.name, &entry.root);
            list = list.child(
                div()
                    .id(("repository-entry", index))
                    .role(Role::Button)
                    .aria_label(format!("切换仓库 {}", entry.name))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .py(px(9.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(current, |this| {
                        this.bg(rgb(if light { 0xddf4ff } else { 0x1a3453 }))
                    })
                    .child(div().w(px(16.)).child(if current { "✓" } else { "" }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(entry.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(muted))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(display_path(&entry.root)),
                            ),
                    )
                    .child(
                        div()
                            .id(("remove-repository", index))
                            .role(Role::Button)
                            .aria_label(format!("从列表移除仓库 {}", entry.name))
                            .px(px(6.))
                            .py(px(3.))
                            .text_color(rgb(muted))
                            .child("×")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.remove_repository(&removed_path, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.menu = None;
                        this.load(selected_path.clone(), cx);
                    }))
                    .copy_menu(copy_items),
            );
        }
        if self.catalog.repositories.is_empty() {
            list = list.child(
                div()
                    .px(px(12.))
                    .py(px(18.))
                    .text_color(rgb(muted))
                    .child("尚未添加仓库"),
            );
        }
        div()
            .occlude()
            .absolute()
            .top(px(89.))
            .left(px(12.))
            .w(px(400.))
            .p(px(5.))
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(border))
            .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
            .shadow_lg()
            .child(
                div()
                    .px(px(10.))
                    .py(px(7.))
                    .text_color(rgb(muted))
                    .child(format!(
                        "本地仓库 · {}{}",
                        self.catalog.repositories.len(),
                        if self.catalog_import_count > 0 {
                            " · 正在添加…"
                        } else {
                            ""
                        }
                    )),
            )
            .child(list)
            .when_some(self.catalog_error.clone(), |this, error| {
                this.child(
                    div()
                        .px(px(10.))
                        .py(px(7.))
                        .text_color(rgb(if light { 0xcf222e } else { 0xf85149 }))
                        .child(error),
                )
            })
            .child(
                div()
                    .id("add-repositories")
                    .role(Role::Button)
                    .aria_label("添加本地仓库")
                    .mt(px(4.))
                    .border_t_1()
                    .border_color(rgb(border))
                    .px(px(10.))
                    .py(px(9.))
                    .cursor_pointer()
                    .child("＋ 添加本地仓库…                      Ctrl O")
                    .on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        this.choose_repository(cx);
                    })),
            )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.modal_open(cx)
            || self.menu.is_some()
            || self.file_search.open
            || self.show_about
            || self.submodule_branch_menu.is_some()
        {
            self.dismiss_history_reference_menu();
        }
        self.bind_preview_file_history(window, cx);
        let light = cx.theme().mode == ThemeMode::Light;
        let mut workbench_actions = HashMap::new();
        let tree = self.make_tree(light, &mut workbench_actions);
        let mut ui_actions = HashMap::new();
        let mut context = RenderContext {
            copy_menus: self.copy_menus(&tree, &workbench_actions),
            ..Default::default()
        };
        collect_actions(
            &tree,
            &mut ui_actions,
            &mut context,
            self.preview,
            &self.code_scroll,
        );
        context
            .interactive_ids
            .extend(workbench_actions.keys().cloned());
        let middle_tabs = workbench_actions
            .iter()
            .filter_map(|(id, action)| match action {
                WorkbenchAction::ActivateTab(index) | WorkbenchAction::CloseTab(index) => {
                    Some((id.clone(), *index))
                }
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        context.middle_click_ids.extend(middle_tabs.keys().cloned());
        let entity = cx.entity().downgrade();
        context.on_middle_click = Some(Rc::new(move |id, _, _, cx| {
            if let Some(index) = middle_tabs.get(&id) {
                cx.stop_propagation();
                let _ = entity.update(cx, |workspace, cx| workspace.close_tab(*index, cx));
            }
        }));
        let entity = cx.entity().downgrade();
        context.on_click = Some(Rc::new(move |id, event, window, cx| {
            cx.stop_propagation();
            if matches!(
                workbench_actions.get(&id),
                Some(WorkbenchAction::OpenSubmodule(_) | WorkbenchAction::CheckoutBranch { .. })
            ) && event.click_count() != 2
                && !event.is_keyboard()
            {
                return;
            }
            let _ = entity.update(cx, |this, cx| {
                this.active_input = None;
                if let Some(action) = workbench_actions.get(&id) {
                    this.handle_workbench(action, window, cx);
                } else if let Some(action) = ui_actions.get(&id) {
                    this.handle(action, window, cx);
                }
            });
        }));
        let active_input = self.active_input;
        let inputs = self.inputs.clone();
        let preview_pane = self.preview_pane.clone();
        let file_list = self.file_list.clone();
        let preview = self.preview;
        let commit_form = std::cell::RefCell::new(
            (!preview && !self.settings.read_only).then(|| self.render_commit_form(cx)),
        );
        let hunk_controls = std::cell::RefCell::new((!preview).then(|| self.hunk_controls(cx)));
        let history = self.history;
        let history_element = std::cell::RefCell::new(history.then(|| self.render_history(cx)));
        let submodule_bar = std::cell::RefCell::new(
            self.submodule_changes
                .as_ref()
                .map(|_| self.submodule_context_bar(light, cx)),
        );
        context.override_element = Some(Rc::new(move |node, _, _| {
            if !preview {
                if node.name == "提交变更区域" {
                    return commit_form
                        .borrow_mut()
                        .take()
                        .map(IntoElement::into_any_element);
                }
                if node.name == "文件查看器 · 多标签 Diff" && history {
                    return history_element.borrow_mut().take();
                }
                if node.name == "变更文件列表 · 四组均展开" {
                    return Some(
                        div()
                            .id("native-file-list")
                            .test_support()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .w_full()
                            .overflow_hidden()
                            .when_some(submodule_bar.borrow_mut().take(), |this, bar| {
                                this.child(bar)
                            })
                            .child(div().flex_1().min_h_0().w_full().child(file_list.clone()))
                            .into_any_element(),
                    );
                }
                if matches!(node.name.as_str(), "双栏代码差异" | "单栏代码差异") {
                    return Some(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .w_full()
                            .overflow_hidden()
                            .when_some(hunk_controls.borrow_mut().take(), |this, controls| {
                                this.child(controls)
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .w_full()
                                    .child(preview_pane.clone()),
                            )
                            .into_any_element(),
                    );
                }
            }
            let field = active_input?;
            if node.name != field.node() {
                return None;
            }
            let bg = if light { 0xffffff } else { 0x0d1117 };
            let border = if light { 0x0969da } else { 0x4493f8 };
            let width = node.width.as_ref().and_then(DesignSize::pixels);
            Some(
                div()
                    .when_some(width, |this, width| this.w(px(width)))
                    .when(width.is_none(), |this| this.w_full())
                    .h(px(node
                        .height
                        .as_ref()
                        .and_then(DesignSize::pixels)
                        .unwrap_or(32.)))
                    .rounded(px(6.))
                    .border_1()
                    .border_color(rgb(border))
                    .bg(rgb(bg))
                    .child(
                        Input::new(&inputs[field.index()])
                            .appearance(false)
                            .bordered(false)
                            .focus_bordered(false)
                            .h(px(30.)),
                    )
                    .into_any_element(),
            )
        }));
        let context = self.with_resizable_columns(context, light);
        let content = design_renderer::render(&tree, &context, window, cx);
        div()
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .key_context("GitViews")
            .text_size(px(12.))
            .font_family("Noto Sans SC")
            .text_color(rgb(if light { 0x1f2328 } else { 0xf0f6fc }))
            .on_action(
                cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)),
            )
            .on_action(cx.listener(|this, _: &OpenGitSettings, window, cx| {
                this.open_git_settings(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ManageWorktrees, window, cx| {
                this.open_worktrees(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenRepository, _, cx| {
                if !this.modal_open(cx) {
                    this.choose_repository(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CloneRepository, window, cx| {
                this.open_repository_setup(gitviews_service::RepositorySetupKind::Clone, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NewRepository, window, cx| {
                this.open_repository_setup(gitviews_service::RepositorySetupKind::Init, window, cx)
            }))
            .on_action(cx.listener(|this, _: &RefreshRepository, _, cx| {
                if !this.modal_open(cx) {
                    this.refresh(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SearchFiles, window, cx| {
                if !this.modal_open(cx) {
                    this.start_input(Field::Search, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextChange, _, cx| {
                if !this.modal_open(cx) {
                    this.navigate_change(1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PreviousChange, _, cx| {
                if !this.modal_open(cx) {
                    this.navigate_change(-1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ShowHistory, _, cx| {
                if !this.modal_open(cx) {
                    this.toggle_history(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ClosePreviewTab, _, cx| {
                if !this.modal_open(cx)
                    && let Some(index) = this.active_tab
                {
                    this.close_tab(index, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextPreviewTab, _, cx| {
                if !this.modal_open(cx) {
                    this.cycle_tab(1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PreviousPreviewTab, _, cx| {
                if !this.modal_open(cx) {
                    this.cycle_tab(-1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FindInFile, window, cx| {
                if this.modal_open(cx) {
                    return;
                }
                this.preview_pane
                    .update(cx, |pane, cx| pane.find(window, cx));
            }))
            .on_action(cx.listener(|this, _: &CommitChanges, window, cx| {
                if !this.modal_open(cx) {
                    this.commit_changes(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ManageBranches, window, cx| {
                if !this.modal_open(cx) {
                    this.open_branch_manager(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ManageStashes, window, cx| {
                if !this.modal_open(cx) {
                    this.open_stashes(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenHistoryTools, window, cx| {
                if !this.modal_open(cx) {
                    this.open_history_tools(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ManageConflicts, window, cx| {
                if !this.modal_open(cx) {
                    this.open_conflicts(window, cx);
                }
            }))
            .child(content)
            .when(
                !self.preview
                    && self.repository.is_none()
                    && !self.loading
                    && self.catalog.repositories.is_empty(),
                |this| {
                    this.child(
                        div()
                            .absolute()
                            .top(px(180.))
                            .left(px(440.))
                            .w(px(570.))
                            .p(px(24.))
                            .rounded(px(8.))
                            .border_1()
                            .border_color(rgb(if light { 0xd0d7de } else { 0x3d444d }))
                            .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
                            .shadow_md()
                            .child(
                                div()
                                    .text_size(px(22.))
                                    .mb(px(12.))
                                    .child("开始使用 GitViews"),
                            )
                            .child(
                                div()
                                    .mb(px(16.))
                                    .child(if self.settings.read_only {
                                        "打开本地仓库，观察 AI 代码的变更、Diff 和提交历史。只读模式不会修改仓库。"
                                    } else { "克隆团队仓库、新建项目，或打开已有的本地仓库。" }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(px(12.))
                                    .when(!self.settings.read_only, |row| row.child(
                                        div()
                                            .id("welcome-clone-repository")
                                            .test_support()
                                            .role(Role::Button)
                                            .aria_label("克隆仓库")
                                            .cursor_pointer()
                                            .px(px(12.))
                                            .py(px(9.))
                                            .rounded(px(5.))
                                            .bg(rgb(if light { 0xddf4ff } else { 0x1f3b56 }))
                                            .child("克隆仓库…")
                                            .on_click(cx.listener(|view, _, window, cx| {
                                                view.open_repository_setup(
                                                    gitviews_service::RepositorySetupKind::Clone,
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id("welcome-new-repository")
                                            .test_support()
                                            .role(Role::Button)
                                            .aria_label("新建仓库")
                                            .cursor_pointer()
                                            .px(px(12.))
                                            .py(px(9.))
                                            .rounded(px(5.))
                                            .border_1()
                                            .border_color(rgb(if light {
                                                0xd0d7de
                                            } else {
                                                0x3d444d
                                            }))
                                            .child("新建仓库…")
                                            .on_click(cx.listener(|view, _, window, cx| {
                                                view.open_repository_setup(
                                                    gitviews_service::RepositorySetupKind::Init,
                                                    window,
                                                    cx,
                                                )
                                            })),
                                    ))
                                    .child(
                                        div()
                                            .id("welcome-open-repository")
                                            .test_support()
                                            .role(Role::Button)
                                            .aria_label("打开本地仓库")
                                            .cursor_pointer()
                                            .px(px(12.))
                                            .py(px(9.))
                                            .rounded(px(5.))
                                            .border_1()
                                            .border_color(rgb(if light {
                                                0xd0d7de
                                            } else {
                                                0x3d444d
                                            }))
                                            .child("打开本地仓库…")
                                            .on_click(cx.listener(|view, _, _, cx| {
                                                view.choose_repository(cx)
                                            })),
                                    ),
                            ),
                    )
                },
            )
            .when(self.menu.is_some(), |this| {
                this.child(self.popup(light, cx))
            })
            .when_some(self.notice.clone(), |this, notice| {
                this.child(
                    div()
                        .absolute()
                        .top(px(106.))
                        .right(px(16.))
                        .max_w(px(560.))
                        .px(px(14.))
                        .py(px(10.))
                        .rounded(px(6.))
                        .border_1()
                        .border_color(rgb(if light { 0xd0d7de } else { 0x3d444d }))
                        .bg(rgb(if light { 0xffffff } else { 0x212830 }))
                        .shadow_md()
                        .child(
                            div().flex().gap(px(16.)).child(notice).child(
                                div()
                                    .id("dismiss-notice")
                                    .cursor_pointer()
                                    .child("×")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.notice = None;
                                        cx.notify();
                                    })),
                            ),
                        ),
                )
            })
            .when(self.show_about, |this| {
                this.child(self.about_panel(light, cx))
            })
            .when(self.remote_job.is_some(), |this| {
                this.child(self.remote_progress_panel(light, cx))
            })
            .when(self.navigation_loading(), |this| {
                this.child(self.navigation_progress_panel(light, cx))
            })
            .child(self.remote_dialog.clone())
            .when_some(self.submodule_branch_popup(light, cx), |this, popup| {
                this.child(popup)
            })
            .when(self.file_search.open, |this| {
                this.child(self.file_search_popup(light, cx))
            })
            .child(self.settings_dialog.clone())
            .child(self.git_settings_dialog.clone())
            .child(self.worktree_dialog.clone())
            .child(self.repository_setup_dialog.clone())
            .child(self.branch_dialog.clone())
            .child(self.stash_dialog.clone())
            .child(self.history_tools_dialog.clone())
            .child(self.conflict_dialog.clone())
            .when(
                !self.git_engine_ready && !self.git_engine_dialog.read(cx).is_open(),
                |this| {
                    this.child(
                        div()
                            .absolute()
                            .inset_0()
                            .occlude()
                            .bg(rgba(0x00000088))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("正在读取 Git 方案…"),
                    )
                },
            )
            .child(self.git_engine_dialog.clone())
    }
}

/// Remove whole controls so simple mode also reclaims their layout space.
fn prune_advanced_controls(node: &mut DesignNode) {
    node.children.retain(|child| {
        !matches!(
            child.name.as_str(),
            "提交变更区域"
                | "获取远端"
                | "立即获取远程更新"
                | "拉取远端"
                | "推送提交"
                | "管理工作树"
                | "新增工作树"
                | "新建分支"
                | "分支切换"
                | "本地分支分区"
                | "远程分支分区"
                | "储藏入口分区"
                | "切换子模块分支"
                | "子模块未初始化，无法切换分支"
                | "编辑菜单"
                | "更多仓库操作"
                | "更多工作区操作"
                | "文件更多操作"
        ) && !child.name.starts_with("暂存全部")
            && !child.name.starts_with("取消暂存")
    });
    for child in &mut node.children {
        prune_advanced_controls(child);
    }
}

fn collect_actions(
    node: &DesignNode,
    actions: &mut HashMap<String, UiAction>,
    context: &mut RenderContext,
    preview: bool,
    scroll: &ScrollHandle,
) {
    let name = node.name.as_str();
    let action = match name {
        "选择当前仓库" | "仓库切换器" | "切换仓库" => {
            Some(UiAction::Menu("仓库列表"))
        }
        "刷新文件变更" => Some(UiAction::Refresh),
        "查看提交历史" => Some(UiAction::History),
        "打开应用设置" => Some(UiAction::Settings),
        "切换只读模式" => Some(UiAction::ToggleReadOnly),
        "管理工作树" => Some(UiAction::Worktrees(false)),
        "新增工作树" => Some(UiAction::Worktrees(true)),
        "文件菜单" => Some(UiAction::Menu("文件")),
        "编辑菜单" => Some(UiAction::Menu("编辑")),
        "查看菜单" => Some(UiAction::Menu("查看")),
        "仓库菜单" | "更多仓库操作" => Some(UiAction::Menu("仓库")),
        "帮助菜单" => Some(UiAction::Menu("帮助")),
        "筛选工作树和分支" => Some(UiAction::Field(Field::Repository)),
        "筛选文件输入框" => Some(UiAction::Field(Field::Files)),
        "搜索命令或文件" => Some(UiAction::Field(Field::Search)),
        "提交摘要输入框" => Some(UiAction::Field(Field::Summary)),
        "可选提交描述输入框" => Some(UiAction::Field(Field::Description)),
        "单栏视图" | "单栏视图 · 当前" => Some(UiAction::Unified(true)),
        "双栏视图" | "双栏视图 · 当前" => Some(UiAction::Unified(false)),
        "忽略空白字符选项" => Some(UiAction::IgnoreWhitespace),
        "上一个修改 · Alt+↑" => Some(UiAction::Change(-1)),
        "下一个修改 · Alt+↓" => Some(UiAction::Change(1)),
        "跳转文件源码" | "展开文件末尾未修改代码" | "单栏展开未修改代码" => {
            Some(UiAction::Source)
        }
        "获取远端" | "立即获取远程更新" => Some(UiAction::Remote(RemoteAction::Fetch)),
        "拉取远端" => Some(UiAction::Remote(RemoteAction::Pull)),
        "推送提交" => Some(UiAction::Remote(RemoteAction::Push)),
        "分支切换" => Some(UiAction::Menu("分支")),
        "新建分支" => Some(UiAction::Branches),
        "储藏入口" => Some(UiAction::Stashes),
        "多栏视图" => Some(UiAction::Unavailable(name.into())),
        "按状态分组" if preview => Some(UiAction::Unavailable("切换文件分组方式".into())),
        _ if name == "提交已暂存文件" || (name.starts_with("提交 ") && node.kind == "frame") => {
            Some(UiAction::Commit)
        }
        _ if name.starts_with("取消暂存全部") => Some(UiAction::UnstageAll),
        _ if name.starts_with("暂存全部") => Some(UiAction::StageAll),
        _ if preview && (name.ends_with(" 标签") || name.starts_with("关闭 ")) => {
            Some(UiAction::PreviewFile)
        }
        _ if name == "工作树标题"
            || name == "本地分支标题"
            || name == "远程分支标题"
            || name.ends_with(" · 已展开") =>
        {
            Some(UiAction::Toggle(name.into()))
        }
        _ => None,
    };
    if let Some(action) = action {
        actions.insert(node.id.clone(), action);
        context.interactive_ids.insert(node.id.clone());
    }
    if matches!(name, "双栏代码差异" | "单栏代码差异") {
        context.scroll_y_ids.insert(node.id.clone());
        context.scroll_x_ids.insert(node.id.clone());
        context
            .scroll_handles
            .insert(node.id.clone(), scroll.clone());
    }
    if name == "打开的文件标签" {
        context.scroll_x_ids.insert(node.id.clone());
    }
    if matches!(
        name,
        "变更文件列表 · 四组均展开" | "仓库信息 · 工作树与分支" | "实际近期提交列表"
    ) {
        context.scroll_y_ids.insert(node.id.clone());
    }
    for child in &node.children {
        collect_actions(child, actions, context, preview, scroll);
    }
}

pub(super) fn find_name_mut<'a>(
    node: &'a mut DesignNode,
    name: &str,
) -> Option<&'a mut DesignNode> {
    if node.name == name {
        return Some(node);
    }
    node.children
        .iter_mut()
        .find_map(|child| find_name_mut(child, name))
}

fn move_hunk_indicator(tree: &mut DesignNode, index: usize) {
    let Some(marker) = find_name_mut(tree, "当前修改块指示").cloned() else {
        return;
    };
    let Some(body) = find_code_mut(tree) else {
        return;
    };
    let mut current = 0;
    for row in &mut body.children {
        if row.name.starts_with("修改块 ") || row.name.starts_with("单栏 · 修改块 ") {
            row.children.retain(|child| child.name != "当前修改块指示");
            if current == index {
                row.children.push(marker.clone());
            }
            current += 1;
        }
    }
}

pub(super) fn set_named_text(node: &mut DesignNode, name: &str, value: String) {
    if let Some(node) = find_name_mut(node, name) {
        node.content = Some(value);
    }
}

fn find_code_mut(node: &mut DesignNode) -> Option<&mut DesignNode> {
    if matches!(node.name.as_str(), "双栏代码差异" | "单栏代码差异") {
        return Some(node);
    }
    node.children.iter_mut().find_map(find_code_mut)
}

fn apply_collapsed(node: &mut DesignNode, collapsed: &HashSet<String>) {
    if let Some(first) = node.children.first()
        && collapsed.contains(&first.name)
    {
        node.children.truncate(1);
        if let Some(icon) = node.children[0]
            .children
            .iter_mut()
            .find(|child| child.kind == "icon")
        {
            icon.icon = Some("chevron-right".into());
        }
    }
    for child in &mut node.children {
        apply_collapsed(child, collapsed);
    }
}

fn simple_text(id: String, content: String, light: bool, mono: bool) -> DesignNode {
    DesignNode {
        kind: "text".into(),
        id,
        name: "源码行".into(),
        content: Some(content),
        font_family: Some(
            if mono {
                "JetBrains Mono"
            } else {
                "Noto Sans SC"
            }
            .into(),
        ),
        font_size: Some(12.),
        fill: Some(if light { "#1f2328" } else { "#f0f6fc" }.into()),
        ..Default::default()
    }
}
