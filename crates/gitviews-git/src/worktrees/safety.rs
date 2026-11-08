//! Filesystem boundaries for worktree administration. Every path that can be
//! renamed or recursively removed is absolute and checked without following
//! symlinks, junctions, mount points, or other Windows reparse points. Explicit
//! forced removal may unlink known leaf links; it never descends into them.

use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use git2::{ErrorCode, Repository};

use super::{Result, WorktreeError, WorktreeErrorKind, changed, git_error, invalid};

const MAX_METADATA_BYTES: u64 = 32 * 1024;
const MAX_REGISTRATIONS: usize = 10_000;
const MAX_TREE_ENTRIES: usize = 200_000;
const MAX_TREE_DEPTH: usize = 128;
static NEXT_FILE: AtomicU64 = AtomicU64::new(1);

pub(super) struct Workspace {
    pub repo: Repository,
    pub root: PathBuf,
    pub main: PathBuf,
    pub common: PathBuf,
}

impl Workspace {
    pub fn open(root: &Path) -> Result<Self> {
        let root = safe_path(root)?;
        safe_path(&root.join(".git"))?;
        let repo = Repository::open(&root).map_err(git_error)?;
        if repo.is_bare()
            || repo
                .workdir()
                .map(safe_path)
                .transpose()?
                .as_ref()
                .is_none_or(|workdir| !same_path(workdir, &root))
        {
            return Err(invalid("请选择工作树根目录后再执行管理操作。"));
        }
        let common = safe_path(repo.commondir())?;
        let main_repo = Repository::open(&common).map_err(git_error)?;
        let main = main_repo
            .workdir()
            .map(safe_path)
            .transpose()?
            .ok_or_else(|| invalid("裸仓库的关联工作树管理暂不支持。"))?;
        check_plain_tree_metadata(&common)?;
        Ok(Self {
            repo,
            root,
            main,
            common,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Registration {
    pub name: String,
    pub admin: PathBuf,
    pub path: PathBuf,
    pub gitdir: Vec<u8>,
    commondir: Vec<u8>,
}

impl Registration {
    pub fn with_path(&self, path: PathBuf, gitdir: Vec<u8>) -> Self {
        Self {
            path,
            gitdir,
            ..self.clone()
        }
    }

    pub fn validate_links(&self, common: &Path, workdir: &Path) -> Result<()> {
        if read_small(&self.admin.join("gitdir"))? != self.gitdir
            || read_small(&self.admin.join("commondir"))? != self.commondir
        {
            return Err(changed());
        }
        if !same_path(&resolve_line(&self.admin, &self.commondir)?, common)
            || !same_path(
                &resolve_line(&self.admin, &self.gitdir)?,
                &workdir.join(".git"),
            )
            || !same_path(
                &dotgit_target(workdir, &read_small(&workdir.join(".git"))?)?,
                &self.admin,
            )
        {
            return Err(invalid("工作树的双向 Git 链接不一致，请先修复登记。"));
        }
        Ok(())
    }
}

pub(super) fn list_registrations(common: &Path) -> Result<Vec<Registration>> {
    let parent = safe_path(&common.join("worktrees"))?;
    if !parent.try_exists().map_err(io_error)? {
        return Ok(Vec::new());
    }
    let mut registrations = Vec::new();
    for entry in fs::read_dir(&parent).map_err(io_error)? {
        if registrations.len() >= MAX_REGISTRATIONS {
            return Err(invalid("工作树登记超过安全读取上限，未执行写入。"));
        }
        let entry = entry.map_err(io_error)?;
        let admin = safe_path(&entry.path())?;
        if !entry.file_type().map_err(io_error)?.is_dir() {
            return Err(invalid("工作树登记目录包含异常文件，未执行写入。"));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid("工作树登记名称不是有效 Unicode。"))?;
        let gitdir = read_small(&admin.join("gitdir"))?;
        let commondir = read_small(&admin.join("commondir"))?;
        if !same_path(&resolve_line(&admin, &commondir)?, common) {
            return Err(invalid("发现指向其他仓库的工作树登记，未执行写入。"));
        }
        let link = resolve_line(&admin, &gitdir)?;
        if link.file_name().is_none_or(|name| name != ".git") {
            return Err(invalid("工作树登记未指向普通 .git 文件。"));
        }
        let path = link
            .parent()
            .ok_or_else(|| invalid("工作树登记路径无效。"))?
            .to_owned();
        // HEAD is required by Git and also ensures missing/broken registrations
        // still reserve their branch until explicitly pruned.
        read_small(&admin.join("HEAD"))?;
        registrations.push(Registration {
            name,
            admin,
            path,
            gitdir,
            commondir,
        });
    }
    Ok(registrations)
}

/// Resolve an absolute path after rejecting every existing symlink/reparse
/// component. Missing final components are kept for stale registrations.
pub(super) fn safe_path(path: &Path) -> Result<PathBuf> {
    let path = native_spelling(path);
    let path = path.as_path();
    if !path.is_absolute() {
        return Err(invalid("工作树路径必须是绝对路径。"));
    }
    validate_spelling(path)?;
    let mut current = PathBuf::new();
    let mut missing = false;
    for component in path.components() {
        if matches!(component, Component::CurDir | Component::ParentDir) {
            return Err(invalid("工作树路径不能包含 . 或 ..。"));
        }
        current.push(component.as_os_str());
        if !matches!(component, Component::Normal(_)) {
            continue;
        }
        if missing {
            continue;
        }
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if is_link(&metadata) {
                    return Err(link_error(&current, &metadata, false));
                }
                current = fs::canonicalize(&current).map_err(io_error)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(current)
}

/// libgit2 normalizes Windows separators, including an extended local prefix
/// into //?/C:/... . Rust recognizes the extended prefix only with backslashes.
fn native_spelling(path: &Path) -> PathBuf {
    crate::native_path(path)
}

fn validate_spelling(path: &Path) -> Result<()> {
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let text = name
            .to_str()
            .ok_or_else(|| invalid("工作树管理暂不支持非 Unicode 路径。"))?;
        if text.is_empty()
            || text
                .chars()
                .any(|ch| ch == '\0' || ch == '\n' || ch == '\r')
        {
            return Err(invalid("工作树路径包含不支持的字符。"));
        }
        #[cfg(windows)]
        {
            let stem = text
                .split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase();
            let reserved = matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) || (stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit());
            if reserved || text.ends_with(['.', ' ']) || text.contains(':') {
                return Err(invalid(
                    "工作树路径包含 Windows 保留名称或数据流，未执行写入。",
                ));
            }
        }
    }
    #[cfg(windows)]
    if let Some(Component::Prefix(prefix)) = path.components().next()
        && !matches!(
            prefix.kind(),
            std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
        )
    {
        return Err(invalid("工作树管理暂时仅支持本地磁盘路径。"));
    }
    Ok(())
}

#[cfg(windows)]
fn is_link(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// `read_link` opens the reparse point itself on Windows and accepts only
/// symbolic-link / mount-point tags. Do not treat every reparse point as an
/// unlinkable link: cloud placeholders and unknown filesystem providers need
/// their own semantics. The link's target is deliberately never resolved.
fn removable_leaf_link(path: &Path, metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink() && fs::read_link(path).is_ok()
}

fn link_description(path: &Path, metadata: &fs::Metadata) -> &'static str {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if !removable_leaf_link(path, metadata) {
            "未知或不可读取的 Windows 重解析点"
        } else if metadata.file_type().is_symlink_dir() {
            "目录符号链接或联接（junction）"
        } else {
            "文件符号链接"
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (path, metadata);
        "符号链接"
    }
}

fn link_error(path: &Path, metadata: &fs::Metadata, force_available: bool) -> WorktreeError {
    let detail = if force_available && removable_leaf_link(path, metadata) {
        "可勾选强制移除，仅删除链接本身，保留链接目标。"
    } else {
        "为避免越界操作，未执行移动或删除。"
    };
    invalid(&format!(
        "检测到{}：{}；{}",
        link_description(path, metadata),
        crate::display_path(path),
        detail
    ))
}

pub(super) fn same_path(left: &Path, right: &Path) -> bool {
    crate::paths_equal(left, right)
}

pub(super) fn contains_path(parent: &Path, child: &Path) -> bool {
    let mut components = child.components();
    parent.components().all(|expected| {
        components.next().is_some_and(|actual| {
            same_path(
                Path::new(expected.as_os_str()),
                Path::new(actual.as_os_str()),
            )
        })
    })
}

pub(super) fn destination(context: &Workspace, target: &Path) -> Result<PathBuf> {
    let target = safe_path(target)?;
    if target.try_exists().map_err(io_error)? {
        return Err(invalid(
            "目标目录已经存在（包括空目录）；请选择一个尚不存在的新目录。",
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| invalid("不能使用磁盘根目录作为工作树。"))?;
    if !parent.is_dir() {
        return Err(invalid("目标目录的父目录必须已经存在。"));
    }
    check_destination_boundaries(context, None, &target)?;
    // Disallow nesting inside any unrelated repository, not merely one of the
    // selected repository's known worktrees.
    match Repository::discover(parent) {
        Ok(_) => return Err(invalid("目标目录位于现有 Git 仓库内，不能创建嵌套工作树。")),
        Err(error) if error.code() == ErrorCode::NotFound => {}
        Err(error) => return Err(git_error(error)),
    }
    Ok(target)
}

fn check_destination_boundaries(
    context: &Workspace,
    skip: Option<&Registration>,
    target: &Path,
) -> Result<()> {
    for protected in [&context.common, &context.main] {
        if contains_path(protected, target) || contains_path(target, protected) {
            return Err(invalid("目标路径与仓库或主工作树重叠，未执行操作。"));
        }
    }
    for registration in list_registrations(&context.common)? {
        if skip.is_some_and(|skip| skip.admin == registration.admin) {
            continue;
        }
        if contains_path(&registration.path, target) || contains_path(target, &registration.path) {
            return Err(invalid("目标路径与已有工作树登记重叠，未执行操作。"));
        }
    }
    Ok(())
}

pub(super) fn source_boundaries(context: &Workspace, selected: &Registration) -> Result<()> {
    for protected in [&context.main, &context.common, &context.root] {
        if contains_path(&selected.path, protected) || contains_path(protected, &selected.path) {
            return Err(invalid(
                "工作树路径与主仓库或当前工作树重叠，不能安全管理。",
            ));
        }
    }
    for other in list_registrations(&context.common)? {
        if selected.admin == other.admin {
            continue;
        }
        if contains_path(&selected.path, &other.path) || contains_path(&other.path, &selected.path)
        {
            return Err(invalid("工作树路径与另一登记重叠，不能安全管理。"));
        }
    }
    Ok(())
}

pub(super) fn repair_destination(
    context: &Workspace,
    registration: &Registration,
    target: &Path,
) -> Result<()> {
    if !target.is_dir() || same_path(target, &registration.path) {
        return Err(invalid("请选择移动后实际存在的新工作目录。"));
    }
    check_destination_boundaries(context, Some(registration), target)?;
    let parent = target
        .parent()
        .ok_or_else(|| invalid("新工作目录路径无效。"))?;
    match Repository::discover(parent) {
        Ok(_) => Err(invalid("新工作目录位于其他 Git 仓库内，不能自动修复。")),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(()),
        Err(error) => Err(git_error(error)),
    }
}

pub(super) fn read_small(path: &Path) -> Result<Vec<u8>> {
    let path = safe_path(path)?;
    let metadata = fs::symlink_metadata(&path).map_err(|error| read_error(&path, error))?;
    if is_link(&metadata) {
        return Err(link_error(&path, &metadata, false));
    }
    if !metadata.is_file() || metadata.len() > MAX_METADATA_BYTES {
        return Err(invalid(&format!(
            "Git 工作树元数据不是有界的普通文件：{}。",
            crate::display_path(&path)
        )));
    }
    let mut bytes = Vec::new();
    fs::File::open(&path)
        .map_err(|error| read_error(&path, error))?
        .take(MAX_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| read_error(&path, error))?;
    if bytes.len() as u64 > MAX_METADATA_BYTES {
        return Err(invalid(&format!(
            "Git 工作树元数据超出安全上限：{}。",
            crate::display_path(&path)
        )));
    }
    Ok(bytes)
}

fn text_line(bytes: &[u8]) -> Result<&str> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("Git 工作树元数据不是 UTF-8。"))?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    if text.is_empty()
        || text
            .chars()
            .any(|ch| ch == '\0' || ch == '\n' || ch == '\r')
    {
        return Err(invalid("Git 工作树元数据包含异常路径。"));
    }
    Ok(text)
}

fn resolve_line(base: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let link = native_spelling(Path::new(text_line(bytes)?));
    let joined = if link.is_absolute() {
        link
    } else {
        base.join(link)
    };
    let mut normalized = PathBuf::new();
    for part in joined.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(invalid("Git 元数据路径越过磁盘根目录。"));
                }
            }
            _ => normalized.push(part.as_os_str()),
        }
    }
    safe_path(&normalized)
}

