//! Start with no repository; exercise real creation forms and catalog loading.
use super::super::{Workspace, WorkspaceOptions};
use gitviews_service::{
    AppSettings, Head, RemoteControl, RepositoryService, RepositorySetupKind,
    RepositorySetupRequest, SettingsStore, display_path, paths_equal,
};
use gpui_kit::{
    App, AppContext as _, Entity, TestAppContext, Window, WindowHandle, base::Root, px, size,
    test::TestWindowExt as _,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    directory: PathBuf,
    window: WindowHandle<Root>,
    workspace: Entity<Workspace>,
}
impl Fixture {
    fn new(cx: &mut TestAppContext) -> Self {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/headless-repository-setup-tests");
        fs::create_dir_all(&base).unwrap();
        let directory = fs::canonicalize(base).unwrap().join(format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let (window, workspace) =
            crate::ui::headless::open_window(cx, size(px(1680.), px(1100.)), |window, cx| {
                cx.new(|cx| {
                    Workspace::new(
                        WorkspaceOptions {
                            initial_paths: Vec::new(),
                            state_file: directory.join("repositories.json"),
                            preview: true,
                            settings: AppSettings {
                                read_only: false,
                                ..AppSettings::default()
                            },
                            settings_store: SettingsStore::new(directory.join("config.toml")),
                            settings_error: None,
                        },
                        window,
                        cx,
                    )
                })
            });
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| window.render_frame(cx))
            .unwrap();
        Self {
            directory,
            window,
            workspace,
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
fn input(window: &mut Window, id: &'static str, value: &str, cx: &mut App) {
    window.click(id, cx);
    window.press("ctrl-a", cx);
    window.input(value, cx);
}

#[gpui_kit::test]
fn new_repository_from_empty_workspace_opens_and_registers_requested_branch(
    cx: &mut TestAppContext,
) {
    let fixture = Fixture::new(cx);
    let root = fixture.directory.join("中文 新项目");
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.press("ctrl-n", cx);
        window.render_frame(cx);
        window.click("repository-setup-submit", cx);
        assert!(window.try_find("repository-setup-error").is_some());
        input(
            window,
            "repository-setup-destination",
            &display_path(&root),
            cx,
        );
        input(window, "repository-setup-branch", "team/start", cx);
        window.click("repository-setup-submit", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.read(|cx| {
        let view = fixture.workspace.read(cx);
        let repository = view
            .repository
            .as_ref()
            .expect("created repo opens automatically");
        assert!(paths_equal(&repository.root, &root));
        assert_eq!(repository.head, Head::Unborn("team/start".into()));
        assert_eq!(view.catalog.repositories.len(), 1);
        assert!(
            view.catalog
                .active
                .as_deref()
                .is_some_and(|path| paths_equal(path, &root))
        );
        assert!(!view.repository_setup_dialog.read(cx).is_open());
    });
}

#[gpui_kit::test]
fn clone_rejects_nonempty_target_then_retries_into_new_directory(cx: &mut TestAppContext) {
    let fixture = Fixture::new(cx);
    let source = fixture.directory.join("source");
    RepositoryService::default()
        .setup_repository(
            &RepositorySetupRequest {
                kind: RepositorySetupKind::Init,
                destination: source.clone(),
                source: String::new(),
                initial_branch: "main".into(),
            },
            &RemoteControl::default(),
        )
        .unwrap();
    let occupied = fixture.directory.join("occupied");
    fs::create_dir(&occupied).unwrap();
    fs::write(occupied.join("keep.txt"), "keep").unwrap();
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.press("ctrl-shift-o", cx);
        window.render_frame(cx);
        input(
            window,
            "repository-setup-source",
            &display_path(&source),
            cx,
        );
        input(
            window,
            "repository-setup-destination",
            &display_path(&occupied),
            cx,
        );
        window.click("repository-setup-submit", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let cloned = fixture.directory.join("克隆结果");
    cx.update_window(fixture.window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("repository-setup-error").is_some());
        input(
            window,
            "repository-setup-destination",
            &display_path(&cloned),
            cx,
        );
        window.click("repository-setup-submit", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        fs::read_to_string(occupied.join("keep.txt")).unwrap(),
        "keep"
    );
    cx.read(|cx| {
        assert!(
            fixture
                .workspace
                .read(cx)
                .repository
                .as_ref()
                .is_some_and(|repo| paths_equal(&repo.root, &cloned))
        )
    });
}
