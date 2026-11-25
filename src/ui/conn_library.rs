// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

use crate::app::RShellApp;
use crate::ui::file_manager::draw_folder_icon;
use crate::ui::icons::{Icon, icon_button};
use egui::{Color32, RichText, vec2};
use zeroize::Zeroize as _;

/// 密钥表单模式
#[derive(Clone, PartialEq, Eq, Default)]
pub enum KeyFormMode {
    #[default]
    Generate,
    Register,
    Edit(String),
}

/// 连接编辑时的认证/密钥选择
#[derive(Clone, PartialEq, Eq, Default)]
pub enum LcKeyMode {
    #[default]
    Password,
    /// 托管密钥 id
    Key(String),
    /// 自定义私钥路径
    Path,
}

/// 连接库文件树的虚拟根节点，仅用于运行时折叠状态。
const LIB_ROOT_NODE: &str = "\0rshell_all_root";

fn connection_library_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("rshell_connection_library")
}

impl RShellApp {
    pub(crate) fn open_connection_library_viewport(&mut self) {
        self.show_library = true;
        self.focus_library_viewport = true;
    }

    /// 连接库窗口:左文件夹 / 右连接列表,可连接/删除
    pub(crate) fn show_conn_library(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if !self.show_library {
            self.focus_library_viewport = false;
            // 允许启动页右键直接打开连接编辑窗口，而不必同时显示连接库主窗。
            self.show_lib_extras(&ctx);
            return;
        }
        let focus_viewport = std::mem::take(&mut self.focus_library_viewport);
        ctx.show_viewport_immediate(
            connection_library_viewport_id(),
            egui::ViewportBuilder::default()
                .with_title("RShell 连接库")
                .with_inner_size([780.0, 500.0])
                .with_min_inner_size([680.0, 400.0]),
            |viewport_ui, viewport_class| {
                let viewport_ctx = viewport_ui.ctx().clone();
                if viewport_ctx.input(|input| input.viewport().close_requested()) {
                    self.show_library = false;
                    return;
                }
                if focus_viewport && !matches!(viewport_class, egui::ViewportClass::EmbeddedWindow)
                {
                    crate::ui::settings::apply_native_window_theme(
                        &viewport_ctx,
                        self.config.theme_mode,
                    );
                    viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }

                let content_frame = egui::Frame::new()
                    .fill(viewport_ui.visuals().window_fill())
                    .inner_margin(egui::Margin::symmetric(14, 12));
                egui::CentralPanel::default()
                    .frame(content_frame)
                    .show(viewport_ui, |ui| {
                        crate::ui::dialog::polish_page_compact(ui);
                        // 工具栏
                        ui.horizontal(|ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::image_and_text(
                                                Icon::Download.image(14.0, Color32::WHITE),
                                                "导入",
                                            )
                                            .image_tint_follows_text_color(true),
                                        )
                                        .clicked()
                                    {
                                        self.open_connection_import();
                                    }
                                    if ui
                                        .add(
                                            egui::Button::image_and_text(
                                                Icon::Upload.image(14.0, Color32::WHITE),
                                                "导出",
                                            )
                                            .image_tint_follows_text_color(true),
                                        )
                                        .clicked()
                                    {
                                        self.open_connection_export(
                                            crate::export::ConnectionExportScope::All,
                                        );
                                    }
                                    if ui
                                        .add(
                                            egui::Button::image_and_text(
                                                Icon::KeyRound.image(14.0, Color32::WHITE),
                                                "管理密钥",
                                            )
                                            .image_tint_follows_text_color(true),
                                        )
                                        .clicked()
                                    {
                                        self.show_keys = true;
                                    }
                                    if ui
                                        .add(
                                            egui::Button::image_and_text(
                                                Icon::Plus.image(14.0, Color32::WHITE),
                                                "连接",
                                            )
                                            .image_tint_follows_text_color(true),
                                        )
                                        .clicked()
                                    {
                                        self.open_lib_new();
                                    }
                                },
                            );
                        });
                        if let Some(e) = &self.lib_error {
                            ui.colored_label(Color32::from_rgb(0xE8, 0x4C, 0x3D), e.clone());
                        }
                        if let Some(notice) = &self.lib_notice {
                            ui.colored_label(Color32::from_rgb(0x35, 0xA8, 0x60), notice.clone());
                        }

                        // 主体：左右独立面板，中间留出清晰分栏。
                        let h = ui.available_height();
                        let body_rect = egui::Rect::from_min_size(
                            ui.next_widget_position(),
                            vec2(ui.available_width(), h),
                        );
                        let _ = ui.allocate_rect(body_rect, egui::Sense::hover());
                        let left_w = 210.0;
                        let column_gap = 10.0;
                        let left_rect = egui::Rect::from_min_size(body_rect.min, vec2(left_w, h));
                        let right_rect = egui::Rect::from_min_max(
                            egui::pos2(left_rect.right() + column_gap, body_rect.top()),
                            body_rect.max,
                        );
                        let panel_fill = ui.visuals().faint_bg_color;
                        let panel_stroke = ui.visuals().widgets.noninteractive.bg_stroke;
                        ui.painter().rect(
                            left_rect,
                            6.0,
                            panel_fill,
                            panel_stroke,
                            egui::StrokeKind::Inside,
                        );
                        ui.painter().rect(
                            right_rect,
                            6.0,
                            panel_fill,
                            panel_stroke,
                            egui::StrokeKind::Inside,
                        );
                        let left_content = left_rect.shrink(8.0);
                        let right_content = right_rect.shrink(8.0);
                        ui.scope_builder(egui::UiBuilder::new().max_rect(body_rect), |ui| {
                            ui.scope_builder(
                                egui::UiBuilder::new()
                                    .max_rect(left_content)
                                    .layout(egui::Layout::top_down(egui::Align::LEFT)),
                                |ui| {
                                    let area = ui.available_rect_before_wrap();
                                    let footer_h = 30.0;
                                    let footer_rect = egui::Rect::from_min_max(
                                        egui::pos2(area.left(), area.bottom() - footer_h),
                                        area.max,
                                    );
                                    let list_rect = egui::Rect::from_min_max(
                                        area.min,
                                        egui::pos2(area.right(), footer_rect.top() - 6.0),
                                    );
                                    let _ = ui.allocate_rect(area, egui::Sense::hover());
                                    ui.scope_builder(
                                        egui::UiBuilder::new()
                                            .max_rect(list_rect)
                                            .layout(egui::Layout::top_down(egui::Align::LEFT)),
                                        |ui| {
                                            egui::ScrollArea::vertical()
                                                .id_salt("lib_folders")
                                                .auto_shrink([false, false])
                                                .show(ui, |ui| self.lib_render_folders(ui));
                                        },
                                    );
                                    ui.scope_builder(
                                        egui::UiBuilder::new().max_rect(footer_rect).layout(
                                            egui::Layout::left_to_right(egui::Align::Center),
                                        ),
                                        |ui| {
                                            if ui
                                                .add(
                                                    egui::Button::image_and_text(
                                                        Icon::Plus.image(13.0, Color32::WHITE),
                                                        "文件夹",
                                                    )
                                                    .image_tint_follows_text_color(true),
                                                )
                                                .clicked()
                                            {
                                                self.open_folder_new();
                                            }
                                            let has = self.lib_folder.is_some();
                                            if ui
                                                .add_enabled(has, egui::Button::new("重命名"))
                                                .clicked()
                                                && let Some(folder) = self.lib_folder.clone()
                                            {
                                                self.open_folder_rename(&folder);
                                            }
                                            if ui
                                                .add_enabled(has, egui::Button::new("删除"))
                                                .clicked()
                                                && let Some(folder) = self.lib_folder.clone()
                                            {
                                                self.folder_delete_pending = Some(folder);
                                                self.folder_delete_armed = false;
                                            }
                                        },
                                    );
                                },
                            );
                            ui.scope_builder(
                                egui::UiBuilder::new()
                                    .max_rect(right_content)
                                    .layout(egui::Layout::top_down(egui::Align::LEFT)),
                                |ui| {
                                    ui.horizontal(|ui| {
                                        let title = self
                                            .lib_folder
                                            .as_deref()
                                            .map(|f| format!("连接列表 · {}", folder_name(f)))
                                            .unwrap_or_else(|| "连接列表 · 全部".to_string());
                                        ui.label(RichText::new(title).strong());
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.label(RichText::new("认证方式").weak().small());
                                            },
                                        );
                                    });
                                    ui.separator();
                                    let area = ui.available_rect_before_wrap();
                                    let footer_h = 30.0;
                                    let footer_rect = egui::Rect::from_min_max(
                                        egui::pos2(area.left(), area.bottom() - footer_h),
                                        area.max,
                                    );
                                    let list_rect = egui::Rect::from_min_max(
                                        area.min,
                                        egui::pos2(area.right(), footer_rect.top() - 6.0),
                                    );
                                    let _ = ui.allocate_rect(area, egui::Sense::hover());
                                    ui.scope_builder(
                                        egui::UiBuilder::new()
                                            .max_rect(list_rect)
                                            .layout(egui::Layout::top_down(egui::Align::LEFT)),
                                        |ui| {
                                            egui::ScrollArea::vertical()
                                                .id_salt("lib_conns")
                                                .auto_shrink([false, false])
                                                .show(ui, |ui| self.lib_render_list(ui, &ctx));
                                        },
                                    );
                                    ui.scope_builder(
                                        egui::UiBuilder::new().max_rect(footer_rect).layout(
                                            egui::Layout::left_to_right(egui::Align::Center),
                                        ),
                                        |ui| {
                                            let has = self.lib_conn.is_some();
                                            if ui
                                                .add_enabled(has, egui::Button::new("连接"))
                                                .clicked()
                                            {
                                                self.lib_connect(&ctx);
                                            }
                                            if ui
                                                .add_enabled(
                                                    has,
                                                    egui::Button::image_and_text(
                                                        Icon::Pencil.image(13.0, Color32::WHITE),
                                                        "编辑",
                                                    )
                                                    .image_tint_follows_text_color(true),
                                                )
                                                .clicked()
                                                && let Some(name) = self.lib_conn.clone()
                                            {
                                                self.open_lib_edit(&name);
                                            }
                                            if ui
                                                .add_enabled(
                                                    has,
                                                    egui::Button::image_and_text(
                                                        Icon::Trash.image(13.0, Color32::WHITE),
                                                        "删除",
                                                    )
                                                    .image_tint_follows_text_color(true),
                                                )
                                                .clicked()
                                            {
                                                self.lib_delete();
                                            }
                                        },
                                    );
                                },
                            );
                        });
                    });
                self.show_lib_extras(&viewport_ctx);
            },
        );
    }

    /// 左栏文件树：“全部”为根节点，各文件夹统一显示为它的子节点。
    fn lib_render_folders(&mut self, ui: &mut egui::Ui) {
        // 层级仅通过缩进表达，不绘制 egui::indent 默认的左侧辅助竖线。
        ui.style_mut().visuals.indent_has_left_vline = false;
        let folders = folder_tree_paths(&self.config.folders);
        let root_selected = self.lib_folder.is_none();
        let root_collapsed = self.lib_collapsed_folders.contains(LIB_ROOT_NODE);
        ui.horizontal(|ui| {
            if folders.is_empty() {
                ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::hover());
            } else {
                let icon = if root_collapsed {
                    Icon::ChevronRight
                } else {
                    Icon::ChevronDown
                };
                if icon_button(
                    ui,
                    icon,
                    if root_collapsed { "展开" } else { "折叠" },
                    vec2(14.0, 14.0),
                    10.0,
                    ui.visuals().weak_text_color(),
                )
                .clicked()
                {
                    if root_collapsed {
                        self.lib_collapsed_folders.remove(LIB_ROOT_NODE);
                    } else {
                        self.lib_collapsed_folders.insert(LIB_ROOT_NODE.into());
                        self.lib_folder = None;
                        self.lib_conn = None;
                    }
                }
            }
            let (icon_rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::hover());
            draw_folder_icon(
                ui,
                icon_rect,
                if root_selected {
                    Color32::from_rgb(0x4C, 0x9B, 0xE8)
                } else {
                    ui.visuals().weak_text_color()
                },
            );
            let root_response = ui.selectable_label(root_selected, RichText::new("全部").strong());
            if root_response.clicked() {
                self.lib_folder = None;
                self.lib_conn = None;
            }
            if root_response.secondary_clicked() {
                self.lib_folder = None;
                self.lib_conn = None;
            }
            root_response.context_menu(|ui| {
                if ui.button("导出…").clicked() {
                    self.open_connection_export(crate::export::ConnectionExportScope::All);
                    ui.close();
                }
            });
        });

        if !root_collapsed {
            ui.indent("lib_all_root_children", |ui| {
                self.lib_render_folder_children(ui, "", &folders);
            });
        }
    }

    /// 递归绘制指定父路径的直接子目录，形成真实的嵌套树。
    fn lib_render_folder_children(&mut self, ui: &mut egui::Ui, parent: &str, folders: &[String]) {
        let children: Vec<String> = folders
            .iter()
            .filter(|folder| folder_parent(folder) == parent)
            .cloned()
            .collect();
        for f in children {
            let sel = self.lib_folder.as_deref() == Some(&f);
            let has_children = folders
                .iter()
                .any(|candidate| folder_parent(candidate) == f);
            let collapsed = self.lib_collapsed_folders.contains(&f);
            ui.horizontal(|ui| {
                if has_children {
                    let icon = if collapsed {
                        Icon::ChevronRight
                    } else {
                        Icon::ChevronDown
                    };
                    if icon_button(
                        ui,
                        icon,
                        if collapsed { "展开" } else { "折叠" },
                        vec2(14.0, 14.0),
                        10.0,
                        ui.visuals().weak_text_color(),
                    )
                    .clicked()
                    {
                        if collapsed {
                            self.lib_collapsed_folders.remove(&f);
                        } else {
                            self.lib_collapsed_folders.insert(f.clone());
                            let child_prefix = format!("{f}/");
                            if self
                                .lib_folder
                                .as_deref()
                                .is_some_and(|selected| selected.starts_with(&child_prefix))
                            {
                                self.lib_folder = Some(f.clone());
                                self.lib_conn = None;
                            }
                        }
                    }
                } else {
                    ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::hover());
                }
                let (ir, _) = ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::hover());
                draw_folder_icon(
                    ui,
                    ir,
                    if sel {
                        Color32::from_rgb(0x4C, 0x9B, 0xE8)
                    } else {
                        ui.visuals().weak_text_color()
                    },
                );
                let folder_response = ui.selectable_label(sel, folder_name(&f));
                if folder_response.clicked() {
                    self.lib_folder = Some(f.clone());
                    self.lib_conn = None;
                }
                if folder_response.secondary_clicked() {
                    self.lib_folder = Some(f.clone());
                    self.lib_conn = None;
                }
                folder_response.context_menu(|ui| {
                    if ui.button("导出…").clicked() {
                        self.open_connection_export(crate::export::ConnectionExportScope::Folder(
                            f.clone(),
                        ));
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("重命名").clicked() {
                        self.open_folder_rename(&f);
                        ui.close();
                    }
                    if ui
                        .button(RichText::new("删除").color(Color32::from_rgb(0xE8, 0x4C, 0x3D)))
                        .clicked()
                    {
                        self.folder_delete_pending = Some(f.clone());
                        self.folder_delete_armed = false;
                        ui.close();
                    }
                });
            });

            if has_children && !collapsed {
                ui.indent(ui.id().with(("folder_children", &f)), |ui| {
                    self.lib_render_folder_children(ui, &f, folders);
                });
            }
        }
    }

    /// 右栏:当前文件夹下的连接(右键菜单 → 动作在循环外分发,避免闭包借用冲突)
    fn lib_render_list(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let folder = self.lib_folder.clone();
        let conns: Vec<_> = self
            .config
            .connections
            .iter()
            .filter(|c| match &folder {
                None => true,
                Some(f) => &c.group == f,
            })
            .collect();
        if conns.is_empty() {
            ui.label(RichText::new("（空，点右上 ＋ 连接 添加）").weak());
            return;
        }
        let mut act: Option<(String, LibConnAction)> = None;
        for (row_index, c) in conns.into_iter().enumerate() {
            let sel = self.lib_conn.as_deref() == Some(&c.name);
            let (dn, host) = connection_list_identity(c);
            let kl = key_label(c, &self.config);
            let fill = if sel {
                ui.visuals().selection.bg_fill.gamma_multiply(0.55)
            } else if row_index % 2 == 0 {
                ui.visuals().extreme_bg_color.gamma_multiply(0.55)
            } else {
                Color32::TRANSPARENT
            };
            let row = egui::Frame::new()
                .fill(fill)
                .corner_radius(egui::CornerRadius::same(4))
                .inner_margin(egui::Margin::symmetric(7, 6))
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(if sel {
                            RichText::new(dn).strong()
                        } else {
                            RichText::new(dn)
                        });
                        ui.add_space(4.0);
                        ui.label(RichText::new(host).weak().size(9.0).monospace());
                        for t in &c.tags {
                            tag_chip(ui, t);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(&kl).weak().small());
                        });
                    });
                });
            let resp = ui.interact(
                row.response.rect,
                ui.id().with(("lib_conn_row", &c.name)),
                egui::Sense::click(),
            );
            if resp.hovered() {
                ui.painter().rect_stroke(
                    row.response.rect,
                    4.0,
                    ui.visuals().widgets.hovered.bg_stroke,
                    egui::StrokeKind::Inside,
                );
            }
            if resp.clicked() {
                act = Some((c.name.clone(), LibConnAction::Select));
            }
            if resp.double_clicked() {
                act = Some((c.name.clone(), LibConnAction::Connect));
            }
            resp.context_menu(|ui| {
                if ui.button("连接").clicked() {
                    act = Some((c.name.clone(), LibConnAction::Connect));
                    ui.close();
                }
                if ui.button("编辑").clicked() {
                    act = Some((c.name.clone(), LibConnAction::Edit));
                    ui.close();
                }
                if ui.button("移动到文件夹…").clicked() {
                    act = Some((c.name.clone(), LibConnAction::Move));
                    ui.close();
                }
                if ui.button("编辑标签…").clicked() {
                    act = Some((c.name.clone(), LibConnAction::Tags));
                    ui.close();
                }
                if ui.button("导出…").clicked() {
                    act = Some((c.name.clone(), LibConnAction::Export));
                    ui.close();
                }
                ui.separator();
                if ui
                    .button(RichText::new("删除").color(Color32::from_rgb(0xE8, 0x4C, 0x3D)))
                    .clicked()
                {
                    act = Some((c.name.clone(), LibConnAction::Delete));
                    ui.close();
                }
            });
        }
        if let Some((name, a)) = act {
            match a {
                LibConnAction::Select => {
                    self.lib_conn = Some(name);
                    self.lib_error = None;
                }
                LibConnAction::Connect => {
                    self.lib_conn = Some(name);
                    self.lib_connect(ctx);
                }
                LibConnAction::Edit => self.open_lib_edit(&name),
                LibConnAction::Delete => {
                    self.lib_conn = Some(name);
                    self.lib_delete();
                }
                LibConnAction::Move => {
                    self.move_folder_name = Some(name);
                }
                LibConnAction::Tags => self.open_tag_edit(&name),
                LibConnAction::Export => self
                    .open_connection_export(crate::export::ConnectionExportScope::Connection(name)),
            }
        }
    }

    fn open_connection_export(&mut self, scope: crate::export::ConnectionExportScope) {
        self.connection_export_preserve_folders = scope.default_preserve_folders();
        self.connection_export_scope = Some(scope);
        self.connection_export_encrypted = true;
        self.connection_export_password.clear();
        self.connection_export_password_confirm.clear();
        self.connection_export_error = None;
    }

    fn show_connection_export_dialog(&mut self, ctx: &egui::Context) {
        let Some(scope) = self.connection_export_scope.clone() else {
            return;
        };
        let count = scope.connections(&self.config).len();
        let mut open = true;
        let mut close = false;
        egui::Window::new("导出 SSH")
            .id(egui::Id::new("connection_export_dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(ctx))
            .show(ctx, |ui| {
                crate::ui::dialog::polish(ui);
                ui.set_width(390.0);
                egui::Grid::new("connection_export_summary")
                    .num_columns(2)
                    .spacing([18.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("导出范围");
                        ui.label(scope.title());
                        ui.end_row();
                        ui.label("连接数量");
                        ui.label(count.to_string());
                        ui.end_row();
                    });

                ui.add_space(8.0);
                ui.label("文件格式");
                ui.horizontal(|ui| {
                    ui.radio_value(&mut self.connection_export_encrypted, true, "加密 JSON");
                    ui.radio_value(&mut self.connection_export_encrypted, false, "明文 JSON");
                });

                ui.add_space(5.0);
                ui.add_enabled(
                    scope.can_choose_preserve_folders(),
                    egui::Checkbox::new(
                        &mut self.connection_export_preserve_folders,
                        "保留目录结构",
                    ),
                );
                self.connection_export_preserve_folders =
                    scope.effective_preserve_folders(self.connection_export_preserve_folders);

                if self.connection_export_encrypted {
                    ui.add_space(5.0);
                    egui::Grid::new("connection_export_passwords")
                        .num_columns(2)
                        .spacing([18.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("加密密码");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.connection_export_password)
                                    .password(true)
                                    .desired_width(240.0),
                            );
                            ui.end_row();
                            ui.label("确认密码");
                            ui.add(
                                egui::TextEdit::singleline(
                                    &mut self.connection_export_password_confirm,
                                )
                                .password(true)
                                .desired_width(240.0),
                            );
                            ui.end_row();
                        });
                } else {
                    ui.add_space(5.0);
                    ui.colored_label(
                        Color32::from_rgb(0xD9, 0x78, 0x32),
                        "明文 JSON 会直接包含已保存的密码或密钥口令。",
                    );
                }

                if let Some(error) = &self.connection_export_error {
                    ui.add_space(5.0);
                    ui.colored_label(Color32::from_rgb(0xE8, 0x4C, 0x3D), error);
                }

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let action_size = vec2(56.0, 28.0);
                    if crate::ui::dialog::compact_primary_button(ui, "导出").clicked()
                        && self.perform_connection_export(&scope)
                    {
                        close = true;
                    }
                    if ui
                        .add_sized(action_size, egui::Button::new("取消"))
                        .clicked()
                    {
                        close = true;
                    }
                });
            });

        if close || !open {
            self.connection_export_scope = None;
            self.connection_export_password.clear();
            self.connection_export_password_confirm.clear();
            self.connection_export_error = None;
        }
    }

    fn perform_connection_export(&mut self, scope: &crate::export::ConnectionExportScope) -> bool {
        let connections = scope.connections(&self.config);
        if connections.is_empty() {
            self.connection_export_error = Some("当前范围内没有可导出的 SSH 连接".into());
            return false;
        }
        if self.connection_export_encrypted {
            if self.connection_export_password.is_empty() {
                self.connection_export_error = Some("请输入加密密码".into());
                return false;
            }
            if self.connection_export_password != self.connection_export_password_confirm {
                self.connection_export_error = Some("两次输入的密码不一致".into());
                return false;
            }
        }

        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .set_file_name(scope.default_file_name())
            .save_file()
        else {
            return false;
        };
        let password = self
            .connection_export_encrypted
            .then_some(self.connection_export_password.as_str());
        let folders = scope.folders(&self.config, self.connection_export_preserve_folders);
        let json = match crate::export::build_json(
            &self.config,
            &connections,
            folders,
            scope.include_connection_group(),
            password,
        ) {
            Ok(json) => json,
            Err(error) => {
                self.connection_export_error = Some(error);
                return false;
            }
        };
        match crate::export::write_json(&path, &json) {
            Ok(()) => {
                self.connection_export_error = None;
                true
            }
            Err(error) => {
                self.connection_export_error = Some(error);
                false
            }
        }
    }

    fn open_connection_import(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("JSON", &["json"])
            .pick_file()
        else {
            return;
        };
        match crate::export::ConnectionImportState::open(path) {
            Ok(state) => {
                self.connection_import = Some(state);
                self.lib_error = None;
            }
            Err(error) => {
                self.lib_error = Some(error);
            }
        }
    }

    fn show_connection_import_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut state) = self.connection_import.take() else {
            return;
        };
        let mut open = true;
        let mut close = false;
        let mut import_requested = false;
        egui::Window::new("导入 SSH")
            .id(egui::Id::new("connection_import_dialog"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(ctx))
            .show(ctx, |ui| {
                crate::ui::dialog::polish(ui);
                ui.set_width(390.0);
                egui::Grid::new("connection_import_summary")
                    .num_columns(2)
                    .spacing([18.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("文件");
                        ui.add_sized(
                            [285.0, 22.0],
                            egui::Label::new(state.file_name()).truncate(),
                        );
                        ui.end_row();
                        ui.label("格式");
                        ui.label(if state.encrypted {
                            "加密 JSON"
                        } else {
                            "明文 JSON"
                        });
                        ui.end_row();
                        if let Some(count) = state.plaintext_connection_count {
                            ui.label("连接数量");
                            ui.label(count.to_string());
                            ui.end_row();
                        }
                    });

                if state.encrypted {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.label("文件密码");
                        ui.add(
                            egui::TextEdit::singleline(&mut state.password)
                                .password(true)
                                .desired_width(255.0),
                        );
                    });
                }

                if let Some(error) = &state.error {
                    ui.add_space(6.0);
                    ui.colored_label(Color32::from_rgb(0xE8, 0x4C, 0x3D), error);
                }

                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let action_size = vec2(56.0, 28.0);
                    import_requested =
                        crate::ui::dialog::compact_primary_button(ui, "导入").clicked();
                    if ui
                        .add_sized(action_size, egui::Button::new("取消"))
                        .clicked()
                    {
                        close = true;
                    }
                });
            });

        if import_requested {
            let password = state.encrypted.then_some(state.password.as_str());
            match crate::export::decode_json(&state.json, password) {
                Ok(package) => {
                    let mut imported_config = self.config.clone();
                    match crate::export::apply_import(&mut imported_config, package) {
                        Ok(summary) => match crate::config::save(&imported_config) {
                            Ok(()) => {
                                self.config = imported_config;
                                self.lib_folder = None;
                                self.lib_conn = None;
                                self.lib_error = None;
                                self.lib_notice = Some(format!(
                                    "导入完成：新增 {}，合并 {}，新增文件夹 {}",
                                    summary.added, summary.merged, summary.folders_added
                                ));
                                close = true;
                            }
                            Err(error) => {
                                state.error = Some(format!("保存导入结果失败：{error:#}"));
                            }
                        },
                        Err(error) => state.error = Some(error),
                    }
                }
                Err(error) => state.error = Some(error),
            }
        }

        if !close && open {
            self.connection_import = Some(state);
        }
    }

    fn lib_connect(&mut self, ctx: &egui::Context) {
        let Some(name) = self.lib_conn.clone() else {
            return;
        };
        let Some(conn) = self
            .config
            .connections
            .iter()
            .find(|c| c.name == name)
            .cloned()
        else {
            return;
        };
        match crate::ui::connect_dialog::params_from_saved(&conn, &self.config) {
            Ok(p) => {
                let title = conn.session_title();
                self.open_ssh(ctx, p);
                if let Some(session) = self.sessions.get_mut(self.active) {
                    session.title = title;
                }
                self.lib_error = None;
            }
            Err(e) => self.lib_error = Some(e),
        }
    }

    fn lib_delete(&mut self) {
        let Some(name) = self.lib_conn.clone() else {
            return;
        };
        if let Some(i) = self.config.connections.iter().position(|c| c.name == name) {
            let c = self.config.connections.remove(i);
            self.config.remove_connection_usage(&c.name);
            crate::secrets::delete(&crate::secrets::account_for(&c.user, &c.host, c.port));
            let _ = crate::config::save(&self.config);
        }
        self.lib_conn = None;
    }

    // =====================================================================
    // 连接编辑
    // =====================================================================
    fn open_lib_new(&mut self) {
        self.lc_orig = None;
        self.lc_alias.clear();
        self.lc_group = self.lib_folder.clone().unwrap_or_default();
        self.lc_tags.clear();
        self.lc_host.clear();
        self.lc_port = "22".into();
        self.lc_user = "root".into();
        self.lc_pass.zeroize();
        self.lc_remember = true;
        self.lc_key_mode = LcKeyMode::Password;
        self.lc_key_path.clear();
        self.lc_error = None;
        self.lc_open = true;
    }

    pub(crate) fn open_lib_edit(&mut self, name: &str) {
        let Some(c) = self
            .config
            .connections
            .iter()
            .find(|c| c.name == name)
            .cloned()
        else {
            return;
        };
        self.lc_orig = Some(c.name.clone());
        self.lc_alias = c.alias.clone();
        self.lc_group = c.group.clone();
        self.lc_tags = c.tags.join(", ");
        self.lc_host = c.host.clone();
        self.lc_port = c.port.to_string();
        self.lc_user = c.user.clone();
        self.lc_key_mode = if let Some(kid) = c.key_id {
            LcKeyMode::Key(kid)
        } else if !c.key_path.is_empty() {
            LcKeyMode::Path
        } else {
            LcKeyMode::Password
        };
        self.lc_key_path = c.key_path.clone();
        // 密码认证时回填 keyring 口令
        self.lc_pass.zeroize();
        self.lc_pass = if matches!(self.lc_key_mode, LcKeyMode::Password) {
            crate::secrets::get(&crate::secrets::account_for(&c.user, &c.host, c.port))
                .map(|value| value.expose().to_owned())
                .unwrap_or_default()
        } else {
            String::new()
        };
        self.lc_remember = !self.lc_pass.is_empty();
        self.lc_error = None;
        self.lc_open = true;
    }

    fn save_lib_edit(&mut self) -> bool {
        let host = self.lc_host.trim().to_string();
        if host.is_empty() {
            self.lc_error = Some("主机不能为空".into());
            return false;
        }
        let port: u16 = match self.lc_port.trim().parse() {
            Ok(p) => p,
            Err(_) => {
                self.lc_error = Some("端口非法".into());
                return false;
            }
        };
        let user = self.lc_user.trim().to_string();
        if user.is_empty() {
            self.lc_error = Some("用户名不能为空".into());
            return false;
        }
        let tags = parse_tags(&self.lc_tags);
        let (key_id, key_path) = match &self.lc_key_mode {
            LcKeyMode::Password => (None, String::new()),
            LcKeyMode::Key(id) => (Some(id.clone()), String::new()),
            LcKeyMode::Path => (None, self.lc_key_path.trim().to_string()),
        };
        let new_name = crate::secrets::account_for(&user, &host, port);
        let account = new_name.clone();
        // keyring:仅密码认证存口令
        if matches!(self.lc_key_mode, LcKeyMode::Password) {
            if self.lc_remember && !self.lc_pass.is_empty() {
                let _ = crate::secrets::set(&account, &self.lc_pass);
            } else {
                crate::secrets::delete(&account);
            }
        } else {
            crate::secrets::delete(&account);
        }
        // 身份变了:删旧条目 + 旧 keyring
        if let Some(orig) = &self.lc_orig
            && orig != &new_name
        {
            self.config.rename_connection_usage(orig, &new_name);
            self.config.connections.retain(|c| &c.name != orig);
            crate::secrets::delete(orig);
        }
        let conn = crate::config::SavedConnection {
            name: new_name.clone(),
            host,
            port,
            user,
            alias: self.lc_alias.trim().to_string(),
            group: self.lc_group.trim().to_string(),
            tags,
            key_id,
            key_path,
        };
        if let Some(c) = self
            .config
            .connections
            .iter_mut()
            .find(|c| c.name == new_name)
        {
            *c = conn;
        } else {
            self.config.connections.push(conn);
        }
        // 确保分组文件夹存在
        let g = self.lc_group.trim().to_string();
        if !g.is_empty() && !self.config.folders.iter().any(|f| f == &g) {
            self.config.folders.push(g);
        }
        let _ = crate::config::save(&self.config);
        self.lc_error = None;
        true
    }

    fn show_lib_edit_window(&mut self, ctx: &egui::Context) {
        if !self.lc_open {
            return;
        }
        let title = if self.lc_orig.is_some() {
            "编辑连接"
        } else {
            "新建连接"
        };
        let mut open = self.lc_open;
        let mut close = false;
        egui::Window::new(title)
            .id(egui::Id::new("conn_library_connection_form"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(ctx))
            .show(ctx, |ui| {
                crate::ui::dialog::polish(ui);
                ui.set_width(340.0);
                egui::Grid::new("lc_edit_grid")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("显示名");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.lc_alias)
                                .hint_text("可选，用于列表和标签页"),
                        );
                        ui.end_row();
                        ui.label("分组");
                        self.lc_group_combo(ui);
                        ui.end_row();
                        ui.label("标签");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.lc_tags)
                                .hint_text("逗号分隔,如 web, prod"),
                        );
                        ui.end_row();
                        ui.label("主机");
                        ui.text_edit_singleline(&mut self.lc_host);
                        ui.end_row();
                        ui.label("端口");
                        ui.add(egui::TextEdit::singleline(&mut self.lc_port).desired_width(80.0));
                        ui.end_row();
                        ui.label("用户名");
                        ui.text_edit_singleline(&mut self.lc_user);
                        ui.end_row();
                        ui.label("认证");
                        ui.horizontal(|ui| {
                            self.lc_key_combo(ui);
                            ui.menu_button("＋ 新建密钥", |ui| {
                                if ui.button("生成新密钥…").clicked() {
                                    self.open_key_form(KeyFormMode::Generate);
                                    self.key_form_assign_to_lc = true;
                                    ui.close();
                                }
                                if ui.button("登记已有私钥…").clicked() {
                                    self.open_key_form(KeyFormMode::Register);
                                    self.key_form_assign_to_lc = true;
                                    ui.close();
                                }
                            })
                            .response
                            .on_hover_text("在这里添加密钥，完成后自动用于当前连接");
                        });
                        ui.end_row();
                    });
                match &self.lc_key_mode {
                    LcKeyMode::Password => {
                        egui::Grid::new("lc_pass_grid")
                            .num_columns(2)
                            .spacing([10.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("密码");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.lc_pass).password(true),
                                );
                                ui.end_row();
                                ui.label("");
                                ui.checkbox(
                                    &mut self.lc_remember,
                                    self.config.password_storage.remember_password_label(),
                                );
                                ui.end_row();
                            });
                    }
                    LcKeyMode::Path => {
                        ui.horizontal(|ui| {
                            ui.label("私钥");
                            ui.text_edit_singleline(&mut self.lc_key_path);
                            if ui.button("…").clicked()
                                && let Some(p) = rfd::FileDialog::new().pick_file()
                            {
                                self.lc_key_path = p.to_string_lossy().into_owned();
                            }
                        });
                    }
                    LcKeyMode::Key(_) => {}
                }
                if let Some(e) = &self.lc_error {
                    ui.add_space(6.0);
                    ui.colored_label(Color32::from_rgb(0xE8, 0x4C, 0x3D), e.clone());
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let action_button_size = egui::vec2(56.0, 28.0);
                    if crate::ui::dialog::compact_primary_button(ui, "保存").clicked()
                        && self.save_lib_edit()
                    {
                        close = true;
                    }
                    if ui
                        .add_sized(action_button_size, egui::Button::new("取消"))
                        .clicked()
                    {
                        close = true;
                    }
                });
            });
        if close {
            open = false;
        }
        self.lc_open = open;
        if !open {
            self.lc_pass.zeroize();
        }
    }

    /// 分组选择器：只能选择连接库中存在的文件夹，也可放回根目录。
    fn lc_group_combo(&mut self, ui: &mut egui::Ui) {
        connection_group_combo(ui, "lc_group", &mut self.lc_group, &self.config.folders);
    }

    /// 认证/密钥下拉
    fn lc_key_combo(&mut self, ui: &mut egui::Ui) {
        connection_key_combo(ui, "lc_key_mode", &mut self.lc_key_mode, &self.config.keys);
    }

    // =====================================================================
    // 文件夹 CRUD
    // =====================================================================
    fn open_folder_new(&mut self) {
        self.folder_new_parent = self.lib_folder.clone().unwrap_or_default();
        self.folder_new_name.clear();
        self.folder_new_focus = true;
        self.folder_new_open = true;
    }

    fn apply_folder_new(&mut self) -> bool {
        let name = self.folder_new_name.trim().to_string();
        if name.is_empty() {
            self.lib_error = Some("文件夹名不能为空".into());
            return false;
        }
        let path = if self.folder_new_parent.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", self.folder_new_parent, name)
        };
        if self.config.folders.iter().any(|f| f == &path) {
            self.lib_error = Some("文件夹已存在".into());
            return false;
        }
        self.config.folders.push(path.clone());
        let _ = crate::config::save(&self.config);
        self.lib_folder = Some(path);
        self.lib_error = None;
        true
    }

    fn open_folder_rename(&mut self, from: &str) {
        self.folder_rename_from = Some(from.to_string());
        self.folder_rename_to = from.rsplit('/').next().unwrap_or(from).to_string();
        self.folder_rename_focus = true;
    }

    fn apply_folder_rename(&mut self) -> bool {
        let Some(from) = self.folder_rename_from.clone() else {
            return false;
        };
        let to_name = self.folder_rename_to.trim().to_string();
        if to_name.is_empty() {
            self.lib_error = Some("名称不能为空".into());
            return false;
        }
        let new_path = match from.rfind('/') {
            Some(i) => format!("{}/{}", &from[..i], to_name),
            None => to_name,
        };
        if new_path == from {
            return true;
        }
        if self.config.folders.iter().any(|f| f == &new_path) {
            self.lib_error = Some("目标已存在".into());
            return false;
        }
        // 改文件夹表 + 连接分组(含子目录前缀替换)
        for f in &mut self.config.folders {
            if f == &from || f.starts_with(&format!("{from}/")) {
                *f = format!("{new_path}{}", &f[from.len()..]);
            }
        }
        for c in &mut self.config.connections {
            if c.group == from || c.group.starts_with(&format!("{from}/")) {
                c.group = format!("{new_path}{}", &c.group[from.len()..]);
            }
        }
        let old_collapsed = std::mem::take(&mut self.lib_collapsed_folders);
        for folder in old_collapsed {
            if folder == from || folder.starts_with(&format!("{from}/")) {
                self.lib_collapsed_folders
                    .insert(format!("{new_path}{}", &folder[from.len()..]));
            } else {
                self.lib_collapsed_folders.insert(folder);
            }
        }
        let _ = crate::config::save(&self.config);
        if let Some(selected) = self.lib_folder.clone()
            && (selected == from || selected.starts_with(&format!("{from}/")))
        {
            self.lib_folder = Some(format!("{new_path}{}", &selected[from.len()..]));
        }
        self.lib_error = None;
        true
    }

    fn delete_folder(&mut self, f: &str) {
        let has_sub = self
            .config
            .folders
            .iter()
            .any(|x| x.starts_with(&format!("{f}/")));
        let has_conn = self.config.connections.iter().any(|c| c.group == f);
        if has_sub || has_conn {
            self.lib_error = Some("文件夹非空,请先移走内容".into());
            return;
        }
        self.config.folders.retain(|x| x != f);
        self.lib_collapsed_folders.remove(f);
        if self.lib_folder.as_deref() == Some(f) {
            self.lib_folder = None;
        }
        let _ = crate::config::save(&self.config);
        self.lib_error = None;
    }

    fn show_folder_dialogs(&mut self, ctx: &egui::Context) {
        if self.folder_new_open {
            let parent = self.folder_new_parent.clone();
            let mut open = true;
            let mut close = false;
            let title_close = crate::ui::dialog::compact_dialog_window(
                ctx,
                "新建文件夹",
                Some(egui::Id::new("conn_library_folder_new")),
                &mut open,
                |ui| {
                    crate::ui::dialog::polish(ui);
                    ui.label(if parent.is_empty() {
                        "在根目录新建".to_string()
                    } else {
                        format!("在 [{parent}] 下新建")
                    });
                    let name_id = egui::Id::new("conn_library_folder_new_name");
                    let mut submit = false;
                    let mut cancel = false;
                    ui.horizontal(|ui| {
                        ui.label("名称");
                        let resp = ui
                            .add(egui::TextEdit::singleline(&mut self.folder_new_name).id(name_id));
                        if std::mem::take(&mut self.folder_new_focus) {
                            crate::ui::dialog::focus_text_input(
                                &resp,
                                ui,
                                name_id,
                                self.folder_new_name.chars().count(),
                            );
                        }
                        submit |=
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    });
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 30.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let action_button_size = egui::vec2(56.0, 28.0);
                            submit |=
                                crate::ui::dialog::compact_primary_button(ui, "创建").clicked();
                            cancel |= ui
                                .add_sized(action_button_size, egui::Button::new("取消"))
                                .clicked();
                        },
                    );
                    cancel |= ui.input(|i| i.key_pressed(egui::Key::Escape));
                    if submit {
                        crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                        if self.apply_folder_new() {
                            close = true;
                        } else {
                            // 名称无效/重名时把焦点留回弹窗。
                            self.folder_new_focus = true;
                        }
                    }
                    if cancel {
                        crate::ui::dialog::consume_handled_key(ui, egui::Key::Escape);
                        close = true;
                    }
                },
            );
            if title_close || close {
                open = false;
            }
            if !open {
                self.folder_new_open = false;
            }
        }
        if self.folder_rename_from.is_some() {
            let mut open = true;
            let mut close = false;
            let title_close = crate::ui::dialog::compact_dialog_window(
                ctx,
                "重命名文件夹",
                Some(egui::Id::new("conn_library_folder_rename")),
                &mut open,
                |ui| {
                    crate::ui::dialog::polish(ui);
                    let name_id = egui::Id::new("conn_library_folder_rename_name");
                    let mut submit = false;
                    let mut cancel = false;
                    ui.horizontal(|ui| {
                        ui.label("新名称");
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.folder_rename_to).id(name_id),
                        );
                        if std::mem::take(&mut self.folder_rename_focus) {
                            crate::ui::dialog::focus_text_input(
                                &resp,
                                ui,
                                name_id,
                                self.folder_rename_to.chars().count(),
                            );
                        }
                        // 单行文本框回车会交出焦点，在这里确认，避免回车落到下层终端。
                        submit |=
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    });
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 30.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let action_button_size = egui::vec2(56.0, 28.0);
                            submit |=
                                crate::ui::dialog::compact_primary_button(ui, "确定").clicked();
                            cancel |= ui
                                .add_sized(action_button_size, egui::Button::new("取消"))
                                .clicked();
                        },
                    );
                    cancel |= ui.input(|i| i.key_pressed(egui::Key::Escape));
                    if submit {
                        crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                        if !self.folder_rename_to.trim().is_empty() && self.apply_folder_rename() {
                            close = true;
                        } else {
                            // 名称无效/重名时保留弹窗焦点，别掉到下层终端。
                            self.folder_rename_focus = true;
                        }
                    }
                    if cancel {
                        crate::ui::dialog::consume_handled_key(ui, egui::Key::Escape);
                        close = true;
                    }
                },
            );
            if title_close || close {
                open = false;
            }
            if !open {
                self.folder_rename_from = None;
            }
        }
        if let Some(folder) = self.folder_delete_pending.clone() {
            let mut open = true;
            let mut close = false;
            let mut confirm = false;
            let title_close = crate::ui::dialog::compact_dialog_window(
                ctx,
                "删除文件夹",
                Some(egui::Id::new("conn_library_folder_delete")),
                &mut open,
                |ui| {
                    crate::ui::dialog::polish(ui);
                    ui.label(format!("确定删除“{}”吗？", folder_name(&folder)));
                    ui.label(RichText::new("此操作无法撤销。").weak());
                    ui.add_space(6.0);
                    let mut cancel = false;
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 30.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let action_button_size = egui::vec2(56.0, 28.0);
                            confirm |=
                                crate::ui::dialog::compact_destructive_button(ui, "删除").clicked();
                            cancel |= ui
                                .add_sized(action_button_size, egui::Button::new("取消"))
                                .clicked();
                        },
                    );
                    let armed = std::mem::replace(&mut self.folder_delete_armed, true);
                    if armed && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        confirm = true;
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        confirm = false;
                        cancel = true;
                    }
                    if confirm {
                        crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                    }
                    if cancel {
                        crate::ui::dialog::consume_handled_key(ui, egui::Key::Escape);
                        close = true;
                    }
                },
            );
            if confirm {
                self.delete_folder(&folder);
                close = true;
            }
            if title_close || close || !open {
                self.folder_delete_pending = None;
            }
        }
    }

    // =====================================================================
    // 标签快编
    // =====================================================================
    fn open_tag_edit(&mut self, name: &str) {
        let tags = self
            .config
            .connections
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.tags.join(", "))
            .unwrap_or_default();
        self.tag_edit_name = Some(name.to_string());
        self.tag_edit_focus = true;
        self.tag_edit_value = tags;
    }

    fn save_tag_edit(&mut self) {
        let Some(name) = self.tag_edit_name.clone() else {
            return;
        };
        if let Some(c) = self.config.connections.iter_mut().find(|c| c.name == name) {
            c.tags = parse_tags(&self.tag_edit_value);
            let _ = crate::config::save(&self.config);
        }
        self.tag_edit_name = None;
    }

    fn show_tag_dialog(&mut self, ctx: &egui::Context) {
        if self.tag_edit_name.is_none() {
            return;
        }
        let mut open = true;
        egui::Window::new("编辑标签")
            .id(egui::Id::new("conn_library_tag_edit"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(ctx))
            .show(ctx, |ui| {
                crate::ui::dialog::polish(ui);
                ui.label("逗号分隔");
                let value_id = egui::Id::new("conn_library_tag_edit_value");
                let mut submit = false;
                let mut cancel = false;
                let resp =
                    ui.add(egui::TextEdit::singleline(&mut self.tag_edit_value).id(value_id));
                if std::mem::take(&mut self.tag_edit_focus) {
                    crate::ui::dialog::focus_text_input(
                        &resp,
                        ui,
                        value_id,
                        self.tag_edit_value.chars().count(),
                    );
                }
                submit |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.horizontal(|ui| {
                    submit |= crate::ui::dialog::primary_button(ui, "保存").clicked();
                    cancel |= crate::ui::dialog::secondary_button(ui, "取消").clicked();
                });
                cancel |= ui.input(|i| i.key_pressed(egui::Key::Escape));
                if submit {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                    self.save_tag_edit();
                }
                if cancel {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Escape);
                    self.tag_edit_name = None;
                }
            });
        if !open {
            self.tag_edit_name = None;
        }
    }

    // =====================================================================
    // 移动到文件夹
    // =====================================================================
    fn show_move_dialog(&mut self, ctx: &egui::Context) {
        if self.move_folder_name.is_none() {
            return;
        }
        let mut open = true;
        egui::Window::new("移动到文件夹")
            .id(egui::Id::new("conn_library_move"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(ctx))
            .show(ctx, |ui| {
                crate::ui::dialog::polish(ui);
                let mut pick: Option<String> = None;
                if ui.selectable_label(false, "（根目录）").clicked() {
                    pick = Some(String::new());
                }
                let mut folders = self.config.folders.clone();
                folders.sort();
                for f in folders {
                    if ui.selectable_label(false, &f).clicked() {
                        pick = Some(f);
                    }
                }
                if let Some(g) = pick {
                    if let Some(name) = self.move_folder_name.clone()
                        && let Some(c) = self.config.connections.iter_mut().find(|c| c.name == name)
                    {
                        c.group = g;
                        let _ = crate::config::save(&self.config);
                    }
                    self.move_folder_name = None;
                }
                if ui.button("取消").clicked() {
                    self.move_folder_name = None;
                }
            });
        if !open {
            self.move_folder_name = None;
        }
    }

    // =====================================================================
    // 密钥管理窗口 + 表单
    // =====================================================================
    fn show_keys_window(&mut self, ctx: &egui::Context) {
        if !self.show_keys {
            return;
        }
        let mut open = self.show_keys;
        egui::Window::new("密钥管理")
            // egui 会按 ID 记忆窗口尺寸；布局版本变化时更新 ID 以应用新的默认尺寸。
            .id(egui::Id::new("conn_library_key_manager_v2"))
            .open(&mut open)
            .collapsible(false)
            .default_size([480.0, 400.0])
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(ctx))
            .show(ctx, |ui| {
                crate::ui::dialog::polish(ui);
                ui.horizontal(|ui| {
                    if ui.button("生成").clicked() {
                        self.open_key_form(KeyFormMode::Generate);
                    }
                    if ui.button("登记").clicked() {
                        self.open_key_form(KeyFormMode::Register);
                    }
                    let has = self.key_sel.is_some();
                    if ui.add_enabled(has, egui::Button::new("编辑")).clicked()
                        && let Some(id) = self.key_sel.clone()
                    {
                        self.open_key_form(KeyFormMode::Edit(id));
                    }
                    if ui.add_enabled(has, egui::Button::new("删除")).clicked()
                        && let Some(id) = self.key_sel.clone()
                    {
                        crate::secrets::delete(&crate::keys::key_account(&id));
                        crate::keys::delete(&id, &mut self.config);
                        let _ = crate::config::save(&self.config);
                        self.key_sel = None;
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.config.keys.is_empty() {
                        ui.label(RichText::new("（无密钥，点 生成 或 登记 添加）").weak());
                    }
                    for k in self.config.keys.clone() {
                        let sel = self.key_sel.as_deref() == Some(&k.id);
                        ui.horizontal(|ui| {
                            let r = ui.selectable_label(sel, &k.name);
                            ui.label(RichText::new(short_fp(&k.fingerprint)).weak().small());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(RichText::new(&k.path).weak().small());
                                },
                            );
                            if r.clicked() {
                                self.key_sel = Some(k.id.clone());
                            }
                        });
                    }
                });
            });
        self.show_keys = open;
    }

    pub(crate) fn open_key_form(&mut self, mode: KeyFormMode) {
        self.key_form_assign_to_lc = false;
        self.key_form_assign_to_edit = false;
        self.key_form_mode = mode.clone();
        self.key_form_name.clear();
        self.key_form_path.clear();
        self.key_form_pass.zeroize();
        self.key_form_algo = crate::keys::KeyAlgo::Ed25519;
        self.key_form_error = None;
        if let KeyFormMode::Edit(id) = &mode
            && let Some(k) = self.config.keys.iter().find(|k| k.id == *id)
        {
            self.key_form_name = k.name.clone();
            self.key_form_path = k.path.clone();
        }
        self.key_form_focus = true;
        self.key_form_open = true;
    }

    fn show_key_form(&mut self, ctx: &egui::Context) {
        if !self.key_form_open {
            return;
        }
        let title = match &self.key_form_mode {
            KeyFormMode::Generate => "生成密钥",
            KeyFormMode::Register => "登记密钥",
            KeyFormMode::Edit(_) => "编辑密钥",
        };
        let mut open = self.key_form_open;
        let mut close = false;
        egui::Window::new(title)
            .id(egui::Id::new("conn_library_key_form"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(ctx))
            .show(ctx, |ui| {
                crate::ui::dialog::polish(ui);
                ui.set_width(360.0);
                egui::Grid::new("key_form_grid")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("名称");
                        let name_id = egui::Id::new("conn_library_key_form_name");
                        let name_resp =
                            ui.add(egui::TextEdit::singleline(&mut self.key_form_name).id(name_id));
                        if std::mem::take(&mut self.key_form_focus) {
                            crate::ui::dialog::focus_text_input(
                                &name_resp,
                                ui,
                                name_id,
                                self.key_form_name.chars().count(),
                            );
                        }
                        ui.end_row();
                        if matches!(self.key_form_mode, KeyFormMode::Generate) {
                            ui.label("算法");
                            egui::ComboBox::from_id_salt("key_algo")
                                .selected_text(self.key_form_algo.label())
                                .show_ui(ui, |ui| {
                                    for a in
                                        [crate::keys::KeyAlgo::Ed25519, crate::keys::KeyAlgo::Rsa]
                                    {
                                        if ui
                                            .selectable_label(self.key_form_algo == a, a.label())
                                            .clicked()
                                        {
                                            self.key_form_algo = a;
                                        }
                                    }
                                });
                            ui.end_row();
                        }
                        if !matches!(self.key_form_mode, KeyFormMode::Generate) {
                            ui.label("私钥路径");
                            ui.horizontal(|ui| {
                                ui.text_edit_singleline(&mut self.key_form_path);
                                if ui.button("…").clicked()
                                    && let Some(p) = rfd::FileDialog::new().pick_file()
                                {
                                    self.key_form_path = p.to_string_lossy().into_owned();
                                }
                            });
                            ui.end_row();
                        }
                        ui.label("口令");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.key_form_pass)
                                .password(true)
                                .hint_text(match self.key_form_mode {
                                    KeyFormMode::Generate => "可选:加密落盘",
                                    _ => "登记/重校验用",
                                }),
                        );
                        ui.end_row();
                    });
                if let Some(e) = &self.key_form_error {
                    ui.add_space(6.0);
                    ui.colored_label(Color32::from_rgb(0xE8, 0x4C, 0x3D), e.clone());
                }
                ui.add_space(8.0);
                let mut submit = false;
                let mut cancel = false;
                ui.horizontal(|ui| {
                    let ok_label = match self.key_form_mode {
                        KeyFormMode::Generate => "生成",
                        KeyFormMode::Register => "登记",
                        KeyFormMode::Edit(_) => "保存",
                    };
                    submit |= crate::ui::dialog::primary_button(ui, ok_label).clicked();
                    cancel |= crate::ui::dialog::secondary_button(ui, "取消").clicked();
                });
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    submit = false;
                    cancel = true;
                }
                if submit {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                    if self.apply_key_form() {
                        close = true;
                    } else {
                        self.key_form_focus = true;
                    }
                }
                if cancel {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Escape);
                    self.key_form_assign_to_lc = false;
                    self.key_form_assign_to_edit = false;
                    close = true;
                }
            });
        if close {
            open = false;
        }
        self.key_form_open = open;
        if !open {
            self.key_form_assign_to_lc = false;
            self.key_form_assign_to_edit = false;
            self.key_form_pass.zeroize();
        }
    }

    fn apply_key_form(&mut self) -> bool {
        let name = self.key_form_name.trim().to_string();
        if name.is_empty() {
            self.key_form_error = Some("名称不能为空".into());
            return false;
        }
        let pass = zeroize::Zeroizing::new(self.key_form_pass.clone());
        let pass_ref = if pass.is_empty() {
            None
        } else {
            Some(pass.as_str())
        };
        let mode = self.key_form_mode.clone();
        let ok = match &mode {
            KeyFormMode::Generate => {
                match crate::keys::generate(&name, self.key_form_algo, pass_ref) {
                    Ok(k) => {
                        let id = k.id.clone();
                        self.config.keys.push(k);
                        if let Some(p) = pass_ref {
                            let _ = crate::secrets::set(&crate::keys::key_account(&id), p);
                        }
                        let _ = crate::config::save(&self.config);
                        if self.key_form_assign_to_lc {
                            self.lc_key_mode = LcKeyMode::Key(id);
                            self.key_form_assign_to_lc = false;
                        } else if self.key_form_assign_to_edit {
                            self.edit_key_mode = LcKeyMode::Key(id);
                            self.key_form_assign_to_edit = false;
                        }
                        true
                    }
                    Err(e) => {
                        self.key_form_error = Some(format!("{e:#}"));
                        false
                    }
                }
            }
            KeyFormMode::Register => {
                let path = self.key_form_path.trim();
                if path.is_empty() {
                    self.key_form_error = Some("私钥路径不能为空".into());
                    return false;
                }
                match crate::keys::register(&name, path, pass_ref) {
                    Ok(k) => {
                        let id = k.id.clone();
                        self.config.keys.push(k);
                        if let Some(p) = pass_ref {
                            let _ = crate::secrets::set(&crate::keys::key_account(&id), p);
                        }
                        let _ = crate::config::save(&self.config);
                        if self.key_form_assign_to_lc {
                            self.lc_key_mode = LcKeyMode::Key(id);
                            self.key_form_assign_to_lc = false;
                        } else if self.key_form_assign_to_edit {
                            self.edit_key_mode = LcKeyMode::Key(id);
                            self.key_form_assign_to_edit = false;
                        }
                        true
                    }
                    Err(e) => {
                        self.key_form_error = Some(format!("登记失败: {e:#}"));
                        false
                    }
                }
            }
            KeyFormMode::Edit(id) => {
                let id = id.clone();
                if let Err(e) = crate::keys::rename(&id, &name, &mut self.config) {
                    self.key_form_error = Some(format!("{e:#}"));
                    return false;
                }
                let new_path = self.key_form_path.trim().to_string();
                let cur_path = self
                    .config
                    .keys
                    .iter()
                    .find(|k| k.id == id)
                    .map(|k| k.path.clone())
                    .unwrap_or_default();
                if new_path != cur_path
                    && let Err(e) = crate::keys::repoint(&id, &new_path, pass_ref, &mut self.config)
                {
                    self.key_form_error = Some(format!("改路径失败: {e:#}"));
                    return false;
                }
                if let Some(p) = pass_ref {
                    let _ = crate::secrets::set(&crate::keys::key_account(&id), p);
                }
                let _ = crate::config::save(&self.config);
                true
            }
        };
        if ok {
            self.key_form_error = None;
        }
        ok
    }

    // =====================================================================
    // 汇总:所有连接库子弹窗
    // =====================================================================
    fn show_lib_extras(&mut self, ctx: &egui::Context) {
        self.show_lib_edit_window(ctx);
        self.show_folder_dialogs(ctx);
        self.show_tag_dialog(ctx);
        self.show_move_dialog(ctx);
        self.show_connection_export_dialog(ctx);
        self.show_connection_import_dialog(ctx);
        self.show_keys_window(ctx);
        self.show_key_form(ctx);
    }
}

