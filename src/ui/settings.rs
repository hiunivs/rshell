// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

use crate::app::RShellApp;
use crate::config::{
    ColorScheme, DEFAULT_TERMINAL_BACKGROUND_COLOR, DownloadPathMode, FileOpenMode,
    PasswordStorage, TERMINAL_BACKGROUND_PRESETS, TerminalBackgroundKind, TerminalImageMode,
    ThemeMode, UpdateChannel,
};
use egui::{Color32, CornerRadius, Frame, Margin, RichText, Stroke, ThemePreference, Ui, vec2};

const RSHELL_COPYRIGHT: &str = "Copyright (C) 2026 hiunivs";
const RSHELL_SOURCE_URL: &str = "https://github.com/hiunivs/rshell";
const RSHELL_LICENSE_URL: &str = "https://github.com/hiunivs/rshell/blob/master/LICENSE";

/// 设置窗口左侧页面。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    Security,
    General,
    Transfers,
    Editor,
    Shortcuts,
    Updates,
    Help,
    About,
}

impl SettingsPage {
    fn title(self) -> &'static str {
        match self {
            Self::General => "常规",
            Self::Transfers => "传输设置",
            Self::Editor => "编辑器",
            Self::Security => "安全",
            Self::Shortcuts => "快捷设置",
            Self::Updates => "更新",
            Self::Help => "帮助",
            Self::About => "关于",
        }
    }
}

fn settings_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("rshell_settings")
}

impl RShellApp {
    pub(crate) fn open_settings_viewport(&mut self) {
        self.show_settings = true;
        self.focus_settings_viewport = true;
        if self.settings_sub.is_none() {
            self.settings_sub = Some(SettingsPage::General);
        }
    }

    pub(crate) fn show_settings_dialog(&mut self, ui: &mut Ui) {
        if !self.show_settings {
            self.focus_settings_viewport = false;
            return;
        }

        let ctx = ui.ctx().clone();
        let focus_viewport = std::mem::take(&mut self.focus_settings_viewport);
        let mut config_changed = false;
        if self.settings_sub.is_none() {
            self.settings_sub = Some(SettingsPage::General);
        }

        ctx.show_viewport_immediate(
            settings_viewport_id(),
            egui::ViewportBuilder::default()
                .with_title("RShell 设置")
                .with_inner_size([650.0, 420.0])
                .with_resizable(false),
            |viewport_ui, viewport_class| {
                let viewport_ctx = viewport_ui.ctx().clone();
                if viewport_ctx.input(|input| input.viewport().close_requested()) {
                    self.show_settings = false;
                    return;
                }
                if focus_viewport && !matches!(viewport_class, egui::ViewportClass::EmbeddedWindow)
                {
                    apply_native_window_theme(&viewport_ctx, self.config.theme_mode);
                    viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    viewport_ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }

                let content_frame = Frame::new()
                    .fill(viewport_ui.visuals().window_fill())
                    .inner_margin(Margin::symmetric(14, 12));
                egui::CentralPanel::default()
                    .frame(content_frame)
                    .show(viewport_ui, |ui| {
                        crate::ui::dialog::polish_page_compact(ui);

                        let height = ui.available_height();
                        let body_rect = egui::Rect::from_min_size(
                            ui.next_widget_position(),
                            vec2(ui.available_width(), height),
                        );
                        let _ = ui.allocate_rect(body_rect, egui::Sense::hover());
                        let left_width = 150.0;
                        let column_gap = 10.0;
                        let left_rect =
                            egui::Rect::from_min_size(body_rect.min, vec2(left_width, height));
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
                        let right_content = right_rect.shrink(14.0);
                        ui.scope_builder(egui::UiBuilder::new().max_rect(body_rect), |ui| {
                            ui.scope_builder(
                                egui::UiBuilder::new()
                                    .max_rect(left_content)
                                    .layout(egui::Layout::top_down(egui::Align::LEFT)),
                                |ui| {
                                    ui.label(RichText::new("设置").strong().size(13.0));
                                    ui.add_space(5.0);
                                    for page in [
                                        SettingsPage::General,
                                        SettingsPage::Transfers,
                                        SettingsPage::Security,
                                        SettingsPage::Editor,
                                        SettingsPage::Shortcuts,
                                        SettingsPage::Updates,
                                        SettingsPage::Help,
                                        SettingsPage::About,
                                    ] {
                                        if settings_nav_button(
                                            ui,
                                            page.title(),
                                            self.settings_sub == Some(page),
                                        )
                                        .clicked()
                                        {
                                            self.settings_sub = Some(page);
                                        }
                                    }
                                },
                            );
                            ui.scope_builder(
                                egui::UiBuilder::new()
                                    .max_rect(right_content)
                                    .layout(egui::Layout::top_down(egui::Align::LEFT)),
                                |ui| {
                                    egui::ScrollArea::vertical()
                                        .id_salt("settings_page_scroll")
                                        .auto_shrink([false, false])
                                        .show(ui, |ui| {
                                            let page =
                                                self.settings_sub.unwrap_or(SettingsPage::General);

                                            match page {
                                                SettingsPage::General => {
                                                    self.show_general_settings_page(
                                                        ui,
                                                        &viewport_ctx,
                                                        &mut config_changed,
                                                    );
                                                }
                                                SettingsPage::Transfers => self
                                                    .show_transfer_settings_page(
                                                        ui,
                                                        &mut config_changed,
                                                    ),
                                                SettingsPage::Security => {
                                                    self.show_security_settings_page(ui);
                                                }
                                                SettingsPage::Editor => self
                                                    .show_editor_settings_page(
                                                        ui,
                                                        &mut config_changed,
                                                    ),
                                                SettingsPage::Shortcuts => self
                                                    .show_shortcut_settings_page(
                                                        ui,
                                                        &mut config_changed,
                                                    ),
                                                SettingsPage::Updates => self
                                                    .show_update_settings_page(
                                                        ui,
                                                        &mut config_changed,
                                                    ),
                                                SettingsPage::Help => show_help_settings_page(ui),
                                                SettingsPage::About => show_about_settings_page(ui),
                                            }
                                        });
                                },
                            );
                        });
                    });
            },
        );

        if config_changed {
            self.settings_error = crate::config::save(&self.config)
                .err()
                .map(|error| format!("保存设置失败：{error:#}"));
        }
    }

