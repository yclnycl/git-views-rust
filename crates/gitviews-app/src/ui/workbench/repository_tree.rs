//! Repository tree presentation. Gitlinks are visible leaves, never directories.

use gitviews_service::{FileTreeEntry, FileTreeKind, SubmoduleInfo};

use super::*;

pub(super) fn populate_file_tree(
    design: &mut DesignNode,
    data: &WorkbenchData<'_>,
    actions: &mut HashMap<String, WorkbenchAction>,
    palette: &Palette,
) {
    let selected = find(design, "WorktreePanel.tsx · 当前选中").cloned();
    let normal = find(design, "useWorktree.ts").cloned();
    let Some(list) = find_mut(design, "变更文件列表 · 四组均展开") else {
        return;
    };
    list.gap = Some(0.);
    list.children.clear();
    let (Some(repository), Some(tree), Some(selected), Some(normal)) =
        (data.repository, data.tree, selected, normal)
    else {
        list.children.push(message(
            "tree-loading",
            if data.repository.is_none() {
                "打开仓库以浏览文件树"
            } else if data.loading {
                "正在读取仓库文件树…"
            } else {
                "文件树尚未读取"
            },
            palette,
        ));
        return;
    };
    for (index, (entry, depth)) in
        visible_entries(&tree.entries, data.file_query, data.collapsed_paths)
            .into_iter()
            .enumerate()
    {
        let staged = opens_staged(entry);
        let is_selected =
            data.selected_file == Some(entry.path.as_str()) && data.selected_staged == staged;
        let mut row = if is_selected {
            selected.clone()
        } else {
            normal.clone()
        };
        seed(&mut row, &format!("tree-entry-{index}"));
        row.name = format!("文件树 {}", entry.path);
        row.context = Some(entry.path.clone());
        row.height = Some(32.0.into());
        row.padding = Some(DesignSpacing::Sides(vec![
            0.,
            12.,
            0.,
            12. + depth as f32 * 14.,
        ]));
        row.gap = Some(6.);
        let expanded =
            !data.file_query.trim().is_empty() || !data.collapsed_paths.contains(&entry.path);
        let module = tree
            .submodules
            .iter()
            .find(|module| module.path == entry.path);
        let status = status_text(entry.index_status, entry.worktree_status);
        let color = format!(
            "#{:06x}",
            ChangeKind::from_status(entry.index_status, entry.worktree_status).rgb(palette.light)
        );
        let chevron = if entry.kind == FileTreeKind::Directory {
            icon(
                &format!("tree-arrow-{index}"),
                if expanded {
                    "chevron-down"
                } else {
                    "chevron-right"
                },
                &palette.muted,
                14.,
            )
        } else {
            let mut spacer = frame(&format!("tree-arrow-{index}"), "文件树图标对齐留白");
            spacer.width = Some(14.0.into());
            spacer.height = Some(14.0.into());
            spacer
        };
        let file_icon = icon(
            &format!("tree-icon-{index}"),
            match entry.kind {
                FileTreeKind::Directory if expanded => "folder-open",
                FileTreeKind::Directory => "folder",
                FileTreeKind::Submodule => "folder-git-2",
                FileTreeKind::Symlink => "file-symlink",
                FileTreeKind::File => file_icon_name(&entry.path),
            },
            if is_selected {
                &palette.accent
            } else if status.is_empty() {
                &palette.muted
            } else {
                &color
            },
            16.,
        );
        let mut label = text_node(
            &format!("tree-label-{index}"),
            split_path(&entry.path).1,
            &color,
            12.,
        );
        label.width = Some(fill());
        label.text_growth = Some("fixed-width".into());
        row.children = vec![chevron, file_icon, label];
        if entry.kind == FileTreeKind::Submodule {
            let label = if module.is_some_and(|module| module.initialized) {
                "子模块"
            } else {
                "未初始化"
            };
            row.children.push(badge(
                &format!("tree-module-{index}"),
                label,
                &palette.muted,
                palette,
            ));
        }
        if !status.is_empty() {
            let mut label = code_text(&format!("tree-status-{index}"), &status, &color);
            label.font_size = Some(11.);
            row.children.push(label);
        }
        if entry.kind == FileTreeKind::Submodule {
            row.children.push(submodule_branch_button(
                &format!("tree-module-branch-{index}"),
                repository.root.join(&entry.path),
                module.is_some_and(|module| module.initialized),
                actions,
                palette,
            ));
        }
        match entry.kind {
            FileTreeKind::Directory => {
                actions.insert(
                    row.id.clone(),
                    WorkbenchAction::ToggleDirectory(entry.path.clone()),
                );
            }
            FileTreeKind::Submodule => {
                if module.is_some_and(|module| module.initialized) {
                    actions.insert(
                        row.id.clone(),
                        WorkbenchAction::OpenSubmodule(repository.root.join(&entry.path)),
                    );
                }
            }
            FileTreeKind::File | FileTreeKind::Symlink => {
                actions.insert(
                    row.id.clone(),
                    WorkbenchAction::OpenFile {
                        path: entry.path.clone(),
                        staged,
                    },
                );
            }
        }
        list.children.push(row);
    }
    if list.children.is_empty() {
        list.children.push(message(
            "tree-empty",
            if data.file_query.is_empty() {
                "仓库中没有可显示的文件"
            } else {
                "没有匹配的文件"
            },
            palette,
        ));
    }
}

