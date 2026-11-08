//! An isolated settings draft. Persist and apply only after `Apply` succeeds in the owner.

use std::path::PathBuf;

#[cfg(test)]
mod headless_tests;

use gitviews_service::{AppSettings, LogLevel, ThemePreference, display_path};
use gpui_kit::{
    TestSupportExt as _,
    base::FocusTrapElement as _,
    component::{
        ActiveTheme, Disableable as _, ThemeMode,
        button::{Button, ButtonVariants as _},
        input::{Input, InputEvent, InputState},
    },
    prelude::FluentBuilder as _,
    *,
};

gpui_kit::actions!(gitviews_settings, [CloseSettingsDialog]);

pub(crate) enum SettingsEvent {
    Apply(AppSettings),
    GitSettings,
    Reload,
    Close,
}

#[derive(Clone, Copy)]
enum NumberField {
    FontSize,
    ScanInterval,
    Debounce,
    CacheItems,
    CacheMiB,
    RetainedPanes,
    RetainedMiB,
}

impl NumberField {
    const ALL: [Self; 7] = [
        Self::FontSize,
        Self::ScanInterval,
        Self::Debounce,
        Self::CacheItems,
        Self::CacheMiB,
        Self::RetainedPanes,
        Self::RetainedMiB,
    ];

    fn index(self) -> usize {
        self as usize
    }

    fn label(self) -> &'static str {
        match self {
            Self::FontSize => "编辑器字号",
            Self::ScanInterval => "后台检查间隔",
            Self::Debounce => "文件变更防抖",
            Self::CacheItems => "文件内容缓存条目",
            Self::CacheMiB => "文件内容缓存容量",
            Self::RetainedPanes => "保留的预览面板",
            Self::RetainedMiB => "预览面板缓存容量",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::FontSize => "10–24 px，可填小数；用于源码和差异预览",
            Self::ScanInterval => "2–300 秒；定期检查仓库状态",
            Self::Debounce => "50–2000 毫秒；合并连续的文件变更通知",
            Self::CacheItems => "1–256 条；缓存已读取的源码和差异",
            Self::CacheMiB => "8–256 MiB；达到容量后优先淘汰较早的缓存",
            Self::RetainedPanes => "1–32 个；保留最近使用的编辑器和预览状态",
            Self::RetainedMiB => "8–256 MiB；当前预览面板始终保留",
        }
    }

    fn value(self, settings: &AppSettings) -> String {
        match self {
            Self::FontSize => settings.appearance.editor_font_size.to_string(),
            Self::ScanInterval => settings.refresh.scan_interval_secs.to_string(),
            Self::Debounce => settings.refresh.debounce_ms.to_string(),
            Self::CacheItems => settings.preview.cache_items.to_string(),
            Self::CacheMiB => settings.preview.cache_mib.to_string(),
            Self::RetainedPanes => settings.preview.retained_panes.to_string(),
            Self::RetainedMiB => settings.preview.retained_mib.to_string(),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::FontSize => "settings-font-size",
            Self::ScanInterval => "settings-scan-interval",
            Self::Debounce => "settings-debounce",
            Self::CacheItems => "settings-cache-items",
            Self::CacheMiB => "settings-cache-mib",
            Self::RetainedPanes => "settings-retained-panes",
            Self::RetainedMiB => "settings-retained-mib",
        }
    }
}

struct FormInputs {
    numbers: Vec<Entity<InputState>>,
    _subscriptions: Vec<Subscription>,
}

/// The owning workspace handles disk I/O and calls `set_saving`, `set_error`, or `clear`.
pub(crate) struct SettingsDialog {
    open: bool,
    draft: AppSettings,
    path: PathBuf,
    inputs: Option<FormInputs>,
    loading: bool,
    saving: bool,
    error: Option<String>,
    validation: Option<String>,
    previous_focus: Option<FocusHandle>,
    focus: FocusHandle,
}

impl EventEmitter<SettingsEvent> for SettingsDialog {}

impl SettingsDialog {
    pub(crate) fn new(cx: &mut Context<Self>) -> Self {
        cx.bind_keys([KeyBinding::new(
            "escape",
            CloseSettingsDialog,
            Some("GitViewsSettings"),
        )]);
        Self {
            open: false,
            draft: AppSettings::default(),
            path: PathBuf::new(),
            inputs: None,
            loading: false,
            saving: false,
            error: None,
            validation: None,
            previous_focus: None,
            focus: cx.focus_handle(),
        }
    }

