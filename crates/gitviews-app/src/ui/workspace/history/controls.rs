use super::*;
use gitviews_service::GraphReferenceKind;

#[derive(Clone)]
pub(super) struct ReferenceContext {
    root: PathBuf,
    request_id: u64,
    menu_id: u64,
}

pub(super) enum ReferenceAction {
    Toggle(String),
    All,
    Close,
    Apply,
}

impl Workspace {
    fn history_reference_context(&self) -> Option<ReferenceContext> {
        Some(ReferenceContext {
            root: self
                .history_state
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.root.clone())
                .or_else(|| self.history_state.root.clone())?,
            request_id: self.history_request_id,
            menu_id: self.history_state.ref_menu_id,
        })
    }

    pub(super) fn history_reference_context_valid(&self, context: &ReferenceContext) -> bool {
        self.history
            && self.history_state.ref_menu
            && self.history_request_id == context.request_id
            && self.history_state.ref_menu_id == context.menu_id
            && self
                .history_state
                .root
                .as_ref()
                .is_some_and(|root| paths_equal(root, &context.root))
            && self
                .changes_repository()
                .is_some_and(|repo| paths_equal(&repo.root, &context.root))
            && self
                .history_state
                .snapshot
                .as_ref()
                .is_none_or(|snapshot| paths_equal(&snapshot.root, &context.root))
    }

    fn history_reference_action(
        &mut self,
        context: &ReferenceContext,
        action: ReferenceAction,
        cx: &mut Context<Self>,
    ) {
        if !self.history_reference_context_valid(context) {
            return;
        }
        match action {
            ReferenceAction::Toggle(name) => {
                if self.history_state.snapshot.as_ref().is_none_or(|snapshot| {
                    !snapshot.references.iter().any(|reference| {
                        reference.full_name == name && reference.kind != GraphReferenceKind::Tag
                    })
                }) {
                    return;
                }
                let draft = &mut self.history_state.draft_refs;
                if let Some(index) = draft.iter().position(|reference| reference == &name) {
                    draft.remove(index);
                } else {
                    draft.push(name);
                }
            }
            ReferenceAction::All => self.history_state.draft_refs.clear(),
            ReferenceAction::Close => self.dismiss_history_reference_menu(),
            ReferenceAction::Apply => {
                self.history_state.references = self.history_state.draft_refs.clone();
                self.dismiss_history_reference_menu();
                self.load_history(true, cx);
            }
        }
        cx.notify();
    }
    pub(super) fn init_history_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (index, placeholder) in [
            "作者或邮箱",
            "路径（仓库相对路径）",
            "起始 YYYY-MM-DD",
            "结束 YYYY-MM-DD",
        ]
        .into_iter()
        .enumerate()
        {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
            self.history_state
                .subscriptions
                .push(cx.subscribe(&input, |this, _, event, cx| match event {
                    InputEvent::Change => {
                        this.history_state.filter_dirty = true;
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } if this.history => this.apply_history_filter(cx),
                    _ => {}
                }));
            match index {
                0 => self.history_state.author_input = Some(input),
                1 => self.history_state.path_input = Some(input),
                2 => self.history_state.since_input = Some(input),
                _ => self.history_state.until_input = Some(input),
            }
        }
    }

    pub(super) fn apply_history_filter(&mut self, cx: &mut Context<Self>) {
        let value = |input: &Option<Entity<InputState>>| {
            input
                .as_ref()
                .map(|input| input.read(cx).value().to_string())
                .unwrap_or_default()
        };
        let since = parse_date(&value(&self.history_state.since_input), false);
        let until = parse_date(&value(&self.history_state.until_input), true);
        let path = value(&self.history_state.path_input);
        let (since, until) = match (since, until) {
            (Ok(since), Ok(until)) if !matches!((since, until), (Some(a), Some(b)) if a > b) => {
                (since, until)
            }
            _ => {
                self.history_state.page_error =
                    Some("日期请填写 YYYY-MM-DD（UTC），起始日期不能晚于结束日期。".into());
                cx.notify();
                return;
            }
        };
        self.history_state.filter = GraphHistoryFilter {
            text: value(&self.history_state.input),
            author: value(&self.history_state.author_input),
            since,
            until,
            path: (!path.is_empty()).then_some(path),
        };
        self.history_state.filter_dirty = false;
        self.history_state.ref_menu = false;
        self.load_history(true, cx);
    }

    pub(super) fn next_history_match(&mut self, cx: &mut Context<Self>) {
        let selected = self.history_state.selected_id.as_ref().and_then(|id| {
            self.history_commits
                .iter()
                .position(|commit| &commit.id == id)
        });
        let next = self
            .history_state
            .indices
            .iter()
            .copied()
            .find(|index| selected.is_none_or(|selected| *index > selected))
            .or_else(|| self.history_state.indices.first().copied());
        if let Some(index) = next {
            self.history_state
                .scroll
                .scroll_to_item(index, ScrollStrategy::Center);
            if let Some(id) = self
                .history_commits
                .get(index)
                .map(|commit| commit.id.clone())
            {
                self.select_commit(id, cx);
            }
        }
    }

    pub(super) fn render_history_controls(&self, cx: &mut Context<Self>) -> Div {
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let muted = rgb(if light { 0x59636e } else { 0x9198a1 });
        let button = |id: &'static str, label: String| {
            div()
                .id(id)
                .test_support()
                .role(Role::Button)
                .aria_label(label.clone())
                .px(px(8.))
                .py(px(5.))
                .rounded(px(4.))
                .border_1()
                .border_color(border)
                .cursor_pointer()
                .child(label)
        };
        let mut inputs = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(10.))
            .py(px(5.))
            .border_b_1()
            .border_color(border);
        for (id, input, width) in [
            ("history-text-filter", &self.history_state.input, 210.),
            (
                "history-author-filter",
                &self.history_state.author_input,
                140.,
            ),
            ("history-path-filter", &self.history_state.path_input, 190.),
            (
                "history-since-filter",
                &self.history_state.since_input,
                146.,
            ),
            (
                "history-until-filter",
                &self.history_state.until_input,
                146.,
            ),
        ] {
            if let Some(input) = input {
                inputs = inputs.child(
                    div()
                        .w(px(width))
                        .min_w(px(80.))
                        .flex_shrink(1.)
                        .child(Input::new(input).id(id).appearance(false).h(px(30.))),
                );
            }
        }
        inputs = inputs.child(
            button(
                "history-apply-filter",
                if self.history_state.filter_dirty {
                    "应用筛选*"
                } else {
                    "应用筛选"
                }
                .into(),
            )
            .on_click(cx.listener(|this, _, _, cx| this.apply_history_filter(cx))),
        );
        div()
            .flex_shrink_0()
            .text_size(px(11.))
            .child(
                div()
                    .h(px(40.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        button("exit-history", "← 工作区".into())
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_history(cx))),
                    )
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                            "提交图 · {}",
                            self.changes_repository()
                                .map_or("", |repo| repo.name.as_str())
                        )))
                    .child(
                        button(
                            "history-branches",
                            if self.history_state.references.is_empty() {
                                "全部分支 ▾".into()
                            } else {
                                format!("{} 个引用 ▾", self.history_state.references.len())
                            },
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.history_state.ref_menu = !this.history_state.ref_menu;
                            this.history_state.ref_menu_id =
                                this.history_state.ref_menu_id.wrapping_add(1);
                            this.history_state.draft_refs = this.history_state.references.clone();
                            cx.notify();
                        })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(muted)
                            .child(format!(
                                "固定 {} 个分支顶点 · {} 个引用标签",
                                self.history_state
                                    .snapshot
                                    .as_ref()
                                    .map_or(0, |snapshot| snapshot.tips.len()),
                                self.history_state
                                    .snapshot
                                    .as_ref()
                                    .map_or(0, |snapshot| snapshot.references.len())
                            )),
                    )
                    .child(
                        button(
                            "reload-history",
                            if self.history_state.head_changed {
                                "引用已更新 · 重新载入"
                            } else {
                                "重新载入最新引用"
                            }
                            .into(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.load_history(true, cx))),
                    )
                    .child(
                        button(
                            "history-toggle-details",
                            if self.history_state.details_collapsed {
                                "显示详情"
                            } else {
                                "收起详情"
                            }
                            .into(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.history_state.details_collapsed =
                                !this.history_state.details_collapsed;
                            cx.notify();
                        })),
                    ),
            )
            .child(inputs)
    }

    pub(super) fn history_reference_menu(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.history_state.ref_menu {
            return None;
        }
        let context = Arc::new(self.history_reference_context()?);
        if !self.history_reference_context_valid(&context) {
            return None;
        }
        let backdrop_context = Arc::clone(&context);
        let escape_context = Arc::clone(&context);
        let list_context = Arc::clone(&context);
        let all_context = Arc::clone(&context);
        let close_context = Arc::clone(&context);
        let apply_context = context;
        let references = Arc::new(
            self.history_state
                .snapshot
                .as_ref()
                .map(|snapshot| {
                    snapshot
                        .references
                        .iter()
                        .filter(|reference| reference.kind != GraphReferenceKind::Tag)
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
        let selected = self.history_state.draft_refs.clone();
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let muted = rgb(if light { 0x59636e } else { 0x9198a1 });
        let entity = cx.entity();
        let count = references.len();
        Some(
            div()
                .absolute()
                .inset_0()
                .child(
                    div()
                        .id("history-reference-backdrop")
                        .absolute()
                        .inset_0()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.history_reference_action(
                                &backdrop_context,
                                ReferenceAction::Close,
                                cx,
                            );
                        })),
                )
                .child(
                    div()
                        .id("history-reference-popup")
                        .absolute()
                        .left(px(170.))
                        .top(px(40.))
                        .w(px(440.))
                        .p(px(10.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(border)
                        .shadow_lg()
                        .bg(rgb(if light { 0xffffff } else { 0x161b22 }))
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                            if event.keystroke.key == "escape" {
                                cx.stop_propagation();
                                this.history_reference_action(
                                    &escape_context,
                                    ReferenceAction::Close,
                                    cx,
                                );
                            }
                        }))
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("选择多个分支同时查看"),
                        )
                        .child(
                            div()
                                .py(px(7.))
                                .text_size(px(11.))
                                .text_color(muted)
                                .child("未勾选时显示全部；勾选后仅从选中引用展开。"),
                        )
                        .child(
                            uniform_list("history-reference-list", count, move |range, _, _| {
                                range
                                    .map(|index| {
                                        let reference = references[index].clone();
                                        let checked = selected.contains(&reference.full_name);
                                        let target = entity.clone();
                                        let context = Arc::clone(&list_context);
                                        let kind = match reference.kind {
                                            GraphReferenceKind::Head => "当前",
                                            GraphReferenceKind::LocalBranch => "本地",
                                            _ => "远程",
                                        };
                                        div()
                                            .id(("history-ref", index))
                                            .test_support()
                                            .role(Role::Button)
                                            .aria_label(format!(
                                                "历史引用 {kind} {}",
                                                reference.name
                                            ))
                                            .h(px(28.))
                                            .px(px(6.))
                                            .flex()
                                            .items_center()
                                            .gap(px(7.))
                                            .cursor_pointer()
                                            .hover(|this| {
                                                this.bg(rgb(if light {
                                                    0xf6f8fa
                                                } else {
                                                    0x212830
                                                }))
                                            })
                                            .child(if checked { "☑" } else { "☐" })
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .child(reference.name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(10.))
                                                    .text_color(muted)
                                                    .child(kind),
                                            )
                                            .on_click(move |_, _, cx| {
                                                cx.stop_propagation();
                                                target.update(cx, |this, cx| {
                                                    this.history_reference_action(
                                                        &context,
                                                        ReferenceAction::Toggle(
                                                            reference.full_name.clone(),
                                                        ),
                                                        cx,
                                                    );
                                                });
                                            })
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .h(px((count.min(10) * 28).max(28) as f32))
                            .w_full(),
                        )
                        .when(count == 0, |this| {
                            this.child(
                                div()
                                    .py(px(8.))
                                    .text_color(muted)
                                    .child("尚无引用；请等待读取完成或重新载入。"),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .gap(px(12.))
                                .pt(px(9.))
                                .child(
                                    div()
                                        .id("history-all-branches")
                                        .test_support()
                                        .role(Role::Button)
                                        .aria_label("历史全部分支")
                                        .cursor_pointer()
                                        .child("全部分支")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.history_reference_action(
                                                &all_context,
                                                ReferenceAction::All,
                                                cx,
                                            );
                                        })),
                                )
                                .child(div().flex_1())
                                .child(
                                    div()
                                        .id("history-close-branches")
                                        .test_support()
                                        .role(Role::Button)
                                        .aria_label("取消历史分支选择")
                                        .cursor_pointer()
                                        .child("取消")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.history_reference_action(
                                                &close_context,
                                                ReferenceAction::Close,
                                                cx,
                                            );
                                        })),
                                )
                                .child(
                                    div()
                                        .id("history-apply-branches")
                                        .test_support()
                                        .role(Role::Button)
                                        .aria_label("应用历史分支选择")
                                        .cursor_pointer()
                                        .child("应用")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.history_reference_action(
                                                &apply_context,
                                                ReferenceAction::Apply,
                                                cx,
                                            );
                                        })),
                                ),
                        ),
                ),
        )
    }
}