pub(super) fn populate_submodules(
    design: &mut DesignNode,
    data: &WorkbenchData<'_>,
    actions: &mut HashMap<String, WorkbenchAction>,
    palette: &Palette,
) {
    let (Some(repository), Some(tree)) = (data.repository, data.tree) else {
        return;
    };
    if tree.submodules.is_empty() {
        return;
    }
    let header_template = find(design, "远程分支标题").cloned();
    let Some(worktrees) = find_mut(design, "工作树分区") else {
        return;
    };
    let mut section = frame("submodules-section", "子模块分区");
    section.width = Some(fill());
    section.layout = Some("vertical".into());
    // Keep the worktree section's single bottom border and make its collapse
    // control hide both worktrees and their submodules.
    section.padding = Some(DesignSpacing::Sides(vec![8., 0., 0., 0.]));
    if let Some(mut header) = header_template {
        seed(&mut header, "submodules-header");
        header.name = "子模块标题".into();
        set_text(&mut header, "远程分支标题文字", "子模块");
        set_text(
            &mut header,
            "远程分支数量",
            &tree.submodules.len().to_string(),
        );
        actions.insert(
            header.id.clone(),
            WorkbenchAction::ToggleSection(header.name.clone()),
        );
        section.children.push(header);
    }
    let mut list = frame("sidebar-modules", "实际子模块列表");
    list.width = Some(fill());
    list.layout = Some("vertical".into());
    list.gap = Some(2.);
    list.padding = Some(DesignSpacing::Sides(vec![0., 8.]));
    let query = data.repo_query.trim().to_lowercase();
    for (index, module) in tree.submodules.iter().enumerate() {
        if !format!("{} {}", module.name, module.path)
            .to_lowercase()
            .contains(&query)
        {
            continue;
        }
        let mut row = frame(&format!("sidebar-module-{index}"), "实际子模块入口");
        row.name = format!("子模块 {}", module.path);
        row.width = Some(fill());
        row.height = Some(32.0.into());
        row.padding = Some(DesignSpacing::Sides(vec![0., 8., 0., 20.]));
        row.gap = Some(8.);
        row.align_items = Some("center".into());
        row.context = Some(module.path.clone());
        let module_root = repository.root.join(&module.path);
        let selected = module.initialized && data.selected_submodule == Some(module_root.as_path());
        if selected {
            row.fill = Some(palette.selection.clone());
            row.corner_radius = Some(DesignSpacing::All(6.));
        }
        row.children.push(icon(
            &format!("sidebar-module-icon-{index}"),
            "folder-git-2",
            if selected {
                &palette.accent
            } else {
                &palette.muted
            },
            16.,
        ));
        let mut label = text_node(
            &format!("sidebar-module-label-{index}"),
            &module.path,
            if selected {
                &palette.accent
            } else {
                &palette.foreground
            },
            12.,
        );
        label.width = Some(fill());
        label.text_growth = Some("fixed-width".into());
        row.children.push(label);
        row.children.push(badge(
            &format!("sidebar-module-status-{index}"),
            if !module.initialized {
                "未初始化"
            } else if module.dirty {
                "已修改"
            } else {
                "子模块"
            },
            if module.dirty {
                &palette.modified
            } else {
                &palette.muted
            },
            palette,
        ));
        row.children.push(submodule_branch_button(
            &format!("sidebar-module-branch-{index}"),
            module_root.clone(),
            module.initialized,
            actions,
            palette,
        ));
        if module.initialized {
            actions.insert(row.id.clone(), WorkbenchAction::OpenSubmodule(module_root));
        }
        list.children.push(row);
    }
    section.children.push(list);
    let insert_at = worktrees
        .children
        .iter()
        .position(|node| matches!(node.name.as_str(), "实际工作树列表" | "三个工作树"))
        .map_or(worktrees.children.len(), |index| index + 1);
    worktrees.children.insert(insert_at, section);
}

