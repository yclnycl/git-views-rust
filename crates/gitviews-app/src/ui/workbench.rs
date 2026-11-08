//! Maps repository reads onto the native Pencil workbench without owning I/O.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

use gitviews_service::{
    DiffLine, DiffLineKind, FileChange, FileDiff, Head, RepositoryDetails, RepositorySnapshot,
    RepositoryTree, paths_equal,
};

use super::change_style::ChangeKind;
use super::design_renderer::{
    DesignEdges, DesignNode, DesignSize, DesignSpacing, DesignStrokeWidth,
};
use super::paths::display_path;

#[cfg(test)]
mod branch_tests;
mod repository_tree;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileTab {
    pub root: PathBuf,
    pub path: String,
    pub staged: bool,
    pub revision: Option<String>,
}

#[derive(Clone, Debug)]
pub enum WorkbenchAction {
    OpenFile {
        path: String,
        staged: bool,
    },
    CloseTab(usize),
    ActivateTab(usize),
    OpenWorktree(PathBuf),
    /// Select a submodule's file changes without replacing the main repository.
    OpenSubmodule(PathBuf),
    SubmoduleBranches(PathBuf),
    ToggleDirectory(String),
    ToggleFileView,
    ToggleSection(String),
    CheckoutBranch {
        name: String,
        remote: bool,
        root: Option<PathBuf>,
    },
    OpenBranchMenu,
}

#[derive(Clone, Copy)]
pub struct WorkbenchData<'a> {
    pub native_preview: bool,
    pub repository: Option<&'a RepositorySnapshot>,
    pub details: Option<&'a RepositoryDetails>,
    pub tree: Option<&'a RepositoryTree>,
    pub selected_submodule: Option<&'a Path>,
    pub changes_repository: Option<&'a RepositorySnapshot>,
    pub changes_details: Option<&'a RepositoryDetails>,
    pub changes_tree: Option<&'a RepositoryTree>,
    pub tree_mode: bool,
    pub collapsed_paths: &'a HashSet<String>,
    /// Remote names explicitly expanded by the user, such as `origin`.
    pub expanded_remotes: &'a HashSet<String>,
    pub diff: Option<&'a FileDiff>,
    pub selected_file: Option<&'a str>,
    pub selected_staged: bool,
    pub opened_files: &'a [FileTab],
    pub active_tab: Option<usize>,
    pub file_query: &'a str,
    pub repo_query: &'a str,
    pub loading: bool,
    pub history: bool,
    pub unified: bool,
    pub ignore_whitespace: bool,
}

/// Populate a fresh design clone. Preview mode deliberately bypasses this.
pub fn populate(
    tree: &mut DesignNode,
    data: &WorkbenchData<'_>,
    actions: &mut HashMap<String, WorkbenchAction>,
) {
    let palette = Palette::from_tree(tree);
    let changes_data = WorkbenchData {
        repository: data.changes_repository.or(data.repository),
        tree: data.changes_tree.or(data.tree),
        ..*data
    };
    let repository_name = data
        .repository
        .map_or("打开仓库", |repo| repo.name.as_str());
    let branch = data
        .repository
        .map(|repo| head_label(&repo.head))
        .unwrap_or_else(|| "未选择".into());
    let path = data
        .repository
        .map(|repo| display_path(&repo.root))
        .unwrap_or_else(|| "选择本地 Git 仓库".into());
    let change_count = changes_data.repository.map_or(0, |repo| repo.changes.len());
    let staged_count = changes_data.repository.map_or(0, |repo| {
        repo.changes
            .iter()
            .filter(|change| is_staged(change))
            .count()
    });
    for name in ["仓库名称", "当前仓库"] {
        set_text(tree, name, repository_name);
    }
    set_text(
        tree,
        "窗口仓库标题",
        &format!("{repository_name} — GitScope"),
    );
    set_text(tree, "仓库本地路径", &path);
    set_text(tree, "分支名称", &branch);
    set_text(tree, "领先远端", "—");
    set_text(tree, "推送提交文字", "推送");
    set_text(tree, "上次获取时间", "尚未获取");
    set_text(tree, "同步时间", "尚未获取远程状态");
    set_text(tree, "储藏数量", "—");
    set_text(tree, "12 个文件", &change_count.to_string());
    set_text(tree, "文件变更统计", &format!("{change_count} 个变更文件"));
    set_text(
        tree,
        "本地仓库状态",
        if data.loading {
            "正在读取…"
        } else if data.repository.is_some() {
            "本地仓库"
        } else {
            "未打开仓库"
        },
    );
    set_text(
        tree,
        "已暂存数量提示",
        &format!("已暂存 {staged_count} 个文件"),
    );
    set_text(tree, "提交按钮文字", &format!("提交 {staged_count} 个文件"));
    set_text(tree, "提交摘要内容", "填写提交摘要");
    set_text(tree, "编辑器状态", "UTF-8    LF");
    if let Some(button) = find_mut(tree, "提交 2 个已暂存文件") {
        button.name = "提交已暂存文件".into();
        if staged_count == 0 {
            button.opacity = Some(0.55);
        }
    }

    populate_worktrees(tree, data, actions);
    let branch_data = WorkbenchData {
        repository: changes_data.repository,
        details: if data.changes_repository.is_some() {
            data.changes_details
        } else {
            data.details
        },
        ..*data
    };
    populate_branches(tree, &branch_data, actions);
    repository_tree::populate_submodules(tree, data, actions, &palette);
    let data = &changes_data;
    set_text(
        tree,
        "文件变更标题",
        if data.selected_submodule.is_some() {
            "子模块文件"
        } else if data.tree_mode {
            "仓库文件"
        } else {
            "文件变更"
        },
    );
    if let Some(control) = find(tree, "按状态分组") {
        actions.insert(control.id.clone(), WorkbenchAction::ToggleFileView);
    }
    set_text(
        tree,
        "分组方式",
        if data.tree_mode {
            "文件树"
        } else {
            "按状态分组"
        },
    );
    if data.tree_mode {
        let count = data.tree.map_or(0, |tree| {
            tree.entries
                .iter()
                .filter(|entry| entry.kind != gitviews_service::FileTreeKind::Directory)
                .count()
        });
        set_text(tree, "12 个文件", &count.to_string());
        if let Some(icon) = find_mut(tree, "分组图标") {
            icon.icon = Some("folder-tree".into());
        }
        if !data.native_preview {
            repository_tree::populate_file_tree(tree, data, actions, &palette);
        }
    } else {
        if data.native_preview {
            if let Some(list) = find_mut(tree, "变更文件列表 · 四组均展开") {
                list.children.clear();
            }
        } else {
            populate_files(tree, data, actions, &palette);
        }
    }
    populate_tabs(tree, data, actions);

    if data.native_preview && data.history {
        empty_viewer(tree, "提交历史", &palette, true);
    } else if data.history {
        populate_history(tree, data, &palette);
    } else if data.repository.is_none() {
        empty_viewer(tree, "打开仓库以查看文件变更", &palette, false);
    } else if let Some(diff) = data
        .diff
        .filter(|diff| data.selected_file.is_some_and(|path| path == diff.path))
    {
        populate_diff(tree, data, diff, &palette);
    } else if data.native_preview
        && let Some(path) = data.selected_file
    {
        let (directory, filename) = split_path(path);
        set_text(tree, "父目录", directory);
        set_text(tree, "当前文件", filename);
        set_text(tree, "增加行数", "+0");
        set_text(tree, "删除行数", "−0");
        set_text(
            tree,
            "原始版本标题",
            if data.selected_staged {
                "HEAD"
            } else {
                "暂存区 · Index"
            },
        );
        set_text(
            tree,
            "当前版本标题",
            if data.selected_staged {
                "暂存区 · 已暂存"
            } else {
                "工作区 · 尚未暂存"
            },
        );
        set_text(tree, "文件显示范围", "只读预览");
        set_text(tree, "编辑器状态", "只读");
        for name in ["双栏代码差异", "单栏代码差异"] {
            if let Some(body) = find_mut(tree, name) {
                body.children.clear();
            }
        }
    } else {
        empty_viewer(
            tree,
            if data.selected_file.is_some() && data.loading {
                "正在读取文件差异…"
            } else if data.selected_file.is_some() {
                "文件差异尚未读取"
            } else if change_count == 0 {
                "工作区干净，没有文件变更"
            } else {
                "选择一个文件以查看差异"
            },
            &palette,
            true,
        );
    }
}