fn folder_name(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

fn folder_depth(path: &str) -> usize {
    path.split('/')
        .filter(|part| !part.is_empty())
        .count()
        .saturating_sub(1)
}

fn folder_parent(path: &str) -> &str {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .unwrap_or("")
}

/// 补齐可能缺失的祖先路径，保证旧配置也能组成完整树。
fn folder_tree_paths(paths: &[String]) -> Vec<String> {
    let mut tree = std::collections::BTreeSet::new();
    for path in paths {
        let mut current = String::new();
        for part in path.split('/').filter(|part| !part.is_empty()) {
            if !current.is_empty() {
                current.push('/');
            }
            current.push_str(part);
            tree.insert(current.clone());
        }
    }
    tree.into_iter().collect()
}

fn short_fp(fp: &str) -> String {
    if fp.len() > 24 {
        format!("{}…", &fp[..24])
    } else {
        fp.to_string()
    }
}

#[derive(Clone, Copy)]
enum LibConnAction {
    Select,
    Connect,
    Edit,
    Delete,
    Move,
    Tags,
    Export,
}

pub(crate) fn connection_group_combo(
    ui: &mut egui::Ui,
    id: &'static str,
    group: &mut String,
    configured_folders: &[String],
) {
    let selected = if group.is_empty() {
        "（根目录）".to_string()
    } else {
        group.clone()
    };
    let mut folders = configured_folders.to_vec();
    folders.sort();
    folders.dedup();
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .width(210.0)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(group.is_empty(), "（根目录）")
                .clicked()
            {
                group.clear();
            }
            for folder in folders {
                let selected = *group == folder;
                let label = format!(
                    "{}{}",
                    "　".repeat(folder_depth(&folder)),
                    folder_name(&folder)
                );
                if ui.selectable_label(selected, label).clicked() {
                    *group = folder;
                }
            }
        });
}

