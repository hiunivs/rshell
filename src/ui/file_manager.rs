// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

use crate::app::{BottomTab, RShellApp};
use crate::config::TransferSettings;
use crate::file_manager::{FileManager, SortCol, TransferPos};
use crate::session::files::{FileEntry, SftpCmd, join_path, normalize_path};
use crate::ui::icons::{Icon, icon_button};
use crate::ui::sidebar::{fmt_bytes, fmt_rate};
use egui::{Color32, Pos2, RichText, Stroke, vec2};

/// 按传输设置解析文件下载的本地目标路径;None = 用户取消。
/// 固定目录模式下直接拼接目标文件名(同名覆盖),否则弹系统保存对话框。
pub(crate) fn resolve_file_download_target(ts: &TransferSettings, name: &str) -> Option<String> {
    if let Some(dir) = ts.effective_download_dir() {
        return Some(
            std::path::Path::new(dir)
                .join(name)
                .to_string_lossy()
                .into_owned(),
        );
    }
    rfd::FileDialog::new()
        .set_file_name(name)
        .save_file()
        .map(|p| p.to_string_lossy().into_owned())
}

/// 按传输设置解析目录下载的本地目标;固定模式下以远程目录名建子目录。
pub(crate) fn resolve_dir_download_target(ts: &TransferSettings, name: &str) -> Option<String> {
    if let Some(dir) = ts.effective_download_dir() {
        return Some(
            std::path::Path::new(dir)
                .join(name)
                .to_string_lossy()
                .into_owned(),
        );
    }
    rfd::FileDialog::new()
        .pick_folder()
        .map(|p| p.to_string_lossy().into_owned())
}

/// 常用命令预设(名称, 命令)
const COMMON_COMMANDS: &[(&str, &str)] = &[
    ("磁盘占用", "df -h"),
    ("目录大小", "du -sh ./* 2>/dev/null | sort -rh | head -20"),
    ("内存 Top10", "ps aux --sort=-%mem | head -11"),
    ("CPU Top10", "ps aux --sort=-%cpu | head -11"),
    ("端口监听", "ss -tlnp"),
    ("网络连接统计", "ss -s"),
    ("系统信息", "uname -a"),
    ("运行时间", "uptime"),
    ("最近登录", "last -10"),
    ("内核日志", "dmesg | tail -30"),
    ("失败的服务", "systemctl --failed"),
    (
        "大文件 Top20",
        "find / -xdev -type f -size +100M 2>/dev/null | head -20",
    ),
];

impl RShellApp {
    /// 底部文件管理器面板
    pub(crate) fn show_file_manager(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        // 原生可拖拽 Panel 在悬停/拖动时使用黑色前景描边。
        // 仅为面板分隔条临时换成低对比度细线，内部控件仍使用原样式。
        let hovered_fg = ui.visuals().widgets.hovered.fg_stroke;
        let active_fg = ui.visuals().widgets.active.fg_stroke;
        let divider_stroke = Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
        ui.style_mut().visuals.widgets.hovered.fg_stroke = divider_stroke;
        ui.style_mut().visuals.widgets.active.fg_stroke = divider_stroke;
        egui::Panel::bottom("file_manager")
            .resizable(true)
            .default_size(300.0)
            .size_range(120.0..=600.0)
            .show(ui, |ui| {
                ui.style_mut().visuals.widgets.hovered.fg_stroke = hovered_fg;
                ui.style_mut().visuals.widgets.active.fg_stroke = active_fg;
                // 页签:文件 | 命令
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.bottom_tab, BottomTab::Files, "文件");
                    ui.selectable_value(&mut self.bottom_tab, BottomTab::Commands, "命令");
                    if self.bottom_tab == BottomTab::Commands {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("＋ 新建常用命令").clicked() {
                                self.command_new_name.clear();
                                self.command_new_value.clear();
                                self.command_new_error = None;
                                self.command_new_focus = true;
                                self.command_new_open = true;
                            }
                        });
                    }
                });
                ui.separator();

                match self.bottom_tab {
                    BottomTab::Files => {
                        // SysInfo 标签页时操作源会话的文件服务
                        if let Some(idx) = self.effective_session_idx() {
                            let fm = &mut self.sessions[idx].fm;
                            fm_toolbar(ui, fm, &self.config.transfers);
                            ui.separator();
                            // 剩余区域只在父布局中分配一次；主体和底栏都用绝对定位的
                            // child UI 绘制，内部字体行高不得反向撑大可缩放 Panel。
                            let content_rect = ui.available_rect_before_wrap();
                            let _ = ui.allocate_rect(content_rect, egui::Sense::hover());
                            let footer_h = ui
                                .spacing()
                                .interact_size
                                .y
                                .max(ui.text_style_height(&egui::TextStyle::Button));
                            let footer_rect = egui::Rect::from_min_max(
                                egui::pos2(
                                    content_rect.left(),
                                    (content_rect.bottom() - footer_h).max(content_rect.top()),
                                ),
                                content_rect.max,
                            );
                            let work_rect = egui::Rect::from_min_max(
                                content_rect.min,
                                egui::pos2(
                                    content_rect.right(),
                                    (footer_rect.top() - ui.spacing().item_spacing.y)
                                        .max(content_rect.top()),
                                ),
                            );
                            let mut work_ui = ui.new_child(
                                egui::UiBuilder::new()
                                    .id_salt("fm_work")
                                    .max_rect(work_rect)
                                    .layout(egui::Layout::top_down(egui::Align::LEFT)),
                            );
                            let transfers_visible =
                                fm.show_transfers_pane && !fm.transfers.is_empty();
                            let transfers_h = if transfers_visible {
                                transfers_height(fm, fm.transfer_pos)
                            } else {
                                0.0
                            };
                            if transfers_visible && fm.transfer_pos == TransferPos::Right {
                                // 右侧布局:文件列表(左) + 传输窗(右,固定 220px)
                                let trans_w = 220.0;
                                let body_w = (work_rect.width() - trans_w - 4.0).max(150.0);
                                let content_h = work_rect.height().max(60.0);
                                let body_rect = egui::Rect::from_min_size(
                                    work_rect.min,
                                    vec2(body_w, content_h),
                                );
                                let tree_w =
                                    fm.tree_width.clamp(100.0, (body_w - 220.0).max(100.0));
                                let mut body_ui = work_ui.new_child(
                                    egui::UiBuilder::new()
                                        .id_salt("fm_right_body")
                                        .max_rect(body_rect),
                                );
                                fm_tree_and_table(
                                    &mut body_ui,
                                    fm,
                                    tree_w,
                                    body_rect,
                                    &self.config.transfers,
                                );
                                // 分隔线(文件列表与传输窗之间)
                                let sep_x = body_rect.right() + 1.0;
                                ui.painter().vline(
                                    sep_x,
                                    body_rect.top()..=body_rect.bottom(),
                                    ui.visuals().widgets.noninteractive.bg_stroke,
                                );
                                // 传输窗(右)
                                let trans_rect = egui::Rect::from_min_size(
                                    egui::pos2(sep_x + 5.0, body_rect.top()),
                                    vec2(trans_w, content_h),
                                );
                                let mut transfer_ui = work_ui.new_child(
                                    egui::UiBuilder::new()
                                        .id_salt("fm_right_transfers")
                                        .max_rect(trans_rect)
                                        .layout(egui::Layout::top_down(egui::Align::LEFT)),
                                );
                                fm_transfers(&mut transfer_ui, fm, content_h, TransferPos::Right);
                            } else {
                                // 底部布局(默认)
                                let transfer_gap = if transfers_visible {
                                    work_ui.spacing().item_spacing.y
                                } else {
                                    0.0
                                };
                                let body_h =
                                    (work_rect.height() - transfers_h - transfer_gap).max(40.0);
                                fm_body(&mut work_ui, fm, body_h, &self.config.transfers);
                                if transfers_visible {
                                    fm_transfers(
                                        &mut work_ui,
                                        fm,
                                        transfers_h,
                                        TransferPos::Bottom,
                                    );
                                }
                            }
                            // === 底栏:右下角按钮 ===
                            let mut footer_ui = ui.new_child(
                                egui::UiBuilder::new()
                                    .id_salt("fm_footer")
                                    .max_rect(footer_rect)
                                    .layout(egui::Layout::right_to_left(egui::Align::Center)),
                            );
                            if transfer_pane_toggle(&mut footer_ui, fm.show_transfers_pane)
                                .clicked()
                            {
                                fm.show_transfers_pane = !fm.show_transfers_pane;
                            }
                            // 汉堡按钮嵌在 menu_button 里:弹窗自动贴按钮定位,不占布局
                            footer_ui.menu_button(RichText::new("≡"), |ui| {
                                crate::ui::dialog::mark_modal(ui.ctx());
                                if ui
                                    .selectable_label(
                                        fm.transfer_pos == TransferPos::Bottom,
                                        "底部",
                                    )
                                    .clicked()
                                {
                                    fm.transfer_pos = TransferPos::Bottom;
                                    ui.close();
                                }
                                if ui
                                    .selectable_label(fm.transfer_pos == TransferPos::Right, "右侧")
                                    .clicked()
                                {
                                    fm.transfer_pos = TransferPos::Right;
                                    ui.close();
                                }
                            });
                            fm_dialogs(ui, fm);
                            fm_drag_drop(ui, fm);
                        } else {
                            fill_rect(ui, |ui| {
                                ui.label("（无会话）");
                            });
                        }
                    }
                    BottomTab::Commands => {
                        // 常用命令:点击直接发送到当前终端;同样显式填满,保持面板高度一致
                        fill_rect(ui, |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("cmd_list")
                                // 固定占满,不随内容收缩,保持面板高度恒定
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    let mut run: Option<String> = None;
                                    let commands =
                                        COMMON_COMMANDS
                                            .iter()
                                            .map(|(name, command)| {
                                                ((*name).to_string(), (*command).to_string())
                                            })
                                            .chain(self.config.common_commands.iter().map(|item| {
                                                (item.name.clone(), item.command.clone())
                                            }))
                                            .collect::<Vec<_>>();
                                    egui::Grid::new("cmd_grid")
                                        .num_columns(4)
                                        .spacing([8.0, 6.0])
                                        .show(ui, |ui| {
                                            for (i, (name, command)) in commands.iter().enumerate()
                                            {
                                                if ui.button(name).on_hover_text(command).clicked()
                                                {
                                                    run = Some(command.clone());
                                                }
                                                if i % 4 == 3 {
                                                    ui.end_row();
                                                }
                                            }
                                        });
                                    if let Some(cmd) = run {
                                        self.push_cmd_history(&cmd);
                                        // SysInfo 标签页时发到源会话的终端
                                        if let Some(idx) = self.effective_session_idx() {
                                            let session = &mut self.sessions[idx];
                                            session.send_str(&cmd);
                                            session.send_str("\r");
                                        }
                                    }
                                });
                        });
                    }
                }
            });
        ui.style_mut().visuals.widgets.hovered.fg_stroke = hovered_fg;
        ui.style_mut().visuals.widgets.active.fg_stroke = active_fg;
        self.show_common_command_dialog(&ctx);
    }

    fn show_common_command_dialog(&mut self, ctx: &egui::Context) {
        if !self.command_new_open {
            return;
        }
        let mut open = self.command_new_open;
        let mut close = false;
        let title_close = crate::ui::dialog::compact_dialog_window(
            ctx,
            "新建常用命令",
            Some(egui::Id::new("common_command_new")),
            &mut open,
            |ui| {
                crate::ui::dialog::polish(ui);
                ui.set_width(360.0);
                let name_id = egui::Id::new("common_command_new_name");
                let value_id = egui::Id::new("common_command_new_value");
                egui::Grid::new("common_command_new_grid")
                    .num_columns(2)
                    .spacing([10.0, 9.0])
                    .show(ui, |ui| {
                        ui.label("名称");
                        let name_resp = ui.add(
                            egui::TextEdit::singleline(&mut self.command_new_name)
                                .id(name_id)
                                .desired_width(290.0)
                                .hint_text("例如：查看 Docker 容器"),
                        );
                        if std::mem::take(&mut self.command_new_focus) {
                            crate::ui::dialog::focus_text_input(
                                &name_resp,
                                ui,
                                name_id,
                                self.command_new_name.chars().count(),
                            );
                        }
                        ui.end_row();
                        ui.label("命令");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.command_new_value)
                                .id(value_id)
                                .desired_width(290.0)
                                .hint_text("例如：docker ps -a"),
                        );
                        ui.end_row();
                    });
                if let Some(error) = &self.command_new_error {
                    ui.add_space(4.0);
                    ui.colored_label(Color32::from_rgb(0xE8, 0x4C, 0x3D), error);
                }
                ui.add_space(7.0);
                let mut save = false;
                let mut cancel = false;
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 30.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        save |= crate::ui::dialog::primary_button(ui, "保存").clicked();
                        cancel |= crate::ui::dialog::secondary_button(ui, "取消").clicked();
                    },
                );
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    save = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    save = false;
                    cancel = true;
                }
                if save {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                    if self.save_common_command() {
                        close = true;
                    } else {
                        // 校验失败时把焦点留在弹窗，别掉到下面的终端。
                        self.command_new_focus = true;
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
        self.command_new_open = open;
    }

    fn save_common_command(&mut self) -> bool {
        let name = self.command_new_name.trim().to_string();
        let command = self.command_new_value.trim().to_string();
        if name.is_empty() {
            self.command_new_error = Some("名称不能为空".into());
            return false;
        }
        if command.is_empty() {
            self.command_new_error = Some("命令不能为空".into());
            return false;
        }
        if COMMON_COMMANDS
            .iter()
            .any(|(built_in_name, _)| *built_in_name == name)
            || self
                .config
                .common_commands
                .iter()
                .any(|item| item.name == name)
        {
            self.command_new_error = Some("已存在同名常用命令".into());
            return false;
        }

        self.config
            .common_commands
            .push(crate::config::SavedCommand { name, command });
        if let Err(error) = crate::config::save(&self.config) {
            self.config.common_commands.pop();
            self.command_new_error = Some(format!("保存失败：{error:#}"));
            return false;
        }
        self.command_new_error = None;
        true
    }
}