fn populate_worktrees(
    tree: &mut DesignNode,
    data: &WorkbenchData<'_>,
    actions: &mut HashMap<String, WorkbenchAction>,
) {
    register_section(tree, "工作树标题", actions);
    let selected_template = find(tree, "orbit-desktop 工作树").cloned();
    let normal_template = find(tree, "orbit-hotfix 工作树").cloned();
    let muted = find(tree, "仓库本地路径").and_then(|node| node.fill.clone());
    let positive = find(tree, "增加行数").and_then(|node| node.fill.clone());
    let query = data.repo_query.to_lowercase();
    let mut rows = Vec::new();
    if let (Some(repo), Some(details), Some(selected), Some(normal)) = (
        data.repository,
        data.details,
        selected_template,
        normal_template,
    ) {
        for (index, worktree) in details.worktrees.iter().enumerate() {
            let branch = worktree.branch.clone().unwrap_or_else(|| {
                if worktree.is_bare {
                    "裸仓库".into()
                } else if worktree.prunable {
                    "目录缺失 / 登记失效".into()
                } else if !worktree.is_detached {
                    "分支未读取".into()
                } else {
                    format!(
                        "分离 HEAD · {}",
                        worktree.head.as_deref().map(short_id).unwrap_or_default()
                    )
                }
            });
            let name = worktree.path.file_name().map_or_else(
                || display_path(&worktree.path),
                |name| name.to_string_lossy().into_owned(),
            );
            if !format!("{name} {branch} {}", display_path(&worktree.path))
                .to_lowercase()
                .contains(&query)
            {
                continue;
            }
            let current = paths_equal(&worktree.path, &repo.root);
            let mut row = if current {
                selected.clone()
            } else {
                normal.clone()
            };
            seed(&mut row, &format!("worktree-{index}"));
            row.name = "实际工作树".into();
            if current {
                set_text(&mut row, "orbit-desktop 名称", &name);
                set_text(&mut row, "orbit-desktop 关联分支", &branch);
                set_text(
                    &mut row,
                    "未提交更改数量",
                    if repo.changes.is_empty() {
                        "工作区干净".into()
                    } else {
                        format!("{} 项更改", repo.changes.len())
                    }
                    .as_str(),
                );
                if repo.changes.is_empty()
                    && let Some(icon) = find_mut(&mut row, "未提交更改图标")
                {
                    icon.icon = Some("check".into());
                    icon.fill = positive.clone();
                }
                retain_nodes(&mut row, &|node| {
                    !node.name.contains("领先远程") && node.name != "更改状态弹性空白"
                });
            } else {
                set_text(&mut row, "orbit-hotfix 名称", &name);
                set_text(&mut row, "orbit-hotfix 关联分支", &branch);
                if let Some(status) = find_mut(&mut row, "orbit-hotfix 干净状态") {
                    status.children.retain(|node| node.kind == "text");
                    for node in &mut status.children {
                        node.fill = muted.clone();
                        node.content = Some(
                            if worktree.locked {
                                "已锁定"
                            } else if worktree.prunable {
                                "可清理"
                            } else {
                                "未读取"
                            }
                            .into(),
                        );
                    }
                }
            }
            actions.insert(
                row.id.clone(),
                WorkbenchAction::OpenWorktree(worktree.path.clone()),
            );
            rows.push(row);
        }
    }
    set_text(
        tree,
        "工作树数量",
        &data.details.map_or_else(
            || {
                if data.repository.is_some() {
                    "—".into()
                } else {
                    "0".into()
                }
            },
            |details| details.worktrees.len().to_string(),
        ),
    );
    if let Some(list) = find_mut(tree, "三个工作树") {
        list.name = "实际工作树列表".into();
        list.children = rows;
    }
}

