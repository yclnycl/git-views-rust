//! Stash manager. Workspace owns background reads and mutations.

use gitviews_service::{StashAction, StashEntry, StashRequest, display_path};
use gpui_kit::{
    TestSupportExt as _,
    base::FocusTrapElement as _,
    component::{
        ActiveTheme, Disableable as _, ThemeMode,
        button::{Button, ButtonVariants as _},
        checkbox::Checkbox,
        input::{Input, InputState},
    },
    prelude::FluentBuilder as _,
    *,
};
use std::path::PathBuf;

#[cfg(test)]
mod headless_tests;

gpui_kit::actions!(gitviews_stashes, [CloseStashDialog]);

pub(crate) enum StashDialogEvent {
    Apply(StashRequest),
    Inspect { id: String },
    Reload,
    Close,
}

pub(crate) struct StashDialog {
    open: bool,
    root: PathBuf,
    name: String,
    entries: Vec<StashEntry>,
    expected_head: Option<String>,
    selected: Option<String>,
    message: Option<Entity<InputState>>,
    include_untracked: bool,
    include_ignored: bool,
    reinstate_index: bool,
    busy: bool,
    loaded: bool,
    confirming_drop: Option<String>,
    error: Option<String>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
}
impl EventEmitter<StashDialogEvent> for StashDialog {}

impl StashDialog {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseStashDialog,
            Some("GitViewsStashes"),
        )]);
        Self {
            open: false,
            root: PathBuf::new(),
            name: String::new(),
            entries: Vec::new(),
            expected_head: None,
            selected: None,
            message: None,
            include_untracked: false,
            include_ignored: false,
            reinstate_index: true,
            busy: false,
            loaded: false,
            confirming_drop: None,
            error: None,
            focus: cx.focus_handle(),
            previous_focus: None,
        }
    }
    pub(crate) fn show(&mut self, root: PathBuf, name: String, cx: &mut Context<Self>) {
        self.clear(cx);
        self.open = true;
        self.root = root;
        self.name = name;
        self.busy = true;
        cx.notify();
    }
    pub(crate) fn set_entries(
        &mut self,
        entries: Vec<StashEntry>,
        expected_head: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if !entries
            .iter()
            .any(|entry| Some(&entry.id) == self.selected.as_ref())
        {
            self.selected = None;
        }
        self.entries = entries;
        self.expected_head = expected_head;
        self.loaded = true;
        self.busy = false;
        self.confirming_drop = None;
        self.error = None;
        cx.notify();
    }
    pub(crate) fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.error = error;
        self.busy = false;
        self.confirming_drop = None;
        cx.notify();
    }
    pub(crate) fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        self.busy = busy;
        if busy {
            self.error = None;
        }
        cx.notify();
    }
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.entries.clear();
        self.selected = None;
        self.expected_head = None;
        self.message = None;
        self.loaded = false;
        self.busy = false;
        self.confirming_drop = None;
        self.include_untracked = false;
        self.include_ignored = false;
        self.reinstate_index = true;
        self.error = None;
        self.previous_focus = None;
        cx.notify();
    }
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let previous = self.previous_focus.take();
        self.clear(cx);
        if let Some(focus) = previous {
            focus.focus(window, cx);
        }
        cx.emit(StashDialogEvent::Close);
    }
    fn submit(&mut self, action: StashAction, cx: &mut Context<Self>) {
        if self.busy || !self.loaded {
            return;
        }
        self.confirming_drop = None;
        self.set_busy(true, cx);
        cx.emit(StashDialogEvent::Apply(StashRequest {
            expected_head: self.expected_head.clone(),
            action,
        }));
    }
    fn selected_id(&self) -> Option<String> {
        self.selected
            .as_ref()
            .filter(|id| self.entries.iter().any(|entry| &entry.id == *id))
            .cloned()
    }
}

