//! Native worktree management. The workspace performs all Git operations in the background.

use std::path::{Path, PathBuf};

use gitviews_service::{
    RepositoryDetails, WorktreeAction, WorktreeInfo, display_path, paths_equal,
};
use gpui_kit::{
    TestSupportExt as _,
    base::FocusTrapElement as _,
    component::{
        ActiveTheme, Disableable as _, ThemeMode,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        input::{Input, InputEvent, InputState},
    },
    prelude::FluentBuilder as _,
    *,
};

#[cfg(test)]
mod headless_tests;

gpui_kit::actions!(gitviews_worktrees, [CloseWorktreeDialog]);

pub(crate) enum WorktreeDialogEvent {
    Apply(WorktreeAction),
    Reload,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Create,
    Remove,
    Move,
    Lock,
    Unlock,
    Prune,
    Repair,
}

impl Mode {
    const ALL: [Self; 7] = [
        Self::Create,
        Self::Remove,
        Self::Move,
        Self::Lock,
        Self::Unlock,
        Self::Prune,
        Self::Repair,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Create => "创建",
            Self::Remove => "移除",
            Self::Move => "移动",
            Self::Lock => "锁定",
            Self::Unlock => "解锁",
            Self::Prune => "清理登记",
            Self::Repair => "修复路径",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Create => "worktree-mode-create",
            Self::Remove => "worktree-mode-remove",
            Self::Move => "worktree-mode-move",
            Self::Lock => "worktree-mode-lock",
            Self::Unlock => "worktree-mode-unlock",
            Self::Prune => "worktree-mode-prune",
            Self::Repair => "worktree-mode-repair",
        }
    }

    fn destructive(self) -> bool {
        matches!(self, Self::Remove | Self::Prune)
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Create => "填写新工作树的绝对目录和本地分支。新建分支从当前 HEAD 创建。",
            Self::Remove => {
                "移除所选工作树的目录和登记。默认保护未提交、未跟踪和忽略文件；可勾选强制移除并再次确认，丢弃工作树及子模块中的这些文件。子模块 Git 仓库数据仍会保留。"
            }
            Self::Move => {
                "移动所选工作树的目录并更新 Git 登记。目标目录必须尚不存在；包含子模块的工作树暂不支持移动。"
            }
            Self::Lock => "锁定登记，防止目录暂时离线时被清理。可填写锁定原因。",
            Self::Unlock => "解除所选工作树的登记锁定，保留目录和文件内容。",
            Self::Prune => "只清理所选失效的 Git 工作树登记，不删除任何目录或文件。",
            Self::Repair => "填写已经手动移动后的实际工作树绝对路径，修复 Git 的双向路径关联。",
        }
    }
}

#[derive(Clone, Copy)]
enum Field {
    Path,
    Branch,
    Destination,
    Reason,
}

impl Field {
    const ALL: [Self; 4] = [Self::Path, Self::Branch, Self::Destination, Self::Reason];

    fn id(self) -> &'static str {
        match self {
            Self::Path => "worktree-create-path",
            Self::Branch => "worktree-branch",
            Self::Destination => "worktree-destination",
            Self::Reason => "worktree-lock-reason",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Self::Path => "新工作树的绝对目录，支持中文路径",
            Self::Branch => "填写本地分支名称",
            Self::Destination => "目标绝对路径",
            Self::Reason => "可选，例如：移动磁盘暂时离线",
        }
    }
}

struct FormInputs {
    values: Vec<Entity<InputState>>,
    _subscriptions: Vec<Subscription>,
}

pub(crate) struct WorktreeDialog {
    open: bool,
    root: PathBuf,
    details: RepositoryDetails,
    selected: Option<PathBuf>,
    mode: Option<Mode>,
    create_branch: bool,
    force_remove: bool,
    inputs: Option<FormInputs>,
    busy: bool,
    confirming: bool,
    error: Option<String>,
    validation: Option<String>,
    previous_focus: Option<FocusHandle>,
    focus: FocusHandle,
}