fn populate_branches(
    tree: &mut DesignNode,
    data: &WorkbenchData<'_>,
    actions: &mut HashMap<String, WorkbenchAction>,
) {
    register_section(tree, "本地分支标题", actions);
    register_section(tree, "远程分支标题", actions);
    for name in ["当前分支与同步状态", "分支切换"] {
        if let Some(control) = find(tree, name) {
            actions.insert(control.id.clone(), WorkbenchAction::OpenBranchMenu);
        }
    }
    let selected = find(tree, "feature/worktree-ui 本地分支").cloned();
    let normal = find(tree, "main 本地分支").cloned();
    let remote_header = find(tree, "origin 远程仓库").cloned();
    let remote_row = find(tree, "origin/main 远程分支").cloned();
    let mut locals = Vec::new();
    let mut remotes = BTreeMap::<String, Vec<(usize, String)>>::new();
    let query = data.repo_query.trim().to_lowercase();
    let mut local_count = 0;
    let mut remote_count = 0;
    if let (Some(details), Some(selected), Some(normal)) = (data.details, selected, normal) {
        for (index, branch) in details.branches.iter().enumerate() {
            if branch.is_remote {
                remote_count += 1;
            } else {
                local_count += 1;
            }
            if !branch.name.to_lowercase().contains(&query) {
                continue;
            }
            if branch.is_remote {
                let (remote, _) = branch
                    .name
                    .split_once('/')
                    .unwrap_or(("远程", &branch.name));
                remotes
                    .entry(remote.into())
                    .or_default()
                    .push((index, branch.name.clone()));
            } else {
                let mut row = if branch.is_current {
                    selected.clone()
                } else {
                    normal.clone()
                };
                set_text(&mut row, "feature/worktree-ui 本地分支名称", &branch.name);
                set_text(&mut row, "main 本地分支名称", &branch.name);
                seed(&mut row, &format!("branch-{index}"));
                row.name = "实际本地分支".into();
                row.context = Some(format!("本地分支：{}", branch.name));
                actions.insert(
                    row.id.clone(),
                    WorkbenchAction::CheckoutBranch {
                        name: branch.name.clone(),
                        remote: false,
                        root: data.repository.map(|repository| repository.root.clone()),
                    },
                );
                locals.push(row);
            }
        }
    }
    let count_label = |count: usize| {
        if data.details.is_none() && data.repository.is_some() {
            "—".to_owned()
        } else {
            count.to_string()
        }
    };
    set_text(tree, "本地分支数量", &count_label(local_count));
    set_text(tree, "远程分支数量", &count_label(remote_count));
    if let Some(list) = find_mut(tree, "本地分支列表") {
        list.children = locals;
    }
    if let Some(section) = find_mut(tree, "远程分支分区") {
        section.children.truncate(1);
        if let (Some(header), Some(row)) = (remote_header, remote_row) {
            for (remote_index, (remote, branches)) in remotes.into_iter().enumerate() {
                let expanded = !query.is_empty() || data.expanded_remotes.contains(&remote);
                let mut header = header.clone();
                set_text(&mut header, "远程仓库名称", &remote);
                set_text(&mut header, "远程平台", "");
                header.context = Some(format!("远程仓库：{remote}"));
                if let Some(chevron) = find_mut(&mut header, "展开 origin") {
                    chevron.icon = Some(
                        if expanded {
                            "chevron-down"
                        } else {
                            "chevron-right"
                        }
                        .into(),
                    );
                }
                seed(&mut header, &format!("remote-{remote_index}"));
                header.name = format!("实际远程仓库 {remote}");
                actions.insert(
                    header.id.clone(),
                    WorkbenchAction::ToggleSection(header.name.clone()),
                );
                let mut group = frame(&format!("remote-group-{remote_index}"), "实际远程分组");
                group.layout = Some("vertical".into());
                group.width = Some(fill());
                group.children.push(header);
                if expanded {
                    for (index, name) in branches {
                        let mut branch_row = row.clone();
                        let label = name.split_once('/').map_or(name.as_str(), |(_, name)| name);
                        set_text(&mut branch_row, "origin/main 分支名称", label);
                        seed(&mut branch_row, &format!("remote-branch-{index}"));
                        branch_row.name = "实际远程分支".into();
                        branch_row.context = Some(format!("远程分支：{name}"));
                        actions.insert(
                            branch_row.id.clone(),
                            WorkbenchAction::CheckoutBranch {
                                name,
                                remote: true,
                                root: data.repository.map(|repository| repository.root.clone()),
                            },
                        );
                        group.children.push(branch_row);
                    }
                }
                section.children.push(group);
            }
        }
    }
}