// ---------------------------------------------------------------------------
// 工具栏
// ---------------------------------------------------------------------------

/// 用显式矩形占满剩余高度,保证面板内容高度恒定(不同页签高度一致)
fn fill_rect(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui)) {
    let rect = egui::Rect::from_min_size(
        ui.next_widget_position(),
        vec2(ui.available_width(), ui.available_height()),
    );
    let _ = ui.allocate_rect(rect, egui::Sense::hover());
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), add_contents);
}

/// 传输窗展开/收起图标按钮:收起→向上尖角(展开),展开→向下尖角(收起)
fn transfer_pane_toggle(ui: &mut egui::Ui, expanded: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(22.0, 20.0), egui::Sense::click());
    let resp = resp
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(if expanded {
            "收起传输窗"
        } else {
            "展开传输窗"
        });
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
    let cx = rect.center().x;
    let cy = rect.center().y;
    let s = 5.0;
    if expanded {
        // 向下尖角(点击收起)
        ui.painter()
            .line_segment([Pos2::new(cx - s, cy - 2.0), Pos2::new(cx, cy + s)], stroke);
        ui.painter()
            .line_segment([Pos2::new(cx + s, cy - 2.0), Pos2::new(cx, cy + s)], stroke);
    } else {
        // 向上尖角(点击展开)
        ui.painter()
            .line_segment([Pos2::new(cx - s, cy + 2.0), Pos2::new(cx, cy - s)], stroke);
        ui.painter()
            .line_segment([Pos2::new(cx + s, cy + 2.0), Pos2::new(cx, cy - s)], stroke);
    }
    resp
}

fn fm_toolbar(ui: &mut egui::Ui, fm: &mut FileManager, ts: &TransferSettings) {
    ui.horizontal(|ui| {
        // 弹窗打开时路径框也不能接收键盘：它是文件面板里绘制更早的输入框，
        // 保留焦点时会和弹窗同时响应同一个回车。
        let modal_blocked = crate::ui::dialog::modal_keyboard_blocked(ui.ctx());
        // 路径框:缩短,给右侧按钮 + 加载提示预留固定宽度
        let resp = ui
            .add(
                egui::TextEdit::singleline(&mut fm.path_edit)
                    .desired_width((ui.available_width() - 360.0).max(60.0))
                    .interactive(!modal_blocked),
            )
            .on_hover_cursor(egui::CursorIcon::Default);
        if !modal_blocked && resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            let path = fm.path_edit.clone();
            fm.navigate(&path);
        }
        // 加载提示:历史按钮之前,固定占位避免显隐时路径框宽度跳动
        let loading_text = if fm.loading { "加载中…" } else { "" };
        ui.add_sized(
            [56.0, 14.0],
            egui::Label::new(RichText::new(loading_text).weak().small()),
        );
        // 历史路径
        ui.menu_button("历史", |ui| {
            crate::ui::dialog::mark_modal(ui.ctx());
            let mut goto: Option<String> = None;
            for h in fm.history.iter().rev().take(12) {
                if ui.button(h).clicked() {
                    goto = Some(h.clone());
                    ui.close();
                }
            }
            if fm.history.is_empty() {
                ui.label(RichText::new("（空）").weak());
            }
            if let Some(h) = goto {
                fm.navigate(&h);
            }
        });
        if ui
            .add(egui::Button::image(
                Icon::ChevronUp.image(14.0, ui.visuals().text_color()),
            ))
            .on_hover_text("上级目录")
            .clicked()
        {
            fm.go_parent();
        }
        if ui
            .add(egui::Button::image(
                Icon::RefreshCw.image(14.0, ui.visuals().text_color()),
            ))
            .on_hover_text("刷新")
            .clicked()
        {
            fm.refresh();
        }
        ui.menu_button("新建", |ui| fm_new_menu(ui, fm))
            .response
            .on_hover_text("新建文件夹或文件");
        if ui
            .add(egui::Button::image_and_text(
                Icon::Upload.image(14.0, ui.visuals().text_color()),
                "上传",
            ))
            .clicked()
            && let Some(path) = rfd::FileDialog::new().pick_file()
        {
            fm.start_upload(path.to_string_lossy().into_owned());
        }
        // 下载选中项(文件/目录自动检测)
        let sel_entry = fm.selected.and_then(|i| fm.entries.get(i)).cloned();
        if ui
            .add(egui::Button::image_and_text(
                Icon::Download.image(14.0, ui.visuals().text_color()),
                "下载",
            ))
            .clicked()
        {
            if let Some(entry) = sel_entry {
                if entry.is_dir {
                    if let Some(path) = resolve_dir_download_target(ts, &entry.name) {
                        fm.start_download_dir(&entry, path);
                    }
                } else if let Some(path) = resolve_file_download_target(ts, &entry.name) {
                    fm.start_download(&entry, path);
                }
            } else {
                fm.error = Some("请先选中一个文件或目录".into());
            }
        }
    });
}

// ---------------------------------------------------------------------------
// 主体:左目录树 + 右文件表(显式矩形分栏,高度由调用方给定)
// ---------------------------------------------------------------------------

/// 传输栏所需高度(无传输任务时为 0)
fn transfers_height(fm: &FileManager, pos: TransferPos) -> f32 {
    if fm.transfers.is_empty() {
        0.0
    } else {
        // 底部:单行条目;右侧:双行(文件名 + 细条)
        let row_h: f32 = if pos == TransferPos::Right {
            26.0
        } else {
            20.0
        };
        (44.0 + fm.transfers.len() as f32 * row_h).min(220.0)
    }
}