    fn show_general_settings_page(
        &mut self,
        ui: &mut Ui,
        ctx: &egui::Context,
        config_changed: &mut bool,
    ) {
        settings_section_frame(ui).show(ui, |ui| {
            ui.set_width(396.0);
            ui.label(RichText::new("外观").strong().size(14.0));
            ui.add_space(9.0);

            egui::Grid::new("general_settings_grid_v2")
                .num_columns(2)
                .spacing([32.0, 14.0])
                .show(ui, |ui| {
                    settings_row_label(ui, "主题颜色");
                    let mut theme = self.config.theme_mode;
                    egui::ComboBox::from_id_salt("theme_color_v2")
                        .selected_text(theme.label())
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for option in [ThemeMode::System, ThemeMode::Dark, ThemeMode::Light] {
                                ui.selectable_value(&mut theme, option, option.label());
                            }
                        });
                    if theme != self.config.theme_mode {
                        self.config.theme_mode = theme;
                        self.theme_pref = theme_preference(theme);
                        ctx.set_theme(self.theme_pref);
                        apply_native_window_theme(ctx, theme);
                        apply_color_scheme(ctx, self.config.color_scheme);
                        *config_changed = true;
                    }
                    ui.end_row();

                    settings_row_label(ui, "配色");
                    let mut scheme = self.config.color_scheme;
                    egui::ComboBox::from_id_salt("color_scheme_v2")
                        .selected_text(scheme.label())
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for option in [
                                ColorScheme::Blue,
                                ColorScheme::Green,
                                ColorScheme::Purple,
                                ColorScheme::Orange,
                            ] {
                                ui.selectable_value(&mut scheme, option, option.label());
                            }
                        });
                    if scheme != self.config.color_scheme {
                        self.config.color_scheme = scheme;
                        apply_color_scheme(ctx, scheme);
                        *config_changed = true;
                    }
                    ui.end_row();
                });
        });

        ui.add_space(10.0);
        settings_section_frame(ui).show(ui, |ui| {
            ui.set_width(396.0);
            ui.label(RichText::new("SSH终端").strong().size(14.0));
            ui.add_space(9.0);

            egui::Grid::new("ssh_terminal_settings")
                .num_columns(2)
                .spacing([32.0, 14.0])
                .show(ui, |ui| {
                    settings_row_label(ui, "字体");
                    let mut font_family = self.config.ssh_terminal.font_family.clone();
                    let combo = egui::ComboBox::from_id_salt("ssh_terminal_font_family")
                        .selected_text(&font_family)
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for family in crate::terminal_font_families() {
                                ui.selectable_value(&mut font_family, family.clone(), family);
                            }
                        });
                    combo.response.on_hover_text("重新启动后生效");
                    if font_family != self.config.ssh_terminal.font_family {
                        self.config.ssh_terminal.font_family = font_family;
                        *config_changed = true;
                    }
                    ui.end_row();

                    settings_row_label(ui, "字体大小");
                    let mut font_size = self.config.ssh_terminal.font_size;
                    egui::ComboBox::from_id_salt("ssh_terminal_font_size")
                        .selected_text(format!("{font_size}"))
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for size in 10..=18 {
                                ui.selectable_value(&mut font_size, size, size.to_string());
                            }
                        });
                    if font_size != self.config.ssh_terminal.font_size {
                        self.config.ssh_terminal.font_size = font_size;
                        *config_changed = true;
                    }
                    ui.end_row();

                    settings_row_label(ui, "主题配色");
                    ui.add_enabled_ui(false, |ui| {
                        egui::ComboBox::from_id_salt("ssh_terminal_theme_placeholder")
                            .selected_text("默认")
                            .width(190.0)
                            .show_ui(ui, |_ui| {});
                    });
                    ui.end_row();
                });
        });

        ui.add_space(10.0);
        settings_section_frame(ui).show(ui, |ui| {
            ui.set_width(396.0);
            ui.label(RichText::new("终端背景").strong().size(14.0));
            ui.add_space(9.0);

            egui::Grid::new("terminal_background_settings")
                .num_columns(2)
                .spacing([32.0, 12.0])
                .show(ui, |ui| {
                    settings_row_label(ui, "背景类型");
                    let mut kind = self.config.terminal_background.kind;
                    egui::ComboBox::from_id_salt("terminal_background_kind")
                        .selected_text(kind.label())
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for option in
                                [TerminalBackgroundKind::Solid, TerminalBackgroundKind::Image]
                            {
                                ui.selectable_value(&mut kind, option, option.label());
                            }
                        });
                    if kind != self.config.terminal_background.kind {
                        self.config.terminal_background.kind = kind;
                        *config_changed = true;
                    }
                    ui.end_row();

                    if kind == TerminalBackgroundKind::Solid {
                        settings_row_label(ui, "背景颜色");
                        ui.allocate_ui_with_layout(
                            vec2(190.0, 28.0),
                            egui::Layout::left_to_right(egui::Align::Center),
                            |ui| {
                                ui.spacing_mut().item_spacing.x = 5.0;
                                // 项目主题会为 hovered/active 状态设置 expansion。
                                // 色块紧邻区域底边时，扩展绘制看起来像外框上下跳动；
                                // 此处保持各交互状态尺寸完全一致。
                                let widgets = &mut ui.visuals_mut().widgets;
                                widgets.inactive.expansion = 0.0;
                                widgets.hovered.expansion = 0.0;
                                widgets.active.expansion = 0.0;
                                widgets.open.expansion = 0.0;
                                for (index, preset) in
                                    TERMINAL_BACKGROUND_PRESETS.iter().copied().enumerate()
                                {
                                    let selected = self.config.terminal_background.color == preset;
                                    let (rect, response) = ui.allocate_exact_size(
                                        vec2(28.0, 26.0),
                                        egui::Sense::click(),
                                    );
                                    let response = response
                                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                                        .on_hover_text(format!("预设颜色 {}", index + 1));
                                    // 色块与选中框都画在固定响应区域内部，不使用
                                    // Button 的交互态边框，避免 hover 前后尺寸变化。
                                    let color_rect = egui::Rect::from_center_size(
                                        rect.center(),
                                        vec2(20.0, 18.0),
                                    );
                                    ui.painter().rect(
                                        color_rect,
                                        3.0,
                                        Color32::from_rgb(preset[0], preset[1], preset[2]),
                                        ui.visuals().widgets.noninteractive.bg_stroke,
                                        egui::StrokeKind::Inside,
                                    );
                                    if selected {
                                        ui.painter().rect_stroke(
                                            color_rect.expand(3.0),
                                            5.0,
                                            Stroke::new(2.0, ui.visuals().selection.bg_fill),
                                            egui::StrokeKind::Inside,
                                        );
                                    }
                                    if response.clicked() && !selected {
                                        self.config.terminal_background.color = preset;
                                        *config_changed = true;
                                    }
                                }

                                if ui
                                    .color_edit_button_srgb(
                                        &mut self.config.terminal_background.color,
                                    )
                                    .on_hover_text("自定义颜色")
                                    .changed()
                                {
                                    *config_changed = true;
                                }

                                if ui.small_button("恢复默认").clicked()
                                    && self.config.terminal_background.color
                                        != DEFAULT_TERMINAL_BACKGROUND_COLOR
                                {
                                    self.config.terminal_background.color =
                                        DEFAULT_TERMINAL_BACKGROUND_COLOR;
                                    *config_changed = true;
                                }
                            },
                        );
                        ui.end_row();
                    } else {
                        settings_row_label(ui, "背景图片");
                        ui.horizontal(|ui| {
                            if ui.button("选择图片").clicked()
                                && let Some(path) = rfd::FileDialog::new()
                                    .add_filter("图片", &["png", "jpg", "jpeg"])
                                    .pick_file()
                            {
                                self.config.terminal_background.image_path =
                                    path.to_string_lossy().into_owned();
                                self.terminal_background_texture = None;
                                self.terminal_background_load_attempt = None;
                                self.terminal_background_error = None;
                                *config_changed = true;
                            }
                            let has_image =
                                !self.config.terminal_background.image_path.trim().is_empty();
                            if ui
                                .add_enabled(has_image, egui::Button::new("清除"))
                                .clicked()
                            {
                                self.config.terminal_background.image_path.clear();
                                self.terminal_background_texture = None;
                                self.terminal_background_load_attempt = None;
                                self.terminal_background_error = None;
                                *config_changed = true;
                            }
                        });
                        ui.end_row();

                        settings_row_label(ui, "显示方式");
                        let mut mode = self.config.terminal_background.image_mode;
                        egui::ComboBox::from_id_salt("terminal_background_image_mode")
                            .selected_text(mode.label())
                            .width(190.0)
                            .show_ui(ui, |ui| {
                                for option in [
                                    TerminalImageMode::Fill,
                                    TerminalImageMode::Fit,
                                    TerminalImageMode::Stretch,
                                    TerminalImageMode::Center,
                                    TerminalImageMode::Tile,
                                ] {
                                    ui.selectable_value(&mut mode, option, option.label());
                                }
                            });
                        if mode != self.config.terminal_background.image_mode {
                            self.config.terminal_background.image_mode = mode;
                            *config_changed = true;
                        }
                        ui.end_row();

                        settings_row_label(ui, "图片暗度");
                        if ui
                            .add(
                                egui::Slider::new(
                                    &mut self.config.terminal_background.image_darkness,
                                    0..=100,
                                )
                                .suffix("%")
                                .show_value(true),
                            )
                            .changed()
                        {
                            *config_changed = true;
                        }
                        ui.end_row();
                    }
                });

            if self.config.terminal_background.kind == TerminalBackgroundKind::Image {
                let path = self.config.terminal_background.image_path.trim();
                if !path.is_empty() {
                    ui.add_space(7.0);
                    let filename = std::path::Path::new(path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(path);
                    ui.label(RichText::new(filename).weak()).on_hover_text(path);
                }
                if let Some(error) = &self.terminal_background_error {
                    ui.add_space(5.0);
                    ui.colored_label(Color32::from_rgb(0xE8, 0x4C, 0x3D), error);
                }
            }
        });

        self.show_settings_error(ui);
    }

    fn show_transfer_settings_page(&mut self, ui: &mut Ui, config_changed: &mut bool) {
        settings_section_frame(ui).show(ui, |ui| {
            ui.set_width(396.0);
            ui.label(RichText::new("下载").strong().size(14.0));
            ui.add_space(9.0);

            egui::Grid::new("transfer_settings_grid")
                .num_columns(2)
                .spacing([32.0, 14.0])
                .show(ui, |ui| {
                    settings_row_label(ui, "下载路径");
                    let mut mode = self.config.transfers.download_mode;
                    egui::ComboBox::from_id_salt("download_path_mode")
                        .selected_text(mode.label())
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for option in [
                                DownloadPathMode::AskEachTime,
                                DownloadPathMode::FixedDirectory,
                            ] {
                                ui.selectable_value(&mut mode, option, option.label());
                            }
                        });
                    if mode != self.config.transfers.download_mode {
                        self.config.transfers.download_mode = mode;
                        *config_changed = true;
                    }
                    ui.end_row();

                    if mode == DownloadPathMode::FixedDirectory {
                        settings_row_label(ui, "保存目录");
                        if ui.button("选择目录…").clicked()
                            && let Some(path) = rfd::FileDialog::new().pick_folder()
                        {
                            self.config.transfers.download_dir =
                                path.to_string_lossy().into_owned();
                            *config_changed = true;
                        }
                        ui.end_row();
                    }
                });

            if self.config.transfers.download_mode == DownloadPathMode::FixedDirectory {
                ui.add_space(7.0);
                let dir = self.config.transfers.download_dir.trim();
                if dir.is_empty() {
                    ui.label(
                        RichText::new("尚未选择目录，下载时将回退为每次询问")
                            .color(Color32::from_rgb(0xD9, 0x78, 0x32))
                            .small(),
                    );
                } else {
                    ui.label(RichText::new(dir).weak()).on_hover_text(dir);
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("同名文件将被直接覆盖；目录已删除时回退为每次询问")
                            .weak()
                            .small(),
                    );
                }
            }
        });

        ui.add_space(10.0);
        settings_section_frame(ui).show(ui, |ui| {
            ui.set_width(396.0);
            ui.label(RichText::new("文件打开方式").strong().size(14.0));
            ui.add_space(9.0);

            egui::Grid::new("file_open_settings_grid")
                .num_columns(2)
                .spacing([32.0, 14.0])
                .show(ui, |ui| {
                    settings_row_label(ui, "打开程序");
                    let mut mode = self.config.transfers.open_mode;
                    egui::ComboBox::from_id_salt("file_open_mode")
                        .selected_text(mode.label())
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for option in [
                                FileOpenMode::SystemDefault,
                                FileOpenMode::CustomProgram,
                            ] {
                                ui.selectable_value(&mut mode, option, option.label());
                            }
                        });
                    if mode != self.config.transfers.open_mode {
                        self.config.transfers.open_mode = mode;
                        *config_changed = true;
                    }
                    ui.end_row();

                    if mode == FileOpenMode::CustomProgram {
                        settings_row_label(ui, "程序路径");
                        if ui.button("选择程序…").clicked()
                            && let Some(path) = rfd::FileDialog::new().pick_file()
                        {
                            self.config.transfers.open_program =
                                path.to_string_lossy().into_owned();
                            *config_changed = true;
                        }
                        ui.end_row();
                    }
                });

            ui.add_space(7.0);
            if self.config.transfers.open_mode == FileOpenMode::CustomProgram {
                let program = self.config.transfers.open_program.trim();
                if program.is_empty() {
                    ui.label(
                        RichText::new("尚未选择程序，打开时将回退为系统默认")
                            .color(Color32::from_rgb(0xD9, 0x78, 0x32))
                            .small(),
                    );
                } else {
                    ui.label(RichText::new(program).weak()).on_hover_text(program);
                }
            }
            ui.label(
                RichText::new("用于文件管理器的\"用本地程序打开\"；远程文件会先下载到临时目录，保存后自动传回")
                    .color(ui.visuals().weak_text_color())
                    .size(11.0),
            );
        });

        self.show_settings_error(ui);
    }

    fn show_security_settings_page(&mut self, ui: &mut Ui) {
        settings_section_frame(ui).show(ui, |ui| {
            ui.set_width(396.0);
            ui.label(RichText::new("凭据").strong().size(14.0));
            ui.add_space(9.0);

            egui::Grid::new("security_settings_grid_v2")
                .num_columns(2)
                .spacing([24.0, 12.0])
                .show(ui, |ui| {
                    settings_row_label(ui, "密码保存方式");
                    let current = self.config.password_storage;
                    let mut selected = current;
                    egui::ComboBox::from_id_salt("password_storage_v2")
                        .selected_text(selected.label())
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for storage in [
                                PasswordStorage::SystemKeyring,
                                PasswordStorage::AppEncrypted,
                            ] {
                                ui.selectable_value(&mut selected, storage, storage.label());
                            }
                        });
                    if selected != current {
                        self.change_password_storage(current, selected);
                    }
                    ui.end_row();
                });
            ui.add_space(7.0);
            match self.config.password_storage {
                PasswordStorage::SystemKeyring => {
                    ui.label(
                        RichText::new("密码由 macOS 钥匙串或 Windows 凭据管理器保护")
                            .weak()
                            .small(),
                    );
                }
                PasswordStorage::AppEncrypted => {
                    ui.label(
                        RichText::new(
                            "兼容模式：密钥与密文均保存在 App 配置目录，整个目录泄露时无法保护密码",
                        )
                        .color(Color32::from_rgb(0xD9, 0x78, 0x32))
                        .small(),
                    );
                }
            }
        });

        ui.add_space(12.0);
        settings_section_frame(ui).show(ui, |ui| {
            ui.set_width(396.0);
            ui.label(RichText::new("主机验证").strong().size(14.0));
            ui.add_space(8.0);
            ui.label("SSH 主机指纹记录");
            ui.label(RichText::new("known_hosts.toml").weak().small());
        });

        self.show_settings_error(ui);
    }

    fn show_shortcut_settings_page(&mut self, ui: &mut Ui, config_changed: &mut bool) {
        settings_section_frame(ui).show(ui, |ui| {
            ui.set_width(396.0);
            egui::Grid::new("shortcut_settings_grid")
                .num_columns(2)
                .spacing([32.0, 14.0])
                .show(ui, |ui| {
                    settings_row_label(ui, "右键粘贴");
                    let mut changed = false;
                    ui.allocate_ui_with_layout(
                        vec2(260.0, 24.0),
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            changed =
                                settings_toggle(ui, &mut self.config.shortcuts.right_click_paste)
                                    .changed();
                        },
                    );
                    if changed {
                        *config_changed = true;
                    }
                    ui.end_row();
                });
        });

        self.show_settings_error(ui);
    }

    fn show_update_settings_page(&mut self, ui: &mut Ui, config_changed: &mut bool) {
        settings_section_frame(ui).show(ui, |ui| {
            ui.set_width(396.0);
            ui.label(RichText::new("版本与更新").strong().size(14.0));
            ui.add_space(9.0);

            egui::Grid::new("software_update_settings_grid")
                .num_columns(2)
                .spacing([32.0, 14.0])
                .show(ui, |ui| {
                    settings_row_label(ui, "当前版本");
                    ui.label(env!("CARGO_PKG_VERSION"));
                    ui.end_row();

                    settings_row_label(ui, "更新方式");
                    ui.label(self.updater.platform_label());
                    ui.end_row();

                    settings_row_label(ui, "更新通道");
                    let mut channel = self.config.updates.channel;
                    egui::ComboBox::from_id_salt("software_update_channel")
                        .selected_text(channel.label())
                        .width(190.0)
                        .show_ui(ui, |ui| {
                            for option in [UpdateChannel::Stable, UpdateChannel::Beta] {
                                ui.selectable_value(&mut channel, option, option.label());
                            }
                        });
                    if channel != self.config.updates.channel {
                        self.config.updates.channel = channel;
                        if !self.updater.configure_channel(channel) {
                            self.config.updates.automatic_checks = false;
                        }
                        *config_changed = true;
                    }
                    ui.end_row();

                    if self.updater.manages_automatic_checks() {
                        settings_row_label(ui, "自动检查");
                        let mut automatic = self.config.updates.automatic_checks;
                        let channel_available = self
                            .updater
                            .is_available(self.config.updates.channel);
                        ui.add_enabled_ui(channel_available, |ui| {
                            ui.allocate_ui_with_layout(
                                vec2(190.0, 28.0),
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if settings_toggle(ui, &mut automatic).changed() {
                                        self.config.updates.automatic_checks = automatic;
                                        self.updater.set_automatic_checks(automatic);
                                        *config_changed = true;
                                    }
                                },
                            );
                        });
                        ui.end_row();
                    }
                });

            ui.add_space(12.0);
            let channel = self.config.updates.channel;
            let enabled = self.updater.is_available(channel);
            ui.vertical_centered(|ui| {
                let response = ui.add_enabled(enabled, egui::Button::new("检查更新"));
                if response.clicked()
                    && let Ok(action) = self.updater.check_for_updates(channel)
                    && let crate::update::UpdateAction::OpenUrl(url) = action
                {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                }
            });

            ui.add_space(9.0);
            let status = self.updater.status();
            if !status.is_empty() {
                let status_color = if enabled {
                    ui.visuals().weak_text_color()
                } else {
                    Color32::from_rgb(0xD9, 0x78, 0x32)
                };
                ui.label(RichText::new(status).color(status_color).small());
                ui.add_space(5.0);
            }
            let detail = if cfg!(target_os = "windows") {
                "安装与回滚由 Windows App Installer 处理；自动检查周期由已签名的 .appinstaller 配置。"
            } else if cfg!(target_os = "macos") {
                "下载、签名验证与替换由内嵌 Sparkle 处理，RShell 不会自行覆盖程序文件。"
            } else {
                "当前平台没有配置原生更新后端。"
            };
            ui.label(
                RichText::new(detail)
                    .color(ui.visuals().weak_text_color())
                    .size(11.0),
            );
        });

        self.show_settings_error(ui);
    }

    fn show_settings_error(&self, ui: &mut Ui) {
        if let Some(error) = &self.settings_error {
            ui.add_space(10.0);
            ui.colored_label(Color32::from_rgb(0xE8, 0x4C, 0x3D), error);
        }
    }

    fn show_editor_settings_page(&mut self, ui: &mut Ui, config_changed: &mut bool) {
        settings_section_frame(ui).show(ui, |ui| {
            ui.set_width(396.0);
            egui::Grid::new("editor_settings_grid")
                .num_columns(2)
                .spacing([32.0, 14.0])
                .show(ui, |ui| {
                    settings_row_label(ui, "记住打开的文件");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if settings_toggle(ui, &mut self.config.editor.remember_open_files)
                            .changed()
                        {
                            *config_changed = true;
                        }
                    });
                    ui.end_row();

                    settings_row_label(ui, "自动换行");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if settings_toggle(ui, &mut self.config.editor.auto_wrap).changed() {
                            *config_changed = true;
                        }
                    });
                    ui.end_row();

                    settings_row_label(ui, "深色高亮主题");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if highlight_theme_picker(
                            ui,
                            "editor_hl_theme_dark",
                            &mut self.config.editor.highlight_theme_dark,
                            "base16-ocean.dark",
                        ) {
                            *config_changed = true;
                        }
                    });
                    ui.end_row();

                    settings_row_label(ui, "浅色高亮主题");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if highlight_theme_picker(
                            ui,
                            "editor_hl_theme_light",
                            &mut self.config.editor.highlight_theme_light,
                            "InspiredGitHub",
                        ) {
                            *config_changed = true;
                        }
                    });
                    ui.end_row();
                });
        });

        self.show_settings_error(ui);
    }

    fn change_password_storage(&mut self, from: PasswordStorage, to: PasswordStorage) {
        let mut accounts: Vec<String> = self
            .config
            .connections
            .iter()
            .map(|connection| {
                crate::secrets::account_for(&connection.user, &connection.host, connection.port)
            })
            .chain(
                self.config
                    .keys
                    .iter()
                    .map(|key| crate::keys::key_account(&key.id)),
            )
            .collect();
        accounts.sort();
        accounts.dedup();
        match crate::secrets::switch_backend(from, to, &accounts) {
            Ok(()) => {
                self.config.password_storage = to;
                match crate::config::save(&self.config) {
                    Ok(()) => self.settings_error = None,
                    Err(error) => {
                        self.settings_error = Some(format!("保存设置失败：{error:#}"));
                    }
                }
            }
            Err(error) => {
                self.settings_error = Some(format!("切换密码保存方式失败：{error}"));
            }
        }
    }
}

