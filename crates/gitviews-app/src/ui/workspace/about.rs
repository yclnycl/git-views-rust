use super::*;

// Keep notices inside the executable when users distribute only that file.
const THIRD_PARTY_NOTICES: &str = include_str!("../../../assets/licenses/THIRD-PARTY-NOTICES.txt");

impl Workspace {
    pub(super) fn open_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        if self.license_editor.is_none() {
            self.license_editor = Some(cx.new(|cx| {
                let mut editor = EditorState::new(window, cx).soft_wrap(true);
                editor.set_readonly(true, cx);
                editor.set_value(THIRD_PARTY_NOTICES, window, cx);
                editor
            }));
        }
        self.show_about = true;
        cx.notify();
    }

    pub(super) fn about_panel(&self, light: bool, cx: &mut Context<Self>) -> AnyElement {
        let border = rgb(if light { 0xd0d7de } else { 0x3d444d });
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x00000088))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(920.))
                    .h(px(650.))
                    .max_w(relative(0.95))
                    .max_h(relative(0.95))
                    .flex()
                    .flex_col()
                    .rounded(px(8.))
                    .overflow_hidden()
                    .border_1()
                    .border_color(border)
                    .bg(rgb(if light { 0xffffff } else { 0x0d1117 }))
                    .shadow_lg()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px(px(18.))
                            .py(px(12.))
                            .border_b_1()
                            .border_color(border)
                            .child(
                                div()
                                    .child(concat!("关于 GitViews ", env!("CARGO_PKG_VERSION")))
                                    .child(
                                        div()
                                            .text_size(px(11.))
                                            .text_color(rgb(if light {
                                                0x59636e
                                            } else {
                                                0x9198a1
                                            }))
                                            .child("内嵌资源与第三方许可 · 可选择、复制和搜索"),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(px(16.))
                                    .child(
                                        div()
                                            .id("find-licenses")
                                            .role(Role::Button)
                                            .aria_label("搜索第三方许可")
                                            .cursor_pointer()
                                            .child("搜索许可")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                if let Some(editor) = &this.license_editor {
                                                    editor.update(cx, |editor, cx| {
                                                        editor.open_search(false, cx)
                                                    });
                                                    editor
                                                        .read(cx)
                                                        .focus_handle(cx)
                                                        .focus(window, cx);
                                                }
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id("close-about")
                                            .role(Role::Button)
                                            .aria_label("关闭关于窗口")
                                            .cursor_pointer()
                                            .child("关闭 ×")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.show_about = false;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .when_some(self.license_editor.clone(), |this, editor| {
                        this.child(
                            div().flex_1().min_h_0().child(
                                Editor::new(&editor)
                                    .readonly(true)
                                    .bordered(false)
                                    .appearance(false)
                                    .text_size(px(12.))
                                    .size_full(),
                            ),
                        )
                    }),
            )
            .into_any_element()
    }
}