impl EventEmitter<WorktreeDialogEvent> for WorktreeDialog {}

impl WorktreeDialog {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseWorktreeDialog,
            Some("GitViewsWorktrees"),
        )]);
        Self {
            open: false,
            root: PathBuf::new(),
            details: RepositoryDetails::default(),
            selected: None,
            mode: None,
            create_branch: true,
            force_remove: false,
            inputs: None,
            busy: false,
            confirming: false,
            error: None,
            validation: None,
            previous_focus: None,
            focus: cx.focus_handle(),
        }
    }

    pub(crate) fn show(
        &mut self,
        root: PathBuf,
        details: RepositoryDetails,
        cx: &mut Context<Self>,
    ) {
        self.open = true;
        self.selected = details
            .worktrees
            .iter()
            .find(|worktree| paths_equal(&worktree.path, &root))
            .or_else(|| details.worktrees.first())
            .map(|worktree| worktree.path.clone());
        self.root = root;
        self.details = details;
        self.mode = None;
        self.create_branch = true;
        self.force_remove = false;
        self.inputs = None;
        self.busy = false;
        self.confirming = false;
        self.error = None;
        self.validation = None;
        cx.notify();
    }

    pub(crate) fn start_create(&mut self, cx: &mut Context<Self>) {
        self.choose_mode(Mode::Create, cx);
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        if busy {
            self.error = None;
        }
        cx.notify();
    }

    pub(crate) fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.busy = false;
        self.error = error;
        // Preserve the draft after failures; a new explicit confirmation is required on retry.
        self.confirming = false;
        cx.notify();
    }

    pub(crate) fn replace_details(&mut self, details: RepositoryDetails, cx: &mut Context<Self>) {
        self.refresh_details(details, cx);
        self.mode = None;
        self.force_remove = false;
        self.inputs = None;
        cx.notify();
    }

    /// Update background metadata without replacing an in-progress form. A removed target
    /// invalidates its action, so a fallback selection can never inherit that old action.
    pub(crate) fn refresh_details(&mut self, details: RepositoryDetails, cx: &mut Context<Self>) {
        self.details = details;
        let selection_exists = self.details.worktrees.iter().any(|worktree| {
            self.selected
                .as_ref()
                .is_some_and(|selected| paths_equal(selected, &worktree.path))
        });
        if !selection_exists {
            self.selected = self
                .details
                .worktrees
                .iter()
                .find(|worktree| paths_equal(&worktree.path, &self.root))
                .or_else(|| self.details.worktrees.first())
                .map(|worktree| worktree.path.clone());
            if self.mode != Some(Mode::Create) {
                self.mode = None;
                self.force_remove = false;
                self.inputs = None;
            }
        }
        self.busy = false;
        self.confirming = false;
        self.validation = None;
        self.error = None;
        cx.notify();
    }

    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.force_remove = false;
        self.inputs = None;
        self.busy = false;
        self.confirming = false;
        self.error = None;
        self.validation = None;
        self.previous_focus = None;
        cx.notify();
    }

    fn selected_worktree(&self) -> Option<&WorktreeInfo> {
        self.selected.as_ref().and_then(|path| {
            self.details
                .worktrees
                .iter()
                .find(|worktree| paths_equal(&worktree.path, path))
        })
    }

    fn mode_available(&self, mode: Mode) -> bool {
        if self.busy || self.confirming {
            return false;
        }
        if mode == Mode::Create {
            return true;
        }
        let Some(worktree) = self.selected_worktree() else {
            return false;
        };
        let current = paths_equal(&worktree.path, &self.root);
        match mode {
            Mode::Create => true,
            Mode::Remove | Mode::Move => {
                !worktree.is_main && !current && !worktree.locked && !worktree.prunable
            }
            Mode::Lock => !worktree.is_main && !worktree.locked && !worktree.prunable,
            Mode::Unlock => !worktree.is_main && worktree.locked,
            Mode::Prune => !worktree.is_main && !current && !worktree.locked && worktree.prunable,
            Mode::Repair => !worktree.is_main && !current && !worktree.locked,
        }
    }

    fn choose_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        if !self.mode_available(mode) {
            return;
        }
        self.mode = Some(mode);
        self.force_remove = false;
        self.confirming = false;
        self.inputs = None;
        self.validation = None;
        self.error = None;
        cx.notify();
    }

    fn ensure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.previous_focus.is_none() {
            self.previous_focus = window.focused(cx);
            self.focus.focus(window, cx);
        }
        if self.inputs.is_some() {
            return;
        }
        // Rebuilding an action's controls also removes its old input focus handles.
        self.focus.focus(window, cx);
        let mut subscriptions = Vec::with_capacity(Field::ALL.len());
        let values = Field::ALL
            .into_iter()
            .map(|field| {
                let input =
                    cx.new(|cx| InputState::new(window, cx).placeholder(field.placeholder()));
                subscriptions.push(cx.subscribe_in(
                    &input,
                    window,
                    |this, _, event: &InputEvent, _, cx| match event {
                        InputEvent::Change => {
                            this.validation = None;
                            this.error = None;
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } => this.submit(cx),
                        _ => {}
                    },
                ));
                input
            })
            .collect();
        self.inputs = Some(FormInputs {
            values,
            _subscriptions: subscriptions,
        });
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if self.confirming {
            self.confirming = false;
            self.focus.focus(window, cx);
            cx.notify();
            return;
        }
        let previous = self.previous_focus.take();
        self.clear(cx);
        if let Some(previous) = previous {
            previous.focus(window, cx);
        }
        cx.emit(WorktreeDialogEvent::Close);
    }

    fn build_action(&self, cx: &App) -> Result<WorktreeAction, String> {
        let mode = self.mode.ok_or("请选择要执行的操作。")?;
        let value = |field: Field| {
            self.inputs.as_ref().map_or_else(String::new, |inputs| {
                inputs.values[field as usize].read(cx).value().to_string()
            })
        };
        let absolute = |field: Field| {
            let path = PathBuf::from(value(field));
            if path.is_absolute() {
                Ok(path)
            } else {
                Err("请填写完整的绝对目录路径。".to_owned())
            }
        };
        if mode == Mode::Create {
            let path = absolute(Field::Path)?;
            let branch = value(Field::Branch).trim().to_owned();
            if branch.is_empty() {
                return Err("请填写明确的本地分支名称。".into());
            }
            return Ok(WorktreeAction::Create {
                path,
                branch,
                create_branch: self.create_branch,
            });
        }
        let path = self
            .selected_worktree()
            .ok_or("请选择一个工作树。")?
            .path
            .clone();
        Ok(match mode {
            Mode::Create => unreachable!(),
            Mode::Remove => WorktreeAction::Remove {
                path,
                force: self.force_remove,
            },
            Mode::Move => WorktreeAction::Move {
                path,
                destination: absolute(Field::Destination)?,
            },
            Mode::Lock => WorktreeAction::Lock {
                path,
                reason: value(Field::Reason).trim().to_owned(),
            },
            Mode::Unlock => WorktreeAction::Unlock { path },
            Mode::Prune => WorktreeAction::Prune { path },
            Mode::Repair => WorktreeAction::Repair {
                path,
                destination: absolute(Field::Destination)?,
            },
        })
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.confirming {
            return;
        }
        let Some(mode) = self.mode else {
            return;
        };
        if !self.mode_available(mode) {
            self.validation = Some("当前工作树不允许执行此操作，请重新读取列表。".into());
            cx.notify();
            return;
        }
        match self.build_action(cx) {
            Ok(action) => {
                self.validation = None;
                self.error = None;
                if mode.destructive() {
                    self.confirming = true;
                    cx.notify();
                } else {
                    self.set_busy(true, cx);
                    cx.emit(WorktreeDialogEvent::Apply(action));
                }
            }
            Err(error) => {
                self.validation = Some(error);
                cx.notify();
            }
        }
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if !self.confirming || self.busy {
            return;
        }
        match self.build_action(cx) {
            Ok(action) => {
                self.set_busy(true, cx);
                cx.emit(WorktreeDialogEvent::Apply(action));
            }
            Err(error) => {
                self.confirming = false;
                self.validation = Some(error);
                cx.notify();
            }
        }
    }

    fn input_row(&self, field: Field, label: &'static str) -> Div {
        let row = div().flex().flex_col().gap(px(6.)).child(label);
        let Some(inputs) = &self.inputs else {
            return row;
        };
        row.child(
            Input::new(&inputs.values[field as usize])
                .id(field.id())
                .w_full()
                .h(px(32.))
                .tab_index(20 + field as isize)
                .disabled(self.busy || self.confirming),
        )
    }
}

