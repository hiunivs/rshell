// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

//! 独立弹出窗口的统一视觉样式。

use egui::{Color32, Context, CornerRadius, Frame, Margin, Stroke, Ui, vec2};

pub(crate) fn window_frame(ctx: &Context) -> Frame {
    // 默认 egui 标题栏窗口的内容边距；自定义标题栏弹窗见 compact_dialog_window。
    window_frame_with_margin(ctx, Margin::symmetric(14, 8))
}

pub(crate) fn window_frame_with_margin(ctx: &Context, margin: Margin) -> Frame {
    let visuals = &ctx.style_of(ctx.theme()).visuals;
    let dark = visuals.dark_mode;
    Frame::new()
        .fill(visuals.window_fill())
        .stroke(Stroke::new(
            1.0,
            if dark {
                Color32::from_rgb(62, 64, 68)
            } else {
                Color32::from_rgb(205, 207, 211)
            },
        ))
        .corner_radius(CornerRadius::same(10))
        .shadow(egui::epaint::Shadow {
            offset: [0, 10],
            blur: 22,
            spread: 0,
            color: Color32::from_black_alpha(if dark { 115 } else { 38 }),
        })
        .inner_margin(margin)
}

/// 只作用于当前独立窗口的控件样式，不改变主界面。
///
/// 同时把当前 viewport 标记为“有弹窗”：终端/命令条/路径框据此暂停键盘输入。
pub(crate) fn polish(ui: &mut Ui) {
    mark_modal(ui.ctx());
    style_polish(ui);
}

/// 连接库等工具型窗口使用的紧凑控件尺寸。
pub(crate) fn polish_compact(ui: &mut Ui) {
    mark_modal(ui.ctx());
    style_polish_compact(ui);
}

/// 设置页/连接库页面正文的紧凑样式：这些是独立 viewport 的正常内容，
/// 不是弹窗，不能标记为模态（否则主窗口终端会被误暂停输入）。
pub(crate) fn polish_page_compact(ui: &mut Ui) {
    style_polish_compact(ui);
}

fn style_polish(ui: &mut Ui) {
    ui.spacing_mut().item_spacing.y = ui.spacing().item_spacing.y.max(7.0);
    ui.spacing_mut().button_padding = vec2(12.0, 5.0);
    ui.spacing_mut().interact_size.y = ui.spacing().interact_size.y.max(27.0);

    let widgets = &mut ui.style_mut().visuals.widgets;
    widgets.noninteractive.corner_radius = CornerRadius::same(6);
    widgets.inactive.corner_radius = CornerRadius::same(6);
    widgets.hovered.corner_radius = CornerRadius::same(6);
    widgets.active.corner_radius = CornerRadius::same(6);
    widgets.open.corner_radius = CornerRadius::same(6);
    widgets.hovered.expansion = 0.0;
    widgets.active.expansion = 0.0;
}

fn style_polish_compact(ui: &mut Ui) {
    ui.spacing_mut().item_spacing.y = 5.0;
    ui.spacing_mut().button_padding = vec2(8.0, 3.0);
    ui.spacing_mut().interact_size.y = 23.0;

    let widgets = &mut ui.style_mut().visuals.widgets;
    widgets.noninteractive.corner_radius = CornerRadius::same(5);
    widgets.inactive.corner_radius = CornerRadius::same(5);
    widgets.hovered.corner_radius = CornerRadius::same(5);
    widgets.active.corner_radius = CornerRadius::same(5);
    widgets.open.corner_radius = CornerRadius::same(5);
    widgets.hovered.expansion = 0.0;
    widgets.active.expansion = 0.0;
}

pub(crate) fn modal_frame(ctx: &Context) -> Frame {
    window_frame_with_margin(ctx, Margin::ZERO).corner_radius(CornerRadius::same(12))
}

/// 弹窗首次显示时把键盘焦点移入文本框并全选内容。
///
/// 文件/文件夹重命名这类弹窗如果不主动聚焦，按键会继续落到下层终端：
/// 回车不会确认弹窗，反而被当成终端回车执行。全选后直接输入即可替换旧名称。
pub(crate) fn focus_text_input(resp: &egui::Response, ui: &Ui, id: egui::Id, char_count: usize) {
    resp.request_focus();
    let Some(mut state) = egui::text_edit::TextEditState::load(ui.ctx(), id) else {
        return;
    };
    state
        .cursor
        .set_char_range(Some(egui::text_selection::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(char_count),
        )));
    state.store(ui.ctx(), id);
}

/// 弹窗已经处理过的回车/取消键，从本帧输入里移除。
///
/// 文件面板画在终端之前：弹窗用回车确认并交出焦点后，后面绘制的终端还会
/// 看到同一个回车事件。弹窗确认时消费掉它，保证一次按键只作用于一个窗口。
pub(crate) fn consume_handled_key(ui: &Ui, key: egui::Key) {
    ui.ctx().input_mut(|input| {
        input.consume_key(egui::Modifiers::NONE, key);
    });
}