pub(super) fn dotgit_target(workdir: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let text = text_line(bytes)?
        .strip_prefix("gitdir: ")
        .ok_or_else(|| invalid("关联工作树 .git 不是有效链接文件。"))?;
    resolve_line(workdir, text.as_bytes())
}

pub(super) fn path_line(path: &Path, prefix: &str) -> Result<Vec<u8>> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("Git 工作树链接需要 Unicode 路径。"))?;
    if text.contains(['\n', '\r', '\0']) {
        return Err(invalid("Git 链接路径包含异常字符。"));
    }
    #[cfg(windows)]
    let text = text
        .strip_prefix("\\\\?\\")
        .unwrap_or(text)
        .replace('\\', "/");
    // Ordinary drive syntax is interoperable with both Git and libgit2; the
    // filesystem checks retain canonical extended prefixes for long paths.
    Ok(format!("{prefix}{text}\n").into_bytes())
}

pub(super) fn check_plain_tree(root: &Path, workdir: bool) -> Result<()> {
    check_tree(root, workdir, &[], false)
}

/// Only absorbed, independently validated submodules may contribute nested
/// `.git` files. Their metadata lives outside the directory being removed.
pub(super) fn check_removable_tree(
    root: &Path,
    initialized_module_roots: &[PathBuf],
    force: bool,
) -> Result<()> {
    let root = safe_path(root)?;
    let allowed = initialized_module_roots
        .iter()
        .map(|module| {
            let module = safe_path(module)?;
            if same_path(&module, &root) || !contains_path(&root, &module) {
                return Err(invalid("子模块路径不在所选工作树内，未执行删除。"));
            }
            Ok(module.join(".git"))
        })
        .collect::<Result<Vec<_>>>()?;
    check_tree(&root, true, &allowed, force)
}

