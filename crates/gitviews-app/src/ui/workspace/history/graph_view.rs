use super::*;
use gitviews_service::GraphReferenceKind;

const VISIBLE_LANES: usize = 24;

impl Workspace {
    pub(super) fn render_history_graph(&self, cx: &mut Context<Self>) -> Div {
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let muted = rgb(if light { 0x59636e } else { 0x9198a1 });
        let foreground = rgb(if light { 0x1f2328 } else { 0xe6edf3 });
        let selected_bg = rgb(if light { 0xddf4ff } else { 0x1a3453 });
        let lanes = self
            .history_state
            .graph_rows
            .iter()
            .map(|row| row.lanes)
            .max()
            .unwrap_or(1);
        let width = (lanes.clamp(3, VISIBLE_LANES) as f32 * 14. + 16.).max(70.);
        // The title always owns at least 180 px. Reference chips yield first;
        // unusually narrow parent panes can scroll this shared header/body.
        let minimum_width = width + 470.;
        let graph_root = self.history_state.root.clone();
        let graph_request = self.history_request_id;
        let snapshot_tips = Arc::new(
            self.history_state
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.tips.clone()),
        );
        let entity = cx.entity();
        let count = self.history_state.graph_rows.len();
        let commits = uniform_list("commit-history", count, move |range, _, cx| {
            let view = entity.read(cx);
            range
                .filter_map(|index| {
                    let row = view.history_state.graph_rows.get(index)?;
                    let commit = &row.commit;
                    let id = commit.id.clone();
                    let target = entity.clone();
                    let row_root = graph_root.clone();
                    let row_tips = Arc::clone(&snapshot_tips);
                    let mut labels = div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .flex_shrink(1.)
                        .min_w_0()
                        .max_w(px(120.))
                        .overflow_hidden();
                    if let Some(snapshot) = &view.history_state.snapshot {
                        for reference in snapshot
                            .references
                            .iter()
                            .filter(|reference| reference.target == id)
                            .take(2)
                        {
                            let (prefix, color) = match reference.kind {
                                GraphReferenceKind::Head => {
                                    ("", rgb(if light { 0x8250df } else { 0xd2a8ff }))
                                }
                                GraphReferenceKind::LocalBranch => {
                                    ("", rgb(if light { 0x116329 } else { 0x7ee787 }))
                                }
                                GraphReferenceKind::RemoteBranch => {
                                    ("↗ ", rgb(if light { 0x0550ae } else { 0x79c0ff }))
                                }
                                GraphReferenceKind::Tag => {
                                    ("◇ ", rgb(if light { 0x9a6700 } else { 0xe3b341 }))
                                }
                            };
                            labels = labels.child(
                                div()
                                    .id(format!("history-label-{index}-{}", reference.full_name))
                                    .test_support()
                                    .aria_label(format!("历史标签 {}", reference.full_name))
                                    .max_w(px(100.))
                                    .min_w_0()
                                    .flex_shrink(1.)
                                    .px(px(4.))
                                    .py(px(1.))
                                    .rounded(px(3.))
                                    .border_1()
                                    .border_color(color)
                                    .text_size(px(10.))
                                    .text_color(color)
                                    .truncate()
                                    .child(format!("{prefix}{}", reference.name)),
                            );
                        }
                    }
                    Some(
                        div()
                            .id(("commit", index))
                            .test_support()
                            .role(Role::Button)
                            .aria_label(format!(
                                "提交 {} {}",
                                short_id(&id),
                                display_text(&commit.subject, 160)
                            ))
                            .h(px(32.))
                            .w_full()
                            .min_w(px(minimum_width))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .pr(px(12.))
                            .cursor_pointer()
                            .overflow_hidden()
                            .text_size(px(12.))
                            .text_color(if row.matches { foreground } else { muted })
                            .when(
                                view.history_state.selected_id.as_ref() == Some(&id),
                                |this| this.bg(selected_bg),
                            )
                            .hover(|this| this.bg(rgb(if light { 0xf0f6fc } else { 0x212830 })))
                            .child(glyph(row, width, light))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(180.))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(labels)
                                    .child(
                                        div()
                                            .id(("history-subject", index))
                                            .test_support()
                                            .flex_1()
                                            .min_w(px(180.))
                                            .truncate()
                                            .when(!row.matches, |this| this.opacity(0.65))
                                            .child(display_text(&commit.subject, 2_000)),
                                    ),
                            )
                            .child(
                                div()
                                    .w(px(80.))
                                    .flex_shrink_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .text_color(muted)
                                    .child(display_text(&commit.author, 512)),
                            )
                            .child(
                                div()
                                    .w(px(102.))
                                    .flex_shrink_0()
                                    .truncate()
                                    .text_size(px(10.))
                                    .text_color(muted)
                                    .child({
                                        let date = format_utc(commit.time);
                                        date.get(..16).unwrap_or(&date).to_owned()
                                    }),
                            )
                            .child(
                                div()
                                    .id(("history-hash", index))
                                    .test_support()
                                    .w(px(62.))
                                    .flex_shrink_0()
                                    .text_size(px(11.))
                                    .text_color(muted)
                                    .child(short_id(&id).to_owned()),
                            )
                            .on_click(move |_, _, cx| {
                                target.update(cx, |this, cx| {
                                    if !this.history
                                        || this.history_request_id != graph_request
                                        || this.changes_repository().map(|repo| &repo.root)
                                            != row_root.as_ref()
                                        || this.history_state.root.as_ref() != row_root.as_ref()
                                        || this
                                            .history_state
                                            .snapshot
                                            .as_ref()
                                            .map(|snapshot| &snapshot.tips)
                                            != row_tips.as_ref().as_ref()
                                        || this
                                            .history_state
                                            .snapshot
                                            .as_ref()
                                            .map(|snapshot| &snapshot.root)
                                            != row_root.as_ref()
                                    {
                                        return;
                                    }
                                    this.select_commit(id.clone(), cx);
                                })
                            })
                            .copy_menu(vec![
                                CopyItem::new("复制提交 ID", &commit.id),
                                CopyItem::new("复制提交摘要", &commit.subject),
                            ]),
                    )
                })
                .collect::<Vec<_>>()
        })
        .size_full()
        .track_scroll(&self.history_state.scroll);
        let mut graph = div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .h(px(26.))
                    .flex_shrink_0()
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .text_size(px(10.))
                    .text_color(muted)
                    .child(
                        div()
                            .id("history-match-count")
                            .test_support()
                            .child(format!(
                                "匹配 {} / 已加载 {} 条",
                                self.history_state.indices.len(),
                                count
                            )),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(
                        if self.history_state.filter.is_empty() {
                            "包含本地、远程分支和合并记录 · 点击提交查看详情"
                        } else {
                            "筛选保留连接节点；未匹配记录淡化显示，拓扑关系不变"
                        },
                    ))
                    .child(
                        div()
                            .id("history-next-match")
                            .test_support()
                            .role(Role::Button)
                            .aria_label("下一个匹配提交")
                            .cursor_pointer()
                            .child("下一个匹配 ↓")
                            .on_click(cx.listener(|this, _, _, cx| this.next_history_match(cx))),
                    ),
            )
            .child(
                div()
                    .id("history-graph-scroll")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_x_scroll()
                    .overflow_y_hidden()
                    .child(
                        div()
                            .min_w(px(minimum_width))
                            .w_full()
                            .h_full()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(px(24.))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .pr(px(12.))
                                    .border_b_1()
                                    .border_color(border)
                                    .text_size(px(10.))
                                    .text_color(muted)
                                    .child(div().w(px(width)).flex_shrink_0().pl(px(8.)).child(
                                        if lanes > VISIBLE_LANES {
                                            "拓扑（前24道）"
                                        } else {
                                            "拓扑"
                                        },
                                    ))
                                    .child(div().flex_1().child("引用 / 提交说明"))
                                    .child(div().w(px(80.)).flex_shrink_0().child("作者"))
                                    .child(div().w(px(102.)).flex_shrink_0().child("日期（UTC）"))
                                    .child(div().w(px(62.)).flex_shrink_0().child("哈希")),
                            )
                            .when(count == 0, |this| {
                                this.child(div().p(px(14.)).text_color(muted).child(
                                    if self.history_loading {
                                        "正在读取多分支提交图…"
                                    } else {
                                        "当前引用范围尚无提交。"
                                    },
                                ))
                            })
                            .child(div().flex_1().min_h_0().overflow_hidden().child(commits)),
                    ),
            );
        if let Some(error) = &self.history_state.page_error {
            graph = graph.child(
                div()
                    .px(px(10.))
                    .py(px(5.))
                    .text_size(px(11.))
                    .text_color(rgb(if light { 0xcf222e } else { 0xff7b72 }))
                    .child(error.clone()),
            );
        }
        if self.history_state.limited {
            graph = graph.child(
                div()
                    .px(px(10.))
                    .py(px(5.))
                    .text_size(px(10.))
                    .text_color(muted)
                    .child("已达 5,000 条 / 4 MiB 显示预算；筛选覆盖已加载节点，可调整分支范围。"),
            );
        } else if self.history_more
            || self.history_loading
            || self.history_state.page_error.is_some()
        {
            graph = graph.child(
                div()
                    .id("more-history")
                    .test_support()
                    .role(Role::Button)
                    .aria_label("加载更早的提交")
                    .px(px(10.))
                    .py(px(5.))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .child(if self.history_loading {
                        "读取中…"
                    } else if self.history_state.page_error.is_some() {
                        "重试读取"
                    } else {
                        "加载更早的 100 条 · 扩大筛选范围"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.load_history(this.history_state.snapshot.is_none(), cx)
                    })),
            );
        }
        graph
    }
}