fn parse_date(value: &str, end: bool) -> Result<Option<i64>, ()> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() != 10
        || value.as_bytes()[4] != b'-'
        || value.as_bytes()[7] != b'-'
        || !value
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        return Err(());
    }
    let year = value[0..4].parse::<i64>().map_err(|_| ())?;
    let month = value[5..7].parse::<i64>().map_err(|_| ())?;
    let day = value[8..10].parse::<i64>().map_err(|_| ())?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(()),
    };
    if day < 1 || day > max_day {
        return Err(());
    }
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let shifted = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * shifted + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Ok(Some(days * 86_400 + if end { 86_399 } else { 0 }))
}

#[cfg(test)]
mod tests {
    use super::parse_date;
    #[test]
    fn history_filter_dates_validate_real_calendar_days_and_include_entire_utc_day() {
        assert_eq!(parse_date("1970-01-01", false), Ok(Some(0)));
        assert_eq!(parse_date("1970-01-01", true), Ok(Some(86_399)));
        assert_eq!(parse_date("2000-02-29", false), Ok(Some(951_782_400)));
        for invalid in [
            "2023-02-29",
            "1900-02-29",
            "2024-13-01",
            "2024-01-00",
            "2024-1-01",
            "漢字-01-01",
        ] {
            assert!(parse_date(invalid, false).is_err());
        }
    }
}