fn modal_marker_id(ctx: &Context) -> egui::Id {
    egui::Id::new(("rshell_modal_frame", ctx.viewport_id()))
}

/// 记录当前 viewport 本帧有弹窗打开。
///
/// 所有主窗口弹窗都会调用 [`polish`]/[`polish_compact`]，因此只需在这里统一
/// 打标记；终端据此在弹窗打开期间暂停键盘输入，避免按键落到弹窗下层。
pub(crate) fn mark_modal(ctx: &Context) {
    let marker = modal_marker_id(ctx);
    let frame = ctx.cumulative_frame_nr();
    ctx.data_mut(|data| data.insert_temp(marker, frame));
}

/// 当前 viewport 本帧或上一帧是否有弹窗。
///
/// 多留一帧给“画在终端之后”的弹窗：终端本帧已经绘制完，但弹窗确认/关闭时
/// 交出的回车不能被下一帧的终端再次处理。
pub(crate) fn modal_keyboard_blocked(ctx: &Context) -> bool {
    let marker = modal_marker_id(ctx);
    let Some(marked_at) = ctx.data(|data| data.get_temp::<u64>(marker)) else {
        return false;
    };
    ctx.cumulative_frame_nr().saturating_sub(marked_at) <= 1
}

/// 统一的紧凑弹窗：28pt/32pt 自定义标题栏 + 正文内边距。
///
/// 返回 true 表示用户点击了标题栏关闭按钮。窗口自身的 open 标志仍由调用方
/// 处理（例如 `.open()` 被系统关闭时）。
pub(crate) fn compact_dialog_window(
    ctx: &Context,
    title: &str,
    id: Option<egui::Id>,
    open: &mut bool,
    add_contents: impl FnOnce(&mut Ui),
) -> bool {
    let mut close = false;
    let mut window = egui::Window::new(title)
        .open(open)
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .pivot(egui::Align2::CENTER_CENTER)
        .default_pos(ctx.content_rect().center())
        .frame(flush_dialog_frame(ctx));
    if let Some(id) = id {
        window = window.id(id);
    }
    window.show(ctx, |ui| {
        // 标题栏与正文之间不要额外间距，标题栏高度才能精确等于原生窗口标题栏。
        ui.spacing_mut().item_spacing.y = 0.0;
        close = compact_title_bar(ui, title);
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(14, 10))
            .show(ui, add_contents);
    });
    close
}

/// 原生子窗口（设置、连接库、编辑器）标题栏的高度。
///
/// macOS 标准窗口标题栏是 28pt（主窗口也用 28pt 预留原生标题栏区域）；
/// Windows/Linux 使用接近系统标题栏的 32pt。
pub(crate) fn native_title_bar_height() -> f32 {
    #[cfg(target_os = "macos")]
    {
        28.0
    }
    #[cfg(not(target_os = "macos"))]
    {
        32.0
    }
}

/// 标题栏自己铺满顶部的弹窗外框：不再给窗口内容留统一内边距，
/// 由标题栏和正文各自控制，从而精确控制标题栏高度。
pub(crate) fn flush_dialog_frame(ctx: &Context) -> Frame {
    window_frame_with_margin(ctx, Margin::ZERO)
}

/// 苹果风格窗口交通灯：左上角红/黄/绿三个小灯，只有红色负责关闭。
///
/// 黄/绿保持视觉一致但不执行操作（egui 弹窗没有系统级最小化/缩放语义）。
fn traffic_lights(ui: &mut Ui, rect: egui::Rect) -> bool {
    let center_y = rect.center().y;
    let radius = 6.0;
    let spacing = 20.0;
    let first_x = rect.left() + 14.0;
    let centers = [
        egui::pos2(first_x, center_y),
        egui::pos2(first_x + spacing, center_y),
        egui::pos2(first_x + 2.0 * spacing, center_y),
    ];
    let colors = [
        Color32::from_rgb(0xFF, 0x5F, 0x57),
        Color32::from_rgb(0xFE, 0xBC, 0x2E),
        Color32::from_rgb(0x28, 0xC8, 0x40),
    ];
    let group_rect = egui::Rect::from_min_max(
        centers[0] - vec2(radius + 2.0, radius + 2.0),
        centers[2] + vec2(radius + 2.0, radius + 2.0),
    );
    let group_hovered = ui.rect_contains_pointer(group_rect);

    let close_rect =
        egui::Rect::from_center_size(centers[0], egui::Vec2::splat(radius * 2.0 + 4.0));
    let close = ui
        .interact(
            close_rect,
            ui.id().with("traffic_light_close"),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("关闭");

    for (center, color) in centers.iter().zip(colors) {
        ui.painter().circle_filled(*center, radius, color);
    }
    // 悬停整组时显示 macOS 的 ×/−/+ 记号。
    if group_hovered {
        for (center, glyph) in centers.iter().zip(["×", "−", "+"]) {
            ui.painter().text(
                *center,
                egui::Align2::CENTER_CENTER,
                glyph,
                egui::FontId::proportional(10.0),
                Color32::from_black_alpha(130),
            );
        }
    }
    close.clicked()
}

/// 紧凑弹窗标题栏：高度与设置等原生窗口一致，标题居中，左上角苹果交通灯。
///
/// 返回 true 表示用户点击了红色关闭灯。窗口设置 `.title_bar(false)` 后仍可拖动
/// （egui 会回退为 drag-anywhere），标题栏本身不额外拦截拖动。
pub(crate) fn compact_title_bar(ui: &mut Ui, title: &str) -> bool {
    let height = native_title_bar_height();
    let dark = ui.visuals().dark_mode;
    let fill = ui.visuals().widgets.open.weak_bg_fill;
    let divider = if dark {
        Color32::from_rgb(62, 64, 68)
    } else {
        Color32::from_rgb(218, 219, 222)
    };
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), egui::Sense::hover());
    ui.painter().rect_filled(
        rect,
        CornerRadius {
            nw: 9,
            ne: 9,
            sw: 0,
            se: 0,
        },
        fill,
    );
    ui.painter()
        .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, divider));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        title,
        egui::FontId::proportional(16.0),
        ui.visuals().text_color(),
    );

    traffic_lights(ui, rect)
}