fn populate_files(
    tree: &mut DesignNode,
    data: &WorkbenchData<'_>,
    actions: &mut HashMap<String, WorkbenchAction>,
    palette: &Palette,
) {
    let selected_template = find(tree, "WorktreePanel.tsx · 当前选中").cloned();
    let normal_template = find(tree, "useWorktree.ts").cloned();
    let group_names = [
        "已修改文件分组",
        "已删除文件分组",
        "未追踪文件分组",
        "已暂存文件分组",
    ];
    let query = data.file_query.to_lowercase();
    let mut groups = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    if let Some(repo) = data.repository {
        for change in &repo.changes {
            if !change.path.to_lowercase().contains(&query) {
                continue;
            }
            if change.is_conflicted() {
                groups[0].push((change, false));
                continue;
            }
            if is_staged(change) {
                groups[3].push((change, true));
            }
            if change.index_status == '?' {
                groups[2].push((change, false));
            } else if change.worktree_status == 'D' {
                groups[1].push((change, false));
            } else if change.worktree_status != ' ' {
                groups[0].push((change, false));
            }
        }
    }
    for (group_index, (name, files)) in group_names.into_iter().zip(groups).enumerate() {
        let Some(group) = find_mut(tree, name) else {
            continue;
        };
        let Some(mut header) = group.children.first().cloned() else {
            continue;
        };
        let label = ["已修改", "已删除", "未追踪", "已暂存"][group_index];
        set_text(
            &mut header,
            &format!("{label}数量"),
            &files.len().to_string(),
        );
        actions.insert(
            header.id.clone(),
            WorkbenchAction::ToggleSection(header.name.clone()),
        );
        group.children = vec![header];
        for (file_index, (change, staged)) in files.into_iter().enumerate() {
            let selected =
                data.selected_file == Some(change.path.as_str()) && data.selected_staged == staged;
            let template = if selected {
                &selected_template
            } else {
                &normal_template
            };
            let Some(mut row) = template.clone() else {
                continue;
            };
            seed(&mut row, &format!("file-{group_index}-{file_index}"));
            row.name = "实际变更文件".into();
            row.context = Some(change.path.clone());
            let (directory, filename) = split_path(&change.path);
            let status = if staged {
                change.index_status
            } else {
                change.worktree_status
            };
            let status = if change.index_status == '?' {
                'U'
            } else {
                status
            };
            let module = repository_tree::module_for_change(data, &change.path);
            let kind = if change.is_conflicted() {
                ChangeKind::Conflict
            } else if change.index_status == '?' {
                ChangeKind::Untracked
            } else {
                ChangeKind::from_status(' ', status)
            };
            let status_color = format!("#{:06x}", kind.rgb(palette.light));
            if let Some(icon) = row.children.get_mut(0) {
                icon.icon = Some(
                    match status {
                        _ if module.is_some() => "folder-git-2",
                        'D' => "file-minus",
                        'A' | 'U' => "file-plus",
                        _ if staged => "file-check",
                        _ => "file-pen-line",
                    }
                    .into(),
                );
                icon.fill = Some(if selected {
                    palette.accent.clone()
                } else {
                    status_color.clone()
                });
            }
            if let Some(detail) = row.children.get_mut(1) {
                if let Some(label) = detail.children.first_mut() {
                    label.content = Some(filename.into());
                    label.fill = Some(status_color.clone());
                    if kind == ChangeKind::Conflict {
                        label.font_weight = Some("600".into());
                    }
                }
                if let Some(path) = detail.children.get_mut(1) {
                    path.content = Some(if directory.is_empty() { "/" } else { directory }.into());
                }
            }
            if let Some(label) = row.children.get_mut(2) {
                label.content = Some(if change.is_conflicted() {
                    format!("{}{} · 冲突", change.index_status, change.worktree_status)
                } else if module.is_some() {
                    format!("{status} · 子模块")
                } else {
                    status.to_string()
                });
                label.fill = Some(status_color.clone());
            }
            if let Some(button) = row.children.get_mut(3) {
                button.hidden = change.is_conflicted();
                button.name = if staged {
                    "取消暂存文件"
                } else {
                    "暂存文件"
                }
                .into();
            }
            if let Some(module) = module {
                if module.initialized
                    && let Some(repository) = data.repository
                {
                    actions.insert(
                        row.id.clone(),
                        WorkbenchAction::OpenSubmodule(repository.root.join(&module.path)),
                    );
                }
            } else {
                actions.insert(
                    row.id.clone(),
                    WorkbenchAction::OpenFile {
                        path: change.path.clone(),
                        staged,
                    },
                );
            }
            group.children.push(row);
        }
    }
}

fn populate_tabs(
    tree: &mut DesignNode,
    data: &WorkbenchData<'_>,
    actions: &mut HashMap<String, WorkbenchAction>,
) {
    let selected_template = find(tree, "WorktreePanel.tsx 标签").cloned();
    let normal_template = find(tree, "package.json 标签").cloned();
    let Some(tabs) = find_mut(tree, "打开的文件标签") else {
        return;
    };
    let trailing = tabs
        .children
        .iter()
        .filter(|node| node.name == "标签伸缩间距" || node.name == "文件标签菜单")
        .cloned()
        .collect::<Vec<_>>();
    tabs.children.clear();
    if let (Some(selected_template), Some(normal_template)) = (selected_template, normal_template) {
        for (index, tab) in data.opened_files.iter().enumerate() {
            let selected = data.active_tab == Some(index);
            let mut row = if selected {
                selected_template.clone()
            } else {
                normal_template.clone()
            };
            seed(&mut row, &format!("tab-{index}"));
            row.name = "实际文件标签".into();
            if let Some(label) = row.children.get_mut(1) {
                label.content = Some(split_path(&tab.path).1.into());
            }
            if let Some(status) = row.children.get_mut(2) {
                status.content = Some(if let Some(revision) = &tab.revision {
                    revision.chars().take(8).collect()
                } else if tab.staged {
                    "已暂存".into()
                } else {
                    String::new()
                });
            }
            if let Some(close) = row.children.get_mut(3) {
                close.name = "关闭实际文件标签".into();
                actions.insert(close.id.clone(), WorkbenchAction::CloseTab(index));
            }
            actions.insert(row.id.clone(), WorkbenchAction::ActivateTab(index));
            tabs.children.push(row);
        }
    }
    tabs.children.extend(trailing);
}

