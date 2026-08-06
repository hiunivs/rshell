use crate::app::RShellApp;
use crate::config::{AppConfig, SavedConnection};
use crate::ui::icons::Icon;
use egui::{Color32, RichText};

const LAUNCHER_CARD_MIN_WIDTH: f32 = 280.0;
const LAUNCHER_CARD_HEIGHT: f32 = 54.0;
const LAUNCHER_CARD_GAP: f32 = 7.0;
const LAUNCHER_GROUP_INSET: i8 = 14;
const LAUNCHER_GROUP_FONT_SIZE: f32 = 11.0;

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
                                let frequent_connections = self.config.frequent_connections(3);
                                let actions = launcher_connection_section(
                                    ui,
                                    "frequent",
                                    "常用",
                                    &frequent_connections,
                                    &self.config,
                                );
                                if actions.connect.is_some() {
                                    connect = actions.connect;
                                }
                                if actions.edit.is_some() {
                                    edit = actions.edit;
                                }

                                let groups = group_launcher_connections(connections);
                                for (group, group_connections) in groups {
                                    launcher_group_separator(ui);
                                    let actions = launcher_connection_section(
                                        ui,
                                        ("folder", group),
                                        launcher_group_label(group),
                                        &group_connections,
                                        &self.config,
                                    );
                                    if actions.connect.is_some() {
                                        connect = actions.connect;
                                    }
                                    if actions.edit.is_some() {
                                        edit = actions.edit;
                                    }
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

#[derive(Default)]
struct LauncherSectionActions {
    connect: Option<String>,
    edit: Option<String>,
}

fn launcher_connection_section(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    title: &str,
    connections: &[&SavedConnection],
    config: &AppConfig,
) -> LauncherSectionActions {
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(LAUNCHER_GROUP_INSET, 0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            launcher_section_header(ui, title);
            let mut actions = LauncherSectionActions::default();
            if connections.is_empty() {
                return actions;
            }
            let section_id = ui.id().with(("ssh_launcher_connection_section", &id_salt));
            let available_width = ui.available_width();
            let column_count = launcher_grid_columns(available_width);
            let card_width = launcher_card_width(available_width, column_count);
            egui::Grid::new(section_id.with("grid"))
                .num_columns(column_count)
                .spacing(egui::vec2(LAUNCHER_CARD_GAP, LAUNCHER_CARD_GAP))
                .show(ui, |ui| {
                    for (index, connection) in connections.iter().copied().enumerate() {
                        let card = launcher_connection_card(
                            ui, section_id, connection, config, card_width,
                        );
                        if card.double_clicked() {
                            actions.connect = Some(connection.name.clone());
                        }
                        card.context_menu(|ui| {
                            if ui.button("编辑连接…").clicked() {
                                actions.edit = Some(connection.name.clone());
                                ui.close();
                            }
                        });
                        if (index + 1).is_multiple_of(column_count) {
                            ui.end_row();
                        }
                    }
                });
            actions
        })
        .inner
}

fn launcher_section_header(ui: &mut egui::Ui, title: &str) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        ui.label(RichText::new(title).weak().size(LAUNCHER_GROUP_FONT_SIZE));
    });
    ui.add_space(3.0);
}

fn launcher_group_label(group: &str) -> &str {
    if group.is_empty() { "根目录" } else { group }
}

fn launcher_group_separator(ui: &mut egui::Ui) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 13.0), egui::Sense::hover());
    let inset = f32::from(LAUNCHER_GROUP_INSET);
    ui.painter().line_segment(
        [
            egui::pos2(rect.left() + inset, rect.center().y),
            egui::pos2(rect.right() - inset, rect.center().y),
        ],
        ui.visuals().widgets.noninteractive.bg_stroke,
    );
}

fn group_launcher_connections(
    connections: &[SavedConnection],
) -> Vec<(&str, Vec<&SavedConnection>)> {
    let mut groups: Vec<(&str, Vec<&SavedConnection>)> = Vec::new();
    for connection in connections {
        if let Some((_, group_connections)) = groups
            .iter_mut()
            .find(|(group, _)| *group == connection.group.as_str())
        {
            group_connections.push(connection);
        } else {
            groups.push((connection.group.as_str(), vec![connection]));
        }
    }
    groups
}

