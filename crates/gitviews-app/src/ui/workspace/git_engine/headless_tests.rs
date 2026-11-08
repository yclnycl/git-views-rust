use super::super::{Workspace, WorkspaceOptions};
use crate::ui::headless;
use gitviews_service::{SettingsStore, display_path};
use gpui_kit::{
    AppContext, Entity, TestAppContext, WindowHandle, base::Root, px, size, test::TestWindowExt,
};
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn fixture(cx: &mut TestAppContext) -> (WindowHandle<Root>, Entity<Workspace>, PathBuf) {
    fixture_with_config(cx, None)
}

fn fixture_with_config(
    cx: &mut TestAppContext,
    engine_config: Option<&str>,
) -> (WindowHandle<Root>, Entity<Workspace>, PathBuf) {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/headless-tests")
        .join(format!(
            "engine-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    std::fs::create_dir_all(&directory).unwrap();
    if let Some(config) = engine_config {
        std::fs::write(directory.join("git-engine.toml"), config).unwrap();
    }
    let store = SettingsStore::new(directory.join("config.toml"));
    let mut settings = store.load_or_create().unwrap();
    settings.read_only = false;
    store.save(&settings).unwrap();
    let (window, workspace) = headless::open_window(cx, size(px(1200.), px(800.)), |window, cx| {
        cx.new(|cx| {
            Workspace::new(
                WorkspaceOptions {
                    initial_paths: Vec::new(),
                    state_file: directory.join("repositories.json"),
                    preview: false,
                    settings,
                    settings_store: store,
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
    (window, workspace, directory)
}

#[gpui_kit::test]
fn first_run_requires_explicit_choice_then_persists_and_restores_catalog(cx: &mut TestAppContext) {
    let (window, workspace, directory) = fixture(cx);
    workspace.read_with(cx, |view, cx| {
        assert!(!view.git_engine_ready);
        assert!(!view.catalog_ready);
        assert!(view.modal_open(cx));
    });
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.find("git-engine-save").visible());
        assert_eq!(window.find("git-engine-save").disabled(), Some(true));
        window.press("escape", cx);
        window.press("ctrl-o", cx);
    })
    .unwrap();
    workspace.read_with(cx, |view, cx| {
        assert!(view.git_engine_dialog.read(cx).is_open())
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.click("git-engine-builtin", cx)
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.click("git-engine-save", cx)
    })
    .unwrap();
    cx.run_until_parked();
    workspace.read_with(cx, |view, cx| {
        assert!(view.git_engine_ready);
        assert!(view.catalog_ready);
        assert!(!view.git_engine_dialog.read(cx).is_open());
        assert_eq!(
            view.service.git_engine_preferences().engine,
            Some(gitviews_service::GitEngineKind::BuiltIn)
        );
    });
    let saved = gitviews_service::GitEngineStore::new(directory.join("git-engine.toml"))
        .load()
        .unwrap();
    assert_eq!(saved.engine, Some(gitviews_service::GitEngineKind::BuiltIn));
    cx.update_window(window.into(), |_, window, cx| {
        workspace.update(cx, |view, cx| {
            view.git_engine_ready = false;
            view.initialize_git_engine(Vec::new(), window, cx);
        })
    })
    .unwrap();
    cx.run_until_parked();
    workspace.read_with(cx, |view, cx| {
        assert!(view.git_engine_ready);
        assert!(!view.git_engine_dialog.read(cx).is_open());
    });
}

#[gpui_kit::test]
fn manual_git_path_is_validated_and_failure_keeps_first_run_open(cx: &mut TestAppContext) {
    let (window, workspace, directory) = fixture(cx);
    cx.update_window(window.into(), |_, window, cx| {
        window.click("git-engine-system", cx)
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        assert!(window.find("git-engine-detect").visible());
        assert!(window.find("git-engine-browse").visible());
        window.click("git-engine-executable", cx);
        window.input(
            &format!(
                "\"{}\"",
                directory.join("missing-git.exe").to_string_lossy()
            ),
            cx,
        );
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.click("git-engine-save", cx)
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("git-engine-error").visible());
        assert!(
            window
                .find("git-engine-error")
                .label()
                .unwrap()
                .contains("不存在")
        );
    })
    .unwrap();
    workspace.read_with(cx, |view, cx| {
        assert!(!view.git_engine_ready);
        assert!(!view.catalog_ready);
        assert!(view.git_engine_dialog.read(cx).is_open());
    });
    assert!(!directory.join("git-engine.toml").exists());
}

#[gpui_kit::test]
fn detected_git_requires_selection_and_populates_editable_path(cx: &mut TestAppContext) {
    let (window, workspace, directory) = fixture(cx);
    let candidate = directory.join("detected-git.exe");
    workspace.update(cx, |view, cx| {
        view.git_engine_dialog.update(cx, |dialog, cx| {
            dialog.set_candidates(
                vec![gitviews_service::SystemGitInstallation {
                    executable: candidate.clone(),
                    version: "git version test".into(),
                }],
                cx,
            );
        })
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.click("git-engine-system", cx)
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.click(("git-engine-candidate", 0usize), cx)
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, _| {
        assert_eq!(
            window.find("git-engine-executable").value(),
            Some(display_path(&candidate).as_str())
        );
        assert!(window.find("git-engine-save").visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn corrupt_engine_configuration_is_backed_up_only_on_click_and_can_be_replaced(
    cx: &mut TestAppContext,
) {
    let original = "engine = [ broken configuration\n";
    let (window, workspace, directory) = fixture_with_config(cx, Some(original));
    let path = directory.join("git-engine.toml");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    workspace.read_with(cx, |view, cx| {
        assert!(!view.git_engine_ready);
        assert!(!view.catalog_ready);
        assert!(view.git_engine_dialog.read(cx).is_open());
    });
    cx.update_window(window.into(), |_, window, _| {
        assert!(window.find("git-engine-recover").visible());
        assert!(
            window
                .find("git-engine-error")
                .label()
                .unwrap()
                .contains("git-engine.toml")
        );
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.click("git-engine-recover", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!path.exists());
    let backup = std::fs::read_dir(&directory)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|candidate| std::fs::read(candidate).ok().as_deref() == Some(original.as_bytes()))
        .expect("exact damaged bytes preserved in backup");
    cx.update_window(window.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window
                .find("git-engine-recovery-message")
                .label()
                .unwrap()
                .contains(backup.file_name().unwrap().to_str().unwrap())
        );
        assert!(window.try_find("git-engine-recover").is_none());
        window.click("git-engine-builtin", cx);
    })
    .unwrap();
    cx.update_window(window.into(), |_, window, cx| {
        window.click("git-engine-save", cx)
    })
    .unwrap();
    cx.run_until_parked();
    workspace.read_with(cx, |view, cx| {
        assert!(view.git_engine_ready);
        assert!(view.catalog_ready);
        assert!(!view.git_engine_dialog.read(cx).is_open());
    });
    assert_eq!(
        gitviews_service::GitEngineStore::new(&path)
            .load()
            .unwrap()
            .engine,
        Some(gitviews_service::GitEngineKind::BuiltIn)
    );
    assert_eq!(std::fs::read_to_string(backup).unwrap(), original);
}