fn populate_diff(
    tree: &mut DesignNode,
    data: &WorkbenchData<'_>,
    diff: &FileDiff,
    palette: &Palette,
) {
    let (directory, filename) = split_path(&diff.path);
    set_text(tree, "父目录", &directory.replace('/', " / "));
    set_text(tree, "当前文件", filename);
    let added = diff
        .lines
        .iter()
        .filter(|line| line.kind == DiffLineKind::Added)
        .count();
    let deleted = diff
        .lines
        .iter()
        .filter(|line| line.kind == DiffLineKind::Deleted)
        .count();
    let hunks = diff
        .lines
        .iter()
        .filter(|line| line.kind == DiffLineKind::Section && line.text.starts_with("@@"))
        .count();
    set_text(tree, "增加行数", &format!("+{added}"));
    set_text(tree, "删除行数", &format!("−{deleted}"));
    set_text(
        tree,
        "当前修改定位计数",
        &if hunks == 0 {
            "无差异".into()
        } else {
            format!("修改 1 / {hunks}")
        },
    );
    let head = data
        .repository
        .and_then(|repo| repo.recent_commits.first())
        .map(|commit| short_id(&commit.id));
    set_text(
        tree,
        "原始版本标题",
        &if data.selected_staged {
            format!("HEAD · {}", head.unwrap_or_else(|| "首次提交".into()))
        } else {
            "暂存区 · Index".into()
        },
    );
    set_text(
        tree,
        "当前版本标题",
        if data.selected_staged {
            "暂存区 · 已暂存"
        } else {
            "工作区 · 尚未暂存"
        },
    );
    set_text(
        tree,
        "文件显示范围",
        &format!("{hunks} 处修改 · {} 行", diff.new_text.lines().count()),
    );
    set_text(
        tree,
        "编辑器状态",
        &format!("UTF-8    LF    {}", language(filename)),
    );
    let name = if data.unified {
        "单栏代码差异"
    } else {
        "双栏代码差异"
    };
    let Some(viewport) = find_mut(tree, name) else {
        return;
    };
    if data.native_preview {
        viewport.children.clear();
        return;
    }
    viewport.children = if diff.lines.is_empty() {
        diff.new_text
            .lines()
            .take(2000)
            .enumerate()
            .map(|(index, text)| {
                let line = DiffLine {
                    old_line: Some(index + 1),
                    new_line: Some(index + 1),
                    kind: DiffLineKind::Context,
                    text: text.into(),
                };
                if data.unified {
                    unified_row(index, &line, palette)
                } else {
                    paired_row(index, Some(&line), Some(&line), false, palette)
                }
            })
            .collect()
    } else if data.unified {
        let adjusted = data
            .ignore_whitespace
            .then(|| ignore_whitespace_changes(&diff.lines));
        adjusted
            .as_deref()
            .unwrap_or(&diff.lines)
            .iter()
            .take(2000)
            .enumerate()
            .map(|(index, line)| {
                if line.kind == DiffLineKind::Section {
                    hunk_row(index, &line.text, palette)
                } else {
                    unified_row(index, line, palette)
                }
            })
            .collect()
    } else {
        paired_diff(&diff.lines, data.ignore_whitespace, palette)
    };
    if diff.lines.len() > 2000 || (diff.lines.is_empty() && diff.new_text.lines().count() > 2000) {
        viewport
            .children
            .push(message("diff-limit", "仅显示前 2000 行", palette));
    }
}

fn equal_ignoring_whitespace(left: &str, right: &str) -> bool {
    left.chars()
        .filter(|character| !character.is_whitespace())
        .eq(right.chars().filter(|character| !character.is_whitespace()))
}

fn ignore_whitespace_changes(lines: &[DiffLine]) -> Vec<DiffLine> {
    let mut adjusted = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if !matches!(
            lines[index].kind,
            DiffLineKind::Added | DiffLineKind::Deleted
        ) {
            adjusted.push(lines[index].clone());
            index += 1;
            continue;
        }
        let mut old = Vec::new();
        let mut new = Vec::new();
        while index < lines.len()
            && matches!(
                lines[index].kind,
                DiffLineKind::Added | DiffLineKind::Deleted
            )
        {
            if lines[index].kind == DiffLineKind::Deleted {
                old.push(&lines[index]);
            } else {
                new.push(&lines[index]);
            }
            index += 1;
        }
        let mut pending_old = Vec::new();
        let mut pending_new = Vec::new();
        for offset in 0..old.len().max(new.len()) {
            let left = old.get(offset).copied();
            let right = new.get(offset).copied();
            if let Some((left, right)) = left
                .zip(right)
                .filter(|(left, right)| equal_ignoring_whitespace(&left.text, &right.text))
            {
                // Flush real changes before a shared row so both line-number
                // sequences retain their order within the unified view.
                adjusted.append(&mut pending_old);
                adjusted.append(&mut pending_new);
                adjusted.push(DiffLine {
                    old_line: left.old_line,
                    new_line: right.new_line,
                    kind: DiffLineKind::Context,
                    text: right.text.clone(),
                });
            } else {
                if let Some(left) = left {
                    pending_old.push(left.clone());
                }
                if let Some(right) = right {
                    pending_new.push(right.clone());
                }
            }
        }
        adjusted.append(&mut pending_old);
        adjusted.append(&mut pending_new);
    }
    adjusted
}