fn check_tree(
    root: &Path,
    workdir: bool,
    allowed_gitfiles: &[PathBuf],
    allow_leaf_links: bool,
) -> Result<()> {
    let root = safe_path(root)?;
    let mut pending = vec![(root.clone(), 0)];
    let mut count = 0usize;
    while let Some((directory, depth)) = pending.pop() {
        if depth > MAX_TREE_DEPTH {
            return Err(invalid("目录层级超出安全检查上限，未执行移动或删除。"));
        }
        for entry in fs::read_dir(&directory).map_err(|error| read_error(&directory, error))? {
            count += 1;
            if count > MAX_TREE_ENTRIES {
                return Err(invalid("目录项目超出安全检查上限，未执行移动或删除。"));
            }
            let entry = entry.map_err(|error| read_error(&directory, error))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| read_error(&path, error))?;
            let git_entry = same_path(Path::new(&entry.file_name()), Path::new(".git"));
            if is_link(&metadata) {
                // A .git entry is administrative metadata even when nested;
                // forcing content removal never authorizes following or
                // unlinking a substituted repository metadata link.
                if allow_leaf_links && !git_entry && removable_leaf_link(&path, &metadata) {
                    continue;
                }
                return Err(link_error(&path, &metadata, workdir && !git_entry));
            }
            if !metadata.is_dir() && !metadata.is_file() {
                return Err(invalid(&format!(
                    "{}不是普通文件或目录，未执行移动或删除。",
                    crate::display_path(&path)
                )));
            }
            if workdir
                && git_entry
                && !same_path(&path, &root.join(".git"))
                && !(metadata.is_file()
                    && allowed_gitfiles
                        .iter()
                        .any(|allowed| same_path(allowed, &path)))
            {
                return Err(WorktreeError::new(
                    WorktreeErrorKind::Unsupported,
                    format!(
                        "{}是嵌套 Git 仓库，暂不支持移动或删除。",
                        crate::display_path(&path)
                    ),
                ));
            }
            if workdir
                && metadata.is_dir()
                && path.join("HEAD").is_file()
                && path.join("objects").is_dir()
                && path.join("refs").is_dir()
            {
                return Err(WorktreeError::new(
                    WorktreeErrorKind::Unsupported,
                    format!(
                        "{}是嵌套裸仓库，暂不支持移动或删除。",
                        crate::display_path(&path)
                    ),
                ));
            }
            if metadata.is_dir() {
                pending.push((path, depth + 1));
            }
        }
    }
    Ok(())
}

