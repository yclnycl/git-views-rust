//! Virtualized repository navigation; background row builds coalesce data/filter changes.
use super::change_style::{ChangeKind, is_conflict};
use super::copy_menu::{self, CopyMenuExt as _};
use gitviews_service::{FileChange, FileTreeKind, RepositoryTree};
use gpui_kit::{
    component::{
        ActiveTheme, Disableable as _, ThemeMode,
        button::Button,
        menu::{ContextMenuExt as _, PopupMenuItem},
    },
    prelude::FluentBuilder as _,
    *,
};

#[cfg(test)]
mod headless_tests;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug)]
pub enum FileListEvent {
    UpdateIndex { path: String, unstage: bool },
    OpenFile { path: String, staged: bool },
    OpenSubmodule(PathBuf),
    SubmoduleBranches(PathBuf),
    FileHistory { root: PathBuf, path: String },
}

#[derive(Clone, Debug)]
enum Row {
    Group {
        name: String,
        count: usize,
    },
    File {
        path: String,
        kind: FileTreeKind,
        depth: usize,
        staged: bool,
        status: String,
        change_kind: ChangeKind,
        initialized: bool,
        dirty: bool,
        /// Search rows show the relative path, including matching directories.
        /// Ranges refer to original UTF-8 bytes, never lowercased text offsets.
        search_matches: Option<Vec<Range<usize>>>,
    },
}