fn paired_diff(lines: &[DiffLine], ignore_whitespace: bool, palette: &Palette) -> Vec<DesignNode> {
    let mut rows = Vec::new();
    let mut index = 0;
    while index < lines.len() && rows.len() < 2000 {
        let line = &lines[index];
        match line.kind {
            DiffLineKind::Section => {
                rows.push(hunk_row(index, &line.text, palette));
                index += 1;
            }
            DiffLineKind::Context => {
                rows.push(paired_row(index, Some(line), Some(line), false, palette));
                index += 1;
            }
            DiffLineKind::Added | DiffLineKind::Deleted => {
                let mut old = Vec::new();
                let mut new = Vec::new();
                let start = index;
                while index < lines.len()
                    && matches!(
                        lines[index].kind,
                        DiffLineKind::Added | DiffLineKind::Deleted
                    )
                {
                    if lines[index].kind == DiffLineKind::Deleted {
                        old.push(&lines[index]);
                    } else {
                        new.push(&lines[index]);
                    }
                    index += 1;
                }
                for offset in 0..old.len().max(new.len()) {
                    if rows.len() >= 2000 {
                        break;
                    }
                    let left = old.get(offset).copied();
                    let right = new.get(offset).copied();
                    let whitespace_only = ignore_whitespace
                        && left.zip(right).is_some_and(|(left, right)| {
                            equal_ignoring_whitespace(&left.text, &right.text)
                        });
                    rows.push(paired_row(
                        start + offset,
                        left,
                        right,
                        whitespace_only,
                        palette,
                    ));
                }
            }
        }
    }
    rows
}

fn paired_row(
    index: usize,
    old: Option<&DiffLine>,
    new: Option<&DiffLine>,
    neutral: bool,
    palette: &Palette,
) -> DesignNode {
    let mut row = frame(&format!("diff-row-{index}"), "实际双栏代码行");
    row.width = Some(fill());
    row.height = Some(22.0.into());
    row.align_items = Some("center".into());
    row.children = vec![
        code_cell(index, "old", old, true, neutral, palette),
        code_cell(index, "new", new, false, neutral, palette),
    ];
    row
}

fn code_cell(
    index: usize,
    side: &str,
    line: Option<&DiffLine>,
    old: bool,
    neutral: bool,
    palette: &Palette,
) -> DesignNode {
    let kind = line.map_or(DiffLineKind::Context, |line| line.kind);
    let background = if neutral {
        &palette.background
    } else {
        palette.line_background(kind)
    };
    let mut cell = frame(&format!("diff-{side}-{index}"), "实际代码单元格");
    cell.width = Some(fill());
    cell.height = Some(fill());
    cell.fill = Some(background.clone());
    cell.stroke = Some(palette.border.clone());
    cell.stroke_width = Some(DesignStrokeWidth::Edges(DesignEdges {
        right: 1.,
        ..Default::default()
    }));
    cell.align_items = Some("center".into());
    cell.clip = true;
    let number = line.and_then(|line| if old { line.old_line } else { line.new_line });
    let gutter_background =
        if !neutral && matches!(kind, DiffLineKind::Added | DiffLineKind::Deleted) {
            background
        } else {
            &palette.gutter
        };
    cell.children.push(gutter(
        &format!("{side}-{index}"),
        number,
        gutter_background,
        palette,
    ));
    let symbol = if neutral { "" } else { marker(kind) };
    cell.children.push(change_marker(
        &format!("{side}-{index}"),
        symbol,
        kind,
        palette,
    ));
    let mut content = frame(&format!("code-{side}-{index}"), "实际代码内容");
    content.height = Some(fill());
    content.align_items = Some("center".into());
    content.children.push(code_text(
        &format!("{side}-{index}"),
        line.map_or("", |line| line.text.as_str()),
        &palette.foreground,
    ));
    cell.children.push(content);
    cell
}

fn unified_row(index: usize, line: &DiffLine, palette: &Palette) -> DesignNode {
    let background = palette.line_background(line.kind);
    let mut row = frame(&format!("unified-row-{index}"), "实际单栏代码行");
    row.width = Some(fill());
    row.height = Some(22.0.into());
    row.align_items = Some("center".into());
    row.fill = Some(background.clone());
    row.children = vec![
        gutter(
            &format!("unified-old-{index}"),
            line.old_line,
            background,
            palette,
        ),
        gutter(
            &format!("unified-new-{index}"),
            line.new_line,
            background,
            palette,
        ),
        change_marker(
            &format!("unified-{index}"),
            marker(line.kind),
            line.kind,
            palette,
        ),
        code_text(&format!("unified-{index}"), &line.text, &palette.foreground),
    ];
    row
}

fn gutter(id: &str, number: Option<usize>, background: &str, palette: &Palette) -> DesignNode {
    let mut node = frame(&format!("gutter-{id}"), "实际代码行号");
    node.width = Some(40.0.into());
    node.height = Some(fill());
    node.fill = Some(background.into());
    node.padding = Some(DesignSpacing::Sides(vec![0., 8., 0., 0.]));
    node.align_items = Some("center".into());
    node.justify_content = Some("end".into());
    let mut label = code_text(
        &format!("gutter-label-{id}"),
        &number.map_or_else(String::new, |number| number.to_string()),
        &palette.muted,
    );
    label.font_size = Some(11.);
    node.children.push(label);
    node
}

