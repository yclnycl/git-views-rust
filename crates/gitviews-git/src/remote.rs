//! Native remote operations with explicit branch identity and conservative writes.

use std::{
    cell::{Cell, RefCell},
    error::Error,
    fmt, fs,
    path::Path,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use git2::{
    ErrorCode, FetchOptions, Oid, PushOptions, Reference, Repository, RepositoryState,
    StatusOptions, build::CheckoutBuilder,
};

mod auth;
pub use auth::{AuthChallenge, AuthProtocol, RemoteAuth, remote_protocol};
pub(crate) use auth::{ssh_url_username, validate_settings_url};

/// The toolbar action, always applied to the selected main repository.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteAction {
    Fetch,
    Pull,
    Push,
}

/// Identity captured before dispatching an operation to a background worker.
#[derive(Clone, Debug)]
pub struct RemoteRequest {
    pub action: RemoteAction,
    /// Short local branch name. Required for pull and push.
    pub expected_branch: Option<String>,
    /// Full commit ID at dispatch time. Required for pull and push.
    pub expected_head: Option<String>,
}

/// Phases allow the UI to explain progress without exposing transport messages.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RemotePhase {
    #[default]
    Preparing,
    Connecting,
    Receiving,
    Negotiating,
    Sending,
    UpdatingReferences,
    /// Checkout has entered its non-cancellable local update section.
    CheckingOut,
    Complete,
}

#[derive(Clone, Debug, Default)]
pub struct RemoteProgress {
    pub phase: RemotePhase,
    pub received_objects: usize,
    pub total_objects: usize,
    pub indexed_objects: usize,
    pub received_bytes: usize,
    pub message: String,
}

/// Cancellation is cooperative at transport callbacks and before local writes.
/// A push already accepted by the server and an ongoing checkout cannot be undone.
#[derive(Clone, Default)]
pub struct RemoteControl {
    pub auth: RemoteAuth,
    pub cancel: Arc<AtomicBool>,
    pub progress: Option<Arc<dyn Fn(RemoteProgress) + Send + Sync>>,
}

