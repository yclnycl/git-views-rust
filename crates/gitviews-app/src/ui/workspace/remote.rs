use super::presentation::{find_name_mut, set_named_text};
use super::remote_dialog::RemoteDialogPrompt;
use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone)]
pub(super) struct RemoteTarget {
    root: PathBuf,
    name: String,
    request: RemoteRequest,
    repository_id: u64,
    scope_id: u64,
    authentication: Option<AuthChallenge>,
}

pub(super) struct RemoteJob {
    target: RemoteTarget,
    id: u64,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<RemoteProgress>>,
}

impl Drop for RemoteJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

pub(super) struct RemoteFeedback {
    root: PathBuf,
    text: String,
}

fn action_label(action: RemoteAction) -> &'static str {
    match action {
        RemoteAction::Fetch => "获取",
        RemoteAction::Pull => "拉取",
        RemoteAction::Push => "推送",
    }
}

fn progress_text(progress: &RemoteProgress) -> String {
    if progress.total_objects > 0 {
        format!(
            "{} · {}/{} 个对象 · {} KiB",
            progress.message,
            progress.received_objects,
            progress.total_objects,
            progress.received_bytes / 1024
        )
    } else if progress.message.is_empty() {
        "正在准备远端操作…".into()
    } else {
        progress.message.clone()
    }
}

impl Workspace {
    pub(super) fn remote_blocks_navigation(&mut self, cx: &mut Context<Self>) -> bool {
        if self.mutation_refresh {
            self.notice = Some("正在刷新 Git 状态，请稍候。".into());
            cx.notify();
            return true;
        }
        if self.remote_job.is_some()
            || self.remote_pending.is_some()
            || self.mutation_task.is_some()
            || self.worktree_busy
            || self.modal_open(cx)
        {
            self.notice = Some("请先完成或取消当前 Git 操作，再切换仓库或分支。".into());
            cx.notify();
            true
        } else {
            false
        }
    }