    pub(crate) fn show(&mut self, settings: AppSettings, path: PathBuf, cx: &mut Context<Self>) {
        self.open = true;
        self.draft = settings;
        self.path = path;
        self.inputs = None;
        self.loading = false;
        self.saving = false;
        self.error = None;
        self.validation = None;
        // Retain the original focus if a completed read replaces the loading draft.
        cx.notify();
    }

    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.inputs = None;
        self.error = None;
        self.validation = None;
        self.loading = false;
        self.saving = false;
        self.previous_focus = None;
        cx.notify();
    }

    /// Temporarily hide this modal while an associated settings page is open.
    /// Keep its input entities, including values not yet submitted to the draft.
    pub(crate) fn suspend(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        cx.notify();
    }

    pub(crate) fn resume(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open = true;
        self.focus.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    pub(crate) fn set_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        self.loading = loading;
        cx.notify();
    }

    pub(crate) fn set_saving(&mut self, saving: bool, cx: &mut Context<Self>) {
        self.saving = saving;
        if saving {
            self.error = None;
        }
        cx.notify();
    }

    pub(crate) fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        self.error = error;
        self.saving = false;
        self.loading = false;
        cx.notify();
    }

    fn busy(&self) -> bool {
        self.loading || self.saving
    }

    fn ensure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.previous_focus.is_none() {
            self.previous_focus = window.focused(cx);
        }
        if self.inputs.is_some() {
            return;
        }
        // Reload replaces the old input entities. Their focused handle no
        // longer belongs to this modal, so restore its key context as well.
        self.focus.focus(window, cx);
        let mut subscriptions = Vec::with_capacity(NumberField::ALL.len());
        let numbers = NumberField::ALL
            .into_iter()
            .map(|field| {
                let value = field.value(&self.draft);
                let input = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(field.label())
                        .default_value(value)
                });
                subscriptions.push(cx.subscribe_in(
                    &input,
                    window,
                    |this, _, event: &InputEvent, window, cx| match event {
                        InputEvent::Change => {
                            this.validation = None;
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } => this.submit(window, cx),
                        _ => {}
                    },
                ));
                input
            })
            .collect();
        self.inputs = Some(FormInputs {
            numbers,
            _subscriptions: subscriptions,
        });
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Once writing has started, keep the outcome visible instead of implying cancellation.
        if self.saving {
            return;
        }
        let focus = self.previous_focus.take();
        self.clear(cx);
        if let Some(focus) = focus {
            focus.focus(window, cx);
        }
        cx.emit(SettingsEvent::Close);
    }

    fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        let Some(inputs) = &self.inputs else {
            return;
        };
        let values = inputs
            .numbers
            .iter()
            .map(|input| input.read(cx).value().to_string())
            .collect::<Vec<_>>();
        match apply_numbers(self.draft, &values) {
            Ok(settings) => {
                self.draft = settings;
                self.validation = None;
                self.set_saving(true, cx);
                cx.emit(SettingsEvent::Apply(settings));
            }
            Err(error) => {
                self.validation = Some(error);
                cx.notify();
            }
        }
    }

    fn number_row(&self, field: NumberField, muted: Hsla) -> Div {
        let mut row = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(18.))
            .py(px(7.))
            .child(
                div().flex_1().min_w_0().child(field.label()).child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted)
                        .child(field.hint()),
                ),
            );
        if let Some(inputs) = &self.inputs {
            row = row.child(
                Input::new(&inputs.numbers[field.index()])
                    .id(field.id())
                    .w(px(112.))
                    .h(px(32.))
                    .tab_index(field.index() as isize + 10)
                    .disabled(self.busy()),
            );
        }
        row
    }
}

