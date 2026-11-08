use super::remote_dialog::RemoteDialogPrompt;
use super::*;
use gitviews_service::{GitEngineKind, RepositorySetupKind, RepositorySetupRequest};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

#[cfg(test)]
mod headless_tests;

#[derive(Default)]
pub(super) struct RepositorySetupState {
    task: Option<Task<()>>,
    tick: Option<Task<()>>,
    cancel: Option<Arc<AtomicBool>>,
    pending: Option<(RepositorySetupRequest, AuthChallenge)>,
}
impl RepositorySetupState {
    pub(super) fn is_busy(&self) -> bool {
        self.cancel.is_some() || self.pending.is_some()
    }
}
impl Drop for RepositorySetupState {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Release);
        }
    }
}
impl Workspace {
    pub(super) fn open_repository_setup(
        &mut self,
        kind: RepositorySetupKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        if self.modal_open(cx) || self.remote_blocks_navigation(cx) {
            return;
        }
        self.menu = None;
        self.show_about = false;
        self.close_file_search(cx);
        self.active_input = None;
        self.focus.focus(window, cx);
        let system = self.service.git_engine_preferences().engine == Some(GitEngineKind::System);
        self.repository_setup_dialog
            .update(cx, |dialog, cx| dialog.show(kind, system, cx));
        cx.notify();
    }
    pub(super) fn handle_repository_setup(
        &mut self,
        event: &RepositorySetupEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            RepositorySetupEvent::Run(request) if self.repository_setup_state.cancel.is_none() => {
                self.run_repository_setup(request.clone(), RemoteAuth::default(), cx)
            }
            RepositorySetupEvent::Browse if self.repository_setup_state.cancel.is_none() => {
                let receiver = cx.prompt_for_paths(PathPromptOptions {
                    files: false,
                    directories: true,
                    multiple: false,
                    prompt: Some("选择空目录作为仓库目录".into()),
                });
                self.picker_task = Some(cx.spawn_in(window, async move |this, cx| {
                    if let Ok(Ok(Some(paths))) = receiver.await
                        && let Some(path) = paths.into_iter().next()
                    {
                        let _ = this.update_in(cx, |view, window, cx| {
                            view.repository_setup_dialog
                                .update(cx, |dialog, cx| dialog.set_destination(path, window, cx))
                        });
                    }
                }));
            }
            RepositorySetupEvent::Cancel => {
                if let Some(cancel) = &self.repository_setup_state.cancel {
                    cancel.store(true, Ordering::Release);
                    self.repository_setup_dialog.update(cx, |dialog, cx| {
                        dialog.set_progress("正在请求取消，等待 Git 完成当前步骤…".into(), cx)
                    });
                }
            }
            RepositorySetupEvent::Close if self.repository_setup_state.cancel.is_none() => {
                self.repository_setup_state.pending = None;
                self.repository_setup_dialog
                    .update(cx, |dialog, cx| dialog.clear(cx));
                self.focus.focus(window, cx);
            }
            _ => {}
        }
        cx.notify();
    }
    fn run_repository_setup(
        &mut self,
        request: RepositorySetupRequest,
        auth: RemoteAuth,
        cx: &mut Context<Self>,
    ) {
        if !self.advanced_mode_ready(cx) {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(RemoteProgress::default()));
        let report = Arc::clone(&progress);
        let control = RemoteControl {
            auth,
            cancel: Arc::clone(&cancel),
            progress: Some(Arc::new(move |value| {
                *report.lock().unwrap_or_else(|error| error.into_inner()) = value;
            })),
        };
        self.repository_setup_state.cancel = Some(cancel);
        self.repository_setup_state.pending = None;
        self.repository_setup_dialog
            .update(cx, |dialog, cx| dialog.set_busy(true, cx));
        let service = Arc::clone(&self.service);
        self.repository_setup_state.tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                let keep = this
                    .update(cx, |view, cx| {
                        if view.repository_setup_state.cancel.is_none() {
                            return false;
                        }
                        let value = progress
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .clone();
                        let message = if view
                            .repository_setup_state
                            .cancel
                            .as_ref()
                            .is_some_and(|cancel| cancel.load(Ordering::Acquire))
                        {
                            "正在请求取消，等待 Git 完成当前步骤…".into()
                        } else if value.total_objects > 0 {
                            format!(
                                "{} · {}/{} 个对象 · {} KiB",
                                value.message,
                                value.received_objects,
                                value.total_objects,
                                value.received_bytes / 1024
                            )
                        } else {
                            value.message
                        };
                        if !message.is_empty() {
                            view.repository_setup_dialog
                                .update(cx, |dialog, cx| dialog.set_progress(message, cx));
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
        self.repository_setup_state.task = Some(cx.spawn(async move |this, cx| {
            let original = request.clone();
            let result = tasks::spawn(cx.background_executor(), "repository.setup", 0, move || {
                service.setup_repository(&request, &control)
            })
            .await;
            let _ = this.update(cx, |view, cx| {
                view.repository_setup_state.cancel = None;
                view.repository_setup_state.task = None;
                view.repository_setup_state.tick = None;
                match result {
                    Ok(outcome) => {
                        view.repository_setup_dialog
                            .update(cx, |dialog, cx| dialog.clear(cx));
                        view.add_repositories(vec![outcome.root], cx);
                    }
                    Err(error) => {
                        view.repository_setup_dialog
                            .update(cx, |dialog, cx| dialog.set_error(error.message.clone(), cx));
                        if error.kind == RemoteErrorKind::AuthRequired
                            && let Some(challenge) = error.authentication
                        {
                            view.repository_setup_state.pending =
                                Some((original, challenge.clone()));
                            view.repository_setup_dialog
                                .update(cx, |dialog, cx| dialog.suspend(cx));
                            let prompt = match challenge.protocol {
                                AuthProtocol::Https => RemoteDialogPrompt::Https {
                                    remote: challenge.host,
                                    username: challenge.username,
                                    message: error.message,
                                },
                                AuthProtocol::Ssh => RemoteDialogPrompt::Ssh {
                                    remote: challenge.host,
                                    username: challenge.username,
                                    private_key: challenge.private_key,
                                    message: error.message,
                                },
                            };
                            view.remote_dialog
                                .update(cx, |dialog, cx| dialog.show(prompt, cx));
                        }
                    }
                }
                cx.notify();
            });
        }));
    }
    /// Returns true only for clone-owned auth prompts, leaving normal fetch/push intact.
    pub(super) fn handle_repository_setup_auth(
        &mut self,
        event: &RemoteDialogEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((request, challenge)) = self.repository_setup_state.pending.take() else {
            return false;
        };
        let auth = match event {
            RemoteDialogEvent::SubmitHttps { username, password } => Some(
                RemoteAuth::user_password(username.clone(), password.clone()),
            ),
            RemoteDialogEvent::SubmitSsh {
                username,
                private_key,
                passphrase,
            } => Some(RemoteAuth::ssh_key(
                username.clone(),
                private_key.clone(),
                (!passphrase.is_empty()).then(|| passphrase.clone()),
            )),
            RemoteDialogEvent::Retry { username } => {
                Some(RemoteAuth::default().with_username(username.clone()))
            }
            RemoteDialogEvent::Cancel => {
                self.repository_setup_dialog.update(cx, |dialog, cx| {
                    dialog.set_error("已取消认证，可修改地址后重试克隆。".into(), cx)
                });
                None
            }
            RemoteDialogEvent::DiscoverSshKeys => {
                self.repository_setup_state.pending = Some((request, challenge));
                return false;
            }
        };
        if let Some(auth) = auth {
            self.run_repository_setup(request, auth.with_challenge(&challenge), cx);
        }
        true
    }
}