pub(crate) fn title_bar_with_height(ui: &mut Ui, title: &str, width: f32, height: f32) -> bool {
    let dark = ui.visuals().dark_mode;
    let fill = if dark {
        Color32::from_rgb(35, 36, 39)
    } else {
        Color32::from_rgb(246, 246, 247)
    };
    let divider = if dark {
        Color32::from_rgb(62, 64, 68)
    } else {
        Color32::from_rgb(218, 219, 222)
    };
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), egui::Sense::hover());
    ui.painter().rect_filled(
        rect,
        CornerRadius {
            nw: 11,
            ne: 11,
            sw: 0,
            se: 0,
        },
        fill,
    );
    ui.painter()
        .hline(rect.x_range(), rect.bottom(), Stroke::new(1.0, divider));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        title,
        egui::FontId::proportional(16.0),
        ui.visuals().text_color(),
    );

    traffic_lights(ui, rect)
}

/// 与普通主按钮同宽的次要按钮（取消等），保证一行按钮宽度一致。
pub(crate) fn secondary_button(ui: &mut Ui, label: &str) -> egui::Response {
    ui.add_sized([88.0, 28.0], egui::Button::new(label))
}

/// 与紧凑主按钮同宽的次要按钮。
pub(crate) fn compact_secondary_button(ui: &mut Ui, label: &str) -> egui::Response {
    ui.add_sized([56.0, 28.0], egui::Button::new(label))
}

pub(crate) fn primary_button(ui: &mut Ui, label: &str) -> egui::Response {
    primary_button_enabled(ui, true, label)
}

pub(crate) fn compact_primary_button(ui: &mut Ui, label: &str) -> egui::Response {
    primary_button_enabled_with_size(ui, true, label, vec2(56.0, 28.0))
}

pub(crate) fn primary_button_enabled(ui: &mut Ui, enabled: bool, label: &str) -> egui::Response {
    primary_button_enabled_with_size(ui, enabled, label, vec2(88.0, 28.0))
}

fn primary_button_enabled_with_size(
    ui: &mut Ui,
    enabled: bool,
    label: &str,
    size: egui::Vec2,
) -> egui::Response {
    let fill = ui.visuals().selection.bg_fill;
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(label).color(Color32::WHITE).strong())
            .min_size(size)
            .corner_radius(CornerRadius::same(6))
            .fill(fill)
            .stroke(Stroke::NONE),
    )
}

pub(crate) fn destructive_button(ui: &mut Ui, label: &str) -> egui::Response {
    destructive_button_with_size(ui, label, vec2(88.0, 28.0))
}

pub(crate) fn compact_destructive_button(ui: &mut Ui, label: &str) -> egui::Response {
    destructive_button_with_size(ui, label, vec2(56.0, 28.0))
}

fn destructive_button_with_size(ui: &mut Ui, label: &str, size: egui::Vec2) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(Color32::WHITE).strong())
            .min_size(size)
            .corner_radius(CornerRadius::same(6))
            .fill(Color32::from_rgb(210, 62, 56))
            .stroke(Stroke::NONE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 新建/重命名等弹窗的标题栏必须与设置等原生窗口一致。
    #[test]
    fn compact_title_bar_matches_native_window_title_bar_height() {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(320.0, 120.0))
            .build_ui_state(
                |ui, out: &mut (f32, bool)| {
                    out.1 = compact_title_bar(ui, "标题");
                    out.0 = ui.min_rect().height();
                },
                (0.0, false),
            );
        harness.run_steps(2);
        assert_eq!(harness.state().0, native_title_bar_height());
        assert!(!harness.state().1);
    }
}