impl Render for StashDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        if self.previous_focus.is_none() {
            self.previous_focus = window.focused(cx);
            self.focus.focus(window, cx);
        }
        if self.message.is_none() {
            self.message = Some(cx.new(|cx| {
                InputState::new(window, cx).placeholder("可选，例如：切换分支前保存调试改动")
            }));
        }
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let muted: Hsla = rgb(if light { 0x59636e } else { 0x9198a1 }).into();
        let unavailable = self.busy || !self.loaded || self.confirming_drop.is_some();
        let mut list = div()
            .id("stash-entry-list")
            .test_support()
            .flex()
            .flex_col()
            .gap(px(6.));
        if self.entries.is_empty() {
            list = list.child(div().py(px(20.)).text_color(muted).child(if self.busy {
                "正在读取储藏…"
            } else {
                "当前仓库没有储藏记录。"
            }));
        }
        for (index, entry) in self.entries.iter().enumerate() {
            let id = entry.id.clone();
            let label = format!(
                "stash@{{{}}} · {} · {}",
                entry.index,
                &entry.id[..entry.id.len().min(8)],
                entry.message
            );
            list = list.child(
                Button::new(("stash-entry", index))
                    .label(label)
                    .w_full()
                    .when(self.selected.as_ref() == Some(&id), |button| {
                        button.primary()
                    })
                    .disabled(unavailable)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.busy || this.confirming_drop.is_some() {
                            return;
                        }
                        this.selected = Some(id.clone());
                        this.error = None;
                        cx.notify();
                    })),
            );
        }
        let selected = self.selected_id().is_some();
        let form = div()
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("保存当前改动"),
            )
            .child(
                Input::new(self.message.as_ref().expect("stash input"))
                    .id("stash-message")
                    .disabled(unavailable),
            )
            .child(
                Checkbox::new("stash-include-untracked")
                    .label("包含未跟踪文件")
                    .checked(self.include_untracked)
                    .disabled(unavailable)
                    .on_change(cx.listener(|this, value, _, cx| {
                        if !this.busy {
                            this.include_untracked = *value;
                            cx.notify();
                        }
                    })),
            )
            .child(
                Checkbox::new("stash-include-ignored")
                    .label("包含忽略文件")
                    .checked(self.include_ignored)
                    .disabled(unavailable)
                    .on_change(cx.listener(|this, value, _, cx| {
                        if !this.busy {
                            this.include_ignored = *value;
                            cx.notify();
                        }
                    })),
            )
            .child(
                div()
                    .text_color(muted)
                    .child("默认保存已跟踪文件及暂存区。子模块内部的改动需进入子模块单独储藏。"),
            )
            .child(
                Button::new("stash-save")
                    .label("保存储藏")
                    .primary()
                    .disabled(unavailable || self.expected_head.is_none())
                    .on_click(cx.listener(|this, _, _, cx| {
                        let message = this
                            .message
                            .as_ref()
                            .map(|input| input.read(cx).value().to_string())
                            .unwrap_or_default();
                        this.submit(
                            StashAction::Save {
                                message,
                                include_untracked: this.include_untracked,
                                include_ignored: this.include_ignored,
                            },
                            cx,
                        );
                    })),
            )
            .child(
                div()
                    .mt(px(8.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("已有储藏"),
            )
            .child(
                div()
                    .id("stash-list-scroll")
                    .max_h(px(180.))
                    .overflow_y_scroll()
                    .child(list),
            )
            .child(
                Checkbox::new("stash-reinstate-index")
                    .label("恢复原有暂存状态")
                    .checked(self.reinstate_index)
                    .disabled(unavailable)
                    .on_change(cx.listener(|this, value, _, cx| {
                        if !this.busy {
                            this.reinstate_index = *value;
                            cx.notify();
                        }
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .child(
                        Button::new("stash-inspect")
                            .label("查看变更…")
                            .disabled(unavailable || !selected)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.busy
                                    && let Some(id) = this.selected_id()
                                {
                                    cx.emit(StashDialogEvent::Inspect { id });
                                }
                            })),
                    )
                    .child(
                        Button::new("stash-apply")
                            .label("恢复并保留记录")
                            .disabled(unavailable || !selected)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(id) = this.selected_id() {
                                    this.submit(
                                        StashAction::Apply {
                                            id,
                                            reinstate_index: this.reinstate_index,
                                        },
                                        cx,
                                    );
                                }
                            })),
                    )
                    .child(
                        Button::new("stash-pop")
                            .label("恢复并移除记录")
                            .disabled(unavailable || !selected)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(id) = this.selected_id() {
                                    this.submit(
                                        StashAction::Pop {
                                            id,
                                            reinstate_index: this.reinstate_index,
                                        },
                                        cx,
                                    );
                                }
                            })),
                    )
                    .child(
                        Button::new("stash-drop")
                            .label("删除记录…")
                            .disabled(unavailable || !selected)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if !this.busy {
                                    this.confirming_drop = this.selected_id();
                                    cx.notify();
                                }
                            })),
                    ),
            );
        let mut card = div()
            .w(px(780.))
            .h(px(780.))
            .max_w(relative(0.94))
            .max_h(relative(0.94))
            .rounded(px(8.))
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
            .text_color(rgb(if light { 0x1f2328 } else { 0xf0f6fc }))
            .flex()
            .flex_col()
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
                            .child(format!("储藏与恢复 · {}", self.name)),
                    )
                    .child(
                        div()
                            .id("stash-repository-path")
                            .test_support()
                            .aria_label(display_path(&self.root))
                            .mt(px(6.))
                            .text_color(muted)
                            .child(display_path(&self.root)),
                    ),
            )
            .child(
                div()
                    .id("stash-form-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(22.))
                    .py(px(16.))
                    .child(form),
            );
        if let Some(id) = self.confirming_drop.clone() {
            let message = format!(
                "确认删除储藏 {}？该记录保存的改动将无法从储藏列表恢复；当前工作区不会改变。",
                &id[..id.len().min(8)]
            );
            card = card.child(
                div()
                    .id("stash-drop-confirmation")
                    .test_support()
                    .role(Role::Alert)
                    .aria_label(message.clone())
                    .px(px(22.))
                    .pb(px(12.))
                    .child(message)
                    .child(
                        div()
                            .mt(px(10.))
                            .flex()
                            .gap(px(8.))
                            .child(Button::new("stash-drop-cancel").label("保留记录").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.confirming_drop = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("stash-drop-confirm")
                                    .label("确认删除储藏")
                                    .primary()
                                    .disabled(self.busy)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if this.confirming_drop.as_ref() == Some(&id) {
                                            this.submit(StashAction::Drop { id: id.clone() }, cx);
                                        }
                                    })),
                            ),
                    ),
            );
        }
        if let Some(error) = &self.error {
            card = card.child(
                div()
                    .id("stash-error")
                    .test_support()
                    .role(Role::Alert)
                    .aria_label(error.clone())
                    .px(px(22.))
                    .pb(px(12.))
                    .text_color(rgb(if light { 0xcf222e } else { 0xf85149 }))
                    .child(error.clone()),
            );
        }
        card = card.child(
            div()
                .px(px(22.))
                .py(px(14.))
                .border_t_1()
                .border_color(border)
                .flex()
                .justify_between()
                .child(
                    Button::new("stash-reload")
                        .label(if self.busy {
                            "处理中…"
                        } else {
                            "重新读取"
                        })
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if !this.busy {
                                this.confirming_drop = None;
                                this.set_busy(true, cx);
                                cx.emit(StashDialogEvent::Reload);
                            }
                        })),
                )
                .child(
                    Button::new("stash-close")
                        .label("关闭")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, window, cx| this.close(window, cx))),
                ),
        );
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x00000088))
            .key_context("GitViewsStashes")
            .track_focus(&self.focus)
            .tab_group()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .font_family("Noto Sans SC")
            .on_action(cx.listener(|this, _: &CloseStashDialog, window, cx| this.close(window, cx)))
            .child(card)
            .focus_trap("gitviews-stash-trap", &self.focus)
            .into_any_element()
    }
}
