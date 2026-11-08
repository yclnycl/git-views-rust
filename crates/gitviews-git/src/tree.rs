/// The repository file tree and its nested repositories.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepositoryTree {
    /// Sorted full repository-relative paths, including parent directories.
    pub entries: Vec<FileTreeEntry>,
    /// Configured submodules and index gitlinks, including uninitialized ones.
    pub submodules: Vec<SubmoduleInfo>,
    /// Recoverable failures encountered while enumerating other entries.
    pub warnings: Vec<TreeWarning>,
}

/// One skipped or partially readable item; other tree entries remain usable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeWarning {
    /// Repository-relative affected path, or an empty string for the root.
    pub path: String,
    /// Diagnostic suitable for an error/details panel.
    pub message: String,
}

/// One flat tree entry. Clean entries have a space in both status columns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileTreeEntry {
    /// Full repository-relative path using forward slashes.
    pub path: String,
    /// File type; gitlinks and symlinks are never traversed.
    pub kind: FileTreeKind,
    /// Porcelain-compatible index status, including `U` for conflicts.
    pub index_status: char,
    /// Porcelain-compatible working-file status.
    pub worktree_status: char,
}

/// Node roles understood by the repository tree UI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileTreeKind {
    File,
    Directory,
    Symlink,
    Submodule,
}

/// Read-only information about a configured submodule or an unconfigured gitlink.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmoduleInfo {
    /// Name from .gitmodules, or the path for a gitlink without configuration.
    pub name: String,
    /// Full repository-relative submodule path.
    pub path: String,
    /// Configured source URL, absent for an unconfigured gitlink.
    pub url: Option<String>,
    /// Commit recorded by the superproject's HEAD.
    pub head_id: Option<String>,
    /// Commit recorded by the superproject's index.
    pub index_id: Option<String>,
    /// Actual checked-out submodule commit when available.
    pub workdir_id: Option<String>,
    /// Whether the submodule has a usable local repository.
    pub initialized: bool,
    /// Whether commits, index, or working files differ from the recorded state.
    pub dirty: bool,
}
