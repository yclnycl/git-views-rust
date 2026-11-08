//! Removal checks for absorbed submodules. Never infer that a gitlink is safe
//! from its parent's status: ignore rules can hide both child changes and files
//! placed in an uninitialized submodule directory.

use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

use git2::{ErrorCode, Oid, Repository, TreeWalkMode, TreeWalkResult};

use super::{
    Result, WorktreeError, WorktreeErrorKind, changed, git_error, head_state,
    safety::{
        Registration, contains_path, dotgit_target, io_error, read_small, safe_path, same_path,
    },
};
use crate::Head;

const MAX_MODULES: usize = 10_000;
const MAX_DEPTH: usize = 64;
const MAX_TREE_ENTRIES: usize = 200_000;

#[derive(Eq, PartialEq)]
struct ModuleState {
    root: PathBuf,
    gitlink: Oid,
    gitfile: Option<Vec<u8>>,
    gitdir: Option<PathBuf>,
    head: Option<(Head, Option<String>)>,
}

pub(super) struct RemovalModules {
    states: Vec<ModuleState>,
    pub repositories: Vec<Repository>,
}

impl RemovalModules {
    pub fn inspect(parent: &Repository, registration: &Registration, force: bool) -> Result<Self> {
        let mut modules = Self {
            states: Vec::new(),
            repositories: Vec::new(),
        };
        let store = safe_path(&registration.admin.join("modules"))?;
        modules.visit(parent, &store, 0, force)?;
        Ok(modules)
    }

    fn visit(
        &mut self,
        parent: &Repository,
        store: &Path,
        depth: usize,
        force: bool,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(unsupported("子模块层级超过安全检查上限，未删除工作树。"));
        }
        let workdir = parent
            .workdir()
            .ok_or_else(|| unsupported("无法确认子模块工作目录。"))?;
        let workdir = safe_path(workdir)?;
        for (path, id) in gitlinks(parent, force)? {
            if self.states.len() >= MAX_MODULES {
                return Err(unsupported("子模块数量超过安全检查上限，未删除工作树。"));
            }
            let relative = std::str::from_utf8(&path)
                .map_err(|_| unsupported("子模块路径不是有效 Unicode，未删除工作树。"))?;
            let relative = Path::new(relative);
            if relative.as_os_str().is_empty()
                || relative.components().any(|part| match part {
                    Component::Normal(name) => name.to_string_lossy().eq_ignore_ascii_case(".git"),
                    _ => true,
                })
            {
                return Err(unsupported("子模块路径不安全，未删除工作树。"));
            }
            let root = safe_path(&workdir.join(relative))?;
            if same_path(&root, &workdir) || !contains_path(&workdir, &root) {
                return Err(unsupported("子模块路径越过父工作树，未删除工作树。"));
            }
            let mut state = ModuleState {
                root: root.clone(),
                gitlink: id,
                gitfile: None,
                gitdir: None,
                head: None,
            };
            if !root.try_exists().map_err(io_error)? {
                self.states.push(state);
                continue;
            }
            if !root.is_dir() {
                if !force {
                    return Err(dirty(&root, "子模块路径包含普通文件"));
                }
                self.states.push(state);
                continue;
            }
            let dotgit = safe_path(&root.join(".git"))?;
            if !dotgit.try_exists().map_err(io_error)? {
                if !force
                    && fs::read_dir(&root)
                        .map_err(io_error)?
                        .next()
                        .transpose()
                        .map_err(io_error)?
                        .is_some()
                {
                    return Err(dirty(&root, "未初始化的子模块目录中仍有文件"));
                }
                self.states.push(state);
                continue;
            }
            if !dotgit.is_file() {
                return Err(unsupported(format!(
                    "子模块 {} 使用内嵌 .git 目录；请先将其 Git 数据迁出工作目录，再移除工作树。",
                    crate::display_path(&root)
                )));
            }
            let gitfile = read_small(&dotgit)?;
            let gitdir = dotgit_target(&root, &gitfile)?;
            if same_path(&gitdir, store) || !contains_path(store, &gitdir) {
                return Err(unsupported(format!(
                    "子模块 {} 的 Git 数据不在此工作树的 modules 目录，无法确认独占关系；文件已保留。",
                    crate::display_path(&root)
                )));
            }
            if self.states.iter().any(|state| {
                state
                    .gitdir
                    .as_ref()
                    .is_some_and(|path| same_path(path, &gitdir))
            }) {
                return Err(unsupported("多个子模块共享 Git 目录，未删除工作树。"));
            }
            let repo = Repository::open(&root).map_err(git_error)?;
            if repo.is_bare()
                || !same_path(&safe_path(repo.path())?, &gitdir)
                || !same_path(&safe_path(repo.commondir())?, &gitdir)
                || repo
                    .workdir()
                    .map(safe_path)
                    .transpose()?
                    .as_ref()
                    .is_none_or(|path| !same_path(path, &root))
            {
                return Err(unsupported("子模块 Git 链接或工作目录不一致，文件已保留。"));
            }
            let head = head_state(&repo)?;
            if !force && head.1.as_deref() != Some(id.to_string().as_str()) {
                return Err(dirty(&root, "子模块 HEAD 与父仓库记录的提交不同"));
            }
            state.gitfile = Some(gitfile);
            state.gitdir = Some(gitdir);
            state.head = Some(head);
            self.states.push(state);
            self.visit(&repo, store, depth + 1, force)?;
            self.repositories.push(repo);
        }
        Ok(())
    }

    /// Reopen after acquiring HEAD/index locks to detect replaced gitfiles,
    /// changed gitlinks, initialized/deinitialized modules and HEAD changes.
    pub fn validate(
        &self,
        parent: &Repository,
        registration: &Registration,
        force: bool,
    ) -> Result<()> {
        let current = Self::inspect(parent, registration, force)?;
        if self.states != current.states {
            return Err(changed());
        }
        Ok(())
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        self.states
            .iter()
            .filter(|state| state.gitdir.is_some())
            .map(|state| state.root.clone())
            .collect()
    }

    pub fn gitlink_roots(&self) -> Vec<PathBuf> {
        self.states.iter().map(|state| state.root.clone()).collect()
    }
}

