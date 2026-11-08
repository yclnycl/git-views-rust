//! Selected installed Git for mutations that must honor hooks, signing and helpers.
//!
//! Repository reads remain native and bounded. Commands never pass through a shell;
//! terminal prompts/editors are disabled, while configured credential helpers and
//! signing programs retain their ordinary graphical interaction.
use std::{
    ffi::OsString,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use crate::*;
use git2::{Repository, RepositoryState};

#[derive(Clone, Debug)]
pub struct SystemGit {
    executable: PathBuf,
}

impl SystemGit {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub(crate) fn run(
        &self,
        root: &Path,
        args: &[OsString],
        input: Option<&[u8]>,
        cancel: Option<&AtomicBool>,
    ) -> Result<CommandResult, String> {
        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err("操作已取消。".into());
        }
        let mut command = Command::new(&self.executable);
        // Repository validation is meaningful only if inherited shell state
        // cannot redirect Git's refs, index, worktree or object writes elsewhere.
        // Authentication, SSH and user Git configuration remain inherited.
        for variable in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_SHALLOW_FILE",
            "GIT_QUARANTINE_PATH",
        ] {
            command.env_remove(variable);
        }
        command
            .current_dir(root)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true")
            .env("GIT_MERGE_AUTOEDIT", "no")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000 | 0x00000200);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|_| "无法启动已选择的本地 Git，请重新检测或选择可执行文件。".to_owned())?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (stdout_sender, stdout_receiver) = std::sync::mpsc::channel();
        let (stderr_sender, stderr_receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = stdout_sender.send(drain_bounded(stdout));
        });
        std::thread::spawn(move || {
            let _ = stderr_sender.send(drain_bounded(stderr));
        });
        // A large patch must not block cancellation while the child consumes stdin.
        if let Some(input) = input {
            let mut stdin = child.stdin.take().expect("piped stdin");
            let bytes = input.to_vec();
            std::thread::spawn(move || {
                let _ = stdin.write_all(&bytes);
            });
        }
        let started = Instant::now();
        let status = loop {
            let cancelled = cancel.is_some_and(|flag| flag.load(Ordering::Acquire));
            if cancelled || started.elapsed() > Duration::from_secs(300) {
                terminate_tree(&mut child)?;
                return Err(if cancelled { "操作已取消；Git 可能已完成部分写入，请刷新仓库状态。" } else { "本地 Git 操作超过 5 分钟；已停止进程，请检查 hooks、认证或签名程序，并刷新仓库状态。" }.into());
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(_) => {
                    terminate_tree(&mut child)?;
                    return Err("无法读取本地 Git 执行状态；请刷新仓库状态。".into());
                }
            }
        };
        // A hook may launch a background helper that inherits these pipes. Such
        // helpers must not make a completed Git command hang the worker forever.
        let _stdout = stdout_receiver
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| {
                "Git 已退出，但后台 hook／helper 仍占用输出；请检查本地进程并刷新仓库。".to_owned()
            })?;
        let stderr = stderr_receiver
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| {
                "Git 已退出，但后台 hook／helper 仍占用输出；请检查本地进程并刷新仓库。".to_owned()
            })?;
        Ok(CommandResult { status, stderr })
    }

    fn checked(
        &self,
        root: &Path,
        args: &[OsString],
        input: Option<&[u8]>,
    ) -> Result<CommandResult, String> {
        let result = self.run(root, args, input, None)?;
        if result.status.success() {
            Ok(result)
        } else {
            Err(local_error(&result))
        }
    }
}