fn settings_nav_button(ui: &mut Ui, label: &str, selected: bool) -> egui::Response {
    let width = ui.available_width();
    ui.add_sized(
        [width, 34.0],
        egui::Button::new(RichText::new(label).size(14.0))
            .selected(selected)
            .corner_radius(CornerRadius::same(6)),
    )
}

fn settings_section_frame(ui: &Ui) -> Frame {
    let dark = ui.visuals().dark_mode;
    Frame::new()
        .fill(if dark {
            Color32::from_rgb(39, 40, 44)
        } else {
            Color32::from_rgb(249, 249, 250)
        })
        .stroke(Stroke::new(
            1.0,
            if dark {
                Color32::from_rgb(61, 63, 68)
            } else {
                Color32::from_rgb(226, 227, 230)
            },
        ))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(16, 14))
}

fn settings_row_label(ui: &mut Ui, title: &str) {
    // 与右侧控件同高并垂直居中,避免 Grid 行内标题偏上或偏下。
    ui.scope(|ui| {
        ui.style_mut().override_text_valign = Some(egui::Align::Center);
        ui.add_sized([104.0, 28.0], egui::Label::new(title));
    });
}

fn settings_toggle(ui: &mut Ui, value: &mut bool) -> egui::Response {
    let (rect, mut response) = ui.allocate_exact_size(vec2(38.0, 22.0), egui::Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    let animation = ui.ctx().animate_bool(response.id, *value);
    let track = rect.shrink(1.0);
    let mut fill = if *value {
        ui.visuals().selection.bg_fill
    } else {
        ui.visuals().widgets.inactive.bg_fill
    };
    if response.hovered() {
        fill = fill.gamma_multiply(1.12);
    }
    ui.painter().rect(
        track,
        track.height() * 0.5,
        fill,
        ui.visuals().widgets.noninteractive.bg_stroke,
        egui::StrokeKind::Inside,
    );
    let knob_x = egui::lerp((track.left() + 10.0)..=(track.right() - 10.0), animation);
    ui.painter()
        .circle_filled(egui::pos2(knob_x, track.center().y), 8.0, Color32::WHITE);
    response
}

fn show_help_settings_page(ui: &mut Ui) {
    settings_section_frame(ui).show(ui, |ui| {
        ui.set_width(396.0);
        ui.label(RichText::new("常用操作").strong().size(14.0));
        ui.add_space(10.0);
        for (title, detail) in [
            ("连接", "在 SSH 启动页双击已保存的连接，或新建本机终端。"),
            (
                "文件",
                "双击进入目录，右键打开更多操作；支持拖放上传文件和目录。",
            ),
            ("终端", "使用底部命令栏发送命令，按上下方向键浏览历史记录。"),
            ("复制", "在终端中选中文字后，按 Ctrl+C 复制选区。"),
        ] {
            ui.label(RichText::new(title).strong());
            ui.label(
                RichText::new(detail)
                    .color(ui.visuals().weak_text_color())
                    .size(11.0),
            );
            ui.add_space(8.0);
        }
    });
}

fn paint_rshell_logo(painter: &egui::Painter, rect: egui::Rect) {
    // 与 packaging/macos/make_icon.swift 相同的 ">_" 几何,坐标按 1024 画布缩放。
    let scale = rect.width() / 1024.0;
    let map = |x: f32, y: f32| rect.min + vec2(x * scale, y * scale);
    let green = Color32::from_rgb(0x6B, 0xDB, 0x8C);
    painter.rect_filled(
        rect,
        CornerRadius::same((200.0 * scale) as u8),
        Color32::from_rgb(0x1A, 0x1C, 0x26),
    );
    painter.add(egui::Shape::line(
        vec![map(294.0, 364.0), map(504.0, 512.0), map(294.0, 660.0)],
        egui::Stroke::new(76.0 * scale, green),
    ));
    painter.rect_filled(
        egui::Rect::from_min_max(map(585.0, 630.0), map(805.0, 694.0)),
        CornerRadius::same((16.0 * scale).max(1.0) as u8),
        green,
    );
}

fn show_about_settings_page(ui: &mut Ui) {
    let secondary = ui.visuals().weak_text_color();
    settings_section_frame(ui).show(ui, |ui| {
        ui.set_width(396.0);
        ui.vertical_centered(|ui| {
            ui.add_space(10.0);
            let (logo_rect, _) = ui.allocate_exact_size(vec2(52.0, 52.0), egui::Sense::hover());
            paint_rshell_logo(ui.painter(), logo_rect);
            ui.add_space(9.0);
            ui.label(RichText::new("RShell").strong().size(22.0));
            ui.label(
                RichText::new(format!("版本 {}", env!("CARGO_PKG_VERSION")))
                    .color(secondary)
                    .size(12.0),
            );
            ui.add_space(10.0);
            ui.label("轻量、可靠的 SSH 终端与服务器管理工具");
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(RichText::new(RSHELL_COPYRIGHT).color(secondary).size(11.0));
            ui.label(
                RichText::new("社区版按 GNU AGPL v3 授权；不提供任何担保")
                    .color(secondary)
                    .size(11.0),
            );
            ui.hyperlink_to(RSHELL_SOURCE_URL, RSHELL_SOURCE_URL);
            ui.hyperlink_to("查看许可证与传播条件", RSHELL_LICENSE_URL);
            ui.label(
                RichText::new("另提供单独书面商业授权")
                    .color(secondary)
                    .size(11.0),
            );
            ui.add_space(8.0);
        });
    });
}

/// 语法高亮主题下拉框;空字符串表示跟随内建默认。返回是否有修改。
fn highlight_theme_picker(ui: &mut Ui, id: &str, value: &mut String, default_name: &str) -> bool {
    let mut changed = false;
    let selected = if value.is_empty() {
        format!("默认 ({default_name})")
    } else {
        value.clone()
    };
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected)
        .show_ui(ui, |ui| {
            if ui
                .selectable_label(value.is_empty(), format!("默认 ({default_name})"))
                .clicked()
            {
                value.clear();
                changed = true;
            }
            for name in crate::ui::file_editor::available_highlight_themes() {
                if ui.selectable_label(*value == name, &name).clicked() {
                    *value = name;
                    changed = true;
                }
            }
        });
    changed
}

