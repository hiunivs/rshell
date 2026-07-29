use crate::app::RShellApp;
use crate::config::{AppConfig, SavedConnection};
use crate::ui::icons::Icon;
use egui::{Color32, RichText};

impl RShellApp {
    /// SSH 启动页：展示全部已保存连接，双击后在当前标签内建立连接。
    pub(crate) fn show_ssh_launcher(&mut self, ui: &mut egui::Ui) {
        let mut connect: Option<String> = None;
        let mut edit: Option<String> = None;
        let mut open_local = false;
        let mut open_library = false;
        let connections = &self.config.connections;
        let error = self
            .sessions
            .get(self.active)
            .and_then(|session| session.error.clone());

        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(28, 22))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.heading("SSH 连接");
                        ui.label(RichText::new("双击已保存的连接即可在当前标签中打开").weak());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // right_to_left：先添加的按钮位于最右侧。
                        if ui
                            .add(
                                egui::Button::image_and_text(
                                    Icon::SquareTerminal.image(14.0, Color32::WHITE),
                                    "新建本机终端",
                                )
                                .image_tint_follows_text_color(true),
                            )
                            .clicked()
                        {
                            open_local = true;
                        }
                        if ui
                            .add(
                                egui::Button::image_and_text(
                                    Icon::BookOpen.image(14.0, Color32::WHITE),
                                    "打开连接库",
                                )
                                .image_tint_follows_text_color(true),
                            )
                            .clicked()
                        {
                            open_library = true;
                        }
                    });
                });

                if let Some(error) = error {
                    ui.add_space(8.0);
                    ui.colored_label(Color32::from_rgb(0xE8, 0x4C, 0x3D), error);
                }

                ui.add_space(14.0);
                egui::Frame::new()
                    .fill(ui.visuals().faint_bg_color)
                    .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                    .corner_radius(egui::CornerRadius::same(6))
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("名称 / 地址").strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(RichText::new("分组 / 认证").strong());
                                },
                            );
                        });
                        ui.separator();

                        if connections.is_empty() {
                            ui.allocate_ui_with_layout(
                                egui::vec2(ui.available_width(), 120.0),
                                egui::Layout::centered_and_justified(egui::Direction::TopDown),
                                |ui| {
                                    ui.label(
                                        RichText::new("暂无保存的 SSH 连接，请先在连接库中添加")
                                            .weak(),
                                    );
                                },
                            );
                            return;
                        }

                        egui::ScrollArea::vertical()
                            .id_salt("ssh_launcher_connections")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for (index, connection) in connections.iter().enumerate() {
                                    let row = launcher_connection_row(
                                        ui,
                                        connection,
                                        &self.config,
                                        index,
                                    );
                                    if row.double_clicked() {
                                        connect = Some(connection.name.clone());
                                    }
                                    row.context_menu(|ui| {
                                        if ui.button("编辑连接…").clicked() {
                                            edit = Some(connection.name.clone());
                                            ui.close();
                                        }
                                    });
                                }
                            });
                    });
            });

        if open_library {
            self.show_library = true;
        }
        if open_local {
            self.open_local(ui.ctx());
            return;
        }
        if let Some(name) = edit {
            self.open_lib_edit(&name);
            return;
        }
        if let Some(name) = connect {
            self.connect_saved_from_launcher(ui.ctx(), &name);
        }
    }
}

fn launcher_connection_row(
    ui: &mut egui::Ui,
    connection: &SavedConnection,
    config: &AppConfig,
    index: usize,
) -> egui::Response {
    let fill = if index.is_multiple_of(2) {
        ui.visuals().extreme_bg_color.gamma_multiply(0.55)
    } else {
        Color32::TRANSPARENT
    };
    let row = egui::Frame::new()
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(connection.display_name()).strong());
                    ui.label(
                        RichText::new(format!(
                            "{}@{}:{}",
                            connection.user, connection.host, connection.port
                        ))
                        .weak()
                        .small(),
                    );
                });
                for tag in &connection.tags {
                    ui.label(
                        RichText::new(tag)
                            .small()
                            .color(Color32::from_rgb(0x4C, 0x9B, 0xE8)),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.vertical(|ui| {
                        let group = if connection.group.is_empty() {
                            "根目录"
                        } else {
                            &connection.group
                        };
                        ui.label(group);
                        ui.label(
                            RichText::new(saved_auth_label(connection, config))
                                .weak()
                                .small(),
                        );
                    });
                });
            });
        });
    let response = ui
        .interact(
            row.response.rect,
            ui.id().with(("ssh_launcher_row", &connection.name)),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("双击连接");
    if response.hovered() {
        ui.painter().rect_stroke(
            row.response.rect,
            4.0,
            ui.visuals().widgets.hovered.bg_stroke,
            egui::StrokeKind::Inside,
        );
    }
    response
}

fn saved_auth_label(connection: &SavedConnection, config: &AppConfig) -> String {
    if let Some(key_id) = &connection.key_id {
        config
            .keys
            .iter()
            .find(|key| key.id == *key_id)
            .map(|key| format!("密钥：{}", key.name))
            .unwrap_or_else(|| "密钥已不存在".into())
    } else if !connection.key_path.is_empty() {
        "私钥文件".into()
    } else {
        "密码".into()
    }
}

#[cfg(test)]
mod tests {
    use super::saved_auth_label;
    use crate::config::{AppConfig, SavedConnection, SavedKey};

    #[test]
    fn saved_auth_label_uses_managed_key_name() {
        let mut config = AppConfig::default();
        config.keys.push(SavedKey {
            id: "key-1".into(),
            name: "生产密钥".into(),
            ..Default::default()
        });
        let connection = SavedConnection {
            key_id: Some("key-1".into()),
            ..Default::default()
        };
        assert_eq!(saved_auth_label(&connection, &config), "密钥：生产密钥");
    }
}