/// Keep all module repositories, including deinitialized modules and orphaned
/// local refs, outside Git's worktree-pruning namespace. Keeping the same depth
/// below the common directory preserves the registration's `../..` commondir.
pub(super) struct Retention {
    source: PathBuf,
    destination: PathBuf,
    _reservation: Lease,
}

impl Retention {
    pub fn validate(&self) -> Result<()> {
        self.validate_with_locks(&[])
    }

    pub fn validate_with_locks(&self, allowed_locks: &[PathBuf]) -> Result<()> {
        let source = safe_path(&self.source)?;
        let destination = safe_path(&self.destination)?;
        if !same_path(&source, &self.source)
            || !same_path(&destination, &self.destination)
            || destination.try_exists().map_err(io_error)?
        {
            return Err(changed());
        }
        check_retained_metadata(&source, allowed_locks)
    }

    pub fn archive(self) -> Result<PathBuf> {
        self.validate().map_err(|error| {
            WorktreeError::new(
                error.kind,
                format!(
                    "工作目录已移除，但子模块元数据归档前检查未通过；原工作树登记与元数据已保留，请刷新后重试清理。{}",
                    error.message
                ),
            )
        })?;
        fs::rename(&self.source, &self.destination).map_err(|error| {
            WorktreeError::new(
                WorktreeErrorKind::Io,
                format!(
                    "工作目录已移除，但子模块元数据归档失败；原工作树登记与元数据已保留，请刷新后重试清理。{error}"
                ),
            )
        })?;
        Ok(self.destination.clone())
    }
}