pub(crate) struct CommandResult {
    pub(crate) status: ExitStatus,
    pub(crate) stderr: Vec<u8>,
}
fn terminate_tree(child: &mut std::process::Child) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // PID belongs to the process just spawned by this runner. Do not target
        // image names, unrelated Git processes, or the user's terminal session.
        let stopped = Command::new("taskkill.exe")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !stopped.is_ok_and(|status| status.success())
            && child.try_wait().ok().flatten().is_none()
        {
            // Keep the caller's write gate occupied until this process actually
            // exits if Windows cannot stop its tree; never race a second write.
            let _ = child.wait();
            return Err(
                "无法安全停止本地 Git 进程树；已等待 Git 退出，请检查 hooks／helpers 并刷新仓库。"
                    .into(),
            );
        }
    }
    #[cfg(unix)]
    {
        let stopped = Command::new("kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !stopped.is_ok_and(|status| status.success()) {
            let _ = child.kill();
        }
    }
    let _ = child.wait();
    Ok(())
}
fn drain_bounded(mut reader: impl Read) -> Vec<u8> {
    let mut result = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(length) => {
                let keep = length.min((64usize * 1024).saturating_sub(result.len()));
                result.extend_from_slice(&chunk[..keep]);
            }
        }
    }
    result
}
fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}
fn local_error(result: &CommandResult) -> String {
    // Hook/signature failures need actionable local diagnostics. Strip control
    // characters and limit the display; remote callers use a separate safe error.
    let diagnostic: String = String::from_utf8_lossy(&result.stderr)
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\n' || *ch == '\t')
        .take(2000)
        .collect();
    format!(
        "本地 Git 执行失败（退出码 {}）：{}",
        result
            .status
            .code()
            .map_or("未知".into(), |code| code.to_string()),
        diagnostic.trim()
    )
}
fn open(root: &Path) -> Result<Repository, String> {
    let canonical = fs::canonicalize(root).map_err(|_| "无效的仓库目录。".to_owned())?;
    let repository = Repository::open(&canonical).map_err(|_| "无效的仓库目录。".to_owned())?;
    if repository.is_bare()
        || repository
            .workdir()
            .and_then(|path| fs::canonicalize(path).ok())
            .as_ref()
            != Some(&canonical)
    {
        return Err("写入操作必须使用工作区根目录。".into());
    }
    Ok(repository)
}
fn head(repository: &Repository) -> Option<String> {
    repository
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|id| id.to_string())
}
fn check_head(repository: &Repository, expected: Option<&str>) -> Result<(), String> {
    if head(repository).as_deref() != expected {
        Err("HEAD 已变化，请刷新仓库后重试。".into())
    } else {
        Ok(())
    }
}
fn ready(repository: &Repository) -> Result<(), String> {
    if repository.state() == RepositoryState::Clean {
        Ok(())
    } else {
        Err("仓库正在合并、变基或执行其他操作，请先完成或中止。".into())
    }
}
fn has_changes(repository: &Repository) -> Result<bool, git2::Error> {
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    Ok(!repository.statuses(Some(&mut options))?.is_empty())
}
fn validate_name(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.starts_with('-')
        || value.trim() != value
        || value.contains(['\0', '\n', '\r'])
    {
        Err("Git 参数无效。".into())
    } else {
        Ok(())
    }
}
fn validate_paths(paths: &[String]) -> Result<(), String> {
    if paths.is_empty() || paths.len() > 100_000 {
        return Err("请选择 1 至 100000 个文件。".into());
    }
    for path in paths {
        if path.is_empty()
            || path.len() > 32 * 1024
            || path.contains(['\0', '\\', ':', '\r', '\n'])
            || Path::new(path)
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            || path
                .split('/')
                .any(|part| part.eq_ignore_ascii_case(".git"))
        {
            return Err("文件路径必须位于当前工作区中。".into());
        }
    }
    Ok(())
}