fn color(index: usize, light: bool) -> Rgba {
    rgb(if light {
        [
            0x0969da, 0x1a7f37, 0x8250df, 0xbc4c00, 0xbf3989, 0x0a7a7a, 0x9a6700, 0x57606a,
        ]
    } else {
        [
            0x58a6ff, 0x3fb950, 0xd2a8ff, 0xffa657, 0xf778ba, 0x39c5cf, 0xe3b341, 0x8b949e,
        ]
    }[index % 8])
}

fn glyph(row: &graph::GraphRow, width: f32, light: bool) -> Div {
    let segments = row.segments.clone();
    let lane = row.lane;
    let incoming = row.incoming;
    let node_color = color(row.color, light);
    div()
        .relative()
        .w(px(width))
        .h(px(32.))
        .flex_shrink_0()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let coordinate = |lane: usize, y: f32| {
                        point(
                            bounds.left() + px(10. + lane as f32 * 14.),
                            bounds.top() + px(y),
                        )
                    };
                    for segment in &segments {
                        // Omitted lanes get an explicit marker; never collapse distinct
                        // parents onto a visible lane and draw a false relationship.
                        if segment.from >= VISIBLE_LANES || segment.to >= VISIBLE_LANES {
                            continue;
                        }
                        let mut line = PathBuilder::stroke(px(1.5));
                        line.move_to(coordinate(
                            segment.from,
                            if segment.starts_here { 16. } else { 0. },
                        ));
                        if !segment.starts_here {
                            line.line_to(coordinate(segment.from, 8.));
                        }
                        line.line_to(coordinate(segment.to, 27.));
                        line.line_to(coordinate(segment.to, 32.));
                        if let Ok(path) = line.build() {
                            window.paint_path(path, color(segment.color, light));
                        }
                    }
                    if incoming && lane < VISIBLE_LANES {
                        let mut line = PathBuilder::stroke(px(1.5));
                        line.move_to(coordinate(lane, 0.));
                        line.line_to(coordinate(lane, 16.));
                        if let Ok(path) = line.build() {
                            window.paint_path(path, node_color);
                        }
                    }
                },
            )
            .size_full(),
        )
        .when(lane < VISIBLE_LANES, |this| {
            this.child(
                div()
                    .absolute()
                    .left(px(6. + lane as f32 * 14.))
                    .top(px(12.))
                    .size(px(8.))
                    .rounded_full()
                    .bg(node_color),
            )
        })
        .when(row.lanes > VISIBLE_LANES, |this| {
            this.child(div().absolute().right(px(1.)).top(px(5.)).child("…"))
        })
}