impl Render for SettingsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        self.ensure_inputs(window, cx);
        let light = cx.theme().mode == ThemeMode::Light;
        let border = rgb(if light { 0xd0d7de } else { 0x30363d });
        let muted: Hsla = rgb(if light { 0x59636e } else { 0x9198a1 }).into();
        let busy = self.busy();
        let mut theme_buttons = div().flex().gap(px(6.));
        for (index, (theme, label)) in [
            (ThemePreference::System, "跟随系统"),
            (ThemePreference::Light, "浅色"),
            (ThemePreference::Dark, "深色"),
        ]
        .into_iter()
        .enumerate()
        {
            theme_buttons = theme_buttons.child(
                Button::new(("settings-theme", index))
                    .label(label)
                    .when(self.draft.appearance.theme == theme, |button| {
                        button.primary()
                    })
                    .disabled(busy)
                    .tab_index(index as isize + 1)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.draft.appearance.theme = theme;
                        cx.notify();
                    })),
            );
        }
        let mut log_buttons = div().flex().gap(px(4.)).flex_wrap();
        for (index, (level, label)) in [
            (LogLevel::Off, "关闭"),
            (LogLevel::Error, "错误"),
            (LogLevel::Warn, "警告"),
            (LogLevel::Info, "信息"),
            (LogLevel::Debug, "调试"),
            (LogLevel::Trace, "追踪"),
        ]
        .into_iter()
        .enumerate()
        {
            log_buttons = log_buttons.child(
                Button::new(("settings-log-level", index))
                    .label(label)
                    .when(self.draft.logging.level == level, |button| button.primary())
                    .disabled(busy)
                    .tab_index(index as isize + 20)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.draft.logging.level = level;
                        cx.notify();
                    })),
            );
        }
        let section = |label: &'static str| {
            div()
                .mt(px(12.))
                .mb(px(4.))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(label)
        };
        let body = div()
            .id("settings-form-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(22.))
            .pb(px(16.))
            .child(section("使用模式"))
            .child(
                Button::new("settings-read-only")
                    .label(if self.draft.read_only { "只读简单模式 · 已开启" } else { "只读简单模式 · 已关闭" })
                    .when(self.draft.read_only, |button| button.primary())
                    .disabled(busy)
                    .tab_index(0)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.draft.read_only = !this.draft.read_only;
                        cx.notify();
                    })),
            )
            .child(div().mt(px(6.)).text_size(px(11.)).text_color(muted)
                .child("默认仅观察 AI 代码的 Git 状态、差异和历史。关闭并保存后，显示提交、分支、同步等高级功能。"))
            .when(!self.draft.read_only, |body| body
                .child(section("Git 连接与认证"))
                .child(Button::new("settings-git")
                    .label("当前仓库 Git 设置…")
                    .disabled(busy)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::GitSettings)))))
            .child(section("外观与编辑器"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .py(px(7.))
                    .child("主题")
                    .child(theme_buttons),
            )
            .child(self.number_row(NumberField::FontSize, muted))
            .child(section("仓库自动刷新"))
            .child(self.number_row(NumberField::ScanInterval, muted))
            .child(self.number_row(NumberField::Debounce, muted))
            .child(section("性能与缓存"))
            .child(self.number_row(NumberField::CacheItems, muted))
            .child(self.number_row(NumberField::CacheMiB, muted))
            .child(self.number_row(NumberField::RetainedPanes, muted))
            .child(self.number_row(NumberField::RetainedMiB, muted))
            .child(section("日志"))
            .child(log_buttons)
            .child(
                div()
                    .mt(px(6.))
                    .text_size(px(11.))
                    .text_color(muted)
                    .child("日志等级在下次启动时生效；RUST_LOG 环境变量优先。"),
            );
        let error = self.validation.as_ref().or(self.error.as_ref()).cloned();
        let card = div()
            .w(px(680.))
            .h(px(720.))
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
                    .flex()
                    .flex_col()
                    .gap(px(5.))
                    .px(px(22.))
                    .py(px(16.))
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("设置"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child("保存后应用到当前窗口，并写入 TOML 配置文件。"),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(display_path(&self.path)),
                    ),
            )
            .child(body)
            .when_some(error, |card, error| {
                card.child(
                    div()
                        .id("settings-error-scroll")
                        .test_support()
                        .role(Role::Alert)
                        .aria_label(error.clone())
                        .max_h(px(96.))
                        .overflow_y_scroll()
                        .px(px(22.))
                        .pb(px(10.))
                        .text_size(px(11.))
                        .text_color(rgb(if light { 0xcf222e } else { 0xf85149 }))
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(10.))
                    .px(px(22.))
                    .py(px(14.))
                    .border_t_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                Button::new("settings-defaults")
                                    .label("恢复默认")
                                    .disabled(busy)
                                    .tab_index(30)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.draft = AppSettings::default();
                                        // Recreate inputs from the draft without touching persisted settings.
                                        this.inputs = None;
                                        this.validation = None;
                                        this.focus.focus(window, cx);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("settings-reload")
                                    .label("重新读取")
                                    .tooltip("读取磁盘上的 TOML，放弃尚未保存的修改")
                                    .disabled(busy)
                                    .tab_index(31)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.validation = None;
                                        this.set_loading(true, cx);
                                        cx.emit(SettingsEvent::Reload);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                Button::new("settings-cancel")
                                    .label("取消")
                                    .disabled(self.saving)
                                    .tab_index(32)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("settings-save")
                                    .label(if self.loading {
                                        "读取中…"
                                    } else if self.saving {
                                        "保存中…"
                                    } else {
                                        "保存并应用"
                                    })
                                    .primary()
                                    .disabled(busy)
                                    .tab_index(33)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit(window, cx);
                                    })),
                            ),
                    ),
            );
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba(0x00000088))
            .key_context("GitViewsSettings")
            .track_focus(&self.focus)
            .tab_group()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(12.))
            .font_family("Noto Sans SC")
            .on_action(cx.listener(|this, _: &CloseSettingsDialog, window, cx| {
                this.close(window, cx);
            }))
            .child(card)
            .focus_trap("gitviews-settings-trap", &self.focus)
            .into_any_element()
    }
}

