use crate::app::RShellApp;
use egui::{Align2, Color32, CursorIcon, FontId, Pos2, RichText, Sense, Stroke, StrokeKind, vec2};

impl RShellApp {
    pub(crate) fn show_tab_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("tab_bar").show(ui, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                // 连接库(自绘图书图标)
                if book_button(ui).clicked() {
                    self.show_library = !self.show_library;
                }
                // 新建 SSH 启动页标签，不再弹出连接表单。
                if new_ssh_button(ui).clicked() {
                    self.open_ssh_launcher();
                }
                ui.separator();

                // 自绘标签块:[× 标题],选中标签高亮(同下方"文件｜命令"的
                // selectable 风格),块之间用间隙分隔而非竖线
                let mut close: Option<usize> = None;
                let font = FontId::proportional(13.0);
                for (i, session) in self.sessions.iter().enumerate() {
                    let title = session.title.clone();
                    let middle_click_closes = matches!(
                        session.kind,
                        crate::app::SessionKind::Ssh | crate::app::SessionKind::Launcher
                    );
                    let selected = i == self.active;
                    let text_color = if selected {
                        ui.visuals().strong_text_color()
                    } else {
                        ui.visuals().text_color()
                    };
                    // 先量标题宽度
                    let text_w = ui.fonts_mut(|f| {
                        f.layout_no_wrap(title.clone(), font.clone(), text_color)
                            .size()
                            .x
                    });
                    let pad_l = 5.0;
                    let x_box = 14.0; // × 点击区
                    let gap = 3.0;
                    let pad_r = 8.0;
                    let tab_h = 22.0;
                    let tab_w = pad_l + x_box + gap + text_w + pad_r;

                    let (rect, resp) = ui.allocate_exact_size(vec2(tab_w, tab_h), Sense::click());
                    // 选中 / 悬停背景
                    let bg = if selected {
                        ui.visuals().selection.bg_fill
                    } else if resp.hovered() {
                        ui.visuals().faint_bg_color
                    } else {
                        Color32::TRANSPARENT
                    };
                    ui.painter().rect(
                        rect,
                        egui::CornerRadius::same(4),
                        bg,
                        Stroke::NONE,
                        StrokeKind::Inside,
                    );
                    // 左侧 × 区(后注册→同层命中赢,吃掉该区域的点击=关闭)
                    let x_rect = egui::Rect::from_min_size(
                        Pos2::new(rect.left() + pad_l, rect.top() + (tab_h - x_box) * 0.5),
                        vec2(x_box, x_box),
                    );
                    let x_resp = ui.interact(x_rect, ui.id().with(("tab_x", i)), Sense::click());
                    let x_hovered = x_resp.hovered();
                    let x_clicked = x_resp.clicked();
                    let x_middle_clicked = x_resp.clicked_by(egui::PointerButton::Middle);
                    x_resp
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .on_hover_text("关闭会话");
                    let x_color = if x_hovered {
                        Color32::from_rgb(0xE8, 0x4C, 0x3D)
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    ui.painter().text(
                        x_rect.center(),
                        Align2::CENTER_CENTER,
                        "×",
                        FontId::proportional(13.0),
                        x_color,
                    );
                    if x_clicked
                        || (middle_click_closes
                            && (resp.clicked_by(egui::PointerButton::Middle) || x_middle_clicked))
                    {
                        close = Some(i);
                    }
                    // 标题文字
                    ui.painter().text(
                        Pos2::new(rect.left() + pad_l + x_box + gap, rect.center().y),
                        Align2::LEFT_CENTER,
                        &title,
                        font.clone(),
                        text_color,
                    );
                    // 点击标签体(非 × 区)切换
                    if resp.clicked() && !x_clicked {
                        self.active = i;
                    }
                    resp.on_hover_cursor(CursorIcon::PointingHand);
                }
                if let Some(i) = close {
                    self.sessions.remove(i);
                    if self.sessions.is_empty() {
                        self.open_ssh_launcher();
                    } else if i < self.active {
                        self.active -= 1;
                    } else if self.active >= self.sessions.len() {
                        self.active = self.sessions.len().saturating_sub(1);
                    }
                }
                // 设置按钮(右侧)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let resp = ui.add(
                        egui::Button::new(RichText::new("⚙").size(15.0))
                            .frame(false)
                            .min_size(egui::vec2(22.0, 22.0)),
                    );
                    if resp.clicked() {
                        self.show_settings = !self.show_settings;
                        if self.show_settings && self.settings_sub.is_none() {
                            self.settings_sub = Some(crate::ui::settings::SettingsPage::General);
                        }
                    }
                });
            });
            ui.add_space(2.0);
        });
    }
}

/// 新建 SSH：尺寸、透明背景和悬停效果与连接库图标保持一致。
fn new_ssh_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(22.0, 20.0), Sense::click());
    let response = response
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("新建 SSH 标签");
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 3.0, ui.visuals().widgets.hovered.bg_fill);
    }
    let color = if response.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        "＋",
        FontId::proportional(15.0),
        color,
    );
    response
}

/// 自绘图书图标按钮(连接库)
fn book_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 20.0), Sense::click());
    let resp = resp
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("连接库");
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 3.0, ui.visuals().widgets.hovered.bg_fill);
    }
    let color = if resp.hovered() {
        ui.visuals().strong_text_color()
    } else {
        ui.visuals().weak_text_color()
    };
    let stroke = Stroke::new(1.5, color);
    let book = egui::Rect::from_min_max(
        Pos2::new(rect.left() + 4.0, rect.top() + 4.0),
        Pos2::new(rect.right() - 4.0, rect.bottom() - 4.0),
    );
    let p = ui.painter();
    // 书封
    p.rect_stroke(
        book,
        egui::CornerRadius::same(2),
        stroke,
        StrokeKind::Outside,
    );
    // 书脊
    p.vline(book.center().x, book.y_range(), stroke);
    // 左页两条线
    p.line_segment(
        [
            Pos2::new(book.left() + 2.0, book.top() + 3.0),
            Pos2::new(book.center().x - 1.5, book.top() + 3.0),
        ],
        stroke,
    );
    p.line_segment(
        [
            Pos2::new(book.left() + 2.0, book.top() + 6.0),
            Pos2::new(book.center().x - 1.5, book.top() + 6.0),
        ],
        stroke,
    );
    resp
}