/// Staging a gitlink's removal leaves its working directory behind. Forced
/// removal must still recognize and validate that absorbed repository, rather
/// than treating its .git file as an unrelated nested repository. Index OIDs
/// take precedence; HEAD supplies paths no longer present in the index.
fn gitlinks(repo: &Repository, force: bool) -> Result<BTreeMap<Vec<u8>, Oid>> {
    let mut index = repo.index().map_err(git_error)?;
    index.read(true).map_err(git_error)?;
    let mut links: BTreeMap<_, _> = index
        .iter()
        .filter(|entry| entry.mode == 0o160000)
        .map(|entry| (entry.path, entry.id))
        .collect();
    if force {
        let tree = match repo.head() {
            Ok(head) => Some(head.peel_to_tree().map_err(git_error)?),
            Err(error) if matches!(error.code(), ErrorCode::UnbornBranch | ErrorCode::NotFound) => {
                None
            }
            Err(error) => return Err(git_error(error)),
        };
        if let Some(tree) = tree {
            let mut count = 0usize;
            let mut over_limit = false;
            let result = tree.walk(TreeWalkMode::PreOrder, |directory, entry| {
                count += 1;
                if count > MAX_TREE_ENTRIES
                    || directory.bytes().filter(|byte| *byte == b'/').count() > MAX_DEPTH
                {
                    over_limit = true;
                    return TreeWalkResult::Abort;
                }
                if entry.filemode() == 0o160000 {
                    let mut path = directory.as_bytes().to_vec();
                    path.extend_from_slice(entry.name_bytes());
                    links.entry(path).or_insert(entry.id());
                }
                TreeWalkResult::Ok
            });
            if over_limit {
                return Err(unsupported(
                    "HEAD 目录超出子模块安全检查上限，未删除工作树。",
                ));
            }
            result.map_err(git_error)?;
        }
    }
    if links.len() > MAX_MODULES {
        return Err(unsupported("子模块数量超过安全检查上限，未删除工作树。"));
    }
    Ok(links)
}

fn unsupported(message: impl Into<String>) -> WorktreeError {
    WorktreeError::new(WorktreeErrorKind::Unsupported, message)
}

fn dirty(root: &Path, reason: &str) -> WorktreeError {
    WorktreeError::new(
        WorktreeErrorKind::Dirty,
        format!(
            "{reason}：{}。请先保留这些数据，再移除工作树。",
            crate::display_path(root)
        ),
    )
}
