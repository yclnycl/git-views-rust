//! Shared clipboard menus. Values come from the clicked row, never the selection.

use std::path::Path;

use gitviews_service::BranchInfo;
use gpui_kit::{
    component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    *,
};

use super::paths::display_path;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CopyItem {
    pub label: &'static str,
    pub value: String,
}

impl CopyItem {
    pub fn new(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: value.into(),
        }
    }
}

/// The component handles positioning, dismissal, keyboard navigation and focus.
/// Opening its right-click menu does not dispatch the row's left-click action.
pub(super) trait CopyMenuExt:
    InteractiveElement + ParentElement + Styled + IntoElement + Sized + 'static
{
    fn copy_menu(self, items: Vec<CopyItem>) -> AnyElement {
        if items.is_empty() {
            return self.into_any_element();
        }
        self.context_menu(move |mut menu: PopupMenu, _, _| {
            for item in &items {
                let value = item.value.clone();
                menu = menu.item(PopupMenuItem::new(item.label).on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
                }));
            }
            menu
        })
        .into_any_element()
    }
}

impl<E: InteractiveElement + ParentElement + Styled + IntoElement + Sized + 'static> CopyMenuExt
    for E
{
}

pub(super) fn named_path(name: &str, path: &Path) -> Vec<CopyItem> {
    vec![
        CopyItem::new("复制名称", name),
        CopyItem::new("复制路径", display_path(path)),
    ]
}

pub(super) fn file(root: &Path, path: &str) -> Vec<CopyItem> {
    vec![
        CopyItem::new(
            "复制文件名",
            path.rsplit(['/', '\\']).next().unwrap_or(path),
        ),
        CopyItem::new("复制相对路径", path),
        CopyItem::new("复制绝对路径", display_path(&root.join(path))),
    ]
}

pub(super) fn branch(name: &str, remote: bool) -> Vec<CopyItem> {
    vec![
        CopyItem::new("复制分支名称", name),
        CopyItem::new(
            "复制完整引用",
            format!("refs/{}/{name}", if remote { "remotes" } else { "heads" }),
        ),
    ]
}

pub(super) fn branch_info(branch: &BranchInfo) -> Vec<CopyItem> {
    let mut items = self::branch(&branch.name, branch.is_remote);
    if !branch.commit_id.is_empty() {
        items.push(CopyItem::new("复制提交 ID", &branch.commit_id));
    }
    if let Some(upstream) = &branch.upstream {
        items.push(CopyItem::new("复制跟踪分支", upstream));
    }
    items
}

#[cfg(test)]
mod tests {
    use super::{branch, display_path, file};
    use std::path::Path;

    #[test]
    fn file_paths_use_the_clicked_worktree_or_submodule_root() {
        let root = Path::new("worktrees/另一 工作树");
        let child = root.join("vendor/子模块");
        assert_eq!(
            file(root, "src/中文.rs")[2].value,
            display_path(&root.join("src/中文.rs"))
        );
        assert_eq!(
            file(&child, "src/中文.rs")[2].value,
            display_path(&child.join("src/中文.rs"))
        );
        assert_eq!(file(root, "src/中文.rs")[0].value, "中文.rs");
        assert_eq!(file(root, "src/中文.rs")[1].value, "src/中文.rs");
    }

    #[test]
    fn branch_copy_preserves_remote_and_nested_branch_names() {
        assert_eq!(
            branch("feature/中文", false)[1].value,
            "refs/heads/feature/中文"
        );
        assert_eq!(
            branch("origin/feature/中文", true)[0].value,
            "origin/feature/中文"
        );
        assert_eq!(
            branch("origin/feature/中文", true)[1].value,
            "refs/remotes/origin/feature/中文"
        );
    }

    #[cfg(windows)]
    #[test]
    fn copied_worktree_and_file_paths_use_readable_drive_and_network_paths() {
        let root = Path::new(r"\\?\F:\project\中文 工作树");
        assert_eq!(
            super::named_path("中文 工作树", root)[1].value,
            r"F:\project\中文 工作树"
        );
        assert_eq!(
            file(root, "src/中文.rs")[2].value,
            r"F:\project\中文 工作树\src\中文.rs"
        );
        assert_eq!(
            super::named_path("共享仓库", Path::new(r"\\?\UNC\server\share\共享仓库"))[1].value,
            r"\\server\share\共享仓库"
        );
    }
}