fn theme_preference(theme: ThemeMode) -> ThemePreference {
    match theme {
        ThemeMode::System => ThemePreference::System,
        ThemeMode::Dark => ThemePreference::Dark,
        ThemeMode::Light => ThemePreference::Light,
    }
}

pub(crate) fn apply_native_window_theme(ctx: &egui::Context, theme: ThemeMode) {
    let native_theme = match theme {
        ThemeMode::System => egui::SystemTheme::SystemDefault,
        ThemeMode::Dark => egui::SystemTheme::Dark,
        ThemeMode::Light => egui::SystemTheme::Light,
    };
    ctx.send_viewport_cmd_to(
        egui::ViewportId::ROOT,
        egui::ViewportCommand::SetTheme(native_theme),
    );
    if ctx.viewport_id() != egui::ViewportId::ROOT {
        ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(native_theme));
    }
}

pub(crate) fn apply_color_scheme(ctx: &egui::Context, scheme: ColorScheme) {
    let color = match scheme {
        // 使用更浅、更亮的蓝色，降低它与深色界面背景之间的反差。
        ColorScheme::Blue => Color32::from_rgb(0x6E, 0xA4, 0xE5),
        ColorScheme::Green => Color32::from_rgb(0x35, 0xA8, 0x67),
        ColorScheme::Purple => Color32::from_rgb(0x8A, 0x63, 0xD2),
        ColorScheme::Orange => Color32::from_rgb(0xD9, 0x78, 0x32),
    };
    ctx.all_styles_mut(|style| {
        // egui 默认 Window 使用 Heading 绘制标题；统一到自定义标题栏的 16px。
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(16.0));
        // 默认窗口激活时不再叠加深色标题栏阴影，保持与连接库一致。
        style.visuals.widgets.open.weak_bg_fill = if style.visuals.dark_mode {
            Color32::from_rgb(35, 36, 39)
        } else {
            Color32::from_rgb(246, 246, 247)
        };
        style.visuals.selection.bg_fill = color;
        style.visuals.hyperlink_color = color;
        style.visuals.widgets.active.bg_fill = color;
        style.visuals.widgets.active.bg_stroke.color = color;
        style.visuals.widgets.hovered.bg_stroke.color = color;
    });
}
