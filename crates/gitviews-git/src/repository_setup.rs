//! Repository creation in an owned staging directory, published only on success.

use std::{
    cell::RefCell,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

use crate::{RemoteControl, RemoteError, RemoteErrorKind, RemotePhase, RemoteProgress};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositorySetupKind {
    Init,
    Clone,
}

#[derive(Clone)]
pub struct RepositorySetupRequest {
    pub kind: RepositorySetupKind,
    /// An absolute, absent or empty working-tree directory.
    pub destination: PathBuf,
    /// A local path or supported remote address; credentials in URLs are refused.
    pub source: String,
    pub initial_branch: String,
}

#[derive(Clone, Debug)]
pub struct RepositorySetupOutcome {
    pub root: PathBuf,
    pub kind: RepositorySetupKind,
}

fn invalid(message: &str) -> RemoteError {
    RemoteError::new(RemoteErrorKind::InvalidRequest, message)
}

fn cancelled(control: &RemoteControl) -> Result<(), RemoteError> {
    if control.cancel.load(Ordering::Acquire) {
        Err(RemoteError::new(
            RemoteErrorKind::Cancelled,
            "操作已取消，目标目录未创建。",
        ))
    } else {
        Ok(())
    }
}

fn report(control: &RemoteControl, phase: RemotePhase, message: &str) {
    if let Some(callback) = &control.progress {
        callback(RemoteProgress {
            phase,
            message: message.into(),
            ..Default::default()
        });
    }
}

fn validate(request: &RepositorySetupRequest) -> Result<(), RemoteError> {
    if !request.destination.is_absolute()
        || request.destination.file_name().is_none()
        || request
            .destination
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(invalid(
            "请填写新的仓库绝对路径，不可使用磁盘根目录或上级目录跳转。",
        ));
    }
    if request.kind == RepositorySetupKind::Clone {
        let source = request.source.trim();
        if source.starts_with('-')
            || source != request.source
            || crate::remote::validate_settings_url(source).is_err()
        {
            return Err(invalid(
                "克隆地址无效。请使用 HTTPS、SSH 或本地仓库路径；密码、令牌和查询参数请勿写入地址。",
            ));
        }
    } else if request.initial_branch.trim() != request.initial_branch
        || request.initial_branch == "HEAD"
        || !git2::Reference::is_valid_name(&format!("refs/heads/{}", request.initial_branch))
        || request.initial_branch.starts_with('-')
    {
        return Err(invalid("请填写有效的初始分支名称，例如 main。"));
    }
    ensure_empty(&request.destination)
}

fn ensure_empty(path: &Path) -> Result<(), RemoteError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            let mut entries =
                fs::read_dir(path).map_err(|_| invalid("无法读取目标目录，请检查目录权限。"))?;
            if entries.next().is_some() {
                Err(invalid(
                    "目标目录已包含文件。请选择不存在的目录或空目录，现有文件不会被覆盖。",
                ))
            } else {
                Ok(())
            }
        }
        _ => Err(invalid("目标路径不可用。请选择不存在的目录或空目录。")),
    }
}