fn change_marker(id: &str, symbol: &str, kind: DiffLineKind, palette: &Palette) -> DesignNode {
    let mut node = frame(&format!("marker-{id}"), "实际代码变更标记");
    node.width = Some(18.0.into());
    node.height = Some(fill());
    node.justify_content = Some("center".into());
    node.align_items = Some("center".into());
    node.children.push(code_text(
        &format!("marker-label-{id}"),
        symbol,
        if kind == DiffLineKind::Added {
            &palette.positive
        } else {
            &palette.negative
        },
    ));
    node
}

fn hunk_row(index: usize, text: &str, palette: &Palette) -> DesignNode {
    let mut node = frame(&format!("hunk-{index}"), "实际差异区块");
    node.width = Some(fill());
    node.height = Some(28.0.into());
    node.fill = Some(palette.hunk.clone());
    node.padding = Some(DesignSpacing::Sides(vec![0., 12.]));
    node.align_items = Some("center".into());
    node.gap = Some(12.);
    node.children.push(icon(
        &format!("hunk-icon-{index}"),
        "unfold-vertical",
        &palette.accent,
        14.,
    ));
    let mut label = code_text(&format!("hunk-label-{index}"), text, &palette.muted);
    label.font_size = Some(11.);
    node.children.push(label);
    node
}

fn empty_viewer(tree: &mut DesignNode, text: &str, palette: &Palette, keep_tabs: bool) {
    let Some(viewer) = find_mut(tree, "文件查看器 · 多标签 Diff") else {
        return;
    };
    let tabs = if keep_tabs {
        viewer
            .children
            .iter()
            .find(|node| node.name == "打开的文件标签")
            .cloned()
    } else {
        None
    };
    viewer.children.clear();
    if let Some(tabs) = tabs {
        viewer.children.push(tabs);
    }
    let mut empty = frame("empty-viewer", "文件查看器空状态");
    empty.layout = Some("vertical".into());
    empty.width = Some(fill());
    empty.height = Some(fill());
    empty.align_items = Some("center".into());
    empty.justify_content = Some("center".into());
    empty.gap = Some(12.);
    empty
        .children
        .push(icon("empty-icon", "file-diff", &palette.muted, 32.));
    empty
        .children
        .push(text_node("empty-text", text, &palette.muted, 14.));
    viewer.children.push(empty);
}

fn populate_history(tree: &mut DesignNode, data: &WorkbenchData<'_>, palette: &Palette) {
    let Some(viewer) = find_mut(tree, "文件查看器 · 多标签 Diff") else {
        return;
    };
    viewer.children.clear();
    let mut heading = frame("history-heading", "近期提交标题");
    heading.width = Some(fill());
    heading.height = Some(44.0.into());
    heading.align_items = Some("center".into());
    heading.padding = Some(DesignSpacing::Sides(vec![0., 16.]));
    heading.fill = Some(palette.panel.clone());
    heading.children.push(text_node(
        "history-title",
        "近期提交",
        &palette.foreground,
        14.,
    ));
    viewer.children.push(heading);
    let mut list = frame("history-list", "实际近期提交列表");
    list.layout = Some("vertical".into());
    list.width = Some(fill());
    list.height = Some(fill());
    if let Some(repo) = data.repository {
        for (index, commit) in repo.recent_commits.iter().enumerate() {
            let mut row = frame(&format!("commit-{index}"), "实际提交记录");
            row.width = Some(fill());
            row.height = Some(70.0.into());
            row.align_items = Some("center".into());
            row.padding = Some(DesignSpacing::Sides(vec![12., 16.]));
            row.gap = Some(12.);
            row.stroke = Some(palette.border.clone());
            row.stroke_width = Some(DesignStrokeWidth::Edges(DesignEdges {
                bottom: 1.,
                ..Default::default()
            }));
            row.children.push(icon(
                &format!("commit-icon-{index}"),
                "git-commit-horizontal",
                &palette.muted,
                16.,
            ));
            let mut detail = frame(&format!("commit-detail-{index}"), "提交详情");
            detail.width = Some(fill());
            detail.layout = Some("vertical".into());
            detail.gap = Some(6.);
            detail.children.push(text_node(
                &format!("commit-subject-{index}"),
                &commit.subject,
                &palette.foreground,
                13.,
            ));
            detail.children.push(text_node(
                &format!("commit-author-{index}"),
                &commit.author,
                &palette.muted,
                11.,
            ));
            row.children.push(detail);
            row.children.push(code_text(
                &format!("commit-hash-{index}"),
                &short_id(&commit.id),
                &palette.muted,
            ));
            list.children.push(row);
        }
    }
    if list.children.is_empty() {
        list.children.push(message(
            "no-commits",
            if data.repository.is_some() {
                "仓库还没有提交"
            } else {
                "打开仓库以查看近期提交"
            },
            palette,
        ));
    }
    viewer.children.push(list);
}

struct Palette {
    light: bool,
    background: String,
    panel: String,
    foreground: String,
    muted: String,
    border: String,
    addition: String,
    deletion: String,
    gutter: String,
    hunk: String,
    positive: String,
    negative: String,
    modified: String,
    accent: String,
    selection: String,
}

