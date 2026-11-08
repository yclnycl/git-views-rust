use super::*;
use crate::ui::copy_menu::{self, CopyItem};

impl Workspace {
    pub(super) fn copy_menus(
        &self,
        tree: &DesignNode,
        actions: &HashMap<String, WorkbenchAction>,
    ) -> HashMap<String, Vec<CopyItem>> {
        let mut menus = HashMap::new();
        let Some(repository) = &self.repository else {
            return menus;
        };
        let changes_root = self
            .submodule_changes
            .as_ref()
            .map_or(&repository.root, |scope| &scope.repository.root);
        for (id, action) in actions {
            let items = match action {
                WorkbenchAction::OpenWorktree(path) | WorkbenchAction::OpenSubmodule(path) => {
                    let name = path.file_name().map_or_else(
                        || display_path(path),
                        |name| name.to_string_lossy().into_owned(),
                    );
                    let mut items = copy_menu::named_path(&name, path);
                    if let Some(worktree) = self.details.as_ref().and_then(|details| {
                        details
                            .worktrees
                            .iter()
                            .find(|worktree| paths_equal(&worktree.path, path))
                    }) {
                        if let Some(branch) = &worktree.branch {
                            items.push(CopyItem::new("复制分支名称", branch));
                        }
                        if let Some(head) = &worktree.head {
                            items.push(CopyItem::new("复制提交 ID", head));
                        }
                    }
                    items
                }
                WorkbenchAction::CheckoutBranch { name, remote, .. } => self
                    .branch_picker_details()
                    .and_then(|details| {
                        details
                            .branches
                            .iter()
                            .find(|branch| branch.name == *name && branch.is_remote == *remote)
                    })
                    .map(copy_menu::branch_info)
                    .unwrap_or_else(|| copy_menu::branch(name, *remote)),
                WorkbenchAction::OpenFile { path, .. } | WorkbenchAction::ToggleDirectory(path) => {
                    copy_menu::file(changes_root, path)
                }
                WorkbenchAction::ActivateTab(index) => self
                    .opened_files
                    .get(*index)
                    .map(|tab| {
                        let mut items = copy_menu::file(&tab.root, &tab.path);
                        if let Some(revision) = &tab.revision {
                            items.push(CopyItem::new("复制提交 ID", revision));
                        }
                        items
                    })
                    .unwrap_or_default(),
                WorkbenchAction::OpenBranchMenu => {
                    match &self.changes_repository().unwrap_or(repository).head {
                        Head::Branch(name) | Head::Unborn(name) => copy_menu::branch(name, false),
                        Head::Detached(id) => vec![CopyItem::new("复制提交 ID", id)],
                    }
                }
                WorkbenchAction::ToggleSection(name) => name
                    .strip_prefix("实际远程仓库 ")
                    .map(|name| vec![CopyItem::new("复制远程名称", name)])
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            if !items.is_empty() {
                menus.insert(id.clone(), items);
            }
        }
        fn visit(
            node: &DesignNode,
            root: &std::path::Path,
            name: &str,
            menus: &mut HashMap<String, Vec<CopyItem>>,
        ) {
            if matches!(
                node.name.as_str(),
                "选择当前仓库"
                    | "仓库切换器"
                    | "切换仓库"
                    | "仓库名称"
                    | "仓库本地路径"
                    | "窗口仓库标题"
            ) {
                menus.insert(node.id.clone(), copy_menu::named_path(name, root));
            }
            // Uninitialized submodules still support copying their paths.
            if node.id.starts_with("sidebar-module-")
                && node.name.starts_with("子模块 ")
                && let Some(path) = &node.context
            {
                menus.insert(node.id.clone(), copy_menu::file(root, path));
            }
            for child in &node.children {
                visit(child, root, name, menus);
            }
        }
        visit(tree, &repository.root, &repository.name, &mut menus);
        menus
    }
}