fn fm_body(ui: &mut egui::Ui, fm: &mut FileManager, height: f32, ts: &TransferSettings) {
    let tree_w = fm
        .tree_width
        .clamp(100.0, (ui.available_width() - 220.0).max(100.0));
    let body_rect = egui::Rect::from_min_size(
        ui.next_widget_position(),
        vec2(ui.available_width(), height),
    );
    let _ = ui.allocate_rect(body_rect, egui::Sense::hover());
    // body_rect 已经由父 UI 分配；这里必须使用不参与父布局的 child UI。
    // scope_builder 会再次用子 UI 的 min_rect 推进父游标，子内容只要因字体
    // 行高超出 1px，就会把该 1px 写回可缩放 PanelState 并逐帧累加。
    let mut body_ui = ui.new_child(
        egui::UiBuilder::new()
            .id_salt("fm_body")
            .max_rect(body_rect),
    );
    fm_tree_and_table(&mut body_ui, fm, tree_w, body_rect, ts);
}

/// 目录树 + 文件表的渲染内容(外层已分配矩形,此函数只画内容)
fn fm_tree_and_table(
    ui: &mut egui::Ui,
    fm: &mut FileManager,
    tree_w: f32,
    body_rect: egui::Rect,
    ts: &TransferSettings,
) {
    let height = body_rect.height();
    // 左:目录树
    let tree_rect = egui::Rect::from_min_size(body_rect.min, vec2(tree_w, height));
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(tree_rect)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
        |ui| {
            egui::ScrollArea::vertical()
                .id_salt("fm_tree")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    fm_tree(ui, fm);
                });
        },
    );
    // 分隔条(可拖拽调整树栏宽)
    let sep_x = tree_rect.right() + 1.0;
    let handle_rect =
        egui::Rect::from_min_size(egui::pos2(sep_x - 3.0, body_rect.top()), vec2(7.0, height));
    let resp = ui.interact(handle_rect, ui.id().with("tree_split"), egui::Sense::drag());
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }
    if resp.dragged() {
        fm.tree_width = (tree_w + resp.drag_delta().x).clamp(100.0, 400.0);
    }
    let line_stroke = if resp.hovered() || resp.dragged() {
        ui.visuals().widgets.hovered.bg_stroke
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    ui.painter().vline(sep_x, body_rect.y_range(), line_stroke);
    // 右:文件表(ScrollArea 包裹:行数超出时滚动而非撑大 Panel)
    let table_rect =
        egui::Rect::from_min_max(egui::pos2(sep_x + 5.0, body_rect.top()), body_rect.max);
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(table_rect)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
        |ui| {
            fm_table_scroll(ui, fm, ts);
        },
    );
    // 错误提示:覆盖层横幅,不占布局空间(防止面板高度回弹)
    let overlay = fm
        .error
        .as_ref()
        .map(|err| (err.clone(), Color32::from_rgb(0xD8, 0x3A, 0x3A)));
    if let Some((text, color)) = overlay {
        let banner = egui::Rect::from_min_size(
            table_rect.left_top() + vec2(0.0, 21.0),
            vec2(table_rect.width(), 18.0),
        );
        let painter = ui.painter().with_clip_rect(table_rect);
        // 覆盖层跟随主题：深色主题下不能用半透明白底。
        let banner_bg = ui.visuals().extreme_bg_color;
        painter.rect_filled(
            banner,
            0.0,
            Color32::from_rgba_unmultiplied(banner_bg.r(), banner_bg.g(), banner_bg.b(), 0xE6),
        );
        painter.text(
            banner.left_center() + vec2(6.0, 0.0),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(12.0),
            color,
        );
    }
}