impl GitBackend for SystemGit {
    fn setup_repository(
        &self,
        request: &RepositorySetupRequest,
        control: &RemoteControl,
    ) -> Result<RepositorySetupOutcome, RemoteError> {
        crate::repository_setup::execute_system(&self.executable, request, control)
    }
    fn inspect(&self, path: &Path) -> Result<RepositorySnapshot, GitError> {
        Git2Backend.inspect(path)
    }
    fn write(&self, root: &Path, request: &WriteRequest) -> Result<WriteOutcome, WriteError> {
        let execute = || -> Result<WriteOutcome, String> {
            let repository = open(root)?;
            check_head(&repository, request.expected_head.as_deref())?;
            ready(&repository)?;
            let mut input = None;
            let command = match &request.action {
                WriteAction::StagePaths { paths } => {
                    validate_paths(paths)?;
                    let mut command = args(&["--literal-pathspecs", "add", "--"]);
                    command.extend(paths.iter().map(OsString::from));
                    command
                }
                WriteAction::StageAll => args(&["add", "--all", "--", "."]),
                WriteAction::UnstagePaths { .. }
                | WriteAction::UnstageAll
                | WriteAction::StageHunk { .. }
                | WriteAction::UnstageHunk { .. } => {
                    // Preserve rename-pair unstaging, unborn-index behavior and
                    // patch token validation with the native index transaction.
                    let outcome = crate::write_ops::execute(root, request)
                        .map_err(|error| error.to_string())?;
                    // Publish through Git's own index writer as well, so the
                    // configured post-index-change hook observes this edit.
                    self.checked(root, &args(&["update-index", "--force-write-index"]), None)?;
                    return Ok(outcome);
                }
                WriteAction::Commit {
                    summary,
                    description,
                } => {
                    if summary.trim().is_empty()
                        || summary.contains(['\0', '\n', '\r'])
                        || description.contains('\0')
                        || summary.len().saturating_add(description.len()) > 1024 * 1024
                    {
                        return Err("请填写单行提交摘要，且消息不能包含 NUL 或超过 1 MiB。".into());
                    }
                    input = Some(if description.trim().is_empty() {
                        summary.trim().to_owned()
                    } else {
                        format!("{}\n\n{}", summary.trim(), description.trim())
                    });
                    args(&["commit", "--file=-"])
                }
            };
            self.checked(root, &command, input.as_deref().map(str::as_bytes))?;
            let commit_id = if matches!(request.action, WriteAction::Commit { .. }) {
                head(&open(root)?)
            } else {
                None
            };
            Ok(WriteOutcome {
                summary: if let Some(id) = &commit_id {
                    format!("已提交 {}（本地 Git）", &id[..8])
                } else {
                    "暂存区已更新（本地 Git）".into()
                },
                commit_id,
            })
        };
        execute().map_err(|message| WriteError::new(WriteErrorKind::WriteFailed, message))
    }
    fn checkout(
        &self,
        root: &Path,
        request: &CheckoutRequest,
    ) -> Result<CheckoutOutcome, CheckoutError> {
        let execute = || -> Result<CheckoutOutcome, String> {
            let repository = open(root)?;
            check_head(&repository, request.expected_head.as_deref())?;
            ready(&repository)?;
            let actual = repository
                .head()
                .ok()
                .filter(|head| head.is_branch())
                .and_then(|head| head.shorthand().ok().map(str::to_owned));
            if actual != request.expected_branch {
                return Err("当前分支已变化，请刷新后重试。".into());
            }
            validate_name(&request.branch)?;
            let branch = request
                .branch
                .strip_prefix(if request.remote {
                    "refs/remotes/"
                } else {
                    "refs/heads/"
                })
                .unwrap_or(&request.branch);
            let (command, local, created, upstream) = if request.remote {
                let (_, local) = branch.split_once('/').ok_or("远端分支名称无效。")?;
                validate_name(local)?;
                (
                    args(&[
                        "switch",
                        "--no-overwrite-ignore",
                        "--track",
                        "-c",
                        local,
                        branch,
                    ]),
                    local.to_owned(),
                    true,
                    Some(branch.to_owned()),
                )
            } else {
                (
                    args(&["switch", "--no-overwrite-ignore", "--", branch]),
                    branch.to_owned(),
                    false,
                    None,
                )
            };
            self.checked(root, &command, None)?;
            Ok(CheckoutOutcome {
                branch: local,
                head: head(&open(root)?).ok_or("切换后没有可读取的 HEAD。")?,
                created,
                upstream,
                summary: "已切换分支（本地 Git hooks 已按配置执行）".into(),
            })
        };
        execute().map_err(|message| CheckoutError::new(CheckoutErrorKind::CheckoutFailed, message))
    }
    fn remote(
        &self,
        root: &Path,
        request: &RemoteRequest,
        control: &RemoteControl,
    ) -> Result<RemoteOutcome, RemoteError> {
        let execute = || -> Result<RemoteOutcome, RemoteError> {
            let fail = |message| RemoteError::new(RemoteErrorKind::InvalidRequest, message);
            let repository = open(root).map_err(fail)?;
            if request.action != RemoteAction::Fetch || request.expected_head.is_some() {
                check_head(&repository, request.expected_head.as_deref()).map_err(fail)?;
            }
            ready(&repository).map_err(fail)?;
            let branch = repository
                .head()
                .ok()
                .filter(|head| head.is_branch())
                .and_then(|head| head.shorthand().ok().map(str::to_owned));
            if request.action != RemoteAction::Fetch
                && (branch.is_none()
                    || branch != request.expected_branch
                    || request.expected_head.is_none())
            {
                return Err(RemoteError::new(
                    RemoteErrorKind::ConcurrentChange,
                    "当前分支已变化或尚未提交，请刷新后重试。",
                ));
            }
            let config = repository
                .config()
                .map_err(|_| fail("无法读取远端配置。".into()))?;
            let remote = branch
                .as_ref()
                .and_then(|branch| config.get_string(&format!("branch.{branch}.remote")).ok())
                .or_else(|| {
                    repository
                        .find_remote("origin")
                        .ok()
                        .map(|_| "origin".to_owned())
                })
                .or_else(|| {
                    repository.remotes().ok().and_then(|names| {
                        if names.len() == 1 {
                            names.get(0).ok().flatten().map(str::to_owned)
                        } else {
                            None
                        }
                    })
                })
                .ok_or_else(|| RemoteError::new(RemoteErrorKind::NoRemote, "尚未配置远端。"))?;
            validate_name(&remote).map_err(fail)?;
            if remote == "." {
                return Err(fail("此操作需要命名远端。".into()));
            }
            let command = match request.action {
                RemoteAction::Fetch => args(&[
                    "fetch",
                    "--",
                    &remote,
                    &format!("+refs/heads/*:refs/remotes/{remote}/*"),
                ]),
                RemoteAction::Pull | RemoteAction::Push => {
                    let branch = branch.as_deref().unwrap();
                    let upstream = config
                        .get_string(&format!("branch.{branch}.merge"))
                        .map_err(|_| {
                            RemoteError::new(
                                RemoteErrorKind::NoUpstream,
                                "请先配置当前分支的上游。",
                            )
                        })?;
                    if !upstream.starts_with("refs/heads/") {
                        return Err(fail("上游必须是远端分支。".into()));
                    }
                    if request.action == RemoteAction::Pull {
                        if has_changes(&repository)
                            .map_err(|_| fail("无法读取工作区状态。".into()))?
                        {
                            return Err(RemoteError::new(
                                RemoteErrorKind::DirtyWorkingTree,
                                "请先提交或储藏改动后拉取。",
                            ));
                        }
                        args(&[
                            "fetch",
                            "--",
                            &remote,
                            &format!(
                                "+{upstream}:refs/remotes/{remote}/{}",
                                upstream.strip_prefix("refs/heads/").unwrap()
                            ),
                        ])
                    } else {
                        if upstream != format!("refs/heads/{branch}") {
                            return Err(RemoteError::new(
                                RemoteErrorKind::NoUpstream,
                                "为避免推错分支，请配置同名上游。",
                            ));
                        }
                        args(&[
                            "push",
                            "--",
                            &remote,
                            &format!("refs/heads/{branch}:{upstream}"),
                        ])
                    }
                }
            };
            if let Some(report) = &control.progress {
                report(RemoteProgress {
                    phase: RemotePhase::Connecting,
                    message: "正在使用本地 Git；认证由已有 credential helper／SSH 配置处理…".into(),
                    ..Default::default()
                });
            }
            let before = head(&repository);
            let result = self
                .run(root, &command, None, Some(&control.cancel))
                .map_err(|message| {
                    RemoteError::new(
                        if control.cancel.load(Ordering::Acquire) {
                            RemoteErrorKind::Cancelled
                        } else {
                            RemoteErrorKind::Transport
                        },
                        message,
                    )
                })?;
            if !result.status.success() {
                let diagnostic = String::from_utf8_lossy(&result.stderr).to_ascii_lowercase();
                let auth = [
                    "authentication failed",
                    "could not read username",
                    "terminal prompts disabled",
                    "permission denied",
                    "credential",
                ]
                .iter()
                .any(|needle| diagnostic.contains(needle));
                return Err(RemoteError::new(
                    if auth {
                        RemoteErrorKind::AuthRequired
                    } else {
                        RemoteErrorKind::Rejected
                    },
                    if auth {
                        "本地 Git 认证未完成，请配置系统 Git 的 credential helper／SSH 密钥后重试。"
                    } else {
                        "本地 Git 拒绝远端操作。请检查 hooks、签名、上游及远端权限；详细诊断可在本地 Git 中查看。"
                    },
                ));
            }
            if request.action == RemoteAction::Pull {
                let repository = open(root).map_err(fail)?;
                check_head(&repository, request.expected_head.as_deref()).map_err(fail)?;
                ready(&repository).map_err(fail)?;
                let current_branch = repository
                    .head()
                    .ok()
                    .filter(|head| head.is_branch())
                    .and_then(|head| head.shorthand().ok().map(str::to_owned));
                if current_branch != request.expected_branch {
                    return Err(RemoteError::new(
                        RemoteErrorKind::ConcurrentChange,
                        "获取期间分支已变化，请刷新后重试。",
                    ));
                }
                if has_changes(&repository).map_err(|_| fail("无法读取工作区状态。".into()))?
                {
                    return Err(RemoteError::new(
                        RemoteErrorKind::DirtyWorkingTree,
                        "获取期间工作区已变化，请先提交或储藏改动。",
                    ));
                }
                let upstream = config
                    .get_string(&format!("branch.{}.merge", branch.as_deref().unwrap()))
                    .map_err(|_| {
                        RemoteError::new(RemoteErrorKind::NoUpstream, "上游配置已变化。")
                    })?;
                let target = format!(
                    "refs/remotes/{remote}/{}",
                    upstream
                        .strip_prefix("refs/heads/")
                        .ok_or_else(|| fail("上游配置无效。".into()))?
                );
                let result = self
                    .run(
                        root,
                        &args(&[
                            "merge",
                            "--ff-only",
                            "--no-overwrite-ignore",
                            "--no-autostash",
                            "--no-edit",
                            "--",
                            &target,
                        ]),
                        None,
                        Some(&control.cancel),
                    )
                    .map_err(|message| {
                        RemoteError::new(
                            if control.cancel.load(Ordering::Acquire) {
                                RemoteErrorKind::Cancelled
                            } else {
                                RemoteErrorKind::Transport
                            },
                            message,
                        )
                    })?;
                if !result.status.success() {
                    return Err(RemoteError::new(
                        RemoteErrorKind::Rejected,
                        "本地 Git 无法快进合并；请检查分支分叉、工作文件和 post-merge hook，并刷新仓库状态。",
                    ));
                }
            }
            let summary = match request.action {
                RemoteAction::Fetch => "远端引用已获取（本地 Git）",
                RemoteAction::Pull => "已按快进方式拉取（本地 Git）",
                RemoteAction::Push => "已推送到同名上游（本地 Git）",
            }
            .to_owned();
            if let Some(report) = &control.progress {
                report(RemoteProgress {
                    phase: RemotePhase::Complete,
                    message: summary.clone(),
                    ..Default::default()
                });
            }
            Ok(RemoteOutcome {
                action: request.action,
                remote,
                branch,
                head_before: before,
                head_after: head(&open(root).map_err(fail)?),
                summary,
            })
        };
        execute()
    }
    fn stashes(&self, root: &Path) -> Result<Vec<StashEntry>, StashError> {
        Git2Backend.stashes(root)
    }
    fn stash(&self, root: &Path, request: &StashRequest) -> Result<StashOutcome, StashError> {
        let execute = || -> Result<StashOutcome, String> {
            let repository = open(root)?;
            check_head(&repository, request.expected_head.as_deref())?;
            ready(&repository)?;
            let command = match &request.action {
                StashAction::Save {
                    message,
                    include_untracked,
                    include_ignored,
                } => {
                    if message.contains('\0') {
                        return Err("储藏消息无效。".into());
                    }
                    let mut command = args(&["stash", "push"]);
                    if *include_ignored {
                        command.push("--all".into());
                    } else if *include_untracked {
                        command.push("--include-untracked".into());
                    }
                    command.push("--message".into());
                    command.push(message.into());
                    command
                }
                StashAction::Apply {
                    id,
                    reinstate_index,
                }
                | StashAction::Pop {
                    id,
                    reinstate_index,
                } => {
                    let entries = Git2Backend
                        .stashes(root)
                        .map_err(|error| error.to_string())?;
                    let entry = entries
                        .iter()
                        .find(|entry| &entry.id == id)
                        .ok_or("储藏已变化，请刷新后重试。")?;
                    let mut command = args(&[
                        "stash",
                        if matches!(request.action, StashAction::Pop { .. }) {
                            "pop"
                        } else {
                            "apply"
                        },
                    ]);
                    if *reinstate_index {
                        command.push("--index".into());
                    }
                    command.push(format!("stash@{{{}}}", entry.index).into());
                    command
                }
                StashAction::Drop { id } => {
                    let entries = Git2Backend
                        .stashes(root)
                        .map_err(|error| error.to_string())?;
                    let entry = entries
                        .iter()
                        .find(|entry| &entry.id == id)
                        .ok_or("储藏已变化，请刷新后重试。")?;
                    args(&["stash", "drop", &format!("stash@{{{}}}", entry.index)])
                }
            };
            self.checked(root, &command, None)?;
            Ok(StashOutcome {
                summary: "储藏操作已完成（本地 Git）".into(),
            })
        };
        execute().map_err(|message| StashError::new(StashErrorKind::Failed, message))
    }
    fn conflict_state(&self, root: &Path) -> Result<ConflictState, ConflictError> {
        let mut state = Git2Backend.conflict_state(root)?;
        if let Ok(repository) = open(root) {
            match repository.state() {
                RepositoryState::Rebase
                | RepositoryState::RebaseInteractive
                | RepositoryState::RebaseMerge => {
                    state.kind = ConflictOperation::Rebase;
                    state.label = "正在变基（本地 Git）".into();
                    state.can_abort = true;
                    state.can_continue = state.files.is_empty();
                }
                RepositoryState::Merge => {
                    state.can_abort = true;
                    state.can_continue = state.files.is_empty();
                }
                _ => {}
            }
        }
        Ok(state)
    }
    fn conflict_versions(
        &self,
        root: &Path,
        path: &str,
    ) -> Result<ConflictVersions, ConflictError> {
        Git2Backend.conflict_versions(root, path)
    }
    fn conflict(
        &self,
        root: &Path,
        request: &ConflictRequest,
    ) -> Result<ConflictOutcome, ConflictError> {
        // Resolving one file uses the existing version token and bounded native
        // index transaction. Operation lifecycle/commit creation belongs to Git.
        if matches!(request.action, ConflictAction::Resolve { .. }) {
            let outcome = crate::conflicts::execute(root, request)?;
            self.checked(root, &args(&["update-index", "--force-write-index"]), None)
                .map_err(|message| ConflictError::new(ConflictErrorKind::Failed, message))?;
            return Ok(outcome);
        }
        let execute = || -> Result<ConflictOutcome, String> {
            let repository = open(root)?;
            check_head(&repository, request.expected_head.as_deref())?;
            let command = match &request.action {
                ConflictAction::Merge { target } => {
                    ready(&repository)?;
                    validate_name(target)?;
                    if has_changes(&repository).map_err(|_| "无法读取工作区状态。")? {
                        return Err("请先提交或储藏工作区改动。".into());
                    }
                    args(&[
                        "merge",
                        "--no-overwrite-ignore",
                        "--no-autostash",
                        "--no-edit",
                        "--",
                        target,
                    ])
                }
                ConflictAction::Rebase { onto } => {
                    ready(&repository)?;
                    validate_name(onto)?;
                    if has_changes(&repository).map_err(|_| "无法读取工作区状态。")? {
                        return Err("请先提交或储藏工作区改动。".into());
                    }
                    args(&["rebase", "--no-autostash", "--", onto])
                }
                ConflictAction::Continue => match repository.state() {
                    RepositoryState::Merge => args(&["commit", "--no-edit"]),
                    RepositoryState::Rebase
                    | RepositoryState::RebaseInteractive
                    | RepositoryState::RebaseMerge => args(&["rebase", "--continue"]),
                    _ => return Err("没有可继续的 Git 合并或变基。".into()),
                },
                ConflictAction::Abort => match repository.state() {
                    RepositoryState::Merge => args(&["merge", "--abort"]),
                    RepositoryState::Rebase
                    | RepositoryState::RebaseInteractive
                    | RepositoryState::RebaseMerge => args(&["rebase", "--abort"]),
                    _ => return Err("没有可中止的 Git 合并或变基。".into()),
                },
                ConflictAction::Resolve { .. } => unreachable!(),
            };
            self.checked(root, &command, None)?;
            Ok(ConflictOutcome {
                summary: "操作已完成（本地 Git hooks／签名按配置执行）".into(),
            })
        };
        execute().map_err(|message| ConflictError::new(ConflictErrorKind::Failed, message))
    }
    fn manage_branch(
        &self,
        root: &Path,
        request: &BranchRequest,
    ) -> Result<BranchOutcome, BranchError> {
        let execute = || -> Result<BranchOutcome, String> {
            let repository = open(root)?;
            check_head(&repository, request.expected_head.as_deref())?;
            ready(&repository)?;
            let recovery_reference = if let BranchAction::Delete { name, .. }
            | BranchAction::Rename { old: name, .. } = &request.action
            {
                validate_name(name)?;
                let branch = repository
                    .find_branch(name, git2::BranchType::Local)
                    .map_err(|_| "分支不存在。")?;
                let oid = branch.get().target().ok_or("分支没有有效提交。")?;
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| "系统时钟无效。")?
                    .as_nanos();
                let reference = format!(
                    "refs/gitviews/deleted-branches/{stamp}-{}",
                    std::process::id()
                );
                self.checked(
                    root,
                    &args(&[
                        "update-ref",
                        &reference,
                        &oid.to_string(),
                        "0000000000000000000000000000000000000000",
                    ]),
                    None,
                )?;
                Some(reference)
            } else {
                None
            };
            let command = match &request.action {
                BranchAction::Create { name, start_point } => {
                    validate_name(name)?;
                    let start = start_point.as_deref().unwrap_or("HEAD");
                    validate_name(start)?;
                    args(&["branch", "--", name, start])
                }
                BranchAction::Rename { old, new } => {
                    validate_name(old)?;
                    validate_name(new)?;
                    args(&["branch", "--move", "--", old, new])
                }
                BranchAction::Delete { name, force } => {
                    validate_name(name)?;
                    args(&["branch", if *force { "-D" } else { "--delete" }, "--", name])
                }
                BranchAction::SetUpstream { name, upstream } => {
                    validate_name(name)?;
                    if let Some(upstream) = upstream {
                        validate_name(upstream)?;
                        args(&[
                            "branch",
                            &format!("--set-upstream-to={upstream}"),
                            "--",
                            name,
                        ])
                    } else {
                        args(&["branch", "--unset-upstream", "--", name])
                    }
                }
            };
            self.checked(root, &command, None)?;
            Ok(BranchOutcome {
                summary: "分支配置已更新（本地 Git）".into(),
                recovery_reference,
            })
        };
        execute().map_err(|message| BranchError::new(BranchErrorKind::Failed, message))
    }
    fn history_tools(
        &self,
        root: &Path,
        request: &HistoryToolsRequest,
        cancel: &AtomicBool,
    ) -> Result<HistoryToolsResult, HistoryToolsError> {
        if let HistoryToolsRequest::Restore {
            name,
            commit_id,
            expected_head,
        } = request
        {
            if cancel.load(Ordering::Acquire) {
                return Err(HistoryToolsError::new(
                    HistoryToolsErrorKind::Cancelled,
                    "操作已取消。",
                ));
            }
            if commit_id.len() != 40 || git2::Oid::from_str(commit_id).is_err() {
                return Err(HistoryToolsError::new(
                    HistoryToolsErrorKind::InvalidRequest,
                    "恢复需要完整提交 ID。",
                ));
            }
            let result = self
                .manage_branch(
                    root,
                    &BranchRequest {
                        expected_head: expected_head.clone(),
                        action: BranchAction::Create {
                            name: name.clone(),
                            start_point: Some(commit_id.clone()),
                        },
                    },
                )
                .map_err(|error| {
                    HistoryToolsError::new(HistoryToolsErrorKind::InvalidRequest, error.to_string())
                })?;
            Ok(HistoryToolsResult::Restored {
                summary: result.summary,
            })
        } else {
            Git2Backend.history_tools(root, request, cancel)
        }
    }
    fn worktree(
        &self,
        root: &Path,
        request: &WorktreeRequest,
    ) -> Result<WorktreeOutcome, WorktreeError> {
        // Targeted repair/pruning keep the native ownership checks; neither
        // operation changes commits, refs, or checked-out files.
        if matches!(
            request.action,
            WorktreeAction::Prune { .. } | WorktreeAction::Repair { .. }
        ) {
            return crate::worktrees::execute(root, request);
        }
        let execute = || -> Result<WorktreeOutcome, String> {
            let repository = open(root)?;
            check_head(&repository, request.expected_commit.as_deref())?;
            let snapshot = Git2Backend
                .inspect_metadata(root)
                .map_err(|error| error.to_string())?;
            if snapshot.head != request.expected_head {
                return Err("当前分支已变化，请刷新后重试。".into());
            }
            ready(&repository)?;
            let (command, path, previous_path) = match &request.action {
                WorktreeAction::Create {
                    path,
                    branch,
                    create_branch,
                } => {
                    validate_name(branch)?;
                    let mut command = args(&["worktree", "add"]);
                    if *create_branch {
                        command.push("-b".into());
                        command.push(branch.into());
                    }
                    command.push("--".into());
                    command.push(crate::paths::command_path(path).into_os_string());
                    if !*create_branch {
                        command.push(branch.into());
                    }
                    (command, Some(path.clone()), None)
                }
                WorktreeAction::Remove { path, force } => {
                    let mut command = args(&["worktree", "remove"]);
                    if *force {
                        command.push("--force".into());
                    }
                    command.push("--".into());
                    command.push(crate::paths::command_path(path).into_os_string());
                    (command, Some(path.clone()), None)
                }
                WorktreeAction::Move { path, destination } => {
                    let mut command = args(&["worktree", "move", "--"]);
                    command.push(crate::paths::command_path(path).into_os_string());
                    command.push(crate::paths::command_path(destination).into_os_string());
                    (command, Some(destination.clone()), Some(path.clone()))
                }
                WorktreeAction::Lock { path, reason } => {
                    if reason.contains('\0') {
                        return Err("锁定原因无效。".into());
                    }
                    let mut command = args(&["worktree", "lock", "--reason", reason, "--"]);
                    command.push(crate::paths::command_path(path).into_os_string());
                    (command, Some(path.clone()), None)
                }
                WorktreeAction::Unlock { path } => {
                    let mut command = args(&["worktree", "unlock", "--"]);
                    command.push(crate::paths::command_path(path).into_os_string());
                    (command, Some(path.clone()), None)
                }
                WorktreeAction::Prune { .. } | WorktreeAction::Repair { .. } => unreachable!(),
            };
            self.checked(root, &command, None)?;
            Ok(WorktreeOutcome {
                message: "工作树操作已完成（本地 Git）".into(),
                previous_path,
                path,
            })
        };
        execute().map_err(|message| WorktreeError::new(WorktreeErrorKind::Git, message))
    }
}