fn submodule_branch_button(
    id: &str,
    root: PathBuf,
    initialized: bool,
    actions: &mut HashMap<String, WorkbenchAction>,
    palette: &Palette,
) -> DesignNode {
    let mut button = frame(
        id,
        if initialized {
            "切换子模块分支"
        } else {
            "子模块未初始化，无法切换分支"
        },
    );
    button.width = Some(64.0.into());
    button.height = Some(24.0.into());
    button.align_items = Some("center".into());
    button.justify_content = Some("center".into());
    button.corner_radius = Some(DesignSpacing::All(4.));
    button.stroke = Some(palette.border.clone());
    button.stroke_width = Some(DesignStrokeWidth::All(1.));
    button.children.push(text_node(
        &format!("{id}-label"),
        "分支 ▾",
        if initialized {
            &palette.foreground
        } else {
            &palette.muted
        },
        12.,
    ));
    if initialized {
        actions.insert(button.id.clone(), WorkbenchAction::SubmoduleBranches(root));
    } else {
        button.opacity = Some(0.4);
    }
    button
}

pub(super) fn module_for_change<'a>(
    data: &'a WorkbenchData<'_>,
    path: &str,
) -> Option<&'a SubmoduleInfo> {
    data.tree?
        .submodules
        .iter()
        .find(|module| module.path == path)
}

/// A preorder walk keeps nested paths together and treats symlinks/gitlinks as
/// leaves even if a malformed backend response also contains descendant paths.
fn visible_entries<'a>(
    entries: &'a [FileTreeEntry],
    query: &str,
    collapsed: &HashSet<String>,
) -> Vec<(&'a FileTreeEntry, usize)> {
    let query = query.trim().to_lowercase();
    let mut visible = HashSet::new();
    if !query.is_empty() {
        for entry in entries
            .iter()
            .filter(|entry| entry.path.to_lowercase().contains(&query))
        {
            visible.insert(entry.path.as_str());
            let mut path = entry.path.as_str();
            while let Some((parent, _)) = path.rsplit_once('/') {
                visible.insert(parent);
                path = parent;
            }
        }
    }
    let mut children = BTreeMap::<&str, Vec<&FileTreeEntry>>::new();
    for entry in entries.iter().filter(|entry| !entry.path.is_empty()) {
        let parent = entry.path.rsplit_once('/').map_or("", |(parent, _)| parent);
        children.entry(parent).or_default().push(entry);
    }
    for siblings in children.values_mut() {
        siblings.sort_by(|left, right| {
            (left.kind != FileTreeKind::Directory)
                .cmp(&(right.kind != FileTreeKind::Directory))
                .then_with(|| left.path.to_lowercase().cmp(&right.path.to_lowercase()))
                .then_with(|| left.path.cmp(&right.path))
        });
    }
    let mut stack = children
        .get("")
        .into_iter()
        .flatten()
        .rev()
        .map(|entry| (*entry, 0))
        .collect::<Vec<_>>();
    let mut result = Vec::new();
    while let Some((entry, depth)) = stack.pop() {
        if !query.is_empty() && !visible.contains(entry.path.as_str()) {
            continue;
        }
        result.push((entry, depth));
        if entry.kind == FileTreeKind::Directory
            && (!query.is_empty() || !collapsed.contains(&entry.path))
            && let Some(descendants) = children.get(entry.path.as_str())
        {
            stack.extend(descendants.iter().rev().map(|entry| (*entry, depth + 1)));
        }
    }
    result
}

fn opens_staged(entry: &FileTreeEntry) -> bool {
    entry.index_status != ' ' && entry.index_status != '?' && entry.worktree_status == ' '
}