impl Render for WorktreeDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        self.ensure_inputs(window, cx);
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let muted: Hsla = rgb(if light { 0x59636e } else { 0x9198a1 }).into();
        let error_color = rgb(if light { 0xcf222e } else { 0xf85149 });
        let unavailable = self.busy || self.confirming;
        let mut actions = div().flex().flex_wrap().gap(px(6.));
        for (index, mode) in Mode::ALL.into_iter().enumerate() {
            actions = actions.child(
                Button::new(mode.id())
                    .label(mode.label())
                    .when(self.mode == Some(mode), |button| button.primary())
                    .disabled(!self.mode_available(mode))
                    .tab_index(index as isize + 10)
                    .on_click(cx.listener(move |this, _, _, cx| this.choose_mode(mode, cx))),
            );
        }
        let worktrees = self.details.worktrees.clone();
        let current = self.root.clone();
        let selected = self.selected.clone();
        let entity = cx.entity();
        let count = worktrees.len();
        let list = uniform_list("worktree-manager-list", count, move |range, _, _| {
            range
                .map(|index| {
                    let worktree = &worktrees[index];
                    let path = worktree.path.clone();
                    let active = selected
                        .as_ref()
                        .is_some_and(|selected| paths_equal(selected, &path));
                    let target = entity.clone();
                    let flags = worktree_flags(worktree, &current);
                    let title = format!(
                        "{} · {}  {}",
                        path.file_name().map_or_else(
                            || display_path(&path),
                            |name| name.to_string_lossy().into_owned(),
                        ),
                        worktree.branch.as_deref().unwrap_or(if worktree.is_bare {
                            "裸仓库"
                        } else if worktree.prunable {
                            "目录缺失 / 登记失效"
                        } else if !worktree.is_detached {
                            "分支未读取"
                        } else {
                            "分离 HEAD"
                        }),
                        flags,
                    );
                    Button::new(("worktree-select", index))
                        .w_full()
                        .h(px(62.))
                        .ghost()
                        .when(active, |button| button.primary())
                        .disabled(unavailable)
                        .tab_index(index as isize)
                        .accessibility_label(format!(
                            "选择工作树 {} {}",
                            display_path(&path),
                            flags
                        ))
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .items_start()
                                .gap(px(4.))
                                .child(div().w_full().truncate().child(title))
                                .child(
                                    div()
                                        .w_full()
                                        .truncate()
                                        .text_size(px(11.))
                                        .text_color(muted)
                                        .child(display_path(&path)),
                                ),
                        )
                        .on_click(move |_, _, cx| {
                            target.update(cx, |this, cx| {
                                if this.busy || this.confirming {
                                    return;
                                }
                                this.selected = Some(path.clone());
                                this.mode = None;
                                this.force_remove = false;
                                this.confirming = false;
                                this.inputs = None;
                                this.error = None;
                                this.validation = None;
                                cx.notify();
                            });
                        })
                })
                .collect::<Vec<_>>()
        })
        .h(px((count.clamp(1, 3) * 62) as f32))
        .w_full();
        let mut form = div().flex().flex_col().gap(px(12.));
        if let Some(mode) = self.mode {
            form = form.child(
                div()
                    .text_size(px(12.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{}工作树", mode.label())),
            );
            if mode != Mode::Create
                && let Some(path) = &self.selected
            {
                form = form.child(
                    div()
                        .id("worktree-selected-path")
                        .test_support()
                        .aria_label(display_path(path))
                        .text_size(px(11.))
                        .child(display_path(path)),
                );
            }
            form = form.child(div().text_color(muted).child(mode.hint()));
            form = match mode {
                Mode::Create => form
                    .child(self.input_row(Field::Path, "新工作树目录"))
                    .child(div().flex().gap(px(6.)).children(
                        [(true, "新建分支"), (false, "使用已有分支")].map(|(create, label)| {
                            Button::new(if create {
                                "worktree-branch-new"
                            } else {
                                "worktree-branch-existing"
                            })
                            .label(label)
                            .when(self.create_branch == create, |button| button.primary())
                            .disabled(unavailable)
                            .tab_index(if create { 18 } else { 19 })
                            .on_click(cx.listener(
                                move |this, _, _, cx| {
                                    if !this.busy && !this.confirming {
                                        this.create_branch = create;
                                        this.validation = None;
                                        cx.notify();
                                    }
                                },
                            ))
                        }),
                    ))
                    .child(self.input_row(
                        Field::Branch,
                        if self.create_branch {
                            "新分支名称"
                        } else {
                            "已有本地分支名称（不能已被其他工作树占用）"
                        },
                    )),
                Mode::Move => form.child(self.input_row(Field::Destination, "移动到新目录")),
                Mode::Remove => form.child(
                    Checkbox::new("worktree-force-remove")
                        .label("强制移除（丢弃未提交、未跟踪和忽略文件）")
                        .checked(self.force_remove)
                        .disabled(self.busy)
                        .tab_index(24)
                        .on_change(cx.listener(|this, checked, _, cx| {
                            if !this.busy {
                                this.force_remove = *checked;
                                this.confirming = false;
                                this.validation = None;
                                this.error = None;
                                cx.notify();
                            }
                        })),
                ),
                Mode::Repair => form.child(self.input_row(Field::Destination, "移动后的实际目录")),
                Mode::Lock => form.child(self.input_row(Field::Reason, "锁定原因")),
                _ => form,
            };
        } else {
            form = form.child(div().text_color(muted).child(
                "选择工作树和操作。主工作树、当前工作树不能移除或移动；锁定的工作树须先解锁。",
            ));
        }
        if self.confirming {
            let mode = self.mode.expect("confirmation has an action");
            let force_remove = mode == Mode::Remove && self.force_remove;
            let path = self.selected.as_deref().unwrap_or_else(|| Path::new(""));
            let confirmation = if force_remove {
                format!(
                    "确认强制移除工作树 {}？工作树及其子模块的未提交改动、未跟踪和忽略文件将被永久丢弃，目录及 Git 登记将被移除。子模块 Git 仓库数据会保留，但不能恢复未保存到 Git 的文件内容。",
                    display_path(path)
                )
            } else if mode == Mode::Remove {
                format!(
                    "确认移除工作树 {}？该目录及其 Git 登记将被移除。子模块会先递归检查，Git 仓库数据将保留；有未提交、未跟踪或忽略文件时停止操作。",
                    display_path(path)
                )
            } else {
                format!(
                    "确认清理工作树 {} 的失效登记？仅清理 Git 登记，不删除目录或文件。",
                    display_path(path)
                )
            };
            form = form.child(
                div()
                    .id("worktree-confirmation")
                    .test_support()
                    .role(Role::Alert)
                    .aria_label(confirmation.clone())
                    .p(px(12.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(error_color)
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(confirmation)
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.))
                            .child(
                                Button::new("worktree-confirm-cancel")
                                    .label("返回")
                                    .disabled(self.busy)
                                    .tab_index(26)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        if !this.busy {
                                            this.confirming = false;
                                            this.focus.focus(window, cx);
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(
                                Button::new("worktree-confirm-apply")
                                    .label(if self.busy {
                                        "执行中…"
                                    } else if force_remove {
                                        "确认丢弃改动并强制移除"
                                    } else {
                                        "确认执行"
                                    })
                                    .primary()
                                    .when(force_remove, |button| button.danger())
                                    .disabled(self.busy)
                                    .tab_index(27)
                                    .on_click(cx.listener(|this, _, _, cx| this.confirm(cx))),
                            ),
                    ),
            );
        }
        let error = self.validation.as_ref().or(self.error.as_ref()).cloned();
        let body = div()
            .id("worktree-manager-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(22.))
            .py(px(14.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(list)
            .when(count == 0, |body| {
                body.child(div().text_color(muted).child("未发现已登记的工作树。"))
            })
            .child(actions)
            .child(form);
        let card = div()
            .w(px(820.))
            .h(px(750.))
            .max_w(relative(0.94))
            .max_h(relative(0.94))
            .flex()
            .flex_col()
            .rounded(px(8.))
            .overflow_hidden()
            .border_1()
            .border_color(border)
            .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
            .text_color(rgb(if light { 0x1f2328 } else { 0xf0f6fc }))
            .shadow_lg()
            .child(
                div()
                    .px(px(22.))
                    .py(px(16.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("管理工作树"),
                    )
                    .child(
                        div()
                            .id("worktree-root-path")
                            .test_support()
                            .aria_label(display_path(&self.root))
                            .mt(px(6.))
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(display_path(&self.root)),
                    ),
            )
            .child(body)
            .when_some(error, |card, error| {
                card.child(
                    div()
                        .id("worktree-error")
                        .test_support()
                        .role(Role::Alert)
                        .aria_label(error.clone())
                        .max_h(px(96.))
                        .overflow_y_scroll()
                        .px(px(22.))
                        .pb(px(10.))
                        .text_color(error_color)
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_between()
                    .gap(px(8.))
                    .px(px(22.))
                    .py(px(14.))
                    .border_t_1()
                    .border_color(border)
                    .child(
                        Button::new("worktree-reload")
                            .label("重新读取")
                            .disabled(unavailable)
                            .tab_index(30)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.busy && !this.confirming {
                                    this.set_busy(true, cx);
                                    cx.emit(WorktreeDialogEvent::Reload);
                                }
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                Button::new("worktree-close")
                                    .label("关闭")
                                    .disabled(self.busy)
                                    .tab_index(31)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.close(window, cx)),
                                    ),
                            )
                            .child(
                                Button::new("worktree-apply")
                                    .label(if self.busy {
                                        "执行中…"
                                    } else if self.mode == Some(Mode::Remove) && self.force_remove {
                                        "强制移除…"
                                    } else {
                                        "执行操作"
                                    })
                                    .primary()
                                    .disabled(unavailable || self.mode.is_none())
                                    .tab_index(32)
                                    .on_click(cx.listener(|this, _, _, cx| this.submit(cx))),
                            ),
                    ),
            );
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x00000088))
            .key_context("GitViewsWorktrees")
            .track_focus(&self.focus)
            .tab_group()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .font_family("Noto Sans SC")
            .on_action(
                cx.listener(|this, _: &CloseWorktreeDialog, window, cx| this.close(window, cx)),
            )
            .child(card)
            .focus_trap("gitviews-worktrees-trap", &self.focus)
            .into_any_element()
    }
}

fn worktree_flags(worktree: &WorktreeInfo, current: &Path) -> String {
    let mut flags = Vec::new();
    if worktree.is_main {
        flags.push("主工作树");
    }
    if paths_equal(&worktree.path, current) {
        flags.push("当前");
    }
    if worktree.locked {
        flags.push("已锁定");
    }
    if worktree.prunable {
        flags.push("可清理");
    }
    flags.join(" · ")
}
