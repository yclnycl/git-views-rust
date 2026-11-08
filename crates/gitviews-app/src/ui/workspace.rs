use super::{
    branch_dialog::{BranchDialog, BranchDialogEvent},
    conflict_dialog::{ConflictDialog, ConflictDialogEvent},
    design_renderer::{self, DesignNode, DesignSize, RenderContext},
    file_list::{FileList, FileListEvent},
    git_engine_dialog::{GitEngineDialog, GitEngineEvent},
    git_settings_dialog::{GitSettingsDialog, GitSettingsEvent},
    history_tools_dialog::{HistoryToolsDialog, HistoryToolsDialogEvent},
    paths::display_path,
    preview::{PreviewMode, PreviewPane, PreviewPaneEvent},
    preview_tabs::{PreviewDeck, PreviewKey},
    repository_setup_dialog::{RepositorySetupDialog, RepositorySetupEvent},
    settings_dialog::{SettingsDialog, SettingsEvent},
    stash_dialog::{StashDialog, StashDialogEvent},
    tasks,
    theme::Appearance,
    workbench::{self, FileTab, WorkbenchAction, WorkbenchData},
    worktree_dialog::{WorktreeDialog, WorktreeDialogEvent},
};
use gitviews_service::{
    AppSettings, AuthChallenge, AuthProtocol, BranchAction, BranchRequest, CheckoutRequest,
    CommitDetails, CommitSummary, ConflictRequest, FileDiff, Head, HistoryToolsRequest,
    HistoryToolsResult, LogLevel, RemoteAction, RemoteAuth, RemoteControl, RemoteErrorKind,
    RemoteOutcome, RemotePhase, RemoteProgress, RemoteRequest, RepositoryCatalog,
    RepositoryCatalogStore, RepositoryDetails, RepositoryService, RepositorySnapshot,
    RepositoryTree, RepositoryWatcher, SettingsStore, SourceFile, StashRequest, WorktreeAction,
    WorktreeRequest, WriteAction, WriteRequest, paths_equal,
};
use gpui_kit::{
    component::{
        ActiveTheme, ThemeMode,
        input::{Editor, EditorState, Input, InputEvent, InputState, TextareaState},
        resizable::ResizableState,
    },
    prelude::FluentBuilder as _,
    *,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

mod about;
mod branches;
mod columns;
mod copy;
mod file_history;
mod file_search;
mod git_engine;
mod git_settings;
#[cfg(test)]
mod headless_tests;
mod history;
mod live;
mod navigation;
mod operations;
mod presentation;
mod read_only;
mod remote;
mod remote_dialog;
mod repositories;
mod repository_setup;
mod settings;
mod submodule_branches;
mod submodules;
mod tabs;
mod worktrees;
mod writes;

use remote::{RemoteFeedback, RemoteJob, RemoteTarget};
use remote_dialog::{RemoteDialog, RemoteDialogEvent};
use submodules::SubmoduleChanges;

gpui_kit::actions!(
    gitviews,
    [
        OpenRepository,
        CloneRepository,
        NewRepository,
        RefreshRepository,
        SearchFiles,
        NextChange,
        PreviousChange,
        ShowHistory,
        FindInFile,
        ClosePreviewTab,
        NextPreviewTab,
        PreviousPreviewTab,
        OpenSettings,
        OpenGitSettings,
        ManageWorktrees,
        CommitChanges,
        ManageBranches,
        ManageStashes,
        OpenHistoryTools,
        ManageConflicts
    ]
);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Repository,
    Files,
    Search,
    Summary,
    Description,
}
impl Field {
    fn index(self) -> usize {
        self as usize
    }
    fn node(self) -> &'static str {
        match self {
            Self::Repository => "筛选工作树和分支",
            Self::Files => "筛选文件输入框",
            Self::Search => "搜索命令或文件",
            Self::Summary => "提交摘要输入框",
            Self::Description => "可选提交描述输入框",
        }
    }
}