fn status_text(index: char, worktree: char) -> String {
    match (index, worktree) {
        (' ', ' ') => String::new(),
        ('?', '?') => "?".into(),
        (' ', worktree) => worktree.to_string(),
        (index, ' ') => index.to_string(),
        (index, worktree) => format!("{index}{worktree}"),
    }
}

fn badge(id: &str, label: &str, color: &str, palette: &Palette) -> DesignNode {
    let mut badge = frame(id, "文件类型徽标");
    badge.height = Some(18.0.into());
    badge.padding = Some(DesignSpacing::Sides(vec![0., 4.]));
    badge.corner_radius = Some(DesignSpacing::All(3.));
    badge.stroke = Some(palette.border.clone());
    badge.stroke_width = Some(DesignStrokeWidth::All(1.));
    badge.align_items = Some("center".into());
    badge
        .children
        .push(text_node(&format!("{id}-text"), label, color, 10.));
    badge
}

fn file_icon_name(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "c" | "h" | "cpp" => "file-code",
        _ => "file-text",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named_text(name: &str, content: &str) -> DesignNode {
        let mut node = text_node(name, content, "#f0f6fc", 12.);
        node.name = name.into();
        node
    }

    fn sidebar_design() -> DesignNode {
        let mut design = frame("test-workbench", "工作台");
        let mut sidebar = frame("test-sidebar", "仓库信息 · 工作树与分支");
        let mut worktrees = frame("test-worktrees", "工作树分区");
        worktrees.children = vec![
            frame("test-worktrees-header", "工作树标题"),
            frame("test-worktrees-list", "三个工作树"),
        ];
        let mut remote_header = frame("test-remote-header", "远程分支标题");
        remote_header.children = vec![
            icon("test-remote-chevron", "chevron-down", "#9198a1", 12.),
            named_text("远程分支标题文字", "远程分支"),
            named_text("远程分支数量", "0"),
        ];
        let mut remote = frame("test-remotes", "远程分支分区");
        remote.children.push(remote_header);
        sidebar.children = vec![
            worktrees,
            frame("test-local-branches", "本地分支分区"),
            remote,
            frame("test-stashes", "储藏入口分区"),
        ];
        design.children.push(sidebar);
        for name in [
            "仓库名称",
            "分支名称",
            "文件变更标题",
            "12 个文件",
            "文件变更统计",
            "已暂存数量提示",
            "提交按钮文字",
        ] {
            design.children.push(named_text(name, "设计稿占位"));
        }
        design
    }

    fn repository() -> RepositorySnapshot {
        RepositorySnapshot {
            root: PathBuf::from("F:/repositories/main"),
            name: "main".into(),
            head: Head::Branch("main-branch".into()),
            changes: vec![],
            recent_commits: vec![],
        }
    }

    fn module(path: &str, initialized: bool) -> SubmoduleInfo {
        SubmoduleInfo {
            name: path.into(),
            path: path.into(),
            url: None,
            head_id: None,
            index_id: None,
            workdir_id: None,
            initialized,
            dirty: false,
        }
    }

    fn data<'a>(
        repository: &'a RepositorySnapshot,
        tree: &'a RepositoryTree,
        collapsed: &'a HashSet<String>,
    ) -> WorkbenchData<'a> {
        WorkbenchData {
            native_preview: true,
            repository: Some(repository),
            details: None,
            tree: Some(tree),
            selected_submodule: None,
            changes_repository: None,
            changes_details: None,
            changes_tree: None,
            tree_mode: false,
            collapsed_paths: collapsed,
            expanded_remotes: collapsed,
            diff: None,
            selected_file: None,
            selected_staged: false,
            opened_files: &[],
            active_tab: None,
            file_query: "",
            repo_query: "",
            loading: false,
            history: false,
            unified: false,
            ignore_whitespace: false,
        }
    }

    #[test]
    fn submodules_follow_worktrees_inside_the_same_collapsible_section() {
        let repository = repository();
        let tree = RepositoryTree {
            submodules: vec![module("vendor/engine", true)],
            ..Default::default()
        };
        let collapsed = HashSet::new();
        let mut design = sidebar_design();
        let mut actions = HashMap::new();
        populate(
            &mut design,
            &data(&repository, &tree, &collapsed),
            &mut actions,
        );

        let worktrees = find(&design, "工作树分区").unwrap();
        assert_eq!(
            worktrees
                .children
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>(),
            ["工作树标题", "实际工作树列表", "子模块分区"]
        );
        let sidebar = find(&design, "仓库信息 · 工作树与分支").unwrap();
        assert!(
            sidebar
                .children
                .iter()
                .all(|node| node.name != "子模块分区")
        );
        let section = find(worktrees, "子模块分区").unwrap();
        assert!(
            section.stroke.is_none(),
            "only the outer section draws a separator"
        );
        assert!(matches!(
            actions.get(&worktrees.children[0].id),
            Some(WorkbenchAction::ToggleSection(name)) if name == "工作树标题"
        ));
        assert!(matches!(
            actions.get(&section.children[0].id),
            Some(WorkbenchAction::ToggleSection(name)) if name == "子模块标题"
        ));
    }

    #[test]
    fn selected_initialized_submodule_is_highlighted_without_a_repository_switch_action() {
        let repository = repository();
        let mut selected = module("vendor/engine", true);
        selected.dirty = true;
        let tree = RepositoryTree {
            submodules: vec![selected, module("vendor/missing", false)],
            ..Default::default()
        };
        let collapsed = HashSet::new();
        let selected_root = repository.root.join("vendor/engine");
        let data = WorkbenchData {
            selected_submodule: Some(&selected_root),
            ..data(&repository, &tree, &collapsed)
        };
        let mut design = sidebar_design();
        let palette = Palette::from_tree(&design);
        let mut actions = HashMap::new();
        populate_submodules(&mut design, &data, &mut actions, &palette);

        let selected = find(&design, "子模块 vendor/engine").unwrap();
        assert_eq!(selected.fill.as_ref(), Some(&palette.selection));
        assert_eq!(selected.children[0].fill.as_ref(), Some(&palette.accent));
        assert!(matches!(
            actions.get(&selected.id),
            Some(WorkbenchAction::OpenSubmodule(path)) if path == &selected_root
        ));
        assert!(matches!(
            actions.get("live-sidebar-module-branch-0"),
            Some(WorkbenchAction::SubmoduleBranches(path)) if path == &selected_root
        ));
        let switch = &selected.children[3];
        assert_eq!(switch.children[0].content.as_deref(), Some("分支 ▾"));
        assert_eq!(switch.stroke.as_ref(), Some(&palette.border));
        assert!(matches!(switch.width, Some(DesignSize::Pixels(64.))));
        assert!(matches!(switch.height, Some(DesignSize::Pixels(24.))));
        let unavailable = find(&design, "子模块 vendor/missing").unwrap();
        assert!(unavailable.fill.is_none());
        assert!(!actions.contains_key(&unavailable.id));
        assert!(
            actions
                .values()
                .all(|action| !matches!(action, WorkbenchAction::OpenWorktree(_)))
        );
        assert_eq!(
            unavailable.children[2].children[0].content.as_deref(),
            Some("未初始化")
        );
        assert!(!actions.contains_key("live-sidebar-module-branch-1"));
        assert_eq!(unavailable.children[3].opacity, Some(0.4));
    }

    #[test]
    fn submodule_file_context_keeps_main_repository_navigation_and_uses_child_counts() {
        let mut repository = repository();
        repository.changes.push(FileChange {
            path: "main.rs".into(),
            index_status: ' ',
            worktree_status: 'M',
        });
        let tree = RepositoryTree {
            submodules: vec![module("vendor/engine", true)],
            ..Default::default()
        };
        let selected_root = repository.root.join("vendor/engine");
        let mut child = repository.clone();
        child.root = selected_root.clone();
        child.name = "engine".into();
        child.head = Head::Branch("child-branch".into());
        child.changes = vec![
            FileChange {
                path: "a.rs".into(),
                index_status: 'A',
                worktree_status: ' ',
            },
            FileChange {
                path: "b.rs".into(),
                index_status: 'M',
                worktree_status: ' ',
            },
            FileChange {
                path: "c.rs".into(),
                index_status: ' ',
                worktree_status: 'M',
            },
        ];
        let child_tree = RepositoryTree {
            entries: vec![
                entry("src", FileTreeKind::Directory),
                entry("src/a.rs", FileTreeKind::File),
                entry("src/clean.rs", FileTreeKind::File),
            ],
            ..Default::default()
        };
        let collapsed = HashSet::new();
        let mut data = WorkbenchData {
            selected_submodule: Some(&selected_root),
            changes_repository: Some(&child),
            changes_tree: Some(&child_tree),
            ..data(&repository, &tree, &collapsed)
        };
        let mut design = sidebar_design();
        populate(&mut design, &data, &mut HashMap::new());
        assert_eq!(
            find(&design, "仓库名称").unwrap().content.as_deref(),
            Some("main")
        );
        assert_eq!(
            find(&design, "分支名称").unwrap().content.as_deref(),
            Some("main-branch")
        );
        assert_eq!(
            find(&design, "12 个文件").unwrap().content.as_deref(),
            Some("3")
        );
        assert_eq!(
            find(&design, "已暂存数量提示").unwrap().content.as_deref(),
            Some("已暂存 2 个文件")
        );
        assert_eq!(
            find(&design, "文件变更标题").unwrap().content.as_deref(),
            Some("子模块文件")
        );
        assert!(find(&design, "子模块 vendor/engine").is_some());

        data.tree_mode = true;
        let mut design = sidebar_design();
        populate(&mut design, &data, &mut HashMap::new());
        assert_eq!(
            find(&design, "12 个文件").unwrap().content.as_deref(),
            Some("2")
        );
        assert!(find(&design, "子模块 vendor/engine").is_some());
    }

    fn entry(path: &str, kind: FileTreeKind) -> FileTreeEntry {
        FileTreeEntry {
            path: path.into(),
            kind,
            index_status: ' ',
            worktree_status: ' ',
        }
    }

    #[test]
    fn preorder_sorts_directories_and_preserves_nested_paths() {
        let entries = vec![
            entry("z.rs", FileTreeKind::File),
            entry("src/z.rs", FileTreeKind::File),
            entry("src", FileTreeKind::Directory),
            entry("a.rs", FileTreeKind::File),
            entry("src/a.rs", FileTreeKind::File),
        ];
        let visible = visible_entries(&entries, "", &HashSet::new());
        assert_eq!(
            visible
                .iter()
                .map(|(entry, depth)| (entry.path.as_str(), *depth))
                .collect::<Vec<_>>(),
            vec![
                ("src", 0),
                ("src/a.rs", 1),
                ("src/z.rs", 1),
                ("a.rs", 0),
                ("z.rs", 0)
            ]
        );
    }

    #[test]
    fn search_reveals_ancestors_without_changing_saved_collapsed_paths() {
        let entries = vec![
            entry("src", FileTreeKind::Directory),
            entry("src/deep", FileTreeKind::Directory),
            entry("src/deep/view.rs", FileTreeKind::File),
            entry("src/other.rs", FileTreeKind::File),
        ];
        let collapsed = HashSet::from(["src".into()]);
        assert_eq!(visible_entries(&entries, "", &collapsed).len(), 1);
        let visible = visible_entries(&entries, "view", &collapsed);
        assert_eq!(
            visible
                .iter()
                .map(|(entry, _)| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["src", "src/deep", "src/deep/view.rs"]
        );
        assert!(collapsed.contains("src"));
    }

    #[test]
    fn submodules_and_symlinks_are_not_traversed() {
        let entries = vec![
            entry("vendor", FileTreeKind::Submodule),
            entry("vendor/private.rs", FileTreeKind::File),
            entry("link", FileTreeKind::Symlink),
            entry("link/outside.rs", FileTreeKind::File),
        ];
        let visible = visible_entries(&entries, "", &HashSet::new());
        assert_eq!(
            visible
                .iter()
                .map(|(entry, _)| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["link", "vendor"]
        );
    }

    #[test]
    fn clean_files_open_worktree_content_and_staged_only_files_open_index_diff() {
        let mut file = entry("file.rs", FileTreeKind::File);
        assert!(!opens_staged(&file));
        file.index_status = 'M';
        assert!(opens_staged(&file));
        file.worktree_status = 'M';
        assert!(!opens_staged(&file));
        file.index_status = '?';
        file.worktree_status = '?';
        assert!(!opens_staged(&file));
    }

    #[test]
    fn conflict_statuses_remain_distinct_from_untracked_files() {
        assert_eq!(status_text('?', '?'), "?");
        assert_eq!(status_text('U', 'U'), "UU");
        assert_eq!(status_text('A', 'A'), "AA");
        assert_eq!(status_text('D', 'D'), "DD");
        assert_eq!(status_text('M', 'M'), "MM");
        assert_eq!(status_text('D', '?'), "D?");
    }

    #[test]
    fn filename_colors_match_change_types_in_the_tree_and_grouped_fallback() {
        let mut repository = repository();
        repository.changes = [
            ("added.rs", 'A', ' '),
            ("modified.rs", ' ', 'M'),
            ("deleted.rs", ' ', 'D'),
            ("renamed.rs", 'R', 'M'),
            ("copied.rs", 'C', ' '),
            ("type.rs", ' ', 'T'),
            ("untracked.rs", '?', '?'),
            ("conflict.rs", 'D', 'D'),
        ]
        .map(|(path, index_status, worktree_status)| FileChange {
            path: path.into(),
            index_status,
            worktree_status,
        })
        .into();
        let tree = RepositoryTree {
            entries: repository
                .changes
                .iter()
                .map(|change| FileTreeEntry {
                    path: change.path.clone(),
                    kind: FileTreeKind::File,
                    index_status: change.index_status,
                    worktree_status: change.worktree_status,
                })
                .collect(),
            ..Default::default()
        };
        let collapsed = HashSet::new();
        for light in [false, true] {
            let mut design = sidebar_design();
            design.fill = Some(if light { "#ffffff" } else { "#0d1117" }.into());
            let normal = file_template("useWorktree.ts");
            let selected = file_template("WorktreePanel.tsx · 当前选中");
            design.children.extend([
                normal,
                selected,
                frame("files", "变更文件列表 · 四组均展开"),
            ]);
            for (index, name) in [
                "已修改文件分组",
                "已删除文件分组",
                "未追踪文件分组",
                "已暂存文件分组",
            ]
            .into_iter()
            .enumerate()
            {
                let mut group = frame(&format!("group-{index}"), name);
                group
                    .children
                    .push(frame(&format!("header-{index}"), "header"));
                design.children.push(group);
            }
            let palette = Palette::from_tree(&design);
            let mut data = data(&repository, &tree, &collapsed);
            data.selected_file = Some("modified.rs");
            let mut actions = HashMap::new();
            populate_file_tree(&mut design, &data, &mut actions, &palette);
            let list = find(&design, "变更文件列表 · 四组均展开").unwrap();
            for row in &list.children {
                let entry = tree
                    .entries
                    .iter()
                    .find(|entry| Some(&entry.path) == row.context.as_ref())
                    .unwrap();
                assert_eq!(
                    row.children[2].fill.as_deref(),
                    Some(
                        format!(
                            "#{:06x}",
                            ChangeKind::from_status(entry.index_status, entry.worktree_status)
                                .rgb(light)
                        )
                        .as_str()
                    )
                );
            }
            populate_files(&mut design, &data, &mut actions, &palette);
            let mut conflict_count = 0;
            for name in [
                "已修改文件分组",
                "已删除文件分组",
                "未追踪文件分组",
                "已暂存文件分组",
            ] {
                let group = find(&design, name).unwrap();
                for row in group.children.iter().skip(1) {
                    let change = repository
                        .changes
                        .iter()
                        .find(|change| Some(&change.path) == row.context.as_ref())
                        .unwrap();
                    let kind = if change.is_conflicted() {
                        conflict_count += 1;
                        assert!(row.children[3].hidden);
                        ChangeKind::Conflict
                    } else if change.index_status == '?' {
                        ChangeKind::Untracked
                    } else {
                        ChangeKind::from_status(
                            ' ',
                            if name == "已暂存文件分组" {
                                change.index_status
                            } else {
                                change.worktree_status
                            },
                        )
                    };
                    assert_eq!(
                        row.children[1].children[0].fill.as_deref(),
                        Some(format!("#{:06x}", kind.rgb(light)).as_str())
                    );
                }
            }
            assert_eq!(conflict_count, 1);
        }
    }

    fn file_template(name: &str) -> DesignNode {
        let mut row = frame(name, name);
        let mut detail = frame(&format!("{name}-detail"), "detail");
        detail.children = vec![
            text_node("filename", "template.rs", "#ffffff", 12.),
            text_node("directory", "src", "#9198a1", 12.),
        ];
        row.children = vec![
            icon("file-icon", "file-code", "#9198a1", 16.),
            detail,
            text_node("status", "M", "#9198a1", 12.),
            frame("stage", "stage"),
        ];
        row
    }
}