pub(crate) fn connection_key_combo(
    ui: &mut egui::Ui,
    id: &'static str,
    key_mode: &mut LcKeyMode,
    keys: &[crate::config::SavedKey],
) {
    let selected_text = match &*key_mode {
        LcKeyMode::Password => "密码".to_string(),
        LcKeyMode::Path => "自定义私钥路径".to_string(),
        LcKeyMode::Key(id) => keys
            .iter()
            .find(|key| key.id == *id)
            .map(|key| format!("密钥: {}", key.name))
            .unwrap_or_else(|| "密钥: ?".into()),
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected_text)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(matches!(key_mode, LcKeyMode::Password), "密码")
                .clicked()
            {
                *key_mode = LcKeyMode::Password;
            }
            ui.separator();
            if keys.is_empty() {
                ui.add_enabled(false, egui::Button::new("（暂无管理的密钥）"));
            }
            for key in keys {
                let selected = *key_mode == LcKeyMode::Key(key.id.clone());
                if ui
                    .selectable_label(selected, format!("密钥: {}", key.name))
                    .clicked()
                {
                    *key_mode = LcKeyMode::Key(key.id.clone());
                }
            }
            ui.separator();
            if ui
                .selectable_label(matches!(key_mode, LcKeyMode::Path), "自定义私钥路径")
                .clicked()
            {
                *key_mode = LcKeyMode::Path;
            }
        });
}

