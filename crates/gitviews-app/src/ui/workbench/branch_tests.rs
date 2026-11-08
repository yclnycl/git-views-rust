use super::*;
use gitviews_service::BranchInfo;

fn label(name: &str, content: &str) -> DesignNode {
    let mut node = text_node(name, content, "#f0f6fc", 12.);
    node.name = name.into();
    node
}

fn design() -> DesignNode {
    let mut design = frame("branches-test", "工作台");
    let mut branch_control = frame("branch-menu", "当前分支与同步状态");
    let mut arrow = icon("branch-menu-arrow", "chevron-down", "#9198a1", 14.);
    arrow.name = "分支切换".into();
    branch_control.children = vec![label("分支名称", "main"), arrow];
    let mut local_header = frame("local-header", "本地分支标题");
    local_header.children.push(label("本地分支数量", "0"));
    let mut selected = frame("selected-local", "feature/worktree-ui 本地分支");
    selected.fill = Some("#1a3453".into());
    selected
        .children
        .push(label("feature/worktree-ui 本地分支名称", "旧分支"));
    let mut normal = frame("normal-local", "main 本地分支");
    normal.children.push(label("main 本地分支名称", "main"));
    let mut locals = frame("local-list", "本地分支列表");
    locals.children = vec![selected, normal];
    let mut remote_header = frame("remote-header", "远程分支标题");
    remote_header.children.push(label("远程分支数量", "0"));
    let mut origin = frame("origin-header", "origin 远程仓库");
    let mut origin_arrow = icon("origin-chevron", "chevron-down", "#9198a1", 12.);
    origin_arrow.name = "展开 origin".into();
    origin.children = vec![
        origin_arrow,
        label("远程仓库名称", "origin"),
        label("远程平台", "GitHub"),
    ];
    let mut remote = frame("remote-row", "origin/main 远程分支");
    remote.children.push(label("origin/main 分支名称", "main"));
    let mut remotes = frame("remote-section", "远程分支分区");
    remotes.children = vec![remote_header, origin, remote];
    design.children = vec![branch_control, local_header, locals, remotes];
    design
}

fn branch(name: &str, remote: bool, current: bool) -> BranchInfo {
    BranchInfo {
        name: name.into(),
        commit_id: "fixture".into(),
        subject: String::new(),
        is_current: current,
        upstream: None,
        is_remote: remote,
    }
}

fn details() -> RepositoryDetails {
    RepositoryDetails {
        worktrees: vec![],
        branches: vec![
            branch("main", false, true),
            branch("feature/Worktree", false, false),
            branch("功能/搜索", false, false),
            branch("origin/main", true, false),
            branch("origin/feature/Worktree", true, false),
            branch("upstream/main", true, false),
        ],
    }
}

fn data<'a>(details: &'a RepositoryDetails, expanded: &'a HashSet<String>) -> WorkbenchData<'a> {
    WorkbenchData {
        native_preview: true,
        repository: None,
        details: Some(details),
        tree: None,
        selected_submodule: None,
        changes_repository: None,
        changes_details: None,
        changes_tree: None,
        tree_mode: false,
        collapsed_paths: expanded,
        expanded_remotes: expanded,
        diff: None,
        selected_file: None,
        selected_staged: false,
        opened_files: &[],
        active_tab: None,
        file_query: "",
        repo_query: "",
        loading: false,
        history: false,
        unified: false,
        ignore_whitespace: false,
    }
}

#[test]
fn local_checkout_actions_preserve_full_names_and_the_branch_dropdown_is_active() {
    let details = details();
    let mut design = design();
    let mut actions = HashMap::new();
    populate_branches(&mut design, &data(&details, &HashSet::new()), &mut actions);
    let locals = find(&design, "本地分支列表").unwrap();
    assert_eq!(locals.children.len(), 3);
    for (row, expected) in locals
        .children
        .iter()
        .zip(["main", "feature/Worktree", "功能/搜索"])
    {
        assert!(matches!(
            actions.get(&row.id),
            Some(WorkbenchAction::CheckoutBranch { name, remote: false, .. }) if name == expected
        ));
    }
    assert_eq!(locals.children[0].fill.as_deref(), Some("#1a3453"));
    assert!(locals.children[1].fill.is_none());
    for name in ["当前分支与同步状态", "分支切换"] {
        assert!(matches!(
            actions.get(&find(&design, name).unwrap().id),
            Some(WorkbenchAction::OpenBranchMenu)
        ));
    }
}