#[derive(Clone)]
enum UiAction {
    StageAll,
    UnstageAll,
    Commit,
    Stashes,
    Branches,
    Refresh,
    Menu(&'static str),
    Field(Field),
    Unified(bool),
    IgnoreWhitespace,
    Change(isize),
    Source,
    Remote(RemoteAction),
    History,
    Settings,
    ToggleReadOnly,
    Worktrees(bool),
    Unavailable(String),
    Toggle(String),
    PreviewFile,
}

struct Designs {
    dark: DesignNode,
    light: DesignNode,
    unified_dark: DesignNode,
    unified_light: DesignNode,
}
impl Designs {
    fn new() -> Self {
        let mut designs = Self {
            dark: serde_json::from_str(include_str!("../../assets/design/dark.json"))
                .expect("valid Pencil dark export"),
            light: serde_json::from_str(include_str!("../../assets/design/light.json"))
                .expect("valid Pencil light export"),
            unified_dark: serde_json::from_str(include_str!(
                "../../assets/design/unified-dark.json"
            ))
            .expect("valid Pencil unified dark export"),
            unified_light: serde_json::from_str(include_str!(
                "../../assets/design/unified-light.json"
            ))
            .expect("valid Pencil unified light export"),
        };
        for tree in [
            &mut designs.dark,
            &mut designs.light,
            &mut designs.unified_dark,
            &mut designs.unified_light,
        ] {
            mark_original_content(tree);
        }
        designs
    }
}

fn mark_original_content(node: &mut DesignNode) {
    node.original_content = node.content.clone();
    for child in &mut node.children {
        mark_original_content(child);
    }
}

/// Owns application state and task lifetimes independently of the exported presentation tree.
pub struct Workspace {
    repository_setup_dialog: Entity<RepositorySetupDialog>,
    repository_setup_state: repository_setup::RepositorySetupState,
    git_engine_dialog: Entity<GitEngineDialog>,
    git_engine_store: Arc<gitviews_service::GitEngineStore>,
    git_engine_ready: bool,
    git_engine_task: Option<Task<()>>,
    git_engine_initial_paths: Option<Vec<PathBuf>>,
    branch_dialog: Entity<BranchDialog>,
    stash_dialog: Entity<StashDialog>,
    history_tools_dialog: Entity<HistoryToolsDialog>,
    conflict_dialog: Entity<ConflictDialog>,
    operations_read_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    operations_read_task: Option<Task<()>>,
    operations_read_id: u64,
    commit_description: Entity<TextareaState>,
    settings: AppSettings,
    settings_store: Arc<SettingsStore>,
    settings_error: Option<String>,
    settings_dialog: Entity<SettingsDialog>,
    settings_generation: u64,
    settings_task: Option<Task<()>>,
    settings_saving: bool,
    git_settings_dialog: Entity<GitSettingsDialog>,
    git_settings_generation: u64,
    git_settings_task: Option<Task<()>>,
    git_settings_saving: bool,
    git_settings_return_to_app: bool,
    ssh_discovery_id: u64,
    ssh_discovery_task: Option<Task<()>>,
    startup_log_level: LogLevel,
    worktree_dialog: Entity<WorktreeDialog>,
    worktree_generation: u64,
    worktree_task: Option<Task<()>>,
    worktree_busy: bool,
    designs: Designs,
    service: Arc<RepositoryService>,
    catalog: RepositoryCatalog,
    catalog_store: Arc<RepositoryCatalogStore>,
    catalog_write_allowed: bool,
    catalog_error: Option<String>,
    catalog_import_count: usize,
    catalog_ready: bool,
    pending_repositories: Vec<PathBuf>,
    repository: Option<RepositorySnapshot>,
    details: Option<RepositoryDetails>,
    tree: Option<Arc<RepositoryTree>>,
    diff: Option<Arc<FileDiff>>,
    source_file: Option<Arc<SourceFile>>,
    preview_pane: Entity<PreviewPane>,
    preview_deck: PreviewDeck,
    preview_history_subscription: Option<(PreviewKey, EntityId, Subscription)>,
    submodule_branch_menu: Option<submodule_branches::SubmoduleBranchMenu>,
    active_tab: Option<usize>,
    file_search: file_search::FileSearch,
    history_state: history::HistoryState,
    file_list: Entity<FileList>,
    column_sizes: Entity<ResizableState>,
    remote_dialog: Entity<RemoteDialog>,
    remote_job: Option<RemoteJob>,
    remote_pending: Option<RemoteTarget>,
    remote_feedback: Option<RemoteFeedback>,
    remote_id: u64,
    remote_task: Option<Task<()>>,
    remote_tick_task: Option<Task<()>>,
    remote_progress_text: String,
    last_fetch: HashMap<PathBuf, Instant>,
    scan_epoch: u64,
    navigation: navigation::NavigationRead,
    scan_cancel: Arc<std::sync::atomic::AtomicBool>,
    mutation_refresh: bool,
    mutation_task: Option<Task<()>>,
    branch_search: Entity<InputState>,
    branch_context: Option<branches::BranchContext>,
    branch_context_task: Option<Task<()>>,
    branch_query: String,
    expanded_remotes: HashSet<String>,
    submodule_changes: Option<SubmoduleChanges>,
    submodule_request_id: u64,
    submodule_loading: bool,
    submodule_task: Option<Task<()>>,
    submodule_watch_task: Option<Task<()>>,
    submodule_cancel: Arc<std::sync::atomic::AtomicBool>,
    watcher: Option<RepositoryWatcher>,
    watch_task: Option<Task<()>>,
    scan_task: Option<Task<()>>,
    scan_pending: bool,
    last_scan: Instant,
    scan_count: u64,
    history_commits: Vec<CommitSummary>,
    history_more: bool,
    history_loading: bool,
    selected_commit: Option<CommitDetails>,
    commit_file: Option<String>,
    history_request_id: u64,
    commit_request_id: u64,
    history_task: Option<Task<()>>,
    commit_task: Option<Task<()>>,
    requested_path: Option<PathBuf>,
    selected_file: Option<String>,
    selected_staged: bool,
    opened_files: Vec<FileTab>,
    preview: bool,
    loading: bool,
    diff_loading: bool,
    unified: bool,
    ignore_whitespace: bool,
    source: bool,
    history: bool,
    appearance: Appearance,
    menu: Option<&'static str>,
    notice: Option<String>,
    show_about: bool,
    license_editor: Option<Entity<EditorState>>,
    collapsed: HashSet<String>,
    collapsed_paths: HashSet<String>,
    tree_mode: bool,
    request_id: u64,
    diff_request_id: u64,
    hunk_index: usize,
    code_scroll: ScrollHandle,
    focus: FocusHandle,
    inputs: Vec<Entity<InputState>>,
    active_input: Option<Field>,
    file_query: String,
    repo_query: String,
    summary: String,
    description: String,
    load_task: Option<Task<()>>,
    diff_task: Option<Task<()>>,
    picker_task: Option<Task<()>>,
    startup_task: Option<Task<()>>,
    catalog_import_task: Option<Task<()>>,
    catalog_save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

pub struct WorkspaceOptions {
    pub initial_paths: Vec<PathBuf>,
    pub state_file: PathBuf,
    pub preview: bool,
    pub settings: AppSettings,
    pub settings_store: SettingsStore,
    pub settings_error: Option<String>,
}

impl Workspace {
    pub fn new(options: WorkspaceOptions, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let WorkspaceOptions {
            initial_paths,
            state_file,
            preview,
            settings,
            settings_store,
            settings_error,
        } = options;
        let mut subscriptions = vec![cx.observe_window_appearance(window, |this, window, cx| {
            if this.appearance == Appearance::System {
                this.appearance.apply(window, cx);
            }
        })];
        let fields = [
            Field::Repository,
            Field::Files,
            Field::Search,
            Field::Summary,
            Field::Description,
        ];
        let placeholders = [
            "筛选工作树和分支",
            "筛选文件名或路径…",
            "搜索文件名、路径或内容…",
            "简要描述这次更改",
            "添加详细描述（可选）",
        ];
        let inputs = fields
            .into_iter()
            .zip(placeholders)
            .map(|(field, placeholder)| {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                subscriptions.push(cx.subscribe_in(
                    &input,
                    window,
                    move |this, state, event, window, cx| {
                        if matches!(event, InputEvent::Change) {
                            let value = state.read(cx).value().to_string();
                            match field {
                                Field::Repository => this.repo_query = value,
                                Field::Files => {
                                    this.file_query = value.clone();
                                    this.file_list
                                        .update(cx, |list, cx| list.set_query(value, cx));
                                }
                                Field::Search => this.query_files(value, cx),
                                Field::Summary => this.summary = value,
                                Field::Description => this.description = value,
                            }
                            cx.notify();
                        }
                        if field == Field::Search
                            && matches!(event, InputEvent::PressEnter { .. })
                            && this.file_search.open
                        {
                            this.accept_file_search(window, cx);
                        }
                    },
                ));
                input
            })
            .collect();
        let focus = cx.focus_handle();
        let branch_search =
            cx.new(|cx| InputState::new(window, cx).placeholder("搜索本地或远程分支…"));
        subscriptions.push(cx.subscribe_in(
            &branch_search,
            window,
            |this, state, event, window, cx| match event {
                InputEvent::Change => {
                    this.branch_query = state.read(cx).value().to_string();
                    cx.notify();
                }
                InputEvent::PressEnter { .. } if this.menu == Some("分支") => {
                    this.submit_branch_search(window, cx)
                }
                _ => {}
            },
        ));
        let preview_pane = cx.new(|cx| PreviewPane::new(window, cx));
        let file_list = cx.new(|_| FileList::new());
        let remote_dialog = cx.new(RemoteDialog::new);
        let repository_setup_dialog = cx.new(RepositorySetupDialog::new);
        let settings_dialog = cx.new(SettingsDialog::new);
        let git_engine_dialog = cx.new(GitEngineDialog::new);
        let git_settings_dialog = cx.new(GitSettingsDialog::new);
        let worktree_dialog = cx.new(WorktreeDialog::new);
        let branch_dialog = cx.new(BranchDialog::new);
        let stash_dialog = cx.new(StashDialog::new);
        let history_tools_dialog = cx.new(HistoryToolsDialog::new);
        let conflict_dialog = cx.new(ConflictDialog::new);
        let commit_description = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("详细描述（可选）")
                .rows(2)
        });
        subscriptions.push(cx.subscribe_in(
            &git_engine_dialog,
            window,
            |this, _, event: &GitEngineEvent, window, cx| {
                this.handle_git_engine_event(event, window, cx);
            },
        ));
        subscriptions.push(cx.subscribe(
            &commit_description,
            |this, state, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.description = state.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &conflict_dialog,
            window,
            |this, _, event: &ConflictDialogEvent, window, cx| {
                this.handle_conflicts(event, window, cx);
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &history_tools_dialog,
            window,
            |this, _, event: &HistoryToolsDialogEvent, window, cx| {
                this.handle_history_tools(event, window, cx);
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &branch_dialog,
            window,
            |this, _, event: &BranchDialogEvent, window, cx| {
                this.handle_branch_dialog(event, window, cx);
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &stash_dialog,
            window,
            |this, _, event: &StashDialogEvent, window, cx| {
                this.handle_stash_dialog(event, window, cx);
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &worktree_dialog,
            window,
            |this, _, event: &WorktreeDialogEvent, window, cx| {
                this.handle_worktree_event(event, window, cx);
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &git_settings_dialog,
            window,
            |this, _, event: &GitSettingsEvent, window, cx| {
                this.handle_git_settings_event(event, window, cx);
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &settings_dialog,
            window,
            |this, _, event: &SettingsEvent, window, cx| {
                this.handle_settings_event(event, window, cx);
            },
        ));
        subscriptions.push(cx.subscribe(
            &remote_dialog,
            |this, _, event: &RemoteDialogEvent, cx| {
                this.handle_remote_dialog(event, cx);
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &repository_setup_dialog,
            window,
            |this, _, event: &RepositorySetupEvent, window, cx| {
                this.handle_repository_setup(event, window, cx);
            },
        ));
        subscriptions.push(
            cx.subscribe_in(
                &file_list,
                window,
                |this, _, event, window, cx| match event {
                    FileListEvent::OpenFile { path, staged } => {
                        this.open_changed_file(path.clone(), *staged, window, cx)
                    }
                    FileListEvent::OpenSubmodule(path) => this.select_submodule(path.clone(), cx),
                    FileListEvent::SubmoduleBranches(path) => {
                        this.open_submodule_branch_menu(path.clone(), window, cx)
                    }
                    FileListEvent::FileHistory { root, path } => {
                        this.open_file_history(root.clone(), path.clone(), window, cx)
                    }
                    FileListEvent::UpdateIndex { path, unstage } => this.run_write(
                        if *unstage {
                            WriteAction::UnstagePaths {
                                paths: vec![path.clone()],
                            }
                        } else {
                            WriteAction::StagePaths {
                                paths: vec![path.clone()],
                            }
                        },
                        window,
                        cx,
                    ),
                },
            ),
        );
        window.focus(&focus, cx);
        cx.bind_keys([
            KeyBinding::new("ctrl-,", OpenSettings, Some("GitViews")),
            KeyBinding::new("ctrl-alt-,", OpenGitSettings, Some("GitViews")),
            KeyBinding::new("ctrl-alt-w", ManageWorktrees, Some("GitViews")),
            KeyBinding::new("ctrl-o", OpenRepository, Some("GitViews")),
            KeyBinding::new("ctrl-shift-o", CloneRepository, Some("GitViews")),
            KeyBinding::new("ctrl-n", NewRepository, Some("GitViews")),
            KeyBinding::new("ctrl-r", RefreshRepository, Some("GitViews")),
            KeyBinding::new("ctrl-k", SearchFiles, Some("GitViews")),
            KeyBinding::new("ctrl-p", SearchFiles, Some("GitViews")),
            KeyBinding::new("ctrl-w", ClosePreviewTab, Some("GitViews")),
            KeyBinding::new("ctrl-tab", NextPreviewTab, Some("GitViews")),
            KeyBinding::new("ctrl-shift-tab", PreviousPreviewTab, Some("GitViews")),
            KeyBinding::new("alt-down", NextChange, Some("GitViews")),
            KeyBinding::new("alt-up", PreviousChange, Some("GitViews")),
            KeyBinding::new("ctrl-h", ShowHistory, Some("GitViews")),
            KeyBinding::new("ctrl-f", FindInFile, Some("GitViews")),
            KeyBinding::new("ctrl-enter", CommitChanges, Some("GitViews")),
            KeyBinding::new("ctrl-alt-b", ManageBranches, Some("GitViews")),
            KeyBinding::new("ctrl-alt-h", OpenHistoryTools, Some("GitViews")),
            KeyBinding::new("ctrl-alt-m", ManageConflicts, Some("GitViews")),
            KeyBinding::new("ctrl-alt-s", ManageStashes, Some("GitViews")),
        ]);
        let mut view = Self {
            repository_setup_dialog,
            repository_setup_state: repository_setup::RepositorySetupState::default(),
            git_engine_dialog,
            git_engine_store: Arc::new(gitviews_service::GitEngineStore::new(
                settings_store.path().with_file_name("git-engine.toml"),
            )),
            git_engine_ready: preview,
            git_engine_task: None,
            git_engine_initial_paths: None,
            branch_dialog,
            stash_dialog,
            history_tools_dialog,
            conflict_dialog,
            operations_read_cancel: None,
            operations_read_task: None,
            operations_read_id: 0,
            commit_description,
            settings,
            settings_store: Arc::new(settings_store),
            settings_error,
            settings_dialog,
            settings_generation: 0,
            settings_task: None,
            settings_saving: false,
            git_settings_dialog,
            git_settings_generation: 0,
            git_settings_task: None,
            git_settings_saving: false,
            git_settings_return_to_app: false,
            ssh_discovery_id: 0,
            ssh_discovery_task: None,
            startup_log_level: settings.logging.level,
            worktree_dialog,
            worktree_generation: 0,
            worktree_task: None,
            worktree_busy: false,
            designs: Designs::new(),
            service: Arc::new(RepositoryService::default()),
            catalog: RepositoryCatalog::default(),
            catalog_store: Arc::new(RepositoryCatalogStore::new(state_file)),
            catalog_write_allowed: true,
            catalog_error: None,
            catalog_import_count: 0,
            catalog_ready: false,
            pending_repositories: Vec::new(),
            repository: None,
            details: None,
            tree: None,
            diff: None,
            source_file: None,
            preview_pane,
            preview_deck: PreviewDeck::default(),
            preview_history_subscription: None,
            submodule_branch_menu: None,
            active_tab: None,
            file_search: file_search::FileSearch::default(),
            history_state: history::HistoryState::default(),
            file_list,
            column_sizes: cx.new(|_| ResizableState::default()),
            remote_dialog,
            remote_job: None,
            remote_pending: None,
            remote_feedback: None,
            remote_id: 0,
            remote_task: None,
            remote_tick_task: None,
            remote_progress_text: String::new(),
            last_fetch: HashMap::new(),
            scan_epoch: 0,
            navigation: navigation::NavigationRead::default(),
            scan_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mutation_refresh: false,
            mutation_task: None,
            branch_search,
            branch_context: None,
            branch_context_task: None,
            branch_query: String::new(),
            expanded_remotes: HashSet::new(),
            submodule_changes: None,
            submodule_request_id: 0,
            submodule_loading: false,
            submodule_task: None,
            submodule_watch_task: None,
            submodule_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            watcher: None,
            watch_task: None,
            scan_task: None,
            scan_pending: false,
            last_scan: Instant::now(),
            scan_count: 0,
            history_commits: Vec::new(),
            history_more: false,
            history_loading: false,
            selected_commit: None,
            commit_file: None,
            history_request_id: 0,
            commit_request_id: 0,
            history_task: None,
            commit_task: None,
            requested_path: None,
            selected_file: None,
            selected_staged: false,
            opened_files: Vec::new(),
            preview,
            loading: false,
            diff_loading: false,
            unified: false,
            ignore_whitespace: false,
            source: false,
            history: false,
            appearance: Appearance::from_preference(settings.appearance.theme),
            menu: None,
            notice: None,
            show_about: false,
            license_editor: None,
            collapsed: HashSet::new(),
            collapsed_paths: HashSet::new(),
            tree_mode: false,
            request_id: 0,
            diff_request_id: 0,
            hunk_index: 0,
            code_scroll: ScrollHandle::new(),
            focus,
            inputs,
            active_input: None,
            file_query: String::new(),
            repo_query: String::new(),
            summary: String::new(),
            description: String::new(),
            load_task: None,
            diff_task: None,
            picker_task: None,
            startup_task: None,
            catalog_import_task: None,
            catalog_save_task: None,
            _subscriptions: subscriptions,
        };
        view.apply_settings(settings, window, cx);
        if view.settings_error.is_some() {
            view.notice = Some("配置文件读取失败，原文件已保留。请打开设置查看详情。".into());
        }
        view.init_history(window, cx);
        if !preview {
            view.initialize_git_engine(initial_paths, window, cx);
        }
        view
    }

    fn choose_repository(&mut self, cx: &mut Context<Self>) {
        if self.remote_blocks_navigation(cx) {
            return;
        }
        self.menu = None;
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: true,
            prompt: Some("添加 Git 仓库（可多选）".into()),
        });
        self.picker_task = Some(cx.spawn(async move |this, cx| match receiver.await {
            Ok(Ok(Some(paths))) => {
                let _ = this.update(cx, |view, cx| view.add_repositories(paths, cx));
            }
            Ok(Ok(None)) => {}
            result => {
                let _ = this.update(cx, |view, cx| {
                    view.notice = Some(format!("无法打开目录选择器：{result:?}"));
                    cx.notify();
                });
            }
        }));
        cx.notify();
    }

    fn load(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.remote_blocks_navigation(cx) {
            return;
        }
        self.close_file_search(cx);
        if self
            .repository
            .as_ref()
            .is_some_and(|repo| paths_equal(&repo.root, &path))
            && !self.loading
        {
            if self.submodule_changes.is_some() || self.submodule_loading {
                self.show_parent_changes(cx);
            } else {
                self.refresh(cx);
            }
            return;
        }
        self.discard_submodule();
        self.preview = false;
        self.request_id = self.request_id.wrapping_add(1);
        self.diff_request_id = self.diff_request_id.wrapping_add(1);
        self.diff_task = None;
        self.diff_loading = false;
        self.load_task = None;
        self.scan_task = None;
        self.scan_pending = false;
        if self
            .repository
            .as_ref()
            .is_none_or(|repo| !paths_equal(&repo.root, &path))
        {
            self.expanded_remotes.clear();
            self.reset_history(cx);
            self.clear_tabs(cx);
            self.close_file_search(cx);
            self.repository = None;
            self.details = None;
            self.tree = None;
            self.diff = None;
            self.source_file = None;
            self.preview_pane.update(cx, |pane, cx| pane.clear(cx));
            self.file_list.update(cx, |list, cx| list.clear(cx));
            self.watcher = None;
            self.scan_task = None;
            self.scan_pending = false;
            self.history_request_id = self.history_request_id.wrapping_add(1);
            self.history_task = None;
            self.reset_history(cx);
            self.commit_task = None;
            self.history_loading = false;
            self.history_commits.clear();
            self.selected_commit = None;
            self.commit_file = None;
            self.opened_files.clear();
            self.selected_file = None;
            self.collapsed_paths.clear();
        }
        self.read_repository(path, cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.requested_path.clone() {
            self.invalidate_changes();
            if self.repository.is_some() {
                self.queue_scan(cx);
            } else {
                self.load(path, cx);
            }
        } else {
            self.notice = Some("请先打开一个本地仓库。".into());
            cx.notify();
        }
    }

    fn select_file(&mut self, path: String, staged: bool, cx: &mut Context<Self>) {
        let Some(source) = self
            .changes_repository()
            .map(|repo| !repo.changes.iter().any(|change| change.path == path))
        else {
            return;
        };
        self.open_preview_tab(path, staged, None, source, cx);
        cx.notify();
    }

    fn start_input(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        if field == Field::Search {
            self.open_file_search(window, cx);
            return;
        }
        self.active_input = Some(field);
        self.menu = None;
        let handle = self.inputs[field.index()].read(cx).focus_handle(cx);
        handle.focus(window, cx);
        cx.notify();
    }

    fn navigate_change(&mut self, direction: isize, cx: &mut Context<Self>) {
        if !self.preview {
            let count = self.diff.as_ref().map_or(0, |diff| {
                diff.lines
                    .iter()
                    .filter(|line| {
                        line.kind == gitviews_service::DiffLineKind::Section
                            && line.text.starts_with("@@")
                    })
                    .count()
            });
            if count > 0 {
                self.hunk_index =
                    (self.hunk_index as isize + direction).rem_euclid(count as isize) as usize;
            }
            self.preview_pane
                .update(cx, |pane, cx| pane.next_change(direction > 0, cx));
            cx.notify();
            return;
        }
        let heights = self.hunk_offsets(cx.theme().mode == ThemeMode::Light);
        if !heights.is_empty() {
            self.hunk_index =
                (self.hunk_index as isize + direction).rem_euclid(heights.len() as isize) as usize;
            self.code_scroll
                .set_offset(point(px(0.), px(-heights[self.hunk_index])));
        }
        cx.notify();
    }

    fn handle_workbench(
        &mut self,
        action: &WorkbenchAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.modal_open(cx) {
            return;
        }
        match action {
            WorkbenchAction::OpenFile { path, staged } => {
                self.open_changed_file(path.clone(), *staged, window, cx)
            }
            WorkbenchAction::ActivateTab(index) => self.activate_tab(*index, cx),
            WorkbenchAction::OpenWorktree(path) => self.load(path.clone(), cx),
            WorkbenchAction::OpenSubmodule(path) => self.select_submodule(path.clone(), cx),
            WorkbenchAction::SubmoduleBranches(path) => {
                self.open_submodule_branch_menu(path.clone(), window, cx)
            }
            WorkbenchAction::CheckoutBranch { name, remote, root } => {
                if root.as_ref().is_some_and(|root| {
                    self.changes_repository()
                        .is_none_or(|repo| !paths_equal(&repo.root, root))
                }) {
                    self.notice = Some("当前仓库已变化，请在刷新后的分支列表中选择。".into());
                    cx.notify();
                    return;
                }
                self.checkout_branch(name.clone(), *remote, cx)
            }
            WorkbenchAction::OpenBranchMenu => self.toggle_branch_menu(window, cx),
            WorkbenchAction::ToggleFileView => {
                self.tree_mode = !self.tree_mode;
                self.file_list
                    .update(cx, |list, cx| list.set_tree_mode(self.tree_mode, cx));
                cx.notify();
            }
            WorkbenchAction::ToggleDirectory(path) => {
                if !self.collapsed_paths.remove(path) {
                    self.collapsed_paths.insert(path.clone());
                }
                cx.notify();
            }
            WorkbenchAction::ToggleSection(name) => {
                self.toggle(name);
                cx.notify();
            }
            WorkbenchAction::CloseTab(index) => {
                self.close_tab(*index, cx);
                cx.notify();
            }
        }
    }

    fn toggle(&mut self, name: &str) {
        if let Some(remote) = name.strip_prefix("实际远程仓库 ") {
            if !self.expanded_remotes.remove(remote) {
                self.expanded_remotes.insert(remote.to_owned());
            }
            return;
        }
        if !self.collapsed.remove(name) {
            self.collapsed.insert(name.into());
        }
    }

    fn handle(&mut self, action: &UiAction, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_open(cx) {
            return;
        }
        self.notice = None;
        match action {
            UiAction::StageAll => self.run_write(WriteAction::StageAll, window, cx),
            UiAction::UnstageAll => self.run_write(WriteAction::UnstageAll, window, cx),
            UiAction::Commit => self.commit_changes(window, cx),
            UiAction::Stashes => self.open_stashes(window, cx),
            UiAction::Branches => self.open_branch_manager(window, cx),
            UiAction::Worktrees(create) => self.open_worktrees(*create, window, cx),
            UiAction::Settings => self.open_settings(window, cx),
            UiAction::ToggleReadOnly => self.toggle_read_only(window, cx),
            UiAction::Refresh => self.refresh(cx),
            UiAction::Menu(menu) => {
                if *menu == "分支" {
                    self.toggle_branch_menu(window, cx);
                    return;
                }
                self.active_input = None;
                self.menu = (self.menu != Some(*menu)).then_some(*menu);
            }
            UiAction::Field(field) => self.start_input(*field, window, cx),
            UiAction::Unified(unified) => {
                self.unified = *unified;
                self.source = false;
                self.sync_preview(cx);
            }
            UiAction::IgnoreWhitespace => {
                self.ignore_whitespace = !self.ignore_whitespace;
                self.preview_pane.update(cx, |pane, cx| {
                    pane.set_ignore_whitespace(self.ignore_whitespace, cx)
                });
            }
            UiAction::Change(direction) => self.navigate_change(*direction, cx),
            UiAction::Source => {
                if self.preview {
                    self.notice =
                        Some("设计预览仅包含稿件中的代码；打开本地仓库后可查看完整源文件。".into());
                } else {
                    self.source = !self.source;
                    self.sync_preview(cx);
                    self.code_scroll.set_offset(point(px(0.), px(0.)));
                }
            }
            UiAction::Remote(action) => self.start_remote(*action, cx),
            UiAction::History => self.toggle_history(cx),
            UiAction::Toggle(section) => self.toggle(section),
            UiAction::PreviewFile => {
                self.notice =
                    Some("这是设计稿的示例标签。打开本地仓库后，可查看真实文件与差异。".into())
            }
            UiAction::Unavailable(label) => self.notice = Some(format!("{label}尚未接入。")),
        }
        cx.notify();
    }
}