fn launcher_grid_columns(available_width: f32) -> usize {
    (((available_width + LAUNCHER_CARD_GAP) / (LAUNCHER_CARD_MIN_WIDTH + LAUNCHER_CARD_GAP)).floor()
        as usize)
        .max(1)
}

fn launcher_card_width(available_width: f32, column_count: usize) -> f32 {
    let gaps = LAUNCHER_CARD_GAP * column_count.saturating_sub(1) as f32;
    ((available_width - gaps) / column_count as f32).max(1.0)
}

fn launcher_connection_card(
    ui: &mut egui::Ui,
    section_id: egui::Id,
    connection: &SavedConnection,
    config: &AppConfig,
    width: f32,
) -> egui::Response {
    let card_id = launcher_card_id(section_id, &connection.name);
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(width, LAUNCHER_CARD_HEIGHT),
        egui::Sense::hover(),
    );
    let hovered = ui.rect_contains_pointer(rect);
    let shadow_alpha = match (ui.visuals().dark_mode, hovered) {
        (true, true) => 105,
        (true, false) => 48,
        (false, true) => 55,
        (false, false) => 22,
    };
    let shadow = egui::Shadow {
        offset: [0, if hovered { 3 } else { 2 }],
        blur: if hovered { 11 } else { 7 },
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha),
    };
    ui.painter()
        .add(shadow.as_shape(rect, egui::CornerRadius::same(4)));
    ui.painter().rect_filled(
        rect,
        4.0,
        ui.visuals().extreme_bg_color.gamma_multiply(0.55),
    );

    let mut content_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(card_id.with("content"))
            .max_rect(rect.shrink2(egui::vec2(10.0, 8.0)))
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    content_ui.set_clip_rect(rect);
    content_ui.vertical(|ui| {
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
        content_ui.label(
            RichText::new(tag)
                .small()
                .color(Color32::from_rgb(0x4C, 0x9B, 0xE8)),
        );
    }
    content_ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.label(
            RichText::new(saved_auth_label(connection, config))
                .weak()
                .small(),
        );
    });

    ui.interact(rect, card_id, egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("双击连接")
}

fn launcher_card_id(section_id: egui::Id, connection_name: &str) -> egui::Id {
    section_id.with(("ssh_launcher_card", connection_name))
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
    use super::{
        group_launcher_connections, launcher_card_id, launcher_card_width, launcher_grid_columns,
        launcher_group_label, saved_auth_label,
    };
    use crate::config::{AppConfig, SavedConnection, SavedKey};

    #[test]
    fn launcher_groups_connections_by_directory_preserving_first_seen_order() {
        let connections = vec![
            SavedConnection {
                name: "prod-a".into(),
                group: "生产".into(),
                ..Default::default()
            },
            SavedConnection {
                name: "test".into(),
                group: "测试".into(),
                ..Default::default()
            },
            SavedConnection {
                name: "prod-b".into(),
                group: "生产".into(),
                ..Default::default()
            },
            SavedConnection {
                name: "root".into(),
                group: String::new(),
                ..Default::default()
            },
        ];

        let groups = group_launcher_connections(&connections);
        assert_eq!(
            groups.iter().map(|(group, _)| *group).collect::<Vec<_>>(),
            vec!["生产", "测试", ""]
        );
        assert_eq!(
            groups[0]
                .1
                .iter()
                .map(|connection| connection.name.as_str())
                .collect::<Vec<_>>(),
            vec!["prod-a", "prod-b"]
        );
    }

    #[test]
    fn launcher_grid_column_count_tracks_available_width() {
        assert_eq!(launcher_grid_columns(566.0), 1);
        assert_eq!(launcher_grid_columns(567.0), 2);
        assert_eq!(launcher_grid_columns(854.0), 3);
        assert_eq!(launcher_card_width(567.0, 2), 280.0);
        assert_eq!(launcher_card_width(854.0, 3), 280.0);
    }

    #[test]
    fn duplicate_connection_cards_use_section_scoped_ids() {
        let frequent_section = egui::Id::new(("launcher_section", "frequent"));
        let folder_section = egui::Id::new(("launcher_section", "production"));
        assert_ne!(
            launcher_card_id(frequent_section, "root@example.com"),
            launcher_card_id(folder_section, "root@example.com")
        );
    }

    #[test]
    fn launcher_group_label_names_the_root_directory() {
        assert_eq!(launcher_group_label(""), "根目录");
        assert_eq!(launcher_group_label("生产"), "生产");
    }

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