static NEXT_STAGING: AtomicU64 = AtomicU64::new(0);
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        // This exact directory was allocated with create_dir; no user directory
        // is ever recursively removed, including on authentication retries.
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn staging(request: &RepositorySetupRequest) -> Result<Staging, RemoteError> {
    validate(request)?;
    let parent = request
        .destination
        .parent()
        .ok_or_else(|| invalid("目标路径无效。"))?;
    if !parent.is_dir() {
        return Err(invalid(
            "目标目录的父文件夹不存在，请先创建或选择已有的父文件夹。",
        ));
    }
    for _ in 0..32 {
        let path = parent.join(format!(
            ".gitviews-create-{}-{}",
            std::process::id(),
            NEXT_STAGING.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(Staging(path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(invalid("无法在目标文件夹内创建仓库，请检查写入权限。")),
        }
    }
    Err(invalid("无法分配临时仓库目录，请重试。"))
}

fn publish(
    staging: &Staging,
    request: &RepositorySetupRequest,
    control: &RemoteControl,
) -> Result<RepositorySetupOutcome, RemoteError> {
    cancelled(control)?;
    ensure_empty(&request.destination)?;
    // remove_dir never removes nonempty directories if another process raced us.
    if request.destination.exists() {
        fs::remove_dir(&request.destination)
            .map_err(|_| invalid("目标目录已改变，请选择新的空目录后重试。"))?;
    }
    fs::rename(&staging.0, &request.destination)
        .map_err(|_| invalid("无法完成仓库目录创建，请检查目标目录权限或选择其他路径后重试。"))?;
    let root =
        fs::canonicalize(&request.destination).unwrap_or_else(|_| request.destination.clone());
    report(control, RemotePhase::Complete, "仓库已就绪，正在打开…");
    Ok(RepositorySetupOutcome {
        root,
        kind: request.kind,
    })
}

pub(crate) fn execute(
    request: &RepositorySetupRequest,
    control: &RemoteControl,
) -> Result<RepositorySetupOutcome, RemoteError> {
    cancelled(control)?;
    let staging = staging(request)?;
    report(control, RemotePhase::Preparing, "正在准备仓库目录…");
    match request.kind {
        RepositorySetupKind::Init => {
            let mut options = git2::RepositoryInitOptions::new();
            options
                .initial_head(&request.initial_branch)
                .external_template(false);
            git2::Repository::init_opts(&staging.0, &options)
                .map_err(|_| invalid("无法初始化仓库，请检查目录权限。"))?;
        }
        RepositorySetupKind::Clone => {
            let mut auth = control.auth.clone();
            auth.bind_remote_url(&request.source)
                .map_err(|_| invalid("克隆地址无效或认证目标已改变，请检查地址后重试。"))?;
            let auth = Rc::new(RefCell::new(auth));
            let credential_auth = Rc::clone(&auth);
            let mut callbacks = git2::RemoteCallbacks::new();
            callbacks.credentials(move |url, username, allowed| {
                credential_auth
                    .borrow_mut()
                    .credentials(url, username, allowed)
            });
            callbacks.transfer_progress(|progress| {
                if let Some(callback) = &control.progress {
                    callback(RemoteProgress {
                        phase: RemotePhase::Receiving,
                        received_objects: progress.received_objects(),
                        total_objects: progress.total_objects(),
                        indexed_objects: progress.indexed_objects(),
                        received_bytes: progress.received_bytes(),
                        message: "正在接收仓库对象…".into(),
                    });
                }
                !control.cancel.load(Ordering::Acquire)
            });
            callbacks.sideband_progress(|_| !control.cancel.load(Ordering::Acquire));
            let mut fetch = git2::FetchOptions::new();
            fetch.remote_callbacks(callbacks);
            let mut checkout = git2::build::CheckoutBuilder::new();
            checkout
                .progress(|_, _, _| report(control, RemotePhase::CheckingOut, "正在检出仓库文件…"));
            report(control, RemotePhase::Connecting, "正在连接克隆源…");
            let result = git2::build::RepoBuilder::new()
                .fetch_options(fetch)
                .with_checkout(checkout)
                .clone(&request.source, &staging.0);
            cancelled(control)?;
            if let Err(error) = result {
                let authentication = auth.borrow().challenge();
                if error.code() == git2::ErrorCode::Auth || error.class() == git2::ErrorClass::Ssh {
                    return Err(RemoteError {
                        kind: RemoteErrorKind::AuthRequired,
                        message: "克隆认证失败，请检查用户名、令牌或 SSH 密钥后重试。".into(),
                        authentication,
                    });
                }
                return Err(RemoteError::new(
                    RemoteErrorKind::Transport,
                    "克隆未完成，请检查地址、网络、服务器权限和证书后重试。",
                ));
            }
        }
    }
    publish(&staging, request, control)
}

/// The selected executable retains system Git templates, credential helpers,
/// SSH configuration and post-checkout hooks. No shell or URL-bearing output.
pub(crate) fn execute_system(
    executable: &Path,
    request: &RepositorySetupRequest,
    control: &RemoteControl,
) -> Result<RepositorySetupOutcome, RemoteError> {
    cancelled(control)?;
    let staging = staging(request)?;
    let destination = crate::paths::command_path(&staging.0).into_os_string();
    let source: OsString = if Path::new(&request.source).is_absolute() {
        crate::paths::command_path(Path::new(&request.source)).into_os_string()
    } else {
        request.source.clone().into()
    };
    let args: Vec<OsString> = match request.kind {
        RepositorySetupKind::Init => vec![
            "init".into(),
            format!("--initial-branch={}", request.initial_branch).into(),
            "--".into(),
            destination,
        ],
        RepositorySetupKind::Clone => vec![
            "clone".into(),
            "--progress".into(),
            "--".into(),
            source,
            destination,
        ],
    };
    report(
        control,
        RemotePhase::Connecting,
        "正在使用本地 Git 创建仓库；认证由 Git 凭据管理器或 SSH Agent 处理…",
    );
    let result = match crate::SystemGit::new(executable).run(
        Path::new("."),
        &args,
        None,
        Some(&control.cancel),
    ) {
        Ok(result) => result,
        Err(message) => {
            // A timed out/killed helper can outlive Git. Preserve our staging
            // directory rather than race a helper that might still use it.
            let retained = crate::display_path(&staging.0);
            std::mem::forget(staging);
            return Err(RemoteError::new(
                if control.cancel.load(Ordering::Acquire) {
                    RemoteErrorKind::Cancelled
                } else {
                    RemoteErrorKind::Transport
                },
                format!(
                    "{message} 目标目录未创建；确认 Git 退出后，可清理已保留的临时目录：{retained}"
                ),
            ));
        }
    };
    // Only classify locally. Raw diagnostics can contain URLs or secrets.
    let message = String::from_utf8_lossy(&result.stderr).to_ascii_lowercase();
    let auth_failed = message.contains("authentication")
        || message.contains("permission denied")
        || message.contains("could not read username");
    if !result.status.success() {
        return Err(RemoteError::new(
            if auth_failed {
                RemoteErrorKind::AuthRequired
            } else {
                RemoteErrorKind::Transport
            },
            if auth_failed {
                "本地 Git 认证失败。请先在 Git 凭据管理器或 SSH Agent 中登录，再重试克隆。"
            } else {
                "本地 Git 创建仓库失败。请检查地址、网络、目录权限以及 Git hooks 配置后重试。"
            },
        ));
    }
    publish(&staging, request, control)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let parent =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/repository-setup-tests");
            fs::create_dir_all(&parent).unwrap();
            let path = parent.join(format!(
                "{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_STAGING.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
        fn request(&self, name: &str) -> RepositorySetupRequest {
            RepositorySetupRequest {
                kind: RepositorySetupKind::Init,
                destination: self.0.join(name),
                source: String::new(),
                initial_branch: "main".into(),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn init_opens_on_requested_unborn_branch() {
        let fixture = Fixture::new();
        let result = execute(&fixture.request("新 仓库"), &RemoteControl::default()).unwrap();
        let repo = git2::Repository::open(result.root).unwrap();
        assert_eq!(
            repo.find_reference("HEAD")
                .unwrap()
                .symbolic_target()
                .unwrap(),
            Some("refs/heads/main")
        );
    }
    #[test]
    fn clone_local_repository_preserves_remote_and_files() {
        let fixture = Fixture::new();
        let source = execute(&fixture.request("source"), &RemoteControl::default()).unwrap();
        let repo = git2::Repository::open(&source.root).unwrap();
        fs::write(source.root.join("hello.txt"), "hello").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("hello.txt")).unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.test").unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
            .unwrap();
        let mut request = fixture.request("clone");
        request.kind = RepositorySetupKind::Clone;
        request.source = source.root.to_string_lossy().into_owned();
        let outcome = execute(&request, &RemoteControl::default()).unwrap();
        assert_eq!(
            fs::read_to_string(outcome.root.join("hello.txt")).unwrap(),
            "hello"
        );
        assert_eq!(
            git2::Repository::open(outcome.root)
                .unwrap()
                .find_remote("origin")
                .unwrap()
                .url()
                .unwrap(),
            request.source.as_str()
        );
    }
    #[test]
    fn existing_content_credentials_and_cancel_are_protected() {
        let fixture = Fixture::new();
        let mut request = fixture.request("existing");
        fs::create_dir(&request.destination).unwrap();
        fs::write(request.destination.join("keep.txt"), "keep").unwrap();
        assert_eq!(
            execute(&request, &RemoteControl::default())
                .unwrap_err()
                .kind,
            RemoteErrorKind::InvalidRequest
        );
        assert_eq!(
            fs::read_to_string(request.destination.join("keep.txt")).unwrap(),
            "keep"
        );
        request.destination = fixture.0.join("secrets");
        request.kind = RepositorySetupKind::Clone;
        request.source = "https://user:secret@example.test/repo".into();
        let error = execute(&request, &RemoteControl::default()).unwrap_err();
        assert!(!error.message.contains("secret"));
        assert!(!request.destination.exists());
        request = fixture.request("cancelled");
        let control = RemoteControl::default();
        control.cancel.store(true, Ordering::Release);
        assert_eq!(
            execute(&request, &control).unwrap_err().kind,
            RemoteErrorKind::Cancelled
        );
        assert!(!request.destination.exists());
    }
    #[test]
    fn clone_auth_retry_cannot_change_the_prompt_origin() {
        let fixture = Fixture::new();
        let mut request = fixture.request("bound-origin");
        request.kind = RepositorySetupKind::Clone;
        request.source = "https://changed.example.test/team/repository.git".into();
        let challenge = crate::AuthChallenge {
            protocol: crate::AuthProtocol::Https,
            host: "original.example.test".into(),
            username: Some("user".into()),
            private_key: None,
        };
        let control = RemoteControl {
            auth: crate::RemoteAuth::user_password("user", "secret-token")
                .with_challenge(&challenge),
            ..Default::default()
        };
        let error = execute(&request, &control).unwrap_err();
        assert_eq!(error.kind, RemoteErrorKind::InvalidRequest);
        assert!(!error.message.contains("secret-token"));
        assert!(!request.destination.exists());
        assert!(
            fs::read_dir(&fixture.0).unwrap().next().is_none(),
            "rejected retry leaves no staging repository"
        );
    }
    #[test]
    fn loopback_401_never_sends_credentials_over_http_and_retry_is_bounded() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            time::{Duration, Instant},
        };
        let fixture = Fixture::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut requests = Vec::new();
            while requests.len() < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut request = Vec::new();
                        let mut buffer = [0u8; 1024];
                        while request.len() < 8192 {
                            let count = stream.read(&mut buffer).unwrap_or(0);
                            if count == 0 {
                                break;
                            }
                            request.extend_from_slice(&buffer[..count]);
                            if request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                                break;
                            }
                        }
                        requests.push(String::from_utf8_lossy(&request).to_ascii_lowercase());
                        stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"fixture\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("loopback fixture: {error}"),
                }
            }
            requests
        });
        let mut request = fixture.request("authentication-retry");
        request.kind = RepositorySetupKind::Clone;
        request.source = format!("http://{address}/repository.git");
        for auth in [
            crate::RemoteAuth::default(),
            crate::RemoteAuth::user_password("fixture", "must-not-be-transmitted"),
        ] {
            let error = execute(
                &request,
                &RemoteControl {
                    auth,
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert_eq!(error.kind, RemoteErrorKind::AuthRequired);
            assert!(
                error.authentication.is_none(),
                "plain HTTP must never invite a password prompt"
            );
            assert!(!error.message.contains("must-not-be-transmitted"));
            assert!(!request.destination.exists());
        }
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2, "one network attempt per UI retry");
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("authorization:"))
        );
        assert!(fs::read_dir(&fixture.0).unwrap().next().is_none());
    }
}