pub(super) fn prepare_retention(
    common: &Path,
    registration: &Registration,
) -> Result<Option<Retention>> {
    let common = safe_path(common)?;
    let source = safe_path(&registration.admin)?;
    let registrations = safe_path(&common.join("worktrees"))?;
    if source
        .parent()
        .is_none_or(|parent| !same_path(parent, &registrations))
    {
        return Err(invalid("工作树元数据不在精确登记目录内，未执行操作。"));
    }
    let modules = safe_path(&source.join("modules"))?;
    if !modules.try_exists().map_err(io_error)? {
        return Ok(None);
    }
    if !modules.is_dir() {
        return Err(invalid("子模块元数据不是普通目录，未执行操作。"));
    }
    check_retained_metadata(&source, &[])?;
    let parent = safe_path(&common.join("gitviews-retained-worktrees"))?;
    match fs::create_dir(&parent) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && parent.is_dir() => {}
        Err(error) => return Err(io_error(error)),
    }
    let parent = safe_path(&parent)?;
    for _ in 0..32 {
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let name = format!("{}-{}-{id}", registration.name, std::process::id());
        let destination = safe_path(&parent.join(&name))?;
        if destination.try_exists().map_err(io_error)? {
            continue;
        }
        let reservation = match Lease::acquire(parent.join(format!("{name}.lock"))) {
            Ok(reservation) => reservation,
            Err(error) if error.kind == WorktreeErrorKind::Busy => continue,
            Err(error) => return Err(error),
        };
        let retention = Retention {
            source,
            destination,
            _reservation: reservation,
        };
        retention.validate()?;
        return Ok(Some(retention));
    }
    Err(WorktreeError::new(
        WorktreeErrorKind::Busy,
        "暂时无法分配子模块元数据保留目录，请稍后重试。",
    ))
}