pub struct FileList {
    read_only: bool,
    root: PathBuf,
    tree: Arc<RepositoryTree>,
    changes: Arc<Vec<FileChange>>,
    rows: Arc<Vec<Row>>,
    query: String,
    tree_mode: bool,
    collapsed: Arc<HashSet<String>>,
    selected: Option<(String, bool)>,
    scroll: UniformListScrollHandle,
    generation: u64,
    root_epoch: u64,
    rows_pending: bool,
    preparing: bool,
    rows_task: Option<Task<()>>,
}
impl EventEmitter<FileListEvent> for FileList {}
impl FileList {
    pub fn new() -> Self {
        Self {
            read_only: true,
            root: PathBuf::new(),
            tree: Arc::new(RepositoryTree::default()),
            changes: Arc::new(Vec::new()),
            rows: Arc::new(Vec::new()),
            query: String::new(),
            tree_mode: false,
            collapsed: Arc::new(HashSet::new()),
            selected: None,
            scroll: UniformListScrollHandle::new(),
            generation: 0,
            root_epoch: 0,
            rows_pending: false,
            preparing: false,
            rows_task: None,
        }
    }
    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        if self.read_only != read_only {
            self.read_only = read_only;
            cx.notify();
        }
    }

    pub fn set_data(
        &mut self,
        root: PathBuf,
        tree: Arc<RepositoryTree>,
        changes: Vec<FileChange>,
        cx: &mut Context<Self>,
    ) {
        if self.root != root {
            self.root_epoch = self.root_epoch.wrapping_add(1);
            self.collapsed = Arc::new(HashSet::new());
            self.selected = None;
            // A row from another root must disappear before the replacement
            // background build completes, including its pending click handler.
            self.rows = Arc::new(Vec::new());
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        }
        self.root = root;
        self.tree = tree;
        self.changes = Arc::new(changes);
        self.rebuild(cx);
    }
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        self.root_epoch = self.root_epoch.wrapping_add(1);
        self.root.clear();
        self.tree = Arc::new(RepositoryTree::default());
        self.changes = Arc::new(Vec::new());
        self.rows = Arc::new(Vec::new());
        self.collapsed = Arc::new(HashSet::new());
        self.selected = None;
        self.rows_pending = false;
        self.preparing = false;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        // Do not drop rows_task: a synchronous CPU job cannot be preempted by
        // canceling its awaiting future. It drains before any next root starts.
        cx.notify();
    }
    pub fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        if self.query != query {
            self.query = query;
            self.scroll.scroll_to_item(0, ScrollStrategy::Top);
            self.rebuild(cx);
        }
    }
    pub fn set_tree_mode(&mut self, mode: bool, cx: &mut Context<Self>) {
        if self.tree_mode != mode {
            self.tree_mode = mode;
            self.rebuild(cx);
        }
    }
    pub fn select(&mut self, path: String, staged: bool, cx: &mut Context<Self>) {
        self.selected = Some((path, staged));
        cx.notify();
    }
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        self.rows_pending = !self.root.as_os_str().is_empty();
        self.preparing = self.rows_pending;
        if self.rows_pending && self.rows_task.is_none() {
            self.start_row_build(cx);
        }
        cx.notify();
    }

    fn start_row_build(&mut self, cx: &mut Context<Self>) {
        self.rows_pending = false;
        let generation = self.generation;
        let tree = Arc::clone(&self.tree);
        let changes = Arc::clone(&self.changes);
        let collapsed = Arc::clone(&self.collapsed);
        let query = self.query.clone();
        let tree_mode = self.tree_mode;
        tracing::debug!(
            target: "gitviews::files",
            generation,
            tree_entries = tree.entries.len(),
            change_count = changes.len(),
            collapsed_count = collapsed.len(),
            query_bytes = query.len(),
            tree_mode,
            "file rows requested"
        );
        let work = super::tasks::spawn(
            cx.background_executor(),
            "file_list.rows",
            generation,
            move || Arc::new(build_rows(&tree, &changes, &query, tree_mode, &collapsed)),
        );
        self.rows_task = Some(cx.spawn(async move |this, cx| {
            let rows = work.await;
            let _ = this.update(cx, |this, cx| {
                this.rows_task = None;
                if this.generation == generation {
                    tracing::debug!(
                        target: "gitviews::files",
                        generation,
                        row_count = rows.len(),
                        "file rows applied"
                    );
                    this.rows = rows;
                    this.preparing = false;
                } else {
                    tracing::debug!(
                        target: "gitviews::files",
                        generation,
                        current_generation = this.generation,
                        "discarding stale file rows"
                    );
                }
                if this.rows_pending {
                    this.start_row_build(cx);
                }
                cx.notify();
            });
        }));
    }
    fn activate(&mut self, row: &Row, click_count: usize, cx: &mut Context<Self>) {
        let key = match row {
            Row::Group { name, .. } => name,
            Row::File {
                path,
                kind: FileTreeKind::Directory,
                ..
            } => path,
            Row::File { path, staged, .. } => {
                self.selected = Some((path.clone(), *staged));
                if let Some(event) = file_click_event(row, &self.root, click_count) {
                    cx.emit(event);
                }
                cx.notify();
                return;
            }
        };
        let collapsed = Arc::make_mut(&mut self.collapsed);
        if !collapsed.remove(key) {
            collapsed.insert(key.clone());
        }
        self.rebuild(cx);
    }
}
impl Render for FileList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let light = cx.theme().mode == ThemeMode::Light;
        let muted = rgb(if light { 0x59636e } else { 0x9198a1 });
        let match_style = HighlightStyle {
            background_color: Some(rgb(if light { 0xffe69c } else { 0x75530c }).into()),
            color: Some(rgb(if light { 0x4d2d00 } else { 0xffec99 }).into()),
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        };
        let rows = Arc::clone(&self.rows);
        let selected = self.selected.clone();
        let collapsed = Arc::clone(&self.collapsed);
        let entity = cx.entity();
        let preparing = self.preparing;
        let root_epoch = self.root_epoch;
        let root = self.root.clone();
        let read_only = self.read_only;
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .when(preparing, |this| {
                this.child(
                    div()
                        .id("file-list-preparing")
                        .role(Role::Status)
                        .aria_label("正在准备文件列表")
                        .h(px(26.))
                        .flex_shrink_0()
                        .px(px(14.))
                        .text_size(px(11.))
                        .text_color(muted)
                        .child("正在准备文件列表…"),
                )
            })
            .when(rows.is_empty() && !preparing, |this| {
                this.child(div().p(px(14.)).text_color(muted).child(
                    if self.root.as_os_str().is_empty() {
                        "打开仓库以浏览文件"
                    } else if self.query.is_empty() {
                        "没有文件变更"
                    } else {
                        "没有匹配的文件"
                    },
                ))
            })
            .when(!rows.is_empty(), |this| {
                this.child(
                    uniform_list("virtual-files", rows.len(), move |range, _, _| {
                        range
                            .map(|index| {
                                let row = rows[index].clone();
                                let (label, depth, icon, status, color, active, badge) = match &row
                                {
                                    Row::Group { name, count } => (
                                        format!("{name}  {count}"),
                                        0,
                                        if collapsed.contains(name) {
                                            "chevron-right"
                                        } else {
                                            "chevron-down"
                                        },
                                        String::new(),
                                        muted,
                                        false,
                                        None,
                                    ),
                                    Row::File {
                                        path,
                                        kind,
                                        depth,
                                        status,
                                        change_kind,
                                        staged,
                                        initialized,
                                        dirty,
                                        search_matches,
                                    } => {
                                        let icon = match kind {
                                            FileTreeKind::Directory if collapsed.contains(path) => {
                                                "folder"
                                            }
                                            FileTreeKind::Directory => "folder-open",
                                            FileTreeKind::Submodule => "folder-git-2",
                                            FileTreeKind::Symlink => "file-symlink",
                                            _ => "file-code",
                                        };
                                        let color = rgb(change_kind.rgb(light));
                                        let label = if search_matches.is_some()
                                            || self_contained_filename(path).is_empty()
                                        {
                                            path.clone()
                                        } else {
                                            self_contained_filename(path).into()
                                        };
                                        let badge = (*kind == FileTreeKind::Submodule).then_some(
                                            if !initialized {
                                                "未初始化"
                                            } else if *dirty {
                                                "已修改"
                                            } else {
                                                "子模块"
                                            },
                                        );
                                        (
                                            label,
                                            *depth,
                                            icon,
                                            status.clone(),
                                            color,
                                            selected.as_ref().is_some_and(|(selected, side)| {
                                                selected == path && side == staged
                                            }),
                                            badge,
                                        )
                                    }
                                };
                                let aria = match &row {
                                    Row::Group { name, .. } => format!("折叠或展开{name}"),
                                    Row::File {
                                        path,
                                        kind: FileTreeKind::Submodule,
                                        initialized,
                                        ..
                                    } => format!(
                                        "子模块 {path}，{}",
                                        if *initialized {
                                            "单击查看差异，双击进入变更"
                                        } else {
                                            "未初始化，单击查看差异"
                                        }
                                    ),
                                    Row::File { path, .. } => format!("文件 {path}"),
                                };
                                let description = match &row {
                                    Row::File {
                                        change_kind: ChangeKind::Conflict,
                                        ..
                                    } => {
                                        if read_only {
                                            "冲突，单击查看"
                                        } else {
                                            "冲突，单击解决"
                                        }
                                    }
                                    Row::File { change_kind, .. } => change_kind.label(),
                                    Row::Group { .. } => "折叠或展开文件分组",
                                };
                                let target = entity.clone();
                                let stage_target = entity.clone();
                                let stage_action = match &row {
                                    Row::File {
                                        path,
                                        staged,
                                        kind,
                                        change_kind,
                                        ..
                                    } if !read_only
                                        && *kind != FileTreeKind::Directory
                                        && *change_kind != ChangeKind::Conflict =>
                                    {
                                        Some((path.clone(), *staged))
                                    }
                                    _ => None,
                                };
                                let highlights = match &row {
                                    Row::File {
                                        search_matches: Some(ranges),
                                        ..
                                    } => ranges.clone(),
                                    _ => Vec::new(),
                                };
                                let copy_items = match &row {
                                    Row::File { path, .. } => copy_menu::file(&root, path),
                                    Row::Group { .. } => Vec::new(),
                                };
                                let history_path = match &row {
                                    Row::File { path, kind, .. }
                                        if *kind != FileTreeKind::Directory =>
                                    {
                                        Some(path.clone())
                                    }
                                    _ => None,
                                };
                                let history_root = root.clone();
                                let history_target = entity.clone();
                                let module_action = match &row {
                                    Row::File {
                                        path,
                                        kind: FileTreeKind::Submodule,
                                        initialized,
                                        ..
                                    } if !read_only => Some((root.join(path), *initialized)),
                                    _ => None,
                                };
                                let module_target = entity.clone();
                                let element = div()
                                    .id(("file-row", index))
                                    .test_support()
                                    .role(Role::Button)
                                    .aria_label(aria)
                                    .aria_description(description)
                                    .h(px(32.))
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .gap(px(7.))
                                    .pl(px(12. + depth as f32 * 14.))
                                    .pr(px(12.))
                                    .text_size(px(12.))
                                    .cursor_pointer()
                                    .when(active, |this| {
                                        this.bg(rgb(if light { 0xddf4ff } else { 0x1a3453 }))
                                    })
                                    .child(
                                        svg()
                                            .path(format!("icons/{icon}.svg"))
                                            .size(px(15.))
                                            .flex_shrink_0()
                                            .text_color(muted),
                                    )
                                    .child(
                                        div()
                                            .id(("file-name", index))
                                            .test_support()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .text_color(color)
                                            .when(
                                                matches!(
                                                    &row,
                                                    Row::File {
                                                        change_kind: ChangeKind::Conflict,
                                                        ..
                                                    }
                                                ),
                                                |this| this.font_weight(FontWeight::SEMIBOLD),
                                            )
                                            .child(
                                                StyledText::new(label).with_highlights(
                                                    highlights
                                                        .into_iter()
                                                        .map(|range| (range, match_style)),
                                                ),
                                            ),
                                    )
                                    .when_some(badge, |this, badge| {
                                        this.child(
                                            div().text_size(px(10.)).text_color(muted).child(badge),
                                        )
                                    })
                                    .when_some(module_action, |this, (path, initialized)| {
                                        this.child(
                                            Button::new(("file-module-branch", index))
                                                .label("分支 ▾")
                                                .h(px(24.))
                                                .tooltip(if initialized {
                                                    "切换子模块分支"
                                                } else {
                                                    "子模块未初始化，无法切换分支"
                                                })
                                                .disabled(!initialized)
                                                .on_click(move |_, _, cx| {
                                                    cx.stop_propagation();
                                                    module_target.update(cx, |list, cx| {
                                                        if initialized
                                                            && !list.read_only
                                                            && list.root_epoch == root_epoch
                                                        {
                                                            cx.emit(
                                                                FileListEvent::SubmoduleBranches(
                                                                    path.clone(),
                                                                ),
                                                            );
                                                        }
                                                    });
                                                }),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_color(color)
                                            .font_family("JetBrains Mono")
                                            .child(status),
                                    )
                                    .when_some(stage_action, |this, (path, unstage)| {
                                        this.child(
                                            div()
                                                .id(("file-stage", index))
                                                .test_support()
                                                .role(Role::Button)
                                                .aria_label(format!(
                                                    "{} {path}",
                                                    if unstage { "取消暂存" } else { "暂存" }
                                                ))
                                                .px(px(5.))
                                                .cursor_pointer()
                                                .child(if unstage { "−" } else { "+" })
                                                .on_click(move |_, _, cx| {
                                                    cx.stop_propagation();
                                                    stage_target.update(cx, |list, cx| {
                                                        if !list.read_only
                                                            && list.root_epoch == root_epoch
                                                        {
                                                            cx.emit(FileListEvent::UpdateIndex {
                                                                path: path.clone(),
                                                                unstage,
                                                            });
                                                        }
                                                    });
                                                }),
                                        )
                                    })
                                    .on_click(move |event, _, cx| {
                                        cx.stop_propagation();
                                        target.update(cx, |this, cx| {
                                            if this.root_epoch == root_epoch {
                                                this.activate(&row, event.click_count(), cx);
                                            }
                                        });
                                    });
                                if let Some(path) = history_path {
                                    element
                                        .context_menu(move |mut menu, _, _| {
                                            for item in &copy_items {
                                                let value = item.value.clone();
                                                menu = menu.item(
                                                    PopupMenuItem::new(item.label).on_click(
                                                        move |_, _, cx| {
                                                            cx.write_to_clipboard(
                                                                ClipboardItem::new_string(
                                                                    value.clone(),
                                                                ),
                                                            );
                                                        },
                                                    ),
                                                );
                                            }
                                            let target = history_target.clone();
                                            let root = history_root.clone();
                                            let path = path.clone();
                                            menu.separator().item(
                                                PopupMenuItem::new("查看文件历史").on_click(
                                                    move |_, _, cx| {
                                                        target.update(cx, |list, cx| {
                                                            if list.root_epoch == root_epoch {
                                                                cx.emit(
                                                                    FileListEvent::FileHistory {
                                                                        root: root.clone(),
                                                                        path: path.clone(),
                                                                    },
                                                                );
                                                            }
                                                        });
                                                    },
                                                ),
                                            )
                                        })
                                        .into_any_element()
                                } else {
                                    element.copy_menu(copy_items)
                                }
                            })
                            .collect::<Vec<_>>()
                    })
                    .track_scroll(&self.scroll)
                    .flex_1()
                    .min_h_0()
                    .w_full(),
                )
            })
    }
}