fn fm_tree(ui: &mut egui::Ui, fm: &mut FileManager) {
    let rows = fm.visible_tree_rows();
    for (path, depth, expanded, loading, is_symlink) in rows.iter() {
        let is_cwd = normalize_path(path) == fm.cwd;
        // 子项已加载(展开过/已取回子目录)→ 蓝色;否则灰蓝色
        let loaded = fm.tree_children.contains_key(path.as_str());
        // 先占一个绘制槽，等所有子控件响应合并后再把悬停背景放回文字/图标
        // 之前，确保整行阴影不会盖住内容。
        let selection_background = ui.painter().add(egui::Shape::Noop);
        let hover_background = ui.painter().add(egui::Shape::Noop);
        let row = ui.horizontal(|ui| {
            ui.set_min_width(ui.available_width());
            ui.add_space(4.0 + *depth as f32 * 14.0);
            // 未展开时不显示收起箭头；展开后才显示 ▼，固定占位保持目录对齐。
            let disclosure_rect = if *expanded {
                let collapse = icon_button(
                    ui,
                    Icon::ChevronDown,
                    "收起",
                    vec2(12.0, 14.0),
                    10.0,
                    ui.visuals().weak_text_color(),
                );
                Some(collapse.rect)
            } else {
                ui.allocate_exact_size(vec2(12.0, 14.0), egui::Sense::hover());
                None
            };
            // 文件夹图标:已加载=蓝色,未加载=灰蓝色(与右侧文件表同款)
            let (icon_rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::hover());
            let color = if loaded {
                Color32::from_rgb(0x4C, 0x9B, 0xE8)
            } else {
                Color32::from_rgb(0x9A, 0xB0, 0xC8)
            };
            if *is_symlink {
                // 目录树里的链接目录用链条图标标记，仍可正常展开。
                Icon::Link.paint_at(ui, icon_rect, Color32::from_rgb(0x6F, 0xB7, 0xD6));
            } else {
                draw_folder_icon(ui, icon_rect, color);
            }
            let dir_name = if path == "/" {
                "/".to_string()
            } else {
                path.rsplit('/').next().unwrap_or(path).to_string()
            };
            let label = if is_cwd {
                RichText::new(&dir_name).strong()
            } else {
                RichText::new(&dir_name)
            };
            ui.add(egui::Label::new(label).selectable(false).truncate());
            if *loading {
                ui.label(RichText::new("…").weak().small());
            }
            (dir_name, disclosure_rect)
        });
        // 在内容绘制后为整行注册点击区域，因此文字、图标以及末尾空白都
        // 具有相同交互。箭头区域仍保留单击收起/展开的快捷行为。
        let (dir_name, disclosure_rect) = row.inner;
        let row_response = ui
            .interact(
                row.response.rect,
                ui.id().with(("fm_tree_row", path)),
                egui::Sense::click(),
            )
            .on_hover_and_drag_cursor(egui::CursorIcon::Default)
            .on_hover_text(if *is_symlink {
                format!("{path}（符号链接目录）")
            } else {
                path.clone()
            });
        let over_disclosure = row_response
            .interact_pointer_pos()
            .is_some_and(|pointer| disclosure_rect.is_some_and(|rect| rect.contains(pointer)));
        if over_disclosure {
            if row_response.clicked() && !row_response.double_clicked() {
                fm.toggle_tree(path);
            }
        } else if row_response.double_clicked() {
            fm.expand_tree(path);
        } else if row_response.clicked() {
            fm.select_tree_path(path);
        }
        row_response.context_menu(|ui| {
            crate::ui::dialog::mark_modal(ui.ctx());
            if ui.button("复制目录名").clicked() {
                ui.ctx().copy_text(dir_name.clone());
                ui.close();
            }
            if ui.button("复制完整路径").clicked() {
                ui.ctx().copy_text(path.clone());
                ui.close();
            }
        });
        if is_cwd {
            let accent = ui.visuals().selection.bg_fill;
            ui.painter().set(
                selection_background,
                egui::Shape::rect_filled(
                    row.response.rect,
                    egui::CornerRadius::ZERO,
                    Color32::from_rgba_unmultiplied(
                        accent.r(),
                        accent.g(),
                        accent.b(),
                        if ui.visuals().dark_mode { 72 } else { 42 },
                    ),
                ),
            );
            if fm.tree_scroll_to_cwd {
                ui.scroll_to_rect(row.response.rect, Some(egui::Align::Center));
                fm.tree_scroll_to_cwd = false;
            }
        }
        if row_response.hovered() {
            ui.painter().set(
                hover_background,
                egui::Shape::rect_filled(
                    row.response.rect,
                    egui::CornerRadius::ZERO,
                    ui.visuals().widgets.hovered.bg_fill,
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 文件表格
// ---------------------------------------------------------------------------

fn fm_table_scroll(ui: &mut egui::Ui, fm: &mut FileManager, ts: &TransferSettings) {
    egui::ScrollArea::vertical()
        .id_salt("fm_table_scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            fm_table(ui, fm, ts);
            // 文件表只占用表头和已有行；把余下的可视区域注册为背景响应，
            // 这样空目录或文件行下方的空白处也能打开新建菜单。
            let blank_rect = ui.available_rect_before_wrap();
            if blank_rect.width() > 0.0 && blank_rect.height() > 0.0 {
                ui.allocate_rect(blank_rect, egui::Sense::click())
                    .context_menu(|ui| fm_new_menu(ui, fm));
            }
        });
}

fn fm_table(ui: &mut egui::Ui, fm: &mut FileManager, ts: &TransferSettings) {
    use egui_extras::{Column, TableBuilder};

    // 注意:错误/加载提示不在这里加控件——表格区域是固定矩形,
    // 多出的控件会把 Panel 内容高度顶出去,触发面板高度回弹(整体往上抬);
    // 提示由 fm_body 用 painter 覆盖层绘制,不占布局空间

    let row_h = 20.0;
    // egui_extras 只用 cell 自身的 response 记忆悬停行，cell 内后注册的
    // Label 会截走文字区域的 hover。额外缓存合并响应命中的行，下一帧在
    // 绘制所有单元格前统一 set_hovered，保证文字、图标和空白处高亮一致。
    let content_hover_id = ui.id().with("fm_table_content_hover");
    let ctx = ui.ctx().clone();
    let copy_shortcut = !crate::ui::dialog::modal_keyboard_blocked(&ctx)
        && ui.input(|input| {
            input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::C,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.command
                )
            })
        });
    let previous_hovered = ui
        .ctx()
        .data(|data| data.get_temp::<Option<usize>>(content_hover_id))
        .flatten();
    let mut current_hovered = None;
    TableBuilder::new(ui)
        .striped(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::remainder().at_least(140.0)) // 文件名
        .column(Column::exact(64.0)) // 大小
        .column(Column::exact(76.0)) // 类型
        .column(Column::exact(112.0)) // 修改时间
        .column(Column::exact(86.0)) // 权限
        .column(Column::exact(76.0)) // 用户/用户组
        .sense(egui::Sense::click())
        .header(20.0, |mut header| {
            header.col(|ui| {
                sort_header(ui, fm, SortCol::Name, "文件名");
            });
            header.col(|ui| {
                sort_header(ui, fm, SortCol::Size, "大小");
            });
            header.col(|ui| {
                ui.label("类型");
            });
            header.col(|ui| {
                sort_header(ui, fm, SortCol::Time, "修改时间");
            });
            header.col(|ui| {
                ui.label("权限");
            });
            header.col(|ui| {
                ui.label("用户/用户组");
            });
        })
        .body(|body| {
            body.rows(row_h, fm.entries.len(), |mut row| {
                let i = row.index();
                let entry = fm.entries[i].clone();
                let selected = fm.selected == Some(i);
                row.set_hovered(previous_hovered == Some(i));
                row.set_selected(selected);
                let mut content_responses = Vec::with_capacity(7);

                row.col(|ui| {
                    content_responses.push(draw_file_icon(ui, &entry));
                    content_responses.push(
                        ui.add(
                            egui::Label::new(&entry.name)
                                .truncate()
                                .selectable(false)
                                .sense(egui::Sense::click()),
                        ),
                    );
                });
                row.col(|ui| {
                    content_responses.push(
                        ui.add(
                            egui::Label::new(if entry.is_dir {
                                String::new()
                            } else {
                                fmt_bytes(entry.size)
                            })
                            .selectable(false)
                            .sense(egui::Sense::click()),
                        ),
                    );
                });
                row.col(|ui| {
                    let mut resp = ui.add(
                        egui::Label::new(file_type_label(&entry))
                            .selectable(false)
                            .sense(egui::Sense::click()),
                    );
                    if entry.is_symlink {
                        resp = resp.on_hover_text(if entry.is_broken_symlink {
                            "失效的符号链接：目标不存在"
                        } else if entry.is_dir {
                            "符号链接，指向目录"
                        } else {
                            "符号链接，指向文件"
                        });
                    }
                    content_responses.push(resp);
                });
                row.col(|ui| {
                    content_responses.push(
                        ui.add(
                            egui::Label::new(entry.mtime_string())
                                .selectable(false)
                                .sense(egui::Sense::click()),
                        ),
                    );
                });
                row.col(|ui| {
                    content_responses.push(
                        ui.add(
                            egui::Label::new(entry.perm_string())
                                .selectable(false)
                                .sense(egui::Sense::click()),
                        ),
                    );
                });
                row.col(|ui| {
                    let owner = if entry.owner.is_empty() {
                        format!("{}/{}", entry.uid, entry.gid)
                    } else {
                        format!("{}/{}", entry.owner, entry.group)
                    };
                    content_responses.push(
                        ui.add(
                            egui::Label::new(owner)
                                .selectable(false)
                                .sense(egui::Sense::click()),
                        ),
                    );
                });

                // TableRow 的 cell response 不会自动包含 cell 内后注册控件的点击；
                // 合并所有文字/图标响应后，整行任意位置拥有相同的交互语义。
                let resp = content_responses
                    .into_iter()
                    .fold(row.response(), |row_response, content_response| {
                        row_response.union(content_response)
                    })
                    .on_hover_and_drag_cursor(egui::CursorIcon::Default);
                if resp.contains_pointer() {
                    current_hovered = Some(i);
                }
                if selected && resp.clicked_elsewhere() {
                    fm.table_keyboard_active = false;
                }
                if resp.clicked() {
                    fm.selected = Some(i);
                    fm.table_keyboard_active = true;
                }
                // 仅当最后一次明确点击的是文件行时响应复制快捷键；
                // 点击其他区域会由 clicked_elsewhere 立即撤销，避免抢占终端或编辑器。
                if selected && fm.table_keyboard_active && copy_shortcut {
                    ctx.copy_text(entry.full_path(&fm.cwd));
                }
                if resp.double_clicked() {
                    if entry.is_dir {
                        let p = entry.full_path(&fm.cwd);
                        fm.navigate(&p);
                    } else {
                        // 双击文件:在线预览/编辑(类型判定后决定内建/外部/提示)
                        fm.request_preview = Some(entry.clone());
                    }
                }
                resp.context_menu(|ui| {
                    crate::ui::dialog::mark_modal(ui.ctx());
                    if entry.is_dir && ui.button("打开").clicked() {
                        let p = entry.full_path(&fm.cwd);
                        fm.navigate(&p);
                        ui.close();
                    }
                    if !entry.is_dir && ui.button("打开").clicked() {
                        fm.request_preview = Some(entry.clone());
                        ui.close();
                    }
                    if !entry.is_dir && ui.button("用本地程序打开…").clicked() {
                        fm.request_external_open = Some(entry.clone());
                        ui.close();
                    }
                    if ui.button("下载").clicked() {
                        if entry.is_dir {
                            if let Some(path) = resolve_dir_download_target(ts, &entry.name) {
                                fm.start_download_dir(&entry, path);
                            }
                        } else if let Some(path) = resolve_file_download_target(ts, &entry.name) {
                            fm.start_download(&entry, path);
                        }
                        ui.close();
                    }
                    if ui.button("重命名").clicked() {
                        fm.dialogs.rename_old = Some(entry.full_path(&fm.cwd));
                        fm.dialogs.rename_name = entry.name.clone();
                        fm.dialogs.rename_focus = true;
                        ui.close();
                    }
                    if ui.button("复制完整路径").clicked() {
                        ui.ctx().copy_text(entry.full_path(&fm.cwd));
                        ui.close();
                    }
                    if ui.button("复制文件名").clicked() {
                        ui.ctx().copy_text(entry.name.clone());
                        ui.close();
                    }
                    if ui.button("权限").clicked() {
                        let path = entry.full_path(&fm.cwd);
                        fm.dialogs.perm_target = Some(path);
                        fm.dialogs.perm_armed = false;
                        fm.dialogs.perm_original = entry.permissions;
                        fm.dialogs.perm_mode = entry.permissions & 0o777;
                        fm.dialogs.perm_is_dir = entry.is_dir;
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .button(RichText::new("删除").color(Color32::from_rgb(0xD8, 0x3A, 0x3A)))
                        .clicked()
                    {
                        // 符号链接始终按文件删除（删除链接本身，不删除目标）。
                        fm.dialogs.delete_target = Some((
                            entry.full_path(&fm.cwd),
                            entry.is_dir && !entry.is_symlink,
                            false,
                        ));
                        fm.dialogs.delete_armed = false;
                        ui.close();
                    }
                    if entry.is_dir
                        && !entry.is_symlink
                        && ui
                            .button(
                                RichText::new("递归删除")
                                    .color(Color32::from_rgb(0xD8, 0x3A, 0x3A)),
                            )
                            .clicked()
                    {
                        fm.dialogs.delete_target = Some((entry.full_path(&fm.cwd), true, true));
                        fm.dialogs.delete_armed = false;
                        ui.close();
                    }
                });
            });
        });
    if current_hovered != previous_hovered {
        ui.ctx()
            .data_mut(|data| data.insert_temp(content_hover_id, current_hovered));
        ui.ctx().request_repaint();
    }
}

fn sort_header(ui: &mut egui::Ui, fm: &mut FileManager, col: SortCol, text: &str) {
    let arrow = if fm.sort_col == col {
        if fm.sort_asc { " ▲" } else { " ▼" }
    } else {
        ""
    };
    if ui
        .add(egui::Button::new(format!("{text}{arrow}")).frame(false))
        .clicked()
    {
        fm.sort_by(col);
    }
}

/// 统一的 Lucide 文件夹图标，颜色由调用方给定。
pub(crate) fn draw_folder_icon(ui: &egui::Ui, rect: egui::Rect, color: Color32) {
    Icon::Folder.paint_at(ui, rect, color);
}

/// 小图标:文件夹蓝色/文件灰色
fn file_type_label(entry: &FileEntry) -> &'static str {
    if entry.is_symlink {
        if entry.is_broken_symlink {
            "失效链接"
        } else if entry.is_dir {
            "目录链接"
        } else {
            "文件链接"
        }
    } else if entry.is_dir {
        "文件夹"
    } else {
        "文件"
    }
}

fn draw_file_icon(ui: &mut egui::Ui, entry: &FileEntry) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::click());
    if entry.is_symlink {
        // 符号链接用链条图标，避免和普通文件/目录混淆；失效链接用红色提示。
        let color = if entry.is_broken_symlink {
            Color32::from_rgb(0xD8, 0x6C, 0x75)
        } else {
            Color32::from_rgb(0x6F, 0xB7, 0xD6)
        };
        Icon::Link.paint_at(ui, rect, color);
    } else if entry.is_dir {
        draw_folder_icon(ui, rect, Color32::from_rgb(0x4C, 0x9B, 0xE8));
    } else {
        Icon::File.paint_at(ui, rect, Color32::from_rgb(0xA8, 0xB2, 0xBC));
    }
    response
}

// ---------------------------------------------------------------------------
// 拖拽上传:拖本地文件到文件面板,上传到当前目录
// ---------------------------------------------------------------------------