impl fmt::Debug for RemoteControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteControl")
            .field("cancelled", &self.cancel.load(Ordering::Relaxed))
            .field("has_progress", &self.progress.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct RemoteOutcome {
    pub action: RemoteAction,
    /// Configured remote name, never a URL containing credentials.
    pub remote: String,
    pub branch: Option<String>,
    pub head_before: Option<String>,
    pub head_after: Option<String>,
    pub summary: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteErrorKind {
    Cancelled,
    InvalidRepository,
    InvalidRequest,
    NoRemote,
    NoUpstream,
    DetachedHead,
    DirtyWorkingTree,
    InProgress,
    NonFastForward,
    ConcurrentChange,
    AuthRequired,
    Rejected,
    Transport,
    Unsupported,
}

/// Safe UI diagnostic. Transport URLs, passwords and server sidebands are omitted.
#[derive(Clone, Debug)]
pub struct RemoteError {
    pub kind: RemoteErrorKind,
    pub message: String,
    pub authentication: Option<AuthChallenge>,
}

impl RemoteError {
    pub fn new(kind: RemoteErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            authentication: None,
        }
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for RemoteError {}

impl RemoteControl {
    fn emit(&self, phase: RemotePhase, message: &str) {
        self.report(RemoteProgress {
            phase,
            message: message.into(),
            ..Default::default()
        });
    }

    fn report(&self, progress: RemoteProgress) {
        if let Some(report) = &self.progress {
            report(progress);
        }
    }

    fn check_cancelled(&self) -> Result<(), RemoteError> {
        if self.cancel.load(Ordering::Acquire) {
            Err(RemoteError::new(RemoteErrorKind::Cancelled, "操作已取消。"))
        } else {
            Ok(())
        }
    }
}

/// Entry point used by Git2Backend; no subprocess or shell is involved.
pub(super) fn execute(
    root: &Path,
    request: &RemoteRequest,
    control: &RemoteControl,
) -> Result<RemoteOutcome, RemoteError> {
    control.check_cancelled()?;
    control.emit(RemotePhase::Preparing, "正在检查仓库与远端配置…");
    let canonical = fs::canonicalize(root).map_err(|_| invalid_repository())?;
    // Unlike overview reads, a write must never discover an ancestor repository.
    let repo = Repository::open(&canonical).map_err(|_| invalid_repository())?;
    if repo
        .workdir()
        .and_then(|path| fs::canonicalize(path).ok())
        .as_ref()
        != Some(&canonical)
    {
        return Err(invalid_repository());
    }
    ensure_expected(&repo, request)?;
    let head_before = repo
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|id| id.to_string());
    let branch = repo
        .head()
        .ok()
        .filter(Reference::is_branch)
        .and_then(|head| head.shorthand().ok().map(str::to_owned));
    let (remote_name, summary) = match request.action {
        RemoteAction::Fetch => {
            let remote = fetch_remote(&repo, branch.as_deref())?;
            // Never honor a custom refspec that could update local branches.
            let refspec = format!("+refs/heads/*:refs/remotes/{remote}/*");
            fetch(&repo, &remote, &refspec, request, control)?;
            (remote, "远端引用已获取。".to_owned())
        }
        RemoteAction::Pull => {
            let identity = required_identity(request)?;
            require_clean(&repo)?;
            let upstream = upstream(&repo, &identity.reference)?;
            let target = format!(
                "refs/remotes/{}/{}",
                upstream.remote,
                upstream.reference.strip_prefix("refs/heads/").unwrap()
            );
            let oid = fetch(
                &repo,
                &upstream.remote,
                &format!("+{}:{target}", upstream.reference),
                request,
                control,
            )?
            .ok_or_else(|| {
                RemoteError::new(RemoteErrorKind::NoUpstream, "远端没有配置的上游分支。")
            })?;
            control.emit(RemotePhase::Negotiating, "正在检查上游提交与快进条件…");
            control.check_cancelled()?;
            ensure_expected(&repo, request)?;
            require_clean(&repo)?;
            let summary = fast_forward(&repo, request, &identity, oid, control)?;
            (upstream.remote, summary)
        }
        RemoteAction::Push => {
            let identity = required_identity(request)?;
            require_idle(&repo)?;
            let upstream = upstream(&repo, &identity.reference)?;
            if upstream.reference != identity.reference {
                return Err(RemoteError::new(
                    RemoteErrorKind::NoUpstream,
                    "当前分支的上游名称不同；为避免推错分支，请先配置同名上游。",
                ));
            }
            push(&repo, request, &identity, &upstream, control)?;
            (upstream.remote, "当前分支已推送至同名上游。".to_owned())
        }
    };
    let head_after = repo
        .head()
        .ok()
        .and_then(|head| head.target())
        .map(|id| id.to_string());
    control.emit(RemotePhase::Complete, &summary);
    Ok(RemoteOutcome {
        action: request.action,
        remote: remote_name,
        branch,
        head_before,
        head_after,
        summary,
    })
}

struct Identity {
    reference: String,
    oid: Oid,
}

fn required_identity(request: &RemoteRequest) -> Result<Identity, RemoteError> {
    let branch = request
        .expected_branch
        .as_deref()
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| {
            RemoteError::new(
                RemoteErrorKind::DetachedHead,
                "请先选择一个已有提交的本地分支。",
            )
        })?;
    let reference = format!("refs/heads/{branch}");
    if reference.contains('\0') || !Reference::is_valid_name(&reference) {
        return Err(RemoteError::new(
            RemoteErrorKind::InvalidRequest,
            "分支名称无效。",
        ));
    }
    let oid = request
        .expected_head
        .as_deref()
        .filter(|value| value.len() == 40)
        .and_then(|value| Oid::from_str(value).ok())
        .ok_or_else(|| {
            RemoteError::new(
                RemoteErrorKind::InvalidRequest,
                "缺少操作开始时的完整提交 ID，请刷新后重试。",
            )
        })?;
    Ok(Identity { reference, oid })
}

fn ensure_expected(repo: &Repository, request: &RemoteRequest) -> Result<(), RemoteError> {
    if request.action != RemoteAction::Fetch {
        required_identity(request)?;
    }
    if request.expected_branch.is_none() && request.expected_head.is_none() {
        return Ok(());
    }
    let head = repo.head().map_err(|_| concurrent_change())?;
    if let Some(branch) = &request.expected_branch {
        let expected = format!("refs/heads/{branch}");
        if !head.is_branch() || head.name_bytes() != expected.as_bytes() {
            return Err(concurrent_change());
        }
    }
    if let Some(oid) = &request.expected_head
        && head.target().map(|id| id.to_string()).as_deref() != Some(oid.as_str())
    {
        return Err(concurrent_change());
    }
    Ok(())
}

struct Upstream {
    remote: String,
    reference: String,
}

fn upstream(repo: &Repository, branch: &str) -> Result<Upstream, RemoteError> {
    let missing = || {
        RemoteError::new(
            RemoteErrorKind::NoUpstream,
            "当前分支没有明确的远端上游，请先配置后重试。",
        )
    };
    let remote = repo.branch_upstream_remote(branch).map_err(|_| missing())?;
    let reference = repo.branch_upstream_merge(branch).map_err(|_| missing())?;
    let remote = remote
        .as_str()
        .ok()
        .filter(|value| !value.is_empty() && *value != ".")
        .ok_or_else(missing)?
        .to_owned();
    let reference = reference
        .as_str()
        .ok()
        .filter(|value| value.starts_with("refs/heads/"))
        .ok_or_else(missing)?
        .to_owned();
    if !Reference::is_valid_name(&reference) || !git2::Remote::is_valid_name(&remote) {
        return Err(missing());
    }
    repo.find_remote(&remote).map_err(|_| missing())?;
    Ok(Upstream { remote, reference })
}

fn fetch_remote(repo: &Repository, branch: Option<&str>) -> Result<String, RemoteError> {
    if let Some(branch) = branch
        && let Ok(remote) = repo.branch_upstream_remote(&format!("refs/heads/{branch}"))
        && let Some(remote) = remote
            .as_str()
            .ok()
            .filter(|value| !value.is_empty() && *value != ".")
    {
        repo.find_remote(remote).map_err(|_| {
            RemoteError::new(RemoteErrorKind::NoRemote, "当前分支配置的上游远端不存在。")
        })?;
        return Ok(remote.into());
    }
    if repo.find_remote("origin").is_ok() {
        return Ok("origin".into());
    }
    let names = repo.remotes().map_err(|error| transport_error(&error))?;
    if names.len() == 1 {
        return names
            .get(0)
            .ok()
            .flatten()
            .map(str::to_owned)
            .ok_or_else(|| {
                RemoteError::new(RemoteErrorKind::NoRemote, "远端名称不是有效的 UTF-8。")
            });
    }
    Err(RemoteError::new(
        RemoteErrorKind::NoRemote,
        "无法确定远端：请配置上游、origin，或只保留一个明确的远端。",
    ))
}

fn require_idle(repo: &Repository) -> Result<(), RemoteError> {
    if repo.state() != RepositoryState::Clean {
        return Err(RemoteError::new(
            RemoteErrorKind::InProgress,
            "仓库正在合并、变基或执行其他未结束的 Git 操作，请先完成或中止。",
        ));
    }
    Ok(())
}

fn require_clean(repo: &Repository) -> Result<(), RemoteError> {
    require_idle(repo)?;
    let mut options = StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false);
    let status = repo
        .statuses(Some(&mut options))
        .map_err(|error| transport_error(&error))?;
    if !status.is_empty()
        || repo
            .index()
            .map_err(|error| transport_error(&error))?
            .has_conflicts()
    {
        return Err(RemoteError::new(
            RemoteErrorKind::DirtyWorkingTree,
            "安全拉取要求干净的暂存区和工作目录；请先处理修改与未跟踪文件。",
        ));
    }
    Ok(())
}