#[test]
fn all_remotes_start_collapsed_without_hidden_branch_actions() {
    let details = details();
    let mut design = design();
    let mut actions = HashMap::new();
    populate_branches(&mut design, &data(&details, &HashSet::new()), &mut actions);
    let remotes = find(&design, "远程分支分区").unwrap();
    assert_eq!(remotes.children.len(), 3);
    for group in &remotes.children[1..] {
        assert_eq!(group.children.len(), 1);
        let header = &group.children[0];
        assert_eq!(header.children[0].icon.as_deref(), Some("chevron-right"));
        assert!(matches!(
            actions.get(&header.id),
            Some(WorkbenchAction::ToggleSection(name)) if name == &header.name
        ));
    }
    assert!(
        !actions
            .values()
            .any(|action| matches!(action, WorkbenchAction::CheckoutBranch { remote: true, .. }))
    );
    assert_eq!(
        find(&design, "远程分支数量").unwrap().content.as_deref(),
        Some("3")
    );
}

#[test]
fn expanding_one_remote_exposes_its_full_checkout_targets_and_keeps_other_remotes_closed() {
    let details = details();
    let expanded = HashSet::from(["origin".into()]);
    let mut design = design();
    let mut actions = HashMap::new();
    populate_branches(&mut design, &data(&details, &expanded), &mut actions);
    let remotes = find(&design, "远程分支分区").unwrap();
    let origin = &remotes.children[1];
    assert_eq!(origin.children[0].name, "实际远程仓库 origin");
    assert_eq!(origin.children.len(), 3);
    assert_eq!(
        origin.children[0].children[0].icon.as_deref(),
        Some("chevron-down")
    );
    for (row, expected) in origin.children[1..]
        .iter()
        .zip(["origin/main", "origin/feature/Worktree"])
    {
        assert!(matches!(
            actions.get(&row.id),
            Some(WorkbenchAction::CheckoutBranch { name, remote: true, .. }) if name == expected
        ));
    }
    assert_eq!(
        origin.children[2].children[0].content.as_deref(),
        Some("feature/Worktree")
    );
    assert_eq!(remotes.children[2].children.len(), 1);
}

#[test]
fn sidebar_search_reveals_matching_remote_branches_without_mutating_expansion_state() {
    let details = details();
    let expanded = HashSet::new();
    let mut data = data(&details, &expanded);
    data.repo_query = "  WORKtree  ";
    let mut design = design();
    let mut actions = HashMap::new();
    populate_branches(&mut design, &data, &mut actions);
    assert_eq!(find(&design, "本地分支列表").unwrap().children.len(), 1);
    let remotes = find(&design, "远程分支分区").unwrap();
    assert_eq!(remotes.children.len(), 2);
    let origin = &remotes.children[1];
    assert_eq!(origin.children.len(), 2);
    assert!(matches!(
        actions.get(&origin.children[1].id),
        Some(WorkbenchAction::CheckoutBranch { name, remote: true, .. }) if name == "origin/feature/Worktree"
    ));
    assert!(expanded.is_empty());
    assert_eq!(
        find(&design, "本地分支数量").unwrap().content.as_deref(),
        Some("3")
    );
    assert_eq!(
        find(&design, "远程分支数量").unwrap().content.as_deref(),
        Some("3")
    );

    data.repo_query = "搜索";
    let mut design = self::design();
    populate_branches(&mut design, &data, &mut HashMap::new());
    assert_eq!(find(&design, "本地分支列表").unwrap().children.len(), 1);
    assert_eq!(find(&design, "远程分支分区").unwrap().children.len(), 1);

    data.repo_query = "";
    let mut design = self::design();
    populate_branches(&mut design, &data, &mut HashMap::new());
    assert_eq!(
        find(&design, "远程分支分区").unwrap().children[1]
            .children
            .len(),
        1
    );
}