/// A single click previews the parent repository's gitlink diff. Entering an
/// initialized submodule is a separate gesture handled by the change context.
fn file_click_event(row: &Row, root: &Path, click_count: usize) -> Option<FileListEvent> {
    match row {
        Row::Group { .. }
        | Row::File {
            kind: FileTreeKind::Directory,
            ..
        } => None,
        Row::File {
            path,
            kind: FileTreeKind::Submodule,
            initialized: true,
            ..
        } if click_count == 2 => Some(FileListEvent::OpenSubmodule(root.join(path))),
        Row::File { path, staged, .. } => Some(FileListEvent::OpenFile {
            path: path.clone(),
            staged: *staged,
        }),
    }
}

fn self_contained_filename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Map a case-insensitive match back to the original filename. Unicode lowercase
/// can expand a character (for example `İ`), so offsets in the folded string must
/// not be used directly for styled UTF-8 text.
fn search_match_ranges(path: &str, folded_query: &str) -> Vec<Range<usize>> {
    if folded_query.is_empty() {
        return Vec::new();
    }
    // Use the same whole-string lowercase as filtering (including final sigma).
    // Contextual sigma casing preserves its UTF-8 width; scalar expansions such
    // as dotted I provide the byte lengths for the original-character mapping.
    let folded = path.to_lowercase();
    let mut offsets = Vec::with_capacity(path.chars().count());
    let mut folded_end = 0;
    for (start, character) in path.char_indices() {
        let folded_start = folded_end;
        folded_end += character.to_lowercase().map(char::len_utf8).sum::<usize>();
        offsets.push((
            folded_start..folded_end,
            start..start + character.len_utf8(),
        ));
    }
    let mut matches: Vec<Range<usize>> = Vec::new();
    for (start, text) in folded.match_indices(folded_query) {
        let end = start + text.len();
        let first = offsets.partition_point(|(folded, _)| folded.end <= start);
        let last = offsets.partition_point(|(folded, _)| folded.start < end) - 1;
        let range = offsets[first].1.start..offsets[last].1.end;
        if let Some(previous) = matches.last_mut()
            && previous.end >= range.start
        {
            previous.end = previous.end.max(range.end);
        } else {
            matches.push(range);
        }
    }
    matches
}

