use super::*;
use gpui_kit::{
    base::ResizeHandleState,
    component::resizable::{h_resizable, resizable_panel},
};

impl Workspace {
    pub(super) fn with_resizable_columns(
        &self,
        mut context: RenderContext,
        light: bool,
    ) -> RenderContext {
        let content_context = Rc::new(context.clone());
        let state = self.column_sizes.clone();
        let history = self.history;
        context.override_element = Some(Rc::new(move |node, window, cx| {
            if node.name != "三栏 Git 工作台" || node.children.len() != 3 {
                return content_context
                    .override_element
                    .as_ref()
                    .and_then(|render| render(node, window, cx));
            }

            if history {
                // History needs the full remaining width for parallel lanes and
                // commit metadata. Keep ordinary three-column sizes untouched.
                return Some(
                    div()
                        .id("history-workbench-columns")
                        .test_support()
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .w_full()
                        .flex()
                        .overflow_hidden()
                        .child(
                            div()
                                .w(px(248.))
                                .flex_shrink_0()
                                .min_h_0()
                                .overflow_hidden()
                                .child(design_renderer::render(
                                    &node.children[0],
                                    &content_context,
                                    window,
                                    cx,
                                )),
                        )
                        .child(div().flex_1().min_w_0().min_h_0().overflow_hidden().child(
                            design_renderer::render(
                                &node.children[2],
                                &content_context,
                                window,
                                cx,
                            ),
                        ))
                        .into_any_element(),
                );
            }

            let mut columns = h_resizable("workbench-columns")
                .with_state(&state)
                .with_handle_appearance(Rc::new(move |handle, _, _| {
                    let engaged =
                        handle.is_active() || handle.state() == ResizeHandleState::Hovered;
                    Some(
                        div()
                            .flex_none()
                            .w(px(1.))
                            .h_full()
                            // The exported panes already paint their borders.
                            // Only add paint while the pointer owns a divider.
                            .bg(if engaged {
                                rgba(if light { 0x0969daff } else { 0x4493f8ff })
                            } else {
                                rgba(0x00000000)
                            })
                            .into_any_element(),
                    )
                }));

            for (index, (child, (label, minimum))) in node
                .children
                .iter()
                .zip([("仓库栏", 200.), ("文件变更栏", 260.), ("代码预览栏", 600.)])
                .enumerate()
            {
                let initial_width = child.width.as_ref().and_then(DesignSize::pixels);
                let panel = resizable_panel()
                    .size_range(px(minimum)..Pixels::MAX)
                    .when_some(initial_width, |panel, width| panel.size(px(width)))
                    // Clip the content, not the panel: its drag handle reaches
                    // four pixels across the neighbouring pane's boundary.
                    .child(
                        div()
                            .id(("workbench-column", index))
                            .test_support()
                            .role(Role::Group)
                            .aria_label(label)
                            .size_full()
                            .min_w_0()
                            .min_h_0()
                            .overflow_hidden()
                            .child(design_renderer::render(child, &content_context, window, cx)),
                    );
                columns = columns.child(panel);
            }

            Some(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .child(columns)
                    .into_any_element(),
            )
        }));
        context
    }
}