fn check_retained_metadata(admin: &Path, allowed_locks: &[PathBuf]) -> Result<()> {
    check_plain_tree(admin, false)?;
    let allowed_locks = allowed_locks
        .iter()
        .map(|path| safe_path(path))
        .collect::<Result<Vec<_>>>()?;
    let modules = safe_path(&admin.join("modules"))?;
    let mut pending = vec![(modules, 0)];
    let mut count = 0usize;
    while let Some((directory, depth)) = pending.pop() {
        if depth > MAX_TREE_DEPTH {
            return Err(invalid("子模块元数据层级超出安全检查上限，未执行操作。"));
        }
        // Submodule names and refs may themselves be called `worktrees` or
        // `commondir`; only direct repository metadata has these meanings.
        let repository_root = is_metadata_repository(&directory);
        let repository_objects_info = directory
            .file_name()
            .is_some_and(|name| same_path(Path::new(name), Path::new("info")))
            && directory.parent().is_some_and(|objects| {
                objects
                    .file_name()
                    .is_some_and(|name| same_path(Path::new(name), Path::new("objects")))
                    && objects.parent().is_some_and(is_metadata_repository)
            });
        for entry in fs::read_dir(&directory).map_err(|error| read_error(&directory, error))? {
            count += 1;
            if count > MAX_TREE_ENTRIES {
                return Err(invalid("子模块元数据超出安全检查上限，未执行操作。"));
            }
            let entry = entry.map_err(|error| read_error(&directory, error))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|error| read_error(&path, error))?;
            if is_link(&metadata) {
                return Err(link_error(&path, &metadata, false));
            }
            if !metadata.is_dir() && !metadata.is_file() {
                return Err(invalid(&format!(
                    "子模块元数据不是普通文件或目录：{}，未执行操作。",
                    crate::display_path(&path)
                )));
            }
            let name = entry.file_name();
            if metadata.is_file()
                && name
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .ends_with(".lock")
                && !allowed_locks
                    .iter()
                    .any(|allowed| same_path(allowed, &path))
            {
                return Err(WorktreeError::new(
                    WorktreeErrorKind::Busy,
                    "子模块元数据存在其他 Git 操作的锁文件，请稍后重试。",
                ));
            }
            if repository_root
                && (same_path(Path::new(&name), Path::new("commondir"))
                    || (same_path(Path::new(&name), Path::new("worktrees"))
                        && (!metadata.is_dir()
                            || fs::read_dir(&path).map_err(io_error)?.next().is_some())))
            {
                return Err(WorktreeError::new(
                    WorktreeErrorKind::Unsupported,
                    "子模块元数据被其他工作树共享，暂不能安全移除；请先处理子模块工作树。",
                ));
            }
            if (same_path(Path::new(&name), Path::new("alternates"))
                || same_path(Path::new(&name), Path::new("http-alternates")))
                && repository_objects_info
                && (!metadata.is_file() || metadata.len() != 0)
            {
                return Err(WorktreeError::new(
                    WorktreeErrorKind::Unsupported,
                    "子模块使用共享对象库，暂不能确认归档后的对象完整性；请先取消对象库依赖。",
                ));
            }
            if metadata.is_dir() {
                pending.push((path, depth + 1));
            }
        }
    }
    Ok(())
}

