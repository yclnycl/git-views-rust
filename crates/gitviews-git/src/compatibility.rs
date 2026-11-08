//! Conservative native-mode checks: never silently skip a configured workflow.
use std::{fs, path::Path};

use git2::{ErrorCode, Repository};

/// Return a user-facing reason to select the installed Git implementation.
pub(crate) fn native_workflow_guard(
    root: &Path,
    hooks: &[&str],
    creates_commit: bool,
) -> Result<(), String> {
    // Preserve each operation's existing InvalidRepository error contract.
    let Ok(repository) = Repository::open(root) else {
        return Ok(());
    };
    let config = repository
        .config()
        .map_err(|_| "无法读取 Git 配置。".to_owned())?;
    if creates_commit {
        match config.get_bool("commit.gpgsign") {
            Ok(true) => return Err(
                "仓库要求提交签名，请在 Git 引擎设置中选择本地 Git，以使用已有 GPG／SSH 签名配置。"
                    .into(),
            ),
            Ok(false) => {}
            Err(error) if error.code() == ErrorCode::NotFound => {}
            Err(_) => return Err("commit.gpgsign 配置无效，请检查 Git 配置。".into()),
        }
    }
    let hook_directory = match config.get_path("core.hooksPath") {
        Ok(path) if path.is_absolute() => path,
        Ok(path) => repository.workdir().unwrap_or(repository.path()).join(path),
        Err(error) if error.code() == ErrorCode::NotFound => {
            // Linked worktrees share their hook directory with the common repo.
            repository.commondir().join("hooks")
        }
        Err(_) => return Err("core.hooksPath 配置无效，请检查 Git 配置。".into()),
    };
    for name in hooks {
        let path = hook_directory.join(name);
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(format!("无法检查 Git hook {name}，请切换到本地 Git。")),
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Err(format!(
            "检测到已启用的 Git hook：{name}。内置引擎不会执行 hooks，请切换到本地 Git 后重试。"
        ));
    }
    Ok(())
}

pub(crate) const REFERENCE_HOOKS: &[&str] = &["reference-transaction"];
pub(crate) const INDEX_HOOKS: &[&str] = &["post-index-change"];
pub(crate) const COMMIT_HOOKS: &[&str] = &[
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "reference-transaction",
    "post-index-change",
];
pub(crate) const CHECKOUT_HOOKS: &[&str] = &[
    "post-checkout",
    "reference-transaction",
    "post-index-change",
];
pub(crate) const MERGE_HOOKS: &[&str] = &[
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-merge",
    "post-rewrite",
    "pre-rebase",
    "pre-commit",
    "post-commit",
    "reference-transaction",
    "post-index-change",
];