struct CallbackState {
    failure: Rc<RefCell<Option<RemoteError>>>,
    auth: Rc<RefCell<RemoteAuth>>,
    fetched: Rc<Cell<Option<Oid>>>,
    push_started: Rc<Cell<bool>>,
    push_accepted: Rc<Cell<bool>>,
}

impl CallbackState {
    fn finish(
        &self,
        result: Result<(), git2::Error>,
        control: &RemoteControl,
    ) -> Result<(), RemoteError> {
        if let Some(error) = self.failure.borrow_mut().take() {
            return Err(error);
        }
        // A confirmed server ref update is successful even if cancellation was
        // requested during the subsequent local tracking-ref bookkeeping.
        if self.push_accepted.get() {
            return Ok(());
        }
        result.map_err(|error| {
            if error.code() == ErrorCode::NotFastForward {
                transport_error(&error)
            } else if error.code() == ErrorCode::Auth {
                let mut error = RemoteError::new(
                    RemoteErrorKind::AuthRequired,
                    "远端需要认证，或提供的凭据未被接受。",
                );
                error.authentication = self.auth.borrow().challenge();
                error
            } else if self.push_started.get() {
                RemoteError::new(
                    RemoteErrorKind::Transport,
                    "推送已进入发送阶段，但未收到明确结果；请获取远端后确认分支状态。",
                )
            } else if control.cancel.load(Ordering::Acquire) {
                RemoteError::new(RemoteErrorKind::Cancelled, "操作已取消。")
            } else {
                transport_error(&error)
            }
        })
    }
}