fn is_metadata_repository(directory: &Path) -> bool {
    directory.join("HEAD").is_file()
        && (directory.join("objects").is_dir() || directory.join("commondir").is_file())
}

fn check_plain_tree_metadata(common: &Path) -> Result<()> {
    if !common.is_dir() {
        return Err(invalid("Git 公共目录不可用。"));
    }
    safe_path(&common.join("worktrees"))?;
    Ok(())
}

pub(super) fn unused_name(common: &Path) -> Result<String> {
    for _ in 0..32 {
        let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
        let name = format!("gitviews-{}-{id}", std::process::id());
        if !common
            .join("worktrees")
            .join(&name)
            .try_exists()
            .map_err(io_error)?
        {
            return Ok(name);
        }
    }
    Err(WorktreeError::new(
        WorktreeErrorKind::Busy,
        "暂时无法分配工作树登记名称，请稍后重试。",
    ))
}

pub(super) struct Lease {
    path: PathBuf,
}
impl Lease {
    pub fn acquire(path: PathBuf) -> Result<Self> {
        let path = safe_path(&path)?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists { WorktreeError::new(WorktreeErrorKind::Busy, "其他 GitViews 进程正在管理工作树；若程序异常退出，请确认无操作后检查 GitViews 锁文件。") } else { io_error(error) }
        })?;
        if let Err(error) = writeln!(file, "{}", std::process::id()) {
            let _ = fs::remove_file(&path);
            return Err(io_error(error));
        }
        Ok(Self { path })
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Replace only metadata whose previously validated bytes are still unchanged.
/// A unique sibling file is flushed before rename; no unbounded text is read.
pub(super) fn atomic_replace(path: &Path, expected: &[u8], replacement: &[u8]) -> Result<()> {
    let path = safe_path(path)?;
    if replacement.len() as u64 > MAX_METADATA_BYTES || read_small(&path)? != expected {
        return Err(changed());
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Git 元数据目录无效。"))?;
    let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!("gitviews-edit-{}-{id}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(io_error)?;
        file.write_all(replacement).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        drop(file);
        if read_small(&path)? != expected {
            return Err(changed());
        }
        fs::rename(&temporary, &path).map_err(io_error)?;
        Ok(())
    })();
    if temporary.try_exists().unwrap_or(false) {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(super) fn restore_if_matches(path: &Path, modified: &[u8], original: &[u8]) -> Result<()> {
    let present = read_small(path)?;
    if present == original {
        return Ok(());
    }
    atomic_replace(path, modified, original)
}

pub(super) fn io_error(error: std::io::Error) -> WorktreeError {
    WorktreeError::new(
        WorktreeErrorKind::Io,
        format!("工作树文件操作失败：{error}"),
    )
}

fn read_error(path: &Path, error: std::io::Error) -> WorktreeError {
    WorktreeError::new(
        WorktreeErrorKind::Io,
        format!("读取 {} 失败：{error}", crate::display_path(path)),
    )
}