impl Palette {
    fn from_tree(tree: &DesignNode) -> Self {
        let fill_at = |name: &str, fallback: &str| {
            find(tree, name)
                .and_then(|node| node.fill.clone())
                .unwrap_or_else(|| fallback.into())
        };
        let light = tree.fill.as_deref() == Some("#ffffff")
            || find(tree, "仓库名称").and_then(|node| node.fill.as_deref()) == Some("#1f2328");
        Self {
            light,
            background: fill_at("双栏代码差异", if light { "#ffffff" } else { "#0d1117" }),
            panel: fill_at(
                "仓库信息 · 工作树与分支",
                if light { "#f6f8fa" } else { "#151b23" },
            ),
            foreground: fill_at("仓库名称", if light { "#1f2328" } else { "#f0f6fc" }),
            muted: fill_at("仓库本地路径", if light { "#59636e" } else { "#9198a1" }),
            border: find(tree, "仓库信息 · 工作树与分支")
                .and_then(|node| node.stroke.clone())
                .unwrap_or_else(|| if light { "#d1d9e0" } else { "#3d444d" }.into()),
            addition: if light { "#dafbe1" } else { "#14261e" }.into(),
            deletion: if light { "#ffebe9" } else { "#2b1b20" }.into(),
            gutter: if light { "#f6f8fa" } else { "#010409" }.into(),
            hunk: if light { "#ddf4ff" } else { "#122b49" }.into(),
            positive: fill_at("增加行数", if light { "#1a7f37" } else { "#3fb950" }),
            negative: fill_at("删除行数", if light { "#cf222e" } else { "#f85149" }),
            modified: if light { "#9a6700" } else { "#d29922" }.into(),
            accent: if light { "#0969da" } else { "#79c0ff" }.into(),
            selection: fill_at(
                "feature/worktree-ui 本地分支",
                if light { "#ddf4ff" } else { "#1a3453" },
            ),
        }
    }

    fn line_background(&self, kind: DiffLineKind) -> &String {
        match kind {
            DiffLineKind::Added => &self.addition,
            DiffLineKind::Deleted => &self.deletion,
            _ => &self.background,
        }
    }
}

fn find<'a>(node: &'a DesignNode, name: &str) -> Option<&'a DesignNode> {
    if node.name == name {
        return Some(node);
    }
    node.children.iter().find_map(|child| find(child, name))
}

fn find_mut<'a>(node: &'a mut DesignNode, name: &str) -> Option<&'a mut DesignNode> {
    if node.name == name {
        return Some(node);
    }
    node.children
        .iter_mut()
        .find_map(|child| find_mut(child, name))
}

fn set_text(node: &mut DesignNode, name: &str, content: &str) {
    if node.kind == "text" && node.name == name {
        node.content = Some(content.into());
    }
    for child in &mut node.children {
        set_text(child, name, content);
    }
}

fn seed(node: &mut DesignNode, prefix: &str) {
    node.id = format!("live-{prefix}-{}", node.id);
    for child in &mut node.children {
        seed(child, prefix);
    }
}

fn retain_nodes(node: &mut DesignNode, keep: &impl Fn(&DesignNode) -> bool) {
    node.children.retain(keep);
    for child in &mut node.children {
        retain_nodes(child, keep);
    }
}

fn register_section(tree: &DesignNode, name: &str, actions: &mut HashMap<String, WorkbenchAction>) {
    if let Some(node) = find(tree, name) {
        actions.insert(
            node.id.clone(),
            WorkbenchAction::ToggleSection(node.name.clone()),
        );
    }
}

fn frame(id: &str, name: &str) -> DesignNode {
    DesignNode {
        kind: "frame".into(),
        id: format!("live-{id}"),
        name: name.into(),
        ..Default::default()
    }
}

fn text_node(id: &str, content: &str, color: &str, size: f32) -> DesignNode {
    DesignNode {
        kind: "text".into(),
        id: format!("live-{id}"),
        name: "实际文字".into(),
        content: Some(content.into()),
        fill: Some(color.into()),
        font_size: Some(size),
        line_height: Some(1.5),
        font_family: Some("Noto Sans SC".into()),
        font_weight: Some("normal".into()),
        ..Default::default()
    }
}

fn code_text(id: &str, content: &str, color: &str) -> DesignNode {
    let mut node = text_node(id, content, color, 12.5);
    node.name = "实际代码文字".into();
    node.font_family = Some("JetBrains Mono".into());
    node.line_height = Some(1.6);
    node
}

fn icon(id: &str, name: &str, color: &str, size: f32) -> DesignNode {
    DesignNode {
        kind: "icon".into(),
        id: format!("live-{id}"),
        name: "实际图标".into(),
        icon: Some(name.into()),
        library: Some("lucide".into()),
        fill: Some(color.into()),
        width: Some(size.into()),
        height: Some(size.into()),
        ..Default::default()
    }
}

fn message(id: &str, text: &str, palette: &Palette) -> DesignNode {
    let mut node = frame(id, "实际提示信息");
    node.width = Some(fill());
    node.padding = Some(DesignSpacing::All(16.));
    node.children.push(text_node(
        &format!("message-{id}"),
        text,
        &palette.muted,
        12.,
    ));
    node
}

fn fill() -> DesignSize {
    DesignSize::Sizing("fill_container".into())
}
fn is_staged(change: &FileChange) -> bool {
    change.index_status != ' ' && change.index_status != '?'
}
fn short_id(id: &str) -> String {
    id.chars().take(7).collect()
}
fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}
fn marker(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Added => "+",
        DiffLineKind::Deleted => "−",
        _ => "",
    }
}
fn head_label(head: &Head) -> String {
    match head {
        Head::Branch(branch) | Head::Unborn(branch) => branch.clone(),
        Head::Detached(id) => format!("HEAD · {}", short_id(id)),
    }
}
fn language(filename: &str) -> &'static str {
    match filename.rsplit('.').next().unwrap_or("") {
        "rs" => "Rust",
        "ts" => "TypeScript",
        "tsx" => "TypeScript React",
        "js" => "JavaScript",
        "jsx" => "JavaScript React",
        "json" => "JSON",
        "md" => "Markdown",
        "toml" => "TOML",
        "css" => "CSS",
        "html" => "HTML",
        "py" => "Python",
        "yaml" | "yml" => "YAML",
        _ => "文本",
    }
}