fn callbacks<'a>(
    repo: &'a Repository,
    request: &'a RemoteRequest,
    control: &'a RemoteControl,
    remote_name: &str,
    url: &str,
    push: Option<(&'a Identity, &'a Upstream)>,
    tracking: Option<&str>,
) -> Result<(git2::RemoteCallbacks<'a>, CallbackState), RemoteError> {
    let mut auth = crate::git_settings::remote_auth(repo, remote_name, url, &control.auth)
        .map_err(|_| {
            RemoteError::new(
                RemoteErrorKind::InvalidRequest,
                "远端认证配置无效，请检查 Git 配置。",
            )
        })?;
    if auth.bind_remote_url(url).is_err() {
        let mut error = RemoteError::new(
            RemoteErrorKind::AuthRequired,
            "远端认证地址无效或已改变，请重新执行操作。",
        );
        error.authentication = auth.challenge();
        return Err(error);
    }
    let state = CallbackState {
        failure: Rc::new(RefCell::new(None)),
        auth: Rc::new(RefCell::new(auth)),
        fetched: Rc::new(Cell::new(None)),
        push_started: Rc::new(Cell::new(false)),
        push_accepted: Rc::new(Cell::new(false)),
    };
    let mut callbacks = git2::RemoteCallbacks::new();
    let auth = Rc::clone(&state.auth);
    callbacks.credentials(move |url, username, allowed| {
        if control.cancel.load(Ordering::Acquire) {
            return Err(git2::Error::new(
                ErrorCode::User,
                git2::ErrorClass::Callback,
                "cancelled",
            ));
        }
        auth.borrow_mut().credentials(url, username, allowed)
    });
    callbacks.transfer_progress(move |progress| {
        control.report(RemoteProgress {
            phase: RemotePhase::Receiving,
            received_objects: progress.received_objects(),
            total_objects: progress.total_objects(),
            indexed_objects: progress.indexed_objects(),
            received_bytes: progress.received_bytes(),
            message: "正在接收远端对象…".into(),
        });
        !control.cancel.load(Ordering::Acquire)
    });
    let accepted = Rc::clone(&state.push_accepted);
    callbacks.sideband_progress(move |_| accepted.get() || !control.cancel.load(Ordering::Acquire));
    let fetched = Rc::clone(&state.fetched);
    let accepted = Rc::clone(&state.push_accepted);
    let tracking = tracking.map(str::to_owned);
    callbacks.update_tips(move |name, _, target| {
        if tracking.as_deref() == Some(name) {
            fetched.set(Some(target));
        }
        control.emit(RemotePhase::UpdatingReferences, "正在更新远端跟踪引用…");
        accepted.get() || !control.cancel.load(Ordering::Acquire)
    });
    callbacks.push_transfer_progress(move |current, total, bytes| {
        control.report(RemoteProgress {
            phase: RemotePhase::Sending,
            received_objects: current,
            total_objects: total,
            received_bytes: bytes,
            message: "正在发送提交对象…".into(),
            ..Default::default()
        });
    });
    if let Some((identity, upstream)) = push {
        let failure = Rc::clone(&state.failure);
        let started = Rc::clone(&state.push_started);
        callbacks.push_negotiation(move |updates| {
            control.emit(RemotePhase::Negotiating, "正在确认当前分支与远端更新…");
            let checked = control
                .check_cancelled()
                .and_then(|_| ensure_expected(repo, request))
                .and_then(|_| {
                    if updates.len() > 1
                        || updates.iter().any(|update| {
                            update.dst_refname().ok() != Some(upstream.reference.as_str())
                                || update.dst() != identity.oid
                        })
                    {
                        Err(RemoteError::new(
                            RemoteErrorKind::Rejected,
                            "远端协商产生了非预期分支更新，已停止推送。",
                        ))
                    } else {
                        Ok(())
                    }
                });
            if let Err(error) = checked {
                *failure.borrow_mut() = Some(error);
                return Err(git2::Error::new(
                    ErrorCode::User,
                    git2::ErrorClass::Callback,
                    "push guard rejected",
                ));
            }
            // Past this callback, an interrupted response cannot prove that
            // the server did not apply the ref update.
            started.set(true);
            Ok(())
        });
        let failure = Rc::clone(&state.failure);
        let accepted = Rc::clone(&state.push_accepted);
        callbacks.push_update_reference(move |reference, status| {
            if reference != upstream.reference || status.is_some() {
                *failure.borrow_mut() = Some(RemoteError::new(
                    RemoteErrorKind::Rejected,
                    "远端拒绝更新当前分支，请检查分支保护规则和推送权限。",
                ));
            } else {
                accepted.set(true);
            }
            Ok(())
        });
    }
    Ok((callbacks, state))
}