fn apply_numbers(mut draft: AppSettings, values: &[String]) -> Result<AppSettings, String> {
    fn number<T: std::str::FromStr>(values: &[String], field: NumberField) -> Result<T, String> {
        values
            .get(field.index())
            .and_then(|value| value.trim().parse().ok())
            .ok_or_else(|| format!("{}：请填写有效的数字。", field.label()))
    }
    draft.appearance.editor_font_size = number(values, NumberField::FontSize)?;
    draft.refresh.scan_interval_secs = number(values, NumberField::ScanInterval)?;
    draft.refresh.debounce_ms = number(values, NumberField::Debounce)?;
    draft.preview.cache_items = number(values, NumberField::CacheItems)?;
    draft.preview.cache_mib = number(values, NumberField::CacheMiB)?;
    draft.preview.retained_panes = number(values, NumberField::RetainedPanes)?;
    draft.preview.retained_mib = number(values, NumberField::RetainedMiB)?;
    draft.validate().map_err(|error| error.to_string())?;
    Ok(draft)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;

    fn values(settings: &AppSettings) -> Vec<String> {
        NumberField::ALL
            .into_iter()
            .map(|field| field.value(settings))
            .collect()
    }

    #[test]
    fn form_validates_the_entire_draft_before_applying() {
        let mut draft = AppSettings::default();
        draft.appearance.theme = ThemePreference::Light;
        draft.logging.level = LogLevel::Warn;
        let mut fields = values(&draft);
        fields[NumberField::FontSize.index()] = " 14.5 ".into();
        fields[NumberField::ScanInterval.index()] = "60".into();
        let applied = apply_numbers(draft, &fields).expect("valid draft");
        assert_eq!(applied.appearance.editor_font_size, 14.5);
        assert_eq!(applied.refresh.scan_interval_secs, 60);
        assert_eq!(applied.appearance.theme, ThemePreference::Light);
        assert_eq!(applied.logging.level, LogLevel::Warn);
        assert_eq!(draft.appearance.editor_font_size, 12.5);
    }

    #[test]
    fn invalid_numbers_and_out_of_range_values_cannot_be_applied() {
        let draft = AppSettings::default();
        for (field, invalid) in [
            (NumberField::FontSize, "NaN"),
            (NumberField::FontSize, "inf"),
            (NumberField::FontSize, "9.9"),
            (NumberField::ScanInterval, "-1"),
            (NumberField::Debounce, ""),
            (NumberField::CacheItems, "1.5"),
            (NumberField::CacheMiB, "257"),
            (NumberField::RetainedPanes, "0"),
            (NumberField::RetainedMiB, "7"),
        ] {
            let mut fields = values(&draft);
            fields[field.index()] = invalid.into();
            assert!(apply_numbers(draft, &fields).is_err(), "{invalid}");
        }
        assert!(apply_numbers(draft, &[]).is_err());
    }
}
