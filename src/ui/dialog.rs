//! 独立弹出窗口的统一视觉样式。

use egui::{Color32, Context, CornerRadius, Frame, Margin, Stroke, Ui, vec2};

pub(crate) fn window_frame(ctx: &Context) -> Frame {
    // 独立窗口标题统一为 16px，上下各 8px 后标题栏总高为 32px。
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
pub(crate) fn polish(ui: &mut Ui) {
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

/// 连接库等工具型窗口使用的紧凑控件尺寸。
pub(crate) fn polish_compact(ui: &mut Ui) {
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

    let close_rect = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 19.0, rect.center().y),
        vec2(20.0, 20.0),
    );
    let close = ui
        .interact(
            close_rect,
            ui.id().with(("dialog_close", title)),
            egui::Sense::click(),
        )
        .on_hover_text("关闭");
    ui.painter().circle_filled(
        close_rect.center(),
        6.2,
        if close.hovered() {
            Color32::from_rgb(255, 86, 78)
        } else {
            Color32::from_rgb(255, 95, 87)
        },
    );
    if close.hovered() {
        let center = close_rect.center();
        let stroke = Stroke::new(1.1, Color32::from_rgb(108, 32, 28));
        ui.painter()
            .line_segment([center + vec2(-2.0, -2.0), center + vec2(2.0, 2.0)], stroke);
        ui.painter()
            .line_segment([center + vec2(-2.0, 2.0), center + vec2(2.0, -2.0)], stroke);
    }
    close.clicked()
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