fn fetch(
    repo: &Repository,
    name: &str,
    refspec: &str,
    request: &RemoteRequest,
    control: &RemoteControl,
) -> Result<Option<Oid>, RemoteError> {
    control.check_cancelled()?;
    let mut remote = repo
        .find_remote(name)
        .map_err(|error| transport_error(&error))?;
    let url = remote
        .url()
        .map_err(|error| transport_error(&error))?
        .to_owned();
    let tracking = (request.action == RemoteAction::Pull)
        .then(|| refspec.rsplit_once(':').map(|(_, target)| target))
        .flatten();
    let (callbacks, state) = callbacks(repo, request, control, name, &url, None, tracking)?;
    let mut options = FetchOptions::new();
    options
        .remote_callbacks(callbacks)
        .report_unchanged(true)
        .follow_redirects(git2::RemoteRedirect::None);
    control.emit(RemotePhase::Connecting, "正在连接远端…");
    control.check_cancelled()?;
    let result = remote.fetch(&[refspec], Some(&mut options), Some("gitviews: fetch"));
    state.finish(result, control)?;
    Ok(state.fetched.get())
}

fn fast_forward(
    repo: &Repository,
    request: &RemoteRequest,
    identity: &Identity,
    target: Oid,
    control: &RemoteControl,
) -> Result<String, RemoteError> {
    if identity.oid == target
        || repo
            .graph_descendant_of(identity.oid, target)
            .map_err(|error| transport_error(&error))?
    {
        return Ok("本地分支已包含上游提交，无需快进。".into());
    }
    if !repo
        .graph_descendant_of(target, identity.oid)
        .map_err(|error| transport_error(&error))?
    {
        return Err(RemoteError::new(
            RemoteErrorKind::NonFastForward,
            "本地与上游分支已分叉；仅允许快进拉取，请先手动处理分叉。",
        ));
    }
    let mut transaction = repo
        .transaction()
        .map_err(|error| transport_error(&error))?;
    transaction
        .lock_ref("HEAD")
        .map_err(|_| concurrent_change())?;
    transaction
        .lock_ref(&identity.reference)
        .map_err(|_| concurrent_change())?;
    ensure_expected(repo, request)?;
    require_clean(repo)?;
    let object = repo
        .find_commit(target)
        .and_then(|commit| commit.tree())
        .map_err(|error| transport_error(&error))?;
    let mut check = CheckoutBuilder::new();
    check.overwrite_ignored(false).dry_run();
    repo.checkout_tree(object.as_object(), Some(&mut check))
        .map_err(|error| transport_error(&error))?;
    control.check_cancelled()?;
    let signature = repo
        .signature()
        .or_else(|_| git2::Signature::now("GitViews", "gitviews@localhost"))
        .map_err(|error| transport_error(&error))?;
    transaction
        .set_target(
            &identity.reference,
            target,
            Some(&signature),
            "gitviews: fast-forward pull",
        )
        .map_err(|error| transport_error(&error))?;
    // Cancellation after this point must not leave a half-applied checkout.
    control.emit(
        RemotePhase::CheckingOut,
        "正在安全快进工作目录，完成前请勿关闭…",
    );
    let mut checkout = CheckoutBuilder::new();
    checkout
        .safe()
        .overwrite_ignored(false)
        .progress(|_, current, total| {
            control.report(RemoteProgress {
                phase: RemotePhase::CheckingOut,
                received_objects: current,
                total_objects: total,
                message: "正在更新工作目录…".into(),
                ..Default::default()
            });
        });
    repo.checkout_tree(object.as_object(), Some(&mut checkout))
        .map_err(|error| {
            // libgit2 writes the index after updating working files. Even a
            // lock error can therefore follow a partially applied checkout;
            // retain the old branch ref and never claim this made no changes.
            RemoteError::new(
                transport_error(&error).kind,
                "安全检出失败，工作文件或暂存区可能已部分更新；本次操作尚未快进分支引用。请检查仓库状态后再继续。",
            )
        })?;
    transaction.commit().map_err(|_| {
        RemoteError::new(
            RemoteErrorKind::ConcurrentChange,
            "工作文件已快进，但分支引用更新失败；请检查仓库状态后再继续。",
        )
    })?;
    Ok("当前分支已安全快进至上游提交。".into())
}