fn fm_drag_drop(ui: &mut egui::Ui, fm: &mut FileManager) {
    if fm.sftp_tx.is_none() {
        return;
    }
    let panel_rect = ui.max_rect();
    let ctx = ui.ctx().clone();
    let (n_hovered, dropped, pointer) = ctx.input(|i| {
        (
            i.raw.hovered_files.len(),
            i.raw.dropped_files.clone(),
            i.pointer.latest_pos(),
        )
    });
    let over_panel = pointer.is_some_and(|p| panel_rect.contains(p));

    // 悬停提示遮罩:指针在文件面板上才显示(避免拖到终端也亮)
    if n_hovered > 0 && over_panel {
        let painter = ui.painter();
        painter.rect_filled(
            panel_rect,
            6.0,
            Color32::from_rgba_unmultiplied(60, 130, 220, 60),
        );
        painter.rect_stroke(
            panel_rect,
            6.0,
            egui::Stroke::new(2.0, Color32::from_rgb(0x3C, 0x82, 0xE8)),
            egui::StrokeKind::Inside,
        );
        painter.text(
            panel_rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("松开鼠标，上传 {n_hovered} 个文件到 {}", fm.cwd),
            egui::FontId::proportional(15.0),
            Color32::WHITE,
        );
    }

    // 落下:只要有文件就上传(不依赖 pointer 位置——drop 事件可能在
    // pointer.latest_pos() 为 None 的帧到达,原先的 over_panel 守卫会丢掉文件)
    if dropped.is_empty() {
        return;
    }
    for f in dropped {
        let Some(path) = f.path else { continue };
        if path.is_file() {
            fm.start_upload(path.to_string_lossy().into_owned());
        } else if path.is_dir() {
            fm.start_upload_dir(path.to_string_lossy().into_owned());
        }
    }
}

// ---------------------------------------------------------------------------
// 传输列表
// ---------------------------------------------------------------------------

/// 传输栏的“清除已完成”：保持正常字号，仅去掉 hover 扩张/蓝色描边，
/// 避免鼠标移上去时表头变高把下面列表顶得上下晃。
fn transfer_clear_button(ui: &mut egui::Ui) -> egui::Response {
    ui.scope(|ui| {
        let widgets = &mut ui.style_mut().visuals.widgets;
        widgets.hovered.expansion = 0.0;
        widgets.active.expansion = 0.0;
        widgets.hovered.bg_stroke = egui::Stroke::NONE;
        widgets.active.bg_stroke = egui::Stroke::NONE;
        ui.add(egui::Button::new("清除已完成").small())
    })
    .inner
}

/// 传输完成但最后一个进度事件不一定到达 100%，成功完成时直接显示满格。
fn transfer_fraction(transfer: &crate::file_manager::Transfer) -> f32 {
    if transfer.finished && transfer.error.is_none() {
        return 1.0;
    }
    if transfer.total > 0 {
        (transfer.done as f32 / transfer.total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn transfer_progress_text(transfer: &crate::file_manager::Transfer) -> String {
    let amount = if transfer.total == 0 {
        fmt_bytes(transfer.done)
    } else {
        format!("{}/{}", fmt_bytes(transfer.done), fmt_bytes(transfer.total))
    };
    format!("{amount} · {}/s", fmt_rate(transfer.speed_bps()))
}

fn fm_transfers(ui: &mut egui::Ui, fm: &mut FileManager, max_height: f32, pos: TransferPos) {
    if fm.transfers.is_empty() {
        return;
    }
    let compact = pos == TransferPos::Right;
    let transfers_rect = egui::Rect::from_min_size(
        ui.next_widget_position(),
        vec2(ui.available_width(), max_height),
    );
    let _ = ui.allocate_rect(transfers_rect, egui::Sense::hover());
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(transfers_rect)
            .layout(egui::Layout::top_down(egui::Align::LEFT)),
        |ui| {
            // 底部:顶部分隔线;右侧:不需要(已有竖线)
            if !compact {
                ui.separator();
            }
            let mut clear_finished = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new("传输").weak());
                if transfer_clear_button(ui).clicked() {
                    clear_finished = true;
                }
            });
            let mut cancel_transfer = None;
            let mut toggle_pause = None;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height((max_height - 30.0).max(20.0))
                .show(ui, |ui| {
                    for t in &fm.transfers {
                        if compact {
                            // 右侧窄窗:文件名 + 细条进度
                            ui.horizontal(|ui| {
                                ui.set_min_height(18.0);
                                let transfer_icon = if matches!(t.op, "download" | "download_dir") {
                                    Icon::Download
                                } else {
                                    Icon::Upload
                                };
                                let spacing = ui.spacing().item_spacing.x;
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.add(transfer_icon.image(13.0, ui.visuals().weak_text_color()));
                                ui.add(
                                    egui::Label::new(RichText::new(short_name(&t.path)).small())
                                        .truncate(),
                                );
                                ui.spacing_mut().item_spacing.x = spacing;
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if !t.finished {
                                            if ui
                                                .add(
                                                    egui::Button::image(
                                                        Icon::X
                                                            .image(11.0, ui.visuals().text_color()),
                                                    )
                                                    .small(),
                                                )
                                                .on_hover_text("取消传输")
                                                .clicked()
                                            {
                                                cancel_transfer = Some(t.id);
                                            }
                                            let pause_icon =
                                                if t.paused { Icon::Play } else { Icon::Pause };
                                            if ui
                                                .add(
                                                    egui::Button::image(
                                                        pause_icon
                                                            .image(11.0, ui.visuals().text_color()),
                                                    )
                                                    .small(),
                                                )
                                                .on_hover_text(if t.paused {
                                                    "继续传输"
                                                } else {
                                                    "暂停传输"
                                                })
                                                .clicked()
                                            {
                                                toggle_pause = Some(t.id);
                                            }
                                        }
                                        ui.label(if let Some(err) = &t.error {
                                            RichText::new(format!("失败:{err}"))
                                                .color(Color32::from_rgb(0xD8, 0x3A, 0x3A))
                                                .small()
                                        } else if t.finished {
                                            RichText::new("完成")
                                                .color(Color32::from_rgb(0x35, 0xA8, 0x60))
                                                .small()
                                        } else if t.paused {
                                            RichText::new("已暂停").small()
                                        } else {
                                            RichText::new(transfer_progress_text(t)).small()
                                        });
                                    },
                                );
                            });
                            if !t.finished && t.total > 0 {
                                let frac = transfer_fraction(t);
                                let bar_h = 3.0;
                                let (rect, _) = ui.allocate_exact_size(
                                    vec2(ui.available_width(), bar_h),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(
                                    rect,
                                    2.0,
                                    ui.visuals().widgets.inactive.bg_fill,
                                );
                                let fill_w = rect.width() * frac;
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_size(rect.min, vec2(fill_w, bar_h)),
                                    2.0,
                                    ui.visuals().selection.bg_fill,
                                );
                            }
                        } else {
                            // 底部宽窗:细进度条 + 独立百分比
                            ui.horizontal(|ui| {
                                ui.set_min_height(20.0);
                                let transfer_icon = if matches!(t.op, "download" | "download_dir") {
                                    Icon::Download
                                } else {
                                    Icon::Upload
                                };
                                let spacing = ui.spacing().item_spacing.x;
                                ui.spacing_mut().item_spacing.x = 4.0;
                                ui.add(transfer_icon.image(14.0, ui.visuals().weak_text_color()));
                                ui.add_sized(
                                    [200.0, 16.0],
                                    egui::Label::new(short_name(&t.path)).truncate(),
                                );
                                ui.spacing_mut().item_spacing.x = spacing;
                                let frac = transfer_fraction(t);
                                ui.add(
                                    egui::ProgressBar::new(frac)
                                        .desired_width(120.0)
                                        .desired_height(5.0),
                                );
                                ui.label(
                                    RichText::new(format!("{:.0}%", frac * 100.0))
                                        .small()
                                        .weak(),
                                );
                                ui.label(if let Some(err) = &t.error {
                                    RichText::new(format!("失败:{err}"))
                                        .color(Color32::from_rgb(0xD8, 0x3A, 0x3A))
                                } else if t.finished {
                                    RichText::new("完成").color(Color32::from_rgb(0x35, 0xA8, 0x60))
                                } else if t.paused {
                                    RichText::new("已暂停")
                                } else {
                                    RichText::new(transfer_progress_text(t))
                                });
                                if !t.finished {
                                    if ui
                                        .small_button(if t.paused { "继续" } else { "暂停" })
                                        .clicked()
                                    {
                                        toggle_pause = Some(t.id);
                                    }
                                    if ui.small_button("取消").clicked() {
                                        cancel_transfer = Some(t.id);
                                    }
                                }
                            });
                        }
                    }
                });
            if let Some(id) = toggle_pause {
                fm.toggle_pause_transfer(id);
            }
            if let Some(id) = cancel_transfer {
                fm.cancel_transfer(id);
            }
            if clear_finished {
                fm.transfers.retain(|t| !t.finished);
            }
        },
    );
}

fn short_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn fm_new_menu(ui: &mut egui::Ui, fm: &mut FileManager) {
    // “新建”菜单/右键菜单也是弹层：打开期间不要让终端接收按键。
    crate::ui::dialog::mark_modal(ui.ctx());
    if ui.button("新建文件夹").clicked() {
        fm.dialogs.create_file_open = false;
        fm.dialogs.mkdir_name.clear();
        fm.dialogs.mkdir_focus = true;
        fm.dialogs.mkdir_open = true;
        ui.close();
    }
    if ui.button("新建文件").clicked() {
        fm.dialogs.mkdir_open = false;
        fm.dialogs.create_file_name.clear();
        fm.dialogs.create_file_focus = true;
        fm.dialogs.create_file_open = true;
        ui.close();
    }
}

// ---------------------------------------------------------------------------
// 弹窗:新建文件夹/文件 / 重命名 / 删除确认
// ---------------------------------------------------------------------------