    pub(super) fn start_remote(&mut self, action: RemoteAction, cx: &mut Context<Self>) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        if self.remote_job.is_some()
            || self.remote_pending.is_some()
            || self.mutation_task.is_some()
            || self.worktree_busy
            || self.modal_open(cx)
        {
            return;
        }
        if self.preview || self.loading || self.submodule_loading || self.mutation_refresh {
            self.notice = Some("请先打开并等待仓库读取完成。".into());
            return;
        }
        let Some(repository) = self.changes_repository() else {
            self.notice = Some("请先打开一个本地仓库。".into());
            return;
        };
        let branch = match &repository.head {
            Head::Branch(name) => Some(name.clone()),
            _ => None,
        };
        let head = self.active_head_id();
        let target = RemoteTarget {
            root: repository.root.clone(),
            name: repository.name.clone(),
            request: RemoteRequest {
                action,
                expected_branch: branch,
                expected_head: head,
            },
            repository_id: self.request_id,
            scope_id: self.submodule_request_id,
            authentication: None,
        };
        self.run_remote(target, RemoteAuth::default(), cx);
    }

    fn run_remote(&mut self, target: RemoteTarget, auth: RemoteAuth, cx: &mut Context<Self>) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        if self.remote_job.is_some() || !self.remote_target_is_current(&target) {
            self.notice = Some("仓库上下文已改变，请在当前仓库重新执行远端操作。".into());
            cx.notify();
            return;
        }
        // Keep credentials tied to the host displayed in the prompt even if
        // another application changes the remote URL while that prompt is open.
        let auth = if let Some(challenge) = &target.authentication {
            auth.with_challenge(challenge)
        } else {
            auth
        };
        self.remote_id = self.remote_id.wrapping_add(1);
        let id = self.remote_id;
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(RemoteProgress::default()));
        let reported = Arc::clone(&progress);
        let control = RemoteControl {
            auth,
            cancel: Arc::clone(&cancel),
            progress: Some(Arc::new(move |value| {
                *reported.lock().unwrap_or_else(|error| error.into_inner()) = value;
            })),
        };
        self.remote_pending = None;
        self.pause_repository_reads();
        self.menu = None;
        self.remote_dialog.update(cx, |dialog, cx| dialog.clear(cx));
        self.notice = None;
        self.remote_progress_text = "正在准备远端操作…".into();
        self.remote_job = Some(RemoteJob {
            target: target.clone(),
            id,
            cancel,
            progress,
        });
        // No pre-operation scan may publish a stale tree after a pull.
        self.pause_repository_reads();
        self.remote_tick_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let alive = this.update(cx, |view, cx| {
                    let Some(job) = view.remote_job.as_ref().filter(|job| job.id == id) else {
                        return false;
                    };
                    let text = progress_text(
                        &job.progress
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()),
                    );
                    if view.remote_progress_text != text {
                        view.remote_progress_text = text;
                        cx.notify();
                    }
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        }));
        let service = Arc::clone(&self.service);
        self.remote_task = Some(cx.spawn(async move |this, cx| {
            let worker = target.clone();
            let result = tasks::spawn(cx.background_executor(), "remote.execute", id, move || {
                service.remote(&worker.root, &worker.request, &control)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                if view.remote_job.as_ref().is_none_or(|job| job.id != id) {
                    return;
                }
                view.remote_job = None;
                view.remote_tick_task = None;
                view.remote_task = None;
                view.pause_repository_reads();
                if !view.remote_target_is_current(&target) {
                    if view.request_id == target.repository_id {
                        view.invalidate_changes();
                        view.preview_deck.invalidate_root(&target.root);
                        view.mutation_refresh = true;
                        view.scan_pending = false;
                        let message = match result {
                            Ok(outcome) => outcome.summary,
                            Err(error) => error.message,
                        };
                        view.notice = Some(format!("{} · {message}", display_path(&target.root)));
                        view.queue_scan(cx);
                    }
                    cx.notify();
                    return;
                }
                view.invalidate_changes();
                view.mutation_refresh = true;
                view.scan_pending = false;
                match result {
                    Ok(outcome) => view.remote_completed(&target, outcome, cx),
                    Err(error) => {
                        let text = format!(
                            "{}未完成：{}",
                            action_label(target.request.action),
                            error.message
                        );
                        view.remote_feedback = Some(RemoteFeedback {
                            root: target.root.clone(),
                            text: format!("{}未完成", action_label(target.request.action)),
                        });
                        view.notice = Some(text);
                        if error.kind == RemoteErrorKind::AuthRequired
                            && let Some(challenge) = error.authentication
                        {
                            let mut pending = target.clone();
                            pending.authentication = Some(challenge.clone());
                            let remote = format!("{} · {}", target.name, challenge.host);
                            let prompt = match challenge.protocol {
                                AuthProtocol::Https => RemoteDialogPrompt::Https {
                                    remote,
                                    username: challenge.username,
                                    message: error.message,
                                },
                                AuthProtocol::Ssh => RemoteDialogPrompt::Ssh {
                                    remote,
                                    username: challenge.username,
                                    private_key: challenge.private_key,
                                    message: error.message,
                                },
                            };
                            view.remote_pending = Some(pending);
                            view.remote_dialog
                                .update(cx, |dialog, cx| dialog.show(prompt, cx));
                        }
                    }
                }
                view.queue_scan(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn remote_target_is_current(&self, target: &RemoteTarget) -> bool {
        self.request_id == target.repository_id
            && self.submodule_request_id == target.scope_id
            && self
                .changes_repository()
                .is_some_and(|repo| paths_equal(&repo.root, &target.root))
    }

    fn remote_completed(
        &mut self,
        target: &RemoteTarget,
        outcome: RemoteOutcome,
        _: &mut Context<Self>,
    ) {
        if matches!(outcome.action, RemoteAction::Fetch | RemoteAction::Pull) {
            self.last_fetch.insert(target.root.clone(), Instant::now());
        }
        let text = format!(
            "{}完成 · {}：{}",
            action_label(outcome.action),
            target.name,
            outcome.summary
        );
        self.remote_feedback = Some(RemoteFeedback {
            root: target.root.clone(),
            text: format!("{}完成", action_label(outcome.action)),
        });
        self.notice = Some(text);
    }

    pub(super) fn handle_remote_dialog(
        &mut self,
        event: &RemoteDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, RemoteDialogEvent::DiscoverSshKeys) {
            self.discover_ssh_keys(false, cx);
            return;
        }
        self.cancel_ssh_discovery();
        if self.handle_repository_setup_auth(event, cx) {
            return;
        }
        let Some(target) = self.remote_pending.take() else {
            return;
        };
        match event {
            RemoteDialogEvent::SubmitHttps { username, password } => {
                self.run_remote(
                    target,
                    RemoteAuth::user_password(username.clone(), password.clone()),
                    cx,
                );
            }
            RemoteDialogEvent::SubmitSsh {
                username,
                private_key,
                passphrase,
            } => {
                let passphrase = (!passphrase.is_empty()).then(|| passphrase.clone());
                self.run_remote(
                    target,
                    RemoteAuth::ssh_key(username.clone(), private_key.clone(), passphrase),
                    cx,
                );
            }
            RemoteDialogEvent::Retry { username } => self.run_remote(
                target,
                RemoteAuth::default().with_username(username.clone()),
                cx,
            ),
            RemoteDialogEvent::Cancel => {
                self.notice = Some("已取消远端认证。".into());
                cx.notify();
            }
            RemoteDialogEvent::DiscoverSshKeys => {}
        }
    }

    pub(super) fn remote_progress_panel(&self, light: bool, cx: &mut Context<Self>) -> Div {
        let job = self.remote_job.as_ref().expect("only render while running");
        let critical = matches!(
            job.progress
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .phase,
            RemotePhase::CheckingOut | RemotePhase::Complete
        );
        let cancelling = job.cancel.load(Ordering::Acquire);
        div()
            .absolute()
            .top(px(106.))
            .right(px(16.))
            .w(px(420.))
            .p(px(14.))
            .rounded(px(6.))
            .border_1()
            .border_color(rgb(if light { 0xd0d7de } else { 0x3d444d }))
            .bg(rgb(if light { 0xffffff } else { 0x212830 }))
            .shadow_md()
            .occlude()
            .child(format!(
                "正在{} · {}",
                action_label(job.target.request.action),
                job.target.name
            ))
            .child(div().mt(px(8.)).text_size(px(11.)).child(if cancelling {
                "正在请求取消，等待远端响应…".to_owned()
            } else {
                self.remote_progress_text.clone()
            }))
            .when(!critical && !cancelling, |panel| {
                panel.child(
                    div()
                        .id("cancel-remote")
                        .role(Role::Button)
                        .aria_label("请求取消远端操作")
                        .mt(px(10.))
                        .cursor_pointer()
                        .child("请求取消")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(job) = &this.remote_job {
                                let phase = job
                                    .progress
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner())
                                    .phase;
                                if !matches!(
                                    phase,
                                    RemotePhase::CheckingOut | RemotePhase::Complete
                                ) {
                                    job.cancel.store(true, Ordering::Release);
                                }
                            }
                            cx.notify();
                        })),
                )
            })
    }

    pub(super) fn decorate_remote_controls(&self, tree: &mut DesignNode) {
        if let Some(job) = &self.remote_job {
            let active = match job.target.request.action {
                RemoteAction::Fetch => "获取远端文字",
                RemoteAction::Pull => "拉取远端文字",
                RemoteAction::Push => "推送提交文字",
            };
            set_named_text(
                tree,
                active,
                format!("{}中…", action_label(job.target.request.action)),
            );
        }
        if self.remote_job.is_some()
            || self.remote_pending.is_some()
            || self.mutation_task.is_some()
            || self.mutation_refresh
            || self.worktree_busy
        {
            for name in ["获取远端", "拉取远端", "推送提交", "立即获取远程更新"]
            {
                if let Some(node) = find_name_mut(tree, name) {
                    node.opacity = Some(0.55);
                }
            }
        }
        if let Some(repo) = self.changes_repository() {
            if let Some(at) = self.last_fetch.get(&repo.root) {
                let minutes = at.elapsed().as_secs() / 60;
                set_named_text(
                    tree,
                    "上次获取时间",
                    if minutes == 0 {
                        "刚刚获取".into()
                    } else {
                        format!("{minutes} 分钟前获取")
                    },
                );
            }
            if let Some(feedback) = &self.remote_feedback
                && paths_equal(&feedback.root, &repo.root)
            {
                set_named_text(tree, "同步时间", feedback.text.clone());
            }
        }
    }
}
