//! Native GPUI translation of the Pencil design tree.
//!
//! The exported design remains the source of the spacing, colors and hierarchy.
//! Every frame, glyph and text run is a real GPUI element; measured bounds are
//! used only for text metrics, while the application still uses flex layout.

use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
};

use gpui_kit::*;
use serde::Deserialize;

/// Pencil represents a dimension as either logical pixels or a sizing rule.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum DesignSize {
    Pixels(f32),
    Sizing(String),
}

impl DesignSize {
    pub fn is_fill(&self) -> bool {
        matches!(self, Self::Sizing(value) if value == "fill_container")
    }

    pub fn pixels(&self) -> Option<f32> {
        match self {
            Self::Pixels(value) => Some(*value),
            Self::Sizing(_) => None,
        }
    }
}

impl From<f32> for DesignSize {
    fn from(value: f32) -> Self {
        Self::Pixels(value)
    }
}

/// The design tool's CSS-like shorthand for padding and corner radii.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum DesignSpacing {
    All(f32),
    Sides(Vec<f32>),
}

impl DesignSpacing {
    fn sides(&self) -> [f32; 4] {
        match self {
            Self::All(value) => [*value; 4],
            Self::Sides(values) => match values.as_slice() {
                [all] => [*all; 4],
                [vertical, horizontal] => [*vertical, *horizontal, *vertical, *horizontal],
                [top, horizontal, bottom] => [*top, *horizontal, *bottom, *horizontal],
                [top, right, bottom, left, ..] => [*top, *right, *bottom, *left],
                [] => [0.; 4],
            },
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DesignEdges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum DesignStrokeWidth {
    All(f32),
    Edges(DesignEdges),
}

impl DesignStrokeWidth {
    fn sides(&self) -> [f32; 4] {
        match self {
            Self::All(value) => [*value; 4],
            Self::Edges(edges) => [edges.top, edges.right, edges.bottom, edges.left],
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DesignBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// A lossless description of the properties used by the workbench design.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DesignNode {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    pub name: String,
    pub context: Option<String>,
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub width: Option<DesignSize>,
    pub height: Option<DesignSize>,
    pub measured: Option<DesignBounds>,
    pub layout: Option<String>,
    pub gap: Option<f32>,
    pub padding: Option<DesignSpacing>,
    pub align_items: Option<String>,
    pub justify_content: Option<String>,
    pub fill: Option<String>,
    pub stroke: Option<String>,
    pub stroke_width: Option<DesignStrokeWidth>,
    pub corner_radius: Option<DesignSpacing>,
    pub clip: bool,
    pub hidden: bool,
    pub enabled: Option<bool>,
    pub opacity: Option<f32>,
    pub rotation: Option<f32>,
    pub content: Option<String>,
    /// Preserved template text lets unchanged labels retain Pencil's rounded
    /// measured width without constraining live repository strings.
    #[serde(skip)]
    pub original_content: Option<String>,
    pub text_growth: Option<String>,
    pub font_family: Option<String>,
    pub font_size: Option<f32>,
    pub font_weight: Option<String>,
    pub line_height: Option<f32>,
    pub text_align: Option<String>,
    pub icon: Option<String>,
    pub library: Option<String>,
    pub children: Vec<DesignNode>,
}

pub type DesignClick = Rc<dyn Fn(String, &ClickEvent, &mut Window, &mut App)>;
pub type DesignMouseDown = Rc<dyn Fn(String, &MouseDownEvent, &mut Window, &mut App)>;
pub type DesignOverride = Rc<dyn Fn(&DesignNode, &mut Window, &mut App) -> Option<AnyElement>>;

/// Application behavior is injected without coupling this renderer to Git.
#[derive(Clone, Default)]
pub struct RenderContext {
    pub on_click: Option<DesignClick>,
    pub on_middle_click: Option<DesignMouseDown>,
    pub override_element: Option<DesignOverride>,
    pub interactive_ids: HashSet<String>,
    pub middle_click_ids: HashSet<String>,
    pub scroll_y_ids: HashSet<String>,
    pub scroll_x_ids: HashSet<String>,
    pub scroll_handles: HashMap<String, ScrollHandle>,
    pub(super) copy_menus: HashMap<String, Vec<super::copy_menu::CopyItem>>,
}

/// Render a design root into a native, resizable GPUI layout.
pub fn render(
    node: &DesignNode,
    context: &RenderContext,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    render_node(node, context, None, true, window, cx)
}

fn render_node(
    node: &DesignNode,
    context: &RenderContext,
    parent_vertical: Option<bool>,
    is_root: bool,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    if let Some(override_element) = &context.override_element
        && let Some(element) = override_element(node, window, cx)
    {
        return element;
    }

    let vertical = node.layout.as_deref() == Some("vertical");
    let mut element = div()
        .flex()
        .flex_shrink_0()
        .min_w_0()
        .min_h_0()
        .items_start()
        .font_family("Noto Sans SC")
        .text_size(px(13.))
        .line_height(px(19.5));

    if vertical {
        element = element.flex_col();
    }
    if is_root {
        // The artboard dimensions describe the reference viewport. Its root
        // fills the actual native window, preserving fixed sidebar dimensions.
        element = element.size_full();
    } else {
        element = size_element(element, node, parent_vertical);
    }
    if node.hidden || node.enabled == Some(false) {
        element = element.hidden();
    }
    if let Some(opacity) = node.opacity {
        element = element.opacity(opacity);
    }
    if let Some(gap) = node.gap {
        element = element.gap(px(gap));
    }
    if let Some(padding) = &node.padding {
        let [top, right, bottom, left] = padding.sides();
        element = element
            .pt(px(top))
            .pr(px(right))
            .pb(px(bottom))
            .pl(px(left));
    }
    element = apply_radius(element, node.corner_radius.as_ref());
    element = match node.align_items.as_deref() {
        Some("center") => element.items_center(),
        Some("end") => element.items_end(),
        Some("stretch") => element.items_stretch(),
        Some("baseline") => element.items_baseline(),
        _ => element,
    };
    element = match node.justify_content.as_deref() {
        Some("center") => element.justify_center(),
        Some("end") => element.justify_end(),
        Some("space_between" | "space-between") => element.justify_between(),
        Some("space_around" | "space-around") => element.justify_around(),
        Some("space_evenly" | "space-evenly") => element.justify_evenly(),
        _ => element,
    };

    if node.clip {
        element = element.overflow_hidden();
    }

    if node.kind == "text" {
        let font_size = node.font_size.unwrap_or(13.);
        let measured_height = node
            .measured
            .filter(|bounds| bounds.height > 0.)
            .map(|bounds| bounds.height);
        let line_height = measured_height.unwrap_or(font_size * node.line_height.unwrap_or(1.2));
        element = element
            .text_size(px(font_size))
            .line_height(px(line_height))
            .font_family(
                node.font_family
                    .clone()
                    .unwrap_or_else(|| "Noto Sans SC".into()),
            )
            .font_weight(font_weight(node.font_weight.as_deref()))
            .whitespace_nowrap();
        if node.height.is_none() {
            element = element.h(px(line_height));
        }
        if node.name == "窗口仓库标题" {
            element = element
                .flex_shrink_1()
                .max_w(px(400.))
                .overflow_hidden()
                .text_ellipsis();
        }
        if node.width.is_none()
            && node.original_content.is_some()
            && node.content == node.original_content
            && let Some(bounds) = node.measured
        {
            element = element.w(px(bounds.width));
        }
        if let Some(fill) = node.fill.as_deref().and_then(color) {
            element = element.text_color(fill);
        }
        if node.text_growth.as_deref() == Some("fixed-width") {
            element = element.overflow_hidden().text_ellipsis();
        }
        element = match node.text_align.as_deref() {
            Some("center") => element.text_center(),
            Some("right") => element.text_right(),
            _ => element,
        };
        if let Some(content) = &node.content {
            element = element.child(content.clone());
        }
    } else if node.kind == "icon" {
        // AllAssets includes the complete Lucide SVG collection used by Pencil.
        let mut icon = svg()
            .path(format!(
                "icons/{}.svg",
                node.icon.as_deref().unwrap_or("circle")
            ))
            .size_full()
            .flex_shrink_0();
        if let Some(fill) = node.fill.as_deref().and_then(color) {
            icon = icon.text_color(fill);
        }
        if let Some(rotation) = node.rotation {
            icon = icon.with_transformation(Transformation::rotate(radians(rotation.to_radians())));
        }
        element = element.child(icon);
    } else {
        if let Some(fill) = node.fill.as_deref().and_then(color) {
            element = element.bg(fill);
        }
        element = element.children(
            node.children
                .iter()
                .map(|child| render_node(child, context, Some(vertical), false, window, cx)),
        );
    }

    if let Some(stroke) = stroke_overlay(node) {
        // Pencil's default excludes the stroke from flex layout. A separate
        // absolute paint layer reproduces that geometry without consuming
        // padding space or shifting centered descendants by half a pixel.
        element = element.relative().child(stroke);
    }

    let mut element = element.id(SharedString::from(node.id.clone()));
    element = match node.name.as_str() {
        "标题栏拖动空白" | "窗口仓库标题" => {
            element.window_control_area(WindowControlArea::Drag)
        }
        "最小化" => element
            .occlude()
            .window_control_area(WindowControlArea::Min),
        "最大化" => element
            .occlude()
            .window_control_area(WindowControlArea::Max),
        "关闭" => element
            .occlude()
            .window_control_area(WindowControlArea::Close),
        _ => element,
    };
    if context.scroll_y_ids.contains(&node.id) {
        element = element.overflow_y_scroll();
    }
    if context.scroll_x_ids.contains(&node.id) {
        element = element.overflow_x_scroll();
    }
    if let Some(handle) = context.scroll_handles.get(&node.id) {
        element = element.track_scroll(handle);
    }
    if context.interactive_ids.contains(&node.id)
        && let Some(on_click) = &context.on_click
    {
        let on_click = Rc::clone(on_click);
        let id = node.id.clone();
        element = element
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(node.name.clone())
            .tab_index(0)
            .on_click(move |event, window, cx| on_click(id.clone(), event, window, cx));
        if node.name == "切换子模块分支" {
            use gpui_kit::component::ActiveTheme as _;
            let hover = cx.theme().colors.muted;
            element = element.hover(move |this| this.bg(hover));
        }
    }
    if context.middle_click_ids.contains(&node.id)
        && let Some(on_middle_click) = &context.on_middle_click
    {
        let on_middle_click = Rc::clone(on_middle_click);
        let id = node.id.clone();
        element = element.on_mouse_down(MouseButton::Middle, move |event, window, cx| {
            on_middle_click(id.clone(), event, window, cx);
        });
    }
    use super::copy_menu::CopyMenuExt as _;
    element.test_support().copy_menu(
        context
            .copy_menus
            .get(&node.id)
            .cloned()
            .unwrap_or_default(),
    )
}

fn apply_radius(mut element: Div, radius: Option<&DesignSpacing>) -> Div {
    if let Some(radius) = radius {
        let [top_left, top_right, bottom_right, bottom_left] = radius.sides();
        let style = element.style();
        style.corner_radii.top_left = Some(px(top_left).into());
        style.corner_radii.top_right = Some(px(top_right).into());
        style.corner_radii.bottom_right = Some(px(bottom_right).into());
        style.corner_radii.bottom_left = Some(px(bottom_left).into());
    }
    element
}

fn stroke_overlay(node: &DesignNode) -> Option<Div> {
    let stroke = node.stroke.as_deref().and_then(color)?;
    let mut overlay = apply_radius(
        div().absolute().inset_0().border_color(stroke),
        node.corner_radius.as_ref(),
    );
    let widths = node
        .stroke_width
        .as_ref()
        .map(DesignStrokeWidth::sides)
        .unwrap_or([1.; 4]);
    let style = overlay.style();
    style.border_widths.top = Some(px(widths[0]).into());
    style.border_widths.right = Some(px(widths[1]).into());
    style.border_widths.bottom = Some(px(widths[2]).into());
    style.border_widths.left = Some(px(widths[3]).into());
    // No ID, event handler or occluding behavior: the paint layer contributes
    // neither input targets nor a flex item to the design's content layout.
    Some(overlay)
}

fn size_element(mut element: Div, node: &DesignNode, parent_vertical: Option<bool>) -> Div {
    if node.name == "窗口控制"
        && node.width.is_none()
        && let Some(bounds) = node.measured
    {
        element = element.w(px(bounds.width));
    }
    let fill_width = node.width.as_ref().is_some_and(DesignSize::is_fill);
    let fill_height = node.height.as_ref().is_some_and(DesignSize::is_fill);
    if let Some(width) = node.width.as_ref().and_then(DesignSize::pixels) {
        element = element.w(px(width));
    }
    if let Some(height) = node.height.as_ref().and_then(DesignSize::pixels) {
        element = element.h(px(height));
    }
    if fill_width {
        if parent_vertical == Some(false) {
            element = element.flex_1();
        } else {
            element = element.w_full();
        }
    }
    if fill_height {
        if parent_vertical == Some(true) {
            element = element.flex_1();
        } else {
            element = element.h_full();
        }
    }
    element
}

fn font_weight(weight: Option<&str>) -> FontWeight {
    match weight {
        Some("bold") => FontWeight::BOLD,
        Some("normal") | None => FontWeight::NORMAL,
        Some(value) => FontWeight(value.parse::<f32>().unwrap_or(400.)),
    }
}

fn color(value: &str) -> Option<Rgba> {
    let hex = value.strip_prefix('#')?;
    match hex.len() {
        6 => u32::from_str_radix(hex, 16).ok().map(rgb),
        8 => u32::from_str_radix(hex, 16).ok().map(rgba),
        3 => {
            let mut expanded = String::with_capacity(6);
            for digit in hex.chars() {
                expanded.push(digit);
                expanded.push(digit);
            }
            u32::from_str_radix(&expanded, 16).ok().map(rgb)
        }
        _ => None,
    }
}