fn build_rows(
    tree: &RepositoryTree,
    changes: &[FileChange],
    query: &str,
    tree_mode: bool,
    collapsed: &HashSet<String>,
) -> Vec<Row> {
    let query = query.trim().replace('\\', "/").to_lowercase();
    let modules: HashMap<_, _> = tree
        .submodules
        .iter()
        .map(|module| (module.path.as_str(), module))
        .collect();
    let file_row = |path: &str, kind, depth, staged, index, worktree| {
        let module = modules.get(path);
        Row::File {
            path: path.into(),
            kind,
            depth,
            staged,
            status: match (index, worktree) {
                (' ', ' ') => String::new(),
                ('?', _) => "?".into(),
                (' ', wt) => wt.to_string(),
                (idx, ' ') => idx.to_string(),
                (idx, wt) => format!("{idx}{wt}"),
            },
            change_kind: ChangeKind::from_status(index, worktree),
            initialized: module.is_some_and(|m| m.initialized),
            dirty: module.is_some_and(|m| m.dirty),
            search_matches: (!query.is_empty()).then(|| search_match_ranges(path, &query)),
        }
    };
    if !tree_mode {
        let mut groups = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
        let mut conflicts = Vec::new();
        for change in changes
            .iter()
            .filter(|change| change.path.to_lowercase().contains(&query))
        {
            let kind = if modules.contains_key(change.path.as_str()) {
                FileTreeKind::Submodule
            } else {
                FileTreeKind::File
            };
            let conflicted = is_conflict(change.index_status, change.worktree_status);
            if conflicted {
                conflicts.push(file_row(
                    &change.path,
                    kind,
                    0,
                    false,
                    change.index_status,
                    change.worktree_status,
                ));
                continue;
            }
            if change.index_status != ' ' && change.index_status != '?' {
                groups[3].push(file_row(
                    &change.path,
                    kind,
                    0,
                    true,
                    change.index_status,
                    ' ',
                ));
            }
            let group = if change.index_status == '?' {
                Some(2)
            } else if change.worktree_status == 'D' {
                Some(1)
            } else if change.worktree_status != ' ' {
                Some(0)
            } else {
                None
            };
            if let Some(group) = group {
                groups[group].push(file_row(
                    &change.path,
                    kind,
                    0,
                    false,
                    ' ',
                    change.worktree_status,
                ));
            }
        }
        let mut rows = Vec::new();
        if !conflicts.is_empty() {
            rows.push(Row::Group {
                name: "冲突".into(),
                count: conflicts.len(),
            });
            if !collapsed.contains("冲突") {
                rows.extend(conflicts);
            }
        }
        for (name, files) in ["已修改", "已删除", "未追踪", "已暂存"]
            .into_iter()
            .zip(groups)
        {
            rows.push(Row::Group {
                name: name.into(),
                count: files.len(),
            });
            if !collapsed.contains(name) {
                rows.extend(files);
            }
        }
        return rows;
    }
    let mut visible = HashSet::new();
    let mut children = BTreeMap::<&str, Vec<_>>::new();
    for entry in &tree.entries {
        if !query.is_empty() && entry.path.to_lowercase().contains(&query) {
            visible.insert(entry.path.as_str());
            let mut path = entry.path.as_str();
            while let Some((parent, _)) = path.rsplit_once('/') {
                visible.insert(parent);
                path = parent;
            }
        }
        children
            .entry(entry.path.rsplit_once('/').map_or("", |(parent, _)| parent))
            .or_default()
            .push(entry);
    }
    for entries in children.values_mut() {
        entries.sort_by(|a, b| {
            (a.kind != FileTreeKind::Directory)
                .cmp(&(b.kind != FileTreeKind::Directory))
                .then_with(|| a.path.cmp(&b.path))
        });
    }
    let mut stack = children
        .get("")
        .into_iter()
        .flatten()
        .rev()
        .map(|entry| (*entry, 0))
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    while let Some((entry, depth)) = stack.pop() {
        if !query.is_empty() && !visible.contains(entry.path.as_str()) {
            continue;
        }
        rows.push(file_row(
            &entry.path,
            entry.kind,
            depth,
            entry.index_status != ' ' && entry.index_status != '?' && entry.worktree_status == ' ',
            entry.index_status,
            entry.worktree_status,
        ));
        if entry.kind == FileTreeKind::Directory
            && (!query.is_empty() || !collapsed.contains(&entry.path))
            && let Some(items) = children.get(entry.path.as_str())
        {
            stack.extend(items.iter().rev().map(|entry| (*entry, depth + 1)));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::ChangeKind;
    use super::{FileListEvent, Row, build_rows, file_click_event, search_match_ranges};
    use gitviews_service::{FileChange, FileTreeEntry, FileTreeKind, RepositoryTree};
    use std::{collections::HashSet, path::Path};

    fn submodule_row(initialized: bool, staged: bool) -> Row {
        Row::File {
            path: "modules/子 模块".into(),
            kind: FileTreeKind::Submodule,
            depth: 1,
            staged,
            status: "M".into(),
            change_kind: super::ChangeKind::Modified,
            initialized,
            dirty: true,
            search_matches: None,
        }
    }

    #[test]
    fn submodule_single_click_previews_the_selected_side_of_the_parent_gitlink() {
        for staged in [false, true] {
            let row = submodule_row(true, staged);
            assert!(matches!(
                file_click_event(&row, Path::new("parent"), 1),
                Some(FileListEvent::OpenFile { path, staged: side })
                    if path == "modules/子 模块" && side == staged
            ));
        }
    }

    #[test]
    fn initialized_submodule_requires_a_double_click_to_enter_the_change_context() {
        let root = Path::new("parent repository");
        let row = submodule_row(true, false);
        assert!(matches!(
            file_click_event(&row, root, 2),
            Some(FileListEvent::OpenSubmodule(path))
                if path == root.join("modules/子 模块")
        ));
        // A third click must not repeatedly enter a potentially nested context.
        assert!(matches!(
            file_click_event(&row, root, 3),
            Some(FileListEvent::OpenFile { .. })
        ));
    }

    #[test]
    fn uninitialized_submodule_never_enters_but_keeps_its_gitlink_preview() {
        let row = submodule_row(false, true);
        for click_count in [1, 2] {
            assert!(matches!(
                file_click_event(&row, Path::new("parent"), click_count),
                Some(FileListEvent::OpenFile { path, staged: true })
                    if path == "modules/子 模块"
            ));
        }
    }

    #[test]
    fn ordinary_file_double_click_still_opens_the_file_and_containers_emit_no_open_event() {
        let mut row = submodule_row(true, false);
        if let Row::File { kind, .. } = &mut row {
            *kind = FileTreeKind::File;
        }
        assert!(matches!(
            file_click_event(&row, Path::new("parent"), 2),
            Some(FileListEvent::OpenFile { .. })
        ));
        if let Row::File { kind, .. } = &mut row {
            *kind = FileTreeKind::Directory;
        }
        assert!(file_click_event(&row, Path::new("parent"), 2).is_none());
        let group = Row::Group {
            name: "已修改".into(),
            count: 1,
        };
        assert!(file_click_event(&group, Path::new("parent"), 1).is_none());
    }

    #[test]
    fn large_tree_keeps_all_paths_and_search_reveals_collapsed_ancestors() {
        let mut tree = RepositoryTree::default();
        tree.entries.push(FileTreeEntry {
            path: "src".into(),
            kind: FileTreeKind::Directory,
            index_status: ' ',
            worktree_status: ' ',
        });
        for index in 0..20_000 {
            tree.entries.push(FileTreeEntry {
                path: format!("src/文件{index}.rs"),
                kind: FileTreeKind::File,
                index_status: ' ',
                worktree_status: ' ',
            });
        }
        assert_eq!(
            build_rows(&tree, &[], "", true, &HashSet::new()).len(),
            20_001
        );
        let collapsed = HashSet::from(["src".into()]);
        assert_eq!(build_rows(&tree, &[], "", true, &collapsed).len(), 1);
        assert_eq!(
            build_rows(&tree, &[], "文件19999", true, &collapsed).len(),
            2
        );
    }
    #[test]
    fn staged_and_working_changes_have_independent_rows() {
        let changes = vec![FileChange {
            path: "a.rs".into(),
            index_status: 'M',
            worktree_status: 'M',
        }];
        let rows = build_rows(
            &RepositoryTree::default(),
            &changes,
            "",
            false,
            &HashSet::new(),
        );
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row, Row::File { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn conflict_rows_retain_all_seven_xy_pairs_and_never_appear_as_staged_changes() {
        let changes = ["DD", "AU", "UD", "UA", "DU", "AA", "UU"].map(|xy| {
            let mut chars = xy.chars();
            FileChange {
                path: format!("{xy}.txt"),
                index_status: chars.next().unwrap(),
                worktree_status: chars.next().unwrap(),
            }
        });
        let rows = build_rows(
            &RepositoryTree::default(),
            &changes,
            "",
            false,
            &HashSet::new(),
        );
        assert!(matches!(&rows[0], Row::Group { name, count: 7 } if name == "冲突"));
        let files = rows
            .iter()
            .filter_map(|row| match row {
                Row::File {
                    path,
                    staged,
                    status,
                    change_kind,
                    ..
                } => Some((path, staged, status, change_kind)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 7);
        for (path, staged, status, kind) in files {
            assert!(!staged);
            assert_eq!(status, path.trim_end_matches(".txt"));
            assert_eq!(*kind, ChangeKind::Conflict);
        }
    }

    #[test]
    fn grouped_compound_changes_color_each_side_and_tree_retains_structural_change() {
        let changes = vec![
            FileChange {
                path: "renamed.rs".into(),
                index_status: 'R',
                worktree_status: 'M',
            },
            FileChange {
                path: "type.rs".into(),
                index_status: 'T',
                worktree_status: 'M',
            },
            FileChange {
                path: "added-deleted.rs".into(),
                index_status: 'A',
                worktree_status: 'D',
            },
            FileChange {
                path: "untracked.rs".into(),
                index_status: '?',
                worktree_status: '?',
            },
        ];
        let tree = RepositoryTree {
            entries: changes
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
        let grouped = build_rows(&tree, &changes, "", false, &HashSet::new());
        let find = |path: &str, side: bool| {
            grouped
                .iter()
                .find_map(|row| match row {
                    Row::File {
                        path: candidate,
                        staged,
                        change_kind,
                        ..
                    } if candidate == path && *staged == side => Some(*change_kind),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(find("renamed.rs", true), ChangeKind::Renamed);
        assert_eq!(find("renamed.rs", false), ChangeKind::Modified);
        assert_eq!(find("type.rs", true), ChangeKind::TypeChanged);
        assert_eq!(find("type.rs", false), ChangeKind::Modified);
        assert_eq!(find("added-deleted.rs", true), ChangeKind::Added);
        assert_eq!(find("added-deleted.rs", false), ChangeKind::Deleted);
        assert_eq!(find("untracked.rs", false), ChangeKind::Untracked);
        let rows = build_rows(&tree, &changes, "", true, &HashSet::new());
        assert!(rows.iter().any(|row| matches!(row, Row::File { path, change_kind: ChangeKind::Renamed, .. } if path == "renamed.rs")));
        assert!(rows.iter().any(|row| matches!(row, Row::File { path, change_kind: ChangeKind::TypeChanged, .. } if path == "type.rs")));
    }

    #[test]
    fn highlight_offsets_keep_original_unicode_boundaries_and_all_occurrences() {
        let path = "😀/İstanbul/文件-İ.TXT";
        let ranges = search_match_ranges(path, "i");
        assert_eq!(
            ranges
                .iter()
                .map(|range| &path[range.clone()])
                .collect::<Vec<_>>(),
            ["İ", "İ"]
        );
        assert_eq!(search_match_ranges(path, "i\u{307}"), ranges);
        assert_eq!(
            search_match_ranges(path, "文件"),
            vec![path.find("文件").unwrap()..path.find("文件").unwrap() + "文件".len()]
        );
        assert!(search_match_ranges(path, "").is_empty());
        assert!(search_match_ranges(path, "missing").is_empty());
        let greek = "ΟΣ/Ος.rs";
        assert_eq!(
            search_match_ranges(greek, "ος")
                .iter()
                .map(|range| &greek[range.clone()])
                .collect::<Vec<_>>(),
            ["ΟΣ", "Ος"]
        );
    }
}