pub(crate) fn parse_tags(s: &str) -> Vec<String> {
    s.split([',', '，', ';', '；'])
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn tag_chip(ui: &mut egui::Ui, t: &str) {
    let font = egui::FontId::proportional(11.0);
    let w = ui.fonts_mut(|f| {
        f.layout_no_wrap(t.to_string(), font.clone(), Color32::WHITE)
            .size()
            .x
    });
    let (r, _) = ui.allocate_exact_size(vec2(w + 10.0, 16.0), egui::Sense::hover());
    ui.painter().rect_filled(
        r,
        8.0,
        Color32::from_rgba_unmultiplied(0x3C, 0x82, 0xE8, 0x55),
    );
    ui.painter().text(
        r.center(),
        egui::Align2::CENTER_CENTER,
        t,
        font,
        Color32::from_rgb(0xCF, 0xE2, 0xFF),
    );
}

fn key_label(c: &crate::config::SavedConnection, cfg: &crate::config::AppConfig) -> String {
    if let Some(kid) = &c.key_id {
        cfg.keys
            .iter()
            .find(|k| k.id == *kid)
            .map(|k| format!("[钥] {}", k.name))
            .unwrap_or_else(|| "[钥] ?".into())
    } else if !c.key_path.is_empty() {
        "[钥] 文件".into()
    } else {
        "密码".into()
    }
}

fn connection_list_identity(c: &crate::config::SavedConnection) -> (&str, &str) {
    (c.display_name(), c.host.as_str())
}

#[cfg(test)]
mod tests {
    use super::{connection_list_identity, folder_name, folder_parent, folder_tree_paths};

    #[test]
    fn folder_tree_parts_reflect_path_hierarchy() {
        assert_eq!(folder_parent("生产"), "");
        assert_eq!(folder_parent("生产/华东"), "生产");
        assert_eq!(folder_parent("生产/华东/Web"), "生产/华东");
        assert_eq!(folder_name("生产/华东/Web"), "Web");
    }

    #[test]
    fn folder_tree_adds_missing_ancestors() {
        let paths = vec!["生产/华东/Web".to_string(), "测试".to_string()];
        assert_eq!(
            folder_tree_paths(&paths),
            vec![
                "测试".to_string(),
                "生产".to_string(),
                "生产/华东".to_string(),
                "生产/华东/Web".to_string(),
            ]
        );
    }

    #[test]
    fn connection_list_shows_alias_and_host_separately() {
        let connection = crate::config::SavedConnection {
            name: "root@10.0.0.8:22".into(),
            host: "10.0.0.8".into(),
            alias: "生产服务器".into(),
            ..Default::default()
        };

        assert_eq!(
            connection_list_identity(&connection),
            ("生产服务器", "10.0.0.8")
        );
    }
}