fn fm_dialogs(ui: &mut egui::Ui, fm: &mut FileManager) {
    let ctx = ui.ctx().clone();

    // 新建文件夹
    if fm.dialogs.mkdir_open {
        let mut open = true;
        let title_close = crate::ui::dialog::compact_dialog_window(
            &ctx,
            "新建文件夹",
            None,
            &mut open,
            |ui| {
                crate::ui::dialog::polish(ui);
                let name_id = egui::Id::new("fm_mkdir_name");
                let mut submit = false;
                let mut cancel = false;
                ui.horizontal(|ui| {
                    ui.label("名称:");
                    let resp =
                        ui.add(egui::TextEdit::singleline(&mut fm.dialogs.mkdir_name).id(name_id));
                    if std::mem::take(&mut fm.dialogs.mkdir_focus) {
                        crate::ui::dialog::focus_text_input(
                            &resp,
                            ui,
                            name_id,
                            fm.dialogs.mkdir_name.chars().count(),
                        );
                    }
                    submit |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                });
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 30.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let name_ok = !fm.dialogs.mkdir_name.trim().is_empty();
                        submit |= crate::ui::dialog::primary_button_enabled(ui, name_ok, "创建")
                            .clicked();
                        cancel |= crate::ui::dialog::secondary_button(ui, "取消").clicked();
                    },
                );
                cancel |= ui.input(|i| i.key_pressed(egui::Key::Escape));
                if submit {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                    if fm.dialogs.mkdir_name.trim().is_empty() {
                        // 名称无效时把焦点留回弹窗，别掉到下面的终端。
                        fm.dialogs.mkdir_focus = true;
                    } else {
                        let path = join_path(&fm.cwd, fm.dialogs.mkdir_name.trim());
                        if let Err(error) = fm.send(SftpCmd::Mkdir(path)) {
                            fm.error = Some(error);
                        }
                        fm.dialogs.mkdir_open = false;
                    }
                }
                if cancel {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Escape);
                    fm.dialogs.mkdir_open = false;
                }
            },
        );
        if title_close || !open {
            fm.dialogs.mkdir_open = false;
        }
    }

    // 新建空文件
    if fm.dialogs.create_file_open {
        let mut open = true;
        let title_close =
            crate::ui::dialog::compact_dialog_window(&ctx, "新建文件", None, &mut open, |ui| {
                crate::ui::dialog::polish(ui);
                let name_id = egui::Id::new("fm_create_file_name");
                let mut submit = false;
                let mut cancel = false;
                ui.horizontal(|ui| {
                    ui.label("名称:");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut fm.dialogs.create_file_name).id(name_id),
                    );
                    if std::mem::take(&mut fm.dialogs.create_file_focus) {
                        crate::ui::dialog::focus_text_input(
                            &resp,
                            ui,
                            name_id,
                            fm.dialogs.create_file_name.chars().count(),
                        );
                    }
                    submit |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                });
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 30.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let name_ok = !fm.dialogs.create_file_name.trim().is_empty();
                        submit |= crate::ui::dialog::primary_button_enabled(ui, name_ok, "创建")
                            .clicked();
                        cancel |= crate::ui::dialog::secondary_button(ui, "取消").clicked();
                    },
                );
                cancel |= ui.input(|i| i.key_pressed(egui::Key::Escape));
                if submit {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                    if fm.dialogs.create_file_name.trim().is_empty() {
                        fm.dialogs.create_file_focus = true;
                    } else {
                        let path = join_path(&fm.cwd, fm.dialogs.create_file_name.trim());
                        if let Err(error) = fm.send(SftpCmd::CreateFile(path)) {
                            fm.error = Some(error);
                        }
                        fm.dialogs.create_file_open = false;
                    }
                }
                if cancel {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Escape);
                    fm.dialogs.create_file_open = false;
                }
            });
        if title_close || !open {
            fm.dialogs.create_file_open = false;
        }
    }

    // 重命名
    if let Some(old_path) = fm.dialogs.rename_old.clone() {
        let mut open = true;
        let title_close =
            crate::ui::dialog::compact_dialog_window(&ctx, "重命名", None, &mut open, |ui| {
                crate::ui::dialog::polish(ui);
                ui.label(format!("原路径:{old_path}"));
                let name_id = egui::Id::new("fm_rename_name");
                let mut submit = false;
                let mut cancel = false;
                ui.horizontal(|ui| {
                    ui.label("新名称:");
                    let resp =
                        ui.add(egui::TextEdit::singleline(&mut fm.dialogs.rename_name).id(name_id));
                    if std::mem::take(&mut fm.dialogs.rename_focus) {
                        crate::ui::dialog::focus_text_input(
                            &resp,
                            ui,
                            name_id,
                            fm.dialogs.rename_name.chars().count(),
                        );
                    }
                    // 单行 TextEdit 收到回车会交出焦点；在这里确认，而不是让
                    // 回车继续落到下方终端执行。
                    submit |= resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                });
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 30.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let name_ok = !fm.dialogs.rename_name.trim().is_empty();
                        submit |= crate::ui::dialog::primary_button_enabled(ui, name_ok, "确定")
                            .clicked();
                        cancel |= crate::ui::dialog::secondary_button(ui, "取消").clicked();
                    },
                );
                cancel |= ui.input(|i| i.key_pressed(egui::Key::Escape));
                if submit {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                    if fm.dialogs.rename_name.trim().is_empty() {
                        fm.dialogs.rename_focus = true;
                    } else {
                        let new_path = join_path(&fm.cwd, fm.dialogs.rename_name.trim());
                        // 名称未修改时直接关闭，避免向服务器发送同路径 rename。
                        if new_path != old_path
                            && let Err(error) = fm.send(SftpCmd::Rename {
                                from: old_path.clone(),
                                to: new_path,
                            })
                        {
                            fm.error = Some(error);
                        }
                        fm.dialogs.rename_old = None;
                    }
                }
                if cancel {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Escape);
                    fm.dialogs.rename_old = None;
                }
            });
        if title_close || !open {
            fm.dialogs.rename_old = None;
        }
    }

    // 删除确认
    if let Some((path, is_dir, recursive)) = fm.dialogs.delete_target.clone() {
        let mut open = true;
        let title_close =
            crate::ui::dialog::compact_dialog_window(&ctx, "确认删除", None, &mut open, |ui| {
                crate::ui::dialog::polish(ui);
                if recursive {
                    ui.label(
                        RichText::new("将递归删除目录及其所有内容,不可恢复!")
                            .color(Color32::from_rgb(0xD8, 0x3A, 0x3A)),
                    );
                    ui.label(RichText::new(&path).strong());
                } else {
                    ui.label(format!(
                        "确定删除{}?",
                        if is_dir {
                            "目录(须为空)"
                        } else {
                            "文件"
                        }
                    ));
                    ui.label(RichText::new(&path).strong());
                    if is_dir {
                        ui.label(RichText::new("注意:只能删除空目录").weak());
                    }
                }
                let mut confirm = false;
                let mut cancel = false;
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 30.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        confirm |= crate::ui::dialog::destructive_button(ui, "删除").clicked();
                        cancel |= crate::ui::dialog::secondary_button(ui, "取消").clicked();
                    },
                );
                // 确认框没有可聚焦控件，终端焦点会一直留着；回车/取消在这里
                // 处理并消费，避免按键落到下层终端。第一帧不接收回车，避免
                // 打开弹窗的同一次键盘激活立即确认删除。
                let armed = std::mem::replace(&mut fm.dialogs.delete_armed, true);
                if armed && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    confirm = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    confirm = false;
                    cancel = true;
                }
                if confirm {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                    if recursive {
                        if let Err(error) = fm.send(SftpCmd::RemoveRecursive { path }) {
                            fm.error = Some(error);
                        }
                    } else if let Err(error) = fm.send(SftpCmd::Remove { path, is_dir }) {
                        fm.error = Some(error);
                    }
                    fm.dialogs.delete_target = None;
                }
                if cancel {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Escape);
                    fm.dialogs.delete_target = None;
                }
            });
        if title_close || !open {
            fm.dialogs.delete_target = None;
        }
    }

    // 权限设置
    if let Some(path) = fm.dialogs.perm_target.clone() {
        let mut open = true;
        let title_close =
            crate::ui::dialog::compact_dialog_window(&ctx, "设置权限", None, &mut open, |ui| {
                crate::ui::dialog::polish(ui);
                ui.label(RichText::new(&path).strong());
                // 3×3 复选:所有者/组/其他 × 读/写/执行
                let groups: [(&str, u32, u32, u32); 3] = [
                    ("所有者", 0o400, 0o200, 0o100),
                    ("组", 0o040, 0o020, 0o010),
                    ("其他", 0o004, 0o002, 0o001),
                ];
                egui::Grid::new("perm_grid")
                    .num_columns(4)
                    .spacing([10.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("");
                        ui.label("读");
                        ui.label("写");
                        ui.label("执行");
                        ui.end_row();
                        for (name, r, w, x) in groups {
                            ui.label(name);
                            for bit in [r, w, x] {
                                let mut on = fm.dialogs.perm_mode & bit != 0;
                                if ui.checkbox(&mut on, "").changed() {
                                    if on {
                                        fm.dialogs.perm_mode |= bit;
                                    } else {
                                        fm.dialogs.perm_mode &= !bit;
                                    }
                                }
                            }
                            ui.end_row();
                        }
                    });
                // 实时预览:权限字符串 + 八进制
                let preview_mode =
                    (fm.dialogs.perm_original & !0o777) | (fm.dialogs.perm_mode & 0o777);
                let perm_str =
                    crate::session::files::format_mode(preview_mode, fm.dialogs.perm_is_dir);
                ui.label(format!(
                    "{perm_str}    八进制 0{:03o}",
                    fm.dialogs.perm_mode & 0o777
                ));
                let mut apply = false;
                let mut cancel = false;
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 30.0),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        apply |= crate::ui::dialog::primary_button(ui, "应用").clicked();
                        cancel |= crate::ui::dialog::secondary_button(ui, "取消").clicked();
                    },
                );
                let armed = std::mem::replace(&mut fm.dialogs.perm_armed, true);
                if armed && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    apply = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    apply = false;
                    cancel = true;
                }
                if apply {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                    if let Err(error) = fm.send(SftpCmd::SetPermissions {
                        path,
                        mode: preview_mode,
                    }) {
                        fm.error = Some(error);
                    }
                    fm.dialogs.perm_target = None;
                }
                if cancel {
                    crate::ui::dialog::consume_handled_key(ui, egui::Key::Escape);
                    fm.dialogs.perm_target = None;
                }
            });
        if title_close || !open {
            fm.dialogs.perm_target = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_manager::FileManager;
    use egui_kittest::kittest::Queryable;

    fn entry(name: &str, is_dir: bool, size: u64) -> FileEntry {
        FileEntry {
            name: name.into(),
            is_dir,
            size,
            mtime: 1_752_700_000,
            permissions: if is_dir { 0o40755 } else { 0o100644 },
            uid: 0,
            gid: 0,
            owner: "root".into(),
            group: "root".into(),
            is_symlink: false,
            is_broken_symlink: false,
        }
    }

    #[test]
    fn rename_dialog_focuses_and_selects_name_then_confirms_on_enter() {
        let mut fm = FileManager::new(None);
        fm.cwd = "/tmp".into();
        fm.dialogs.rename_old = Some("/tmp/old.txt".into());
        fm.dialogs.rename_name = "old.txt".into();
        fm.dialogs.rename_focus = true;
        // 文件面板画在终端之前；用一个画在弹窗之后的探针模拟终端，
        // 检查回车是否已经被弹窗消费，而不是留给下层窗口再处理一次。
        let enter_leaked = std::rc::Rc::new(std::cell::Cell::new(false));
        let leaked = enter_leaked.clone();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(600.0, 320.0))
            .build_ui_state(
                move |ui, fm: &mut FileManager| {
                    fm_dialogs(ui, fm);
                    if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                        leaked.set(true);
                    }
                },
                fm,
            );
        harness.run_steps(2);

        let name_id = egui::Id::new("fm_rename_name");
        assert_eq!(
            harness.ctx.memory(|memory| memory.focused()),
            Some(name_id),
            "重命名弹窗打开后名称框必须自动获得焦点"
        );
        let state = egui::text_edit::TextEditState::load(&harness.ctx, name_id)
            .expect("rename TextEdit state");
        let range = state.cursor.char_range().expect("selection");
        let sorted = range.as_sorted_char_range();
        assert_eq!((sorted.start.0, sorted.end.0), (0, 7), "旧名称应被全选");

        // 输入替换选中的旧名称，再按回车确认。
        harness
            .input_mut()
            .events
            .push(egui::Event::Text("new.txt".into()));
        harness.step();
        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run();

        assert_eq!(harness.state().dialogs.rename_name, "new.txt");
        assert!(
            harness.state().dialogs.rename_old.is_none(),
            "回车应确认并关闭重命名弹窗"
        );
        assert!(
            !enter_leaked.get(),
            "回车确认弹窗后必须从本帧输入中消费掉，否则下层终端会再执行一次"
        );
    }

    #[test]
    fn delete_dialog_confirms_on_enter_and_blocks_terminal_keyboard() {
        let mut fm = FileManager::new(None);
        fm.cwd = "/tmp".into();
        fm.dialogs.delete_target = Some(("/tmp/obsolete".into(), false, false));
        let enter_leaked = std::rc::Rc::new(std::cell::Cell::new(false));
        let leaked = enter_leaked.clone();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(600.0, 320.0))
            .build_ui_state(
                move |ui, fm: &mut FileManager| {
                    fm_dialogs(ui, fm);
                    if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                        leaked.set(true);
                    }
                },
                fm,
            );
        harness.run_steps(2);
        assert!(
            crate::ui::dialog::modal_keyboard_blocked(&harness.ctx),
            "删除确认弹窗打开期间必须阻止终端接收键盘"
        );

        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run();
        assert!(
            harness.state().dialogs.delete_target.is_none(),
            "回车应确认并关闭删除弹窗"
        );
        assert!(
            !enter_leaked.get(),
            "回车确认后必须从本帧输入消费掉，不能留给下层终端"
        );

        // 取消键同样应关闭弹窗并消费。
        harness.state_mut().dialogs.delete_target = Some(("/tmp/obsolete".into(), false, false));
        harness.state_mut().dialogs.delete_armed = false;
        harness.step();
        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run();
        assert!(harness.state().dialogs.delete_target.is_none());
    }

    #[test]
    fn compact_dialog_title_bar_can_drag_window() {
        use egui_kittest::kittest::Queryable;
        let mut fm = FileManager::new(None);
        fm.cwd = "/tmp".into();
        fm.dialogs.mkdir_open = true;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 400.0))
            .build_ui_state(
                |ui, fm: &mut FileManager| {
                    fm_dialogs(ui, fm);
                },
                fm,
            );
        harness.run_steps(2);
        let before = harness.get_by_role(egui::accesskit::Role::Window).rect();
        let grab = egui::pos2(before.center().x, before.top() + 14.0);
        let target = grab + egui::vec2(30.0, 20.0);
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: grab,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        harness
            .input_mut()
            .events
            .push(egui::Event::PointerMoved(target));
        harness.step();
        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: target,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        let after = harness.get_by_role(egui::accesskit::Role::Window).rect();
        assert_ne!(after.min, before.min, "自定义标题栏仍应支持拖动窗口");
    }

    #[test]
    fn dialog_primary_and_cancel_buttons_share_width_and_order() {
        use egui_kittest::kittest::Queryable;
        let mut fm = FileManager::new(None);
        fm.cwd = "/tmp".into();
        fm.dialogs.mkdir_open = true;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 400.0))
            .build_ui_state(
                |ui, fm: &mut FileManager| {
                    fm_dialogs(ui, fm);
                },
                fm,
            );
        harness.run_steps(2);
        let primary = harness.get_by_label("创建").rect();
        let cancel = harness.get_by_label("取消").rect();
        assert!(
            (primary.width() - cancel.width()).abs() < 0.5,
            "主按钮与取消按钮宽度应一致: primary={primary:?}, cancel={cancel:?}"
        );
        assert!(
            primary.right() > cancel.right(),
            "主按钮应在取消按钮右侧（整行右对齐）"
        );
    }

    #[test]
    fn compact_dialog_title_bar_close_button_works() {
        use egui_kittest::kittest::Queryable;
        let mut fm = FileManager::new(None);
        fm.cwd = "/tmp".into();
        fm.dialogs.mkdir_open = true;
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 400.0))
            .build_ui_state(
                |ui, fm: &mut FileManager| {
                    fm_dialogs(ui, fm);
                },
                fm,
            );
        harness.run_steps(2);
        let window = harness.get_by_role(egui::accesskit::Role::Window).rect();
        // 标题栏左上角红色关闭灯的中心
        let close = egui::pos2(window.left() + 14.0, window.top() + 14.0);
        harness.hover_at(close);
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton {
                pos: close,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
            harness.step();
        }
        assert!(
            !harness.state().dialogs.mkdir_open,
            "自定义标题栏的关闭按钮应关闭弹窗"
        );
    }

    #[test]
    fn file_type_labels_distinguish_links_and_broken_links() {
        let mut item = entry("item", false, 0);
        assert_eq!(file_type_label(&item), "文件");
        item.is_dir = true;
        assert_eq!(file_type_label(&item), "文件夹");
        item.is_symlink = true;
        assert_eq!(file_type_label(&item), "目录链接");
        item.is_broken_symlink = true;
        assert_eq!(file_type_label(&item), "失效链接");
        item.is_broken_symlink = false;
        item.is_dir = false;
        assert_eq!(file_type_label(&item), "文件链接");
    }

    #[test]
    fn symlink_entry_is_shown_as_link_instead_of_file() {
        use egui_kittest::kittest::Queryable;
        let mut fm = FileManager::new(None);
        fm.cwd = "/tmp".into();
        let mut link = entry("current", false, 0);
        link.is_symlink = true;
        fm.entries = vec![link];
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 180.0))
            .build_ui_state(
                |ui, fm: &mut FileManager| {
                    fm_table(ui, fm, &TransferSettings::default());
                },
                fm,
            );
        harness.run();
        assert!(
            harness.query_by_label("文件链接").is_some(),
            "指向文件的符号链接类型列应显示为“文件链接”"
        );
        assert!(
            harness.query_by_label("文件").is_none(),
            "符号链接不应再被标记为普通文件"
        );
    }

    #[test]
    fn completed_transfer_shows_full_progress() {
        use crate::file_manager::Transfer;
        let mut transfer = Transfer::new(1, "upload", "/tmp/report.pdf".into(), 85, 100);
        assert!((transfer_fraction(&transfer) - 0.85).abs() < 0.01);
        transfer.finished = true;
        assert_eq!(transfer_fraction(&transfer), 1.0);

        transfer.error = Some("network error".into());
        assert!((transfer_fraction(&transfer) - 0.85).abs() < 0.01);
    }

    #[test]
    fn transfer_clear_button_hover_does_not_shift_transfer_list() {
        use crate::file_manager::{Transfer, TransferPos};
        let mut fm = FileManager::new(None);
        fm.transfers = vec![Transfer::new(
            1,
            "upload",
            "/tmp/report-2026.pdf".into(),
            37,
            100,
        )];
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 260.0))
            .build_ui_state(
                |ui, fm: &mut FileManager| {
                    fm_transfers(ui, fm, 240.0, TransferPos::Bottom);
                },
                fm,
            );
        harness.run_steps(2);

        let file_row = harness.get_by_label("report-2026.pdf").rect();
        let clear = harness.get_by_label("清除已完成").rect();
        harness.hover_at(clear.center());
        harness.step();
        let after = harness.get_by_label("report-2026.pdf").rect();
        assert!(
            (file_row.top() - after.top()).abs() < 0.5,
            "悬停“清除已完成”不应改变表头高度并顶动传输列表: before={file_row:?}, after={after:?}"
        );
    }

    #[test]
    fn file_name_text_uses_row_click_action() {
        let mut fm = FileManager::new(None);
        fm.cwd = "/tmp".into();
        fm.entries = vec![entry("notes.txt", false, 2048)];
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 180.0))
            .build_ui_state(
                |ui, fm: &mut FileManager| {
                    fm_table(ui, fm, &TransferSettings::default());
                },
                fm,
            );

        let pointer_pos = {
            let node = harness.get_by_label("notes.txt");
            node.hover();
            node.rect().center()
        };
        harness.step();
        assert_eq!(
            harness.output().platform_output.cursor_icon,
            egui::CursorIcon::Default
        );

        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: pointer_pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        });
        harness.run();
        assert_eq!(
            harness.output().platform_output.cursor_icon,
            egui::CursorIcon::Default
        );

        harness.input_mut().events.push(egui::Event::PointerButton {
            pos: pointer_pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        });
        harness.run();
        assert_eq!(harness.state().selected, Some(0));

        harness.input_mut().events.push(egui::Event::Key {
            key: egui::Key::C,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.step();
        assert!(
            harness
                .output()
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText("/tmp/notes.txt".into())),
            "active={}, commands={:?}",
            harness.state().table_keyboard_active,
            harness.output().platform_output.commands
        );
    }

    #[test]
    fn file_table_blank_area_context_menu_offers_create_actions() {
        let fm = FileManager::new(None);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 180.0))
            .build_ui_state(
                |ui, fm: &mut FileManager| {
                    fm_table_scroll(ui, fm, &TransferSettings::default());
                },
                fm,
            );
        harness.run();

        let blank_point = Pos2::new(350.0, 150.0);
        harness.hover_at(blank_point);
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton {
                pos: blank_point,
                button: egui::PointerButton::Secondary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        harness.step();

        assert!(harness.query_by_label("新建文件夹").is_some());
        assert!(harness.query_by_label("新建文件").is_some());
    }

    #[test]
    fn create_file_menu_action_opens_dialog() {
        let fm = FileManager::new(None);
        let mut harness = egui_kittest::Harness::new_ui_state(|ui, fm| fm_new_menu(ui, fm), fm);
        harness.get_by_label("新建文件").click();
        harness.step();
        assert!(harness.state().dialogs.create_file_open);
    }

    #[test]
    fn path_field_uses_arrow_cursor_while_remaining_editable() {
        let mut fm = FileManager::new(None);
        fm.path_edit = "/tmp".into();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(700.0, 80.0))
            .build_ui_state(
                |ui, fm: &mut FileManager| {
                    fm_toolbar(ui, fm, &TransferSettings::default());
                },
                fm,
            );

        let path_field = harness.get_by_role(egui::accesskit::Role::TextInput);
        path_field.hover();
        harness.run();

        assert_eq!(
            harness.output().platform_output.cursor_icon,
            egui::CursorIcon::Default
        );
        assert_eq!(
            harness
                .get_by_role(egui::accesskit::Role::TextInput)
                .value()
                .as_deref(),
            Some("/tmp")
        );
    }

    #[test]
    fn tree_hover_highlight_covers_the_full_row() {
        fn has_hover_rect(shape: &egui::Shape, color: Color32, point: Pos2) -> bool {
            match shape {
                egui::Shape::Rect(rect) => rect.fill == color && rect.rect.contains(point),
                egui::Shape::Vec(shapes) => shapes
                    .iter()
                    .any(|shape| has_hover_rect(shape, color, point)),
                _ => false,
            }
        }

        let fm = FileManager::new(None);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(320.0, 120.0))
            .build_ui_state(
                |ui, fm: &mut FileManager| {
                    fm_tree(ui, fm);
                },
                fm,
            );
        harness.run();

        let label_rect = harness.get_by_label("/").rect();
        harness.hover_at(label_rect.center());
        harness.run();

        let blank_row_point = Pos2::new(300.0, label_rect.center().y);
        let hover_fill = harness
            .ctx
            .style_of(harness.ctx.theme())
            .visuals
            .widgets
            .hovered
            .bg_fill;
        assert!(
            harness
                .output()
                .shapes
                .iter()
                .any(|clipped| { has_hover_rect(&clipped.shape, hover_fill, blank_row_point) }),
            "hover highlight should extend through the blank area at the end of the tree row"
        );
    }

    #[test]
    fn tree_blank_area_single_clicks_to_select_and_double_clicks_to_expand() {
        let mut fm = FileManager::new(None);
        fm.cwd = "/".into();
        fm.path_edit = "/".into();
        fm.tree_expanded.insert("/".into());
        fm.tree_children
            .insert("/".into(), vec![entry("srv", true, 0)]);
        fm.tree_children
            .insert("/srv".into(), vec![entry("app", true, 0)]);
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(320.0, 120.0))
            .with_step_dt(0.01)
            .build_ui_state(
                |ui, fm: &mut FileManager| {
                    fm_tree(ui, fm);
                },
                fm,
            );
        harness.run();

        let blank_point = Pos2::new(300.0, harness.get_by_label("srv").rect().center().y);
        harness.hover_at(blank_point);
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton {
                pos: blank_point,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        harness.step();
        assert_eq!(harness.state().cwd, "/srv");
        assert!(!harness.state().tree_expanded.contains("/srv"));

        // 相同位置的第二次点击组成双击；节点此时才展开。
        for pressed in [true, false] {
            harness.event(egui::Event::PointerButton {
                pos: blank_point,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            });
        }
        harness.step();
        assert!(harness.state().tree_expanded.contains("/srv"));
    }

    #[test]
    fn current_tree_directory_has_selection_background_and_scrolls_into_view() {
        fn has_selection_rect(shape: &egui::Shape, color: Color32, point: Pos2) -> bool {
            match shape {
                egui::Shape::Rect(rect) => rect.fill == color && rect.rect.contains(point),
                egui::Shape::Vec(shapes) => shapes
                    .iter()
                    .any(|shape| has_selection_rect(shape, color, point)),
                _ => false,
            }
        }

        let mut fm = FileManager::new(None);
        fm.cwd = "/srv".into();
        fm.path_edit = "/srv".into();
        fm.tree_children
            .insert("/".into(), vec![entry("srv", true, 0)]);
        fm.tree_children
            .insert("/srv".into(), vec![entry("logs", true, 0)]);
        fm.tree_expanded.insert("/".into());
        fm.tree_expanded.insert("/srv".into());
        fm.tree_scroll_to_cwd = true;

        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(320.0, 80.0))
            .build_ui_state(
                |ui, fm: &mut FileManager| {
                    egui::ScrollArea::vertical().show(ui, |ui| fm_tree(ui, fm));
                },
                fm,
            );
        harness.run();

        let selected_point = harness.get_by_label("srv").rect().center();
        let visuals = &harness.ctx.style_of(harness.ctx.theme()).visuals;
        let accent = visuals.selection.bg_fill;
        let selected_fill = Color32::from_rgba_unmultiplied(
            accent.r(),
            accent.g(),
            accent.b(),
            if visuals.dark_mode { 72 } else { 42 },
        );
        assert!(
            harness
                .output()
                .shapes
                .iter()
                .any(|clipped| has_selection_rect(&clipped.shape, selected_fill, selected_point)),
            "current tree directory should have a selection background"
        );
        assert!(
            !harness.state().tree_scroll_to_cwd,
            "scroll request should be consumed once the current row is present"
        );
    }

    /// 离屏渲染文件面板,输出 /tmp/rshell_fm.png 供人工检查布局
    #[test]
    #[ignore]
    fn render_file_panel() {
        let mut fm = FileManager::new(None);
        let root_dirs =
            ["bin", "boot", "dev", "etc", "home", "root", "usr", "var"].map(|n| entry(n, true, 0));
        fm.tree_children.insert("/".into(), root_dirs.to_vec());
        fm.tree_expanded.insert("/".into());
        fm.tree_expanded.insert("/root".into());
        fm.tree_children.insert(
            "/root".into(),
            vec![entry(".ssh", true, 0), entry("acme.sh", true, 0)],
        );
        fm.cwd = "/root".into();
        fm.path_edit = "/root".into();
        fm.entries = vec![
            entry(".ssh", true, 0),
            entry("acme.sh", true, 0),
            entry(".bashrc", false, 602),
            entry("frp_0.60.0_darwin_arm64.tar.gz", false, 12_582_912),
            entry("notes.txt", false, 2048),
        ];

        let mut harness = egui_kittest::Harness::new_ui_state(
            |ui, fm: &mut FileManager| {
                egui::Panel::bottom("fm").show(ui, |ui| {
                    fm_toolbar(ui, fm, &TransferSettings::default());
                    ui.separator();
                    let transfers_h = transfers_height(fm, TransferPos::Bottom);
                    let body_h = (ui.available_height() - transfers_h).max(40.0);
                    fm_body(ui, fm, body_h, &TransferSettings::default());
                    fm_transfers(ui, fm, transfers_h, TransferPos::Bottom);
                });
            },
            fm,
        );
        harness.set_size(egui::vec2(900.0, 320.0));
        harness.run();
        let img = harness.render().expect("render");
        img.save("/tmp/rshell_fm.png").expect("save png");
        eprintln!("saved /tmp/rshell_fm.png");
    }

    /// 验证面板高度不随文件数量变化:2 个文件 vs 200 个文件,面板顶边应一致
    #[test]
    #[ignore]
    fn render_panel_height_stability() {
        for (name, count) in [("few", 2usize), ("many", 200)] {
            let mut fm = FileManager::new(None);
            fm.tree_children
                .insert("/".into(), vec![entry("root", true, 0)]);
            fm.entries = (0..count)
                .map(|i| entry(&format!("file_{i:04}.txt"), false, 1024 * i as u64))
                .collect();
            let mut harness = egui_kittest::Harness::new_ui_state(
                |ui, fm: &mut FileManager| {
                    // 与应用一致的 panel 配置
                    egui::Panel::bottom("file_manager")
                        .resizable(true)
                        .default_size(300.0)
                        .size_range(120.0..=600.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label("文件 命令");
                            });
                            ui.separator();
                            fm_toolbar(ui, fm, &TransferSettings::default());
                            ui.separator();
                            let body_h = (ui.available_height()
                                - transfers_height(fm, TransferPos::Bottom))
                            .max(40.0);
                            fm_body(ui, fm, body_h, &TransferSettings::default());
                        });
                },
                fm,
            );
            harness.set_size(egui::vec2(900.0, 700.0));
            harness.run();
            let img = harness.render().expect("render");
            let path = format!("/tmp/rshell_fm_{name}.png");
            img.save(&path).expect("save png");
            eprintln!("saved {path}");
        }
    }
}