fn push(
    repo: &Repository,
    request: &RemoteRequest,
    identity: &Identity,
    upstream: &Upstream,
    control: &RemoteControl,
) -> Result<(), RemoteError> {
    let mut transaction = repo
        .transaction()
        .map_err(|error| transport_error(&error))?;
    transaction
        .lock_ref("HEAD")
        .map_err(|_| concurrent_change())?;
    transaction
        .lock_ref(&identity.reference)
        .map_err(|_| concurrent_change())?;
    ensure_expected(repo, request)?;
    let mut remote = repo
        .find_remote(&upstream.remote)
        .map_err(|error| transport_error(&error))?;
    let url = remote
        .pushurl()
        .map_err(|error| transport_error(&error))?
        .unwrap_or(remote.url().map_err(|error| transport_error(&error))?)
        .to_owned();
    let (callbacks, state) = callbacks(
        repo,
        request,
        control,
        &upstream.remote,
        &url,
        Some((identity, upstream)),
        None,
    )?;
    let mut options = PushOptions::new();
    options
        .remote_callbacks(callbacks)
        .follow_redirects(git2::RemoteRedirect::None);
    // Freeze the source commit, ignore push.default, mirror and configured push refspecs.
    let refspec = format!("{}:{}", identity.oid, upstream.reference);
    control.emit(RemotePhase::Connecting, "正在连接推送远端…");
    control.check_cancelled()?;
    ensure_expected(repo, request)?;
    let result = remote.push(&[refspec], Some(&mut options));
    state.finish(result, control)
}

fn invalid_repository() -> RemoteError {
    RemoteError::new(
        RemoteErrorKind::InvalidRepository,
        "所选路径不是一个可用的仓库根目录，请刷新后重试。",
    )
}

fn concurrent_change() -> RemoteError {
    RemoteError::new(
        RemoteErrorKind::ConcurrentChange,
        "当前分支或 HEAD 在操作期间发生变化，已停止；请刷新后重试。",
    )
}

fn transport_error(error: &git2::Error) -> RemoteError {
    let (kind, message) = match error.code() {
        ErrorCode::NotFastForward => (
            RemoteErrorKind::NonFastForward,
            "远端拒绝非快进推送；请先获取并处理分叉。",
        ),
        ErrorCode::Locked | ErrorCode::Modified => (
            RemoteErrorKind::ConcurrentChange,
            "仓库引用或索引正被其他操作占用，请稍后重试。",
        ),
        ErrorCode::Conflict
        | ErrorCode::Uncommitted
        | ErrorCode::IndexDirty
        | ErrorCode::Unmerged => (
            RemoteErrorKind::DirtyWorkingTree,
            "工作目录存在冲突或修改，已停止安全更新。",
        ),
        ErrorCode::Certificate => (
            RemoteErrorKind::Transport,
            "服务器证书或 SSH 主机验证失败；请检查系统信任与 known_hosts。",
        ),
        _ => (
            RemoteErrorKind::Transport,
            "原生 Git 传输或仓库更新失败，请检查远端配置、网络和权限。",
        ),
    };
    RemoteError::new(kind, message)
}
