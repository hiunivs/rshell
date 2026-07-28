use crate::app::{BottomTab, RShellApp};
use crate::file_manager::{FileManager, SortCol, TransferPos};
use crate::session::files::{FileEntry, SftpCmd, join_path, normalize_path};
use crate::ui::sidebar::fmt_bytes;
use egui::{Color32, Pos2, RichText, Stroke, vec2};

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
                            fm_toolbar(ui, fm);
                            ui.separator();
                            // 关键:为传输栏 + 底栏预留高度,
                            // 主体区域用显式矩形填满,内容始终占满面板
                            let btn_h = 24.0; // 底栏:右下角按钮
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
                                let body_w = (ui.available_width() - trans_w - 4.0).max(150.0);
                                let content_h = (ui.available_height() - btn_h).max(60.0);
                                let body_rect = egui::Rect::from_min_size(
                                    ui.next_widget_position(),
                                    vec2(body_w, content_h),
                                );
                                let tree_w =
                                    fm.tree_width.clamp(100.0, (body_w - 220.0).max(100.0));
                                let _ = ui.allocate_rect(body_rect, egui::Sense::hover());
                                ui.scope_builder(
                                    egui::UiBuilder::new().max_rect(body_rect),
                                    |ui| fm_tree_and_table(ui, fm, tree_w, body_rect),
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
                                let _ = ui.allocate_rect(trans_rect, egui::Sense::hover());
                                ui.scope_builder(
                                    egui::UiBuilder::new()
                                        .max_rect(trans_rect)
                                        .layout(egui::Layout::top_down(egui::Align::LEFT)),
                                    |ui| {
                                        fm_transfers(ui, fm, content_h, TransferPos::Right);
                                    },
                                );
                            } else {
                                // 底部布局(默认)
                                let body_h =
                                    (ui.available_height() - transfers_h - btn_h).max(40.0);
                                fm_body(ui, fm, body_h);
                                if transfers_visible {
                                    fm_transfers(ui, fm, transfers_h, TransferPos::Bottom);
                                }
                            }
                            // === 底栏:右下角按钮 ===
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if transfer_pane_toggle(ui, fm.show_transfers_pane).clicked() {
                                        fm.show_transfers_pane = !fm.show_transfers_pane;
                                    }
                                    // 汉堡按钮嵌在 menu_button 里:弹窗自动贴按钮定位,不占布局
                                    ui.menu_button(RichText::new("≡"), |ui| {
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
                                            .selectable_label(
                                                fm.transfer_pos == TransferPos::Right,
                                                "右侧",
                                            )
                                            .clicked()
                                        {
                                            fm.transfer_pos = TransferPos::Right;
                                            ui.close();
                                        }
                                    });
                                },
                            );
                            fm_dialogs(ui, fm);
                            fm_drag_drop(ui, fm);
                            // 记下传输状态:外层在 Panel 返回后修正高度
                            self.fm_has_transfers = transfers_visible;
                        } else {
                            self.fm_has_transfers = false;
                            fill_rect(ui, |ui| {
                                ui.label("（无会话）");
                            });
                        }
                    }
                    BottomTab::Commands => {
                        // 常用命令:点击直接发送到当前终端;同样显式填满,保持面板高度一致
                        self.fm_has_transfers = false;
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
        egui::Window::new("新建常用命令")
            .id(egui::Id::new("common_command_new"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(ctx))
            .show(ctx, |ui| {
                crate::ui::dialog::polish(ui);
                ui.set_width(360.0);
                egui::Grid::new("common_command_new_grid")
                    .num_columns(2)
                    .spacing([10.0, 9.0])
                    .show(ui, |ui| {
                        ui.label("名称");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.command_new_name)
                                .desired_width(290.0)
                                .hint_text("例如：查看 Docker 容器"),
                        );
                        ui.end_row();
                        ui.label("命令");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.command_new_value)
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
                ui.horizontal(|ui| {
                    let action_size = vec2(56.0, 28.0);
                    if crate::ui::dialog::compact_primary_button(ui, "保存").clicked()
                        && self.save_common_command()
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
        if close {
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

fn fm_toolbar(ui: &mut egui::Ui, fm: &mut FileManager) {
    ui.horizontal(|ui| {
        // 路径框:缩短,给右侧按钮 + 加载提示预留固定宽度
        let resp = ui.add(
            egui::TextEdit::singleline(&mut fm.path_edit)
                .desired_width((ui.available_width() - 360.0).max(60.0)),
        );
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
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
        if ui.button("↑").on_hover_text("上级目录").clicked() {
            fm.go_parent();
        }
        if ui.button("⟳").on_hover_text("刷新").clicked() {
            fm.refresh();
        }
        if ui.button("新建").on_hover_text("新建文件夹").clicked() {
            fm.dialogs.mkdir_open = true;
            fm.dialogs.mkdir_name.clear();
        }
        if ui.button("⇧ 上传").clicked()
            && let Some(path) = rfd::FileDialog::new().pick_file()
        {
            fm.start_upload(path.to_string_lossy().into_owned());
        }
        // 下载选中项(文件/目录自动检测)
        let sel_entry = fm.selected.and_then(|i| fm.entries.get(i)).cloned();
        if ui.button("⇩ 下载").clicked() {
            if let Some(entry) = sel_entry {
                if entry.is_dir {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        fm.start_download_dir(&entry, path.to_string_lossy().into_owned());
                    }
                } else if let Some(path) = rfd::FileDialog::new()
                    .set_file_name(&entry.name)
                    .save_file()
                {
                    fm.start_download(&entry, path.to_string_lossy().into_owned());
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
        44.0 + fm.transfers.len() as f32 * row_h
    }
}

fn fm_body(ui: &mut egui::Ui, fm: &mut FileManager, height: f32) {
    let tree_w = fm
        .tree_width
        .clamp(100.0, (ui.available_width() - 220.0).max(100.0));
    let body_rect = egui::Rect::from_min_size(
        ui.next_widget_position(),
        vec2(ui.available_width(), height),
    );
    let _ = ui.allocate_rect(body_rect, egui::Sense::hover());
    ui.scope_builder(egui::UiBuilder::new().max_rect(body_rect), |ui| {
        fm_tree_and_table(ui, fm, tree_w, body_rect)
    });
}

/// 目录树 + 文件表的渲染内容(外层已分配矩形,此函数只画内容)
fn fm_tree_and_table(ui: &mut egui::Ui, fm: &mut FileManager, tree_w: f32, body_rect: egui::Rect) {
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
            egui::ScrollArea::vertical()
                .id_salt("fm_table_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    fm_table(ui, fm);
                });
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
        painter.rect_filled(
            banner,
            0.0,
            Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, 0xDC),
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

/// 目录树的可见行(深度优先展开)
fn tree_rows(fm: &FileManager) -> Vec<(String, usize, bool, bool)> {
    // (路径, 深度, 已展开, 加载中)
    let mut rows = Vec::new();
    let mut stack = vec![("/".to_string(), 0usize)];
    while let Some((path, depth)) = stack.pop() {
        let expanded = fm.tree_expanded.contains(&path);
        let loading = fm.tree_loading.contains(&path);
        rows.push((path.clone(), depth, expanded, loading));
        if expanded
            && depth < 16
            && let Some(children) = fm.tree_children.get(&path)
        {
            for c in children.iter().rev() {
                stack.push((c.full_path(&path), depth + 1));
            }
        }
    }
    rows
}

fn fm_tree(ui: &mut egui::Ui, fm: &mut FileManager) {
    let rows = tree_rows(fm);
    for (path, depth, expanded, loading) in rows {
        let is_cwd = normalize_path(&path) == fm.cwd;
        // 子项已加载(展开过/已取回子目录)→ 蓝色;否则灰蓝色
        let loaded = fm.tree_children.contains_key(&path);
        ui.horizontal(|ui| {
            ui.add_space(4.0 + depth as f32 * 14.0);
            // 未展开时不显示收起箭头；展开后才显示 ▼，固定占位保持目录对齐。
            if expanded {
                let collapse = ui
                    .add(
                        egui::Button::new(RichText::new("▼").size(9.0))
                            .frame(false)
                            .min_size(vec2(12.0, 14.0)),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .on_hover_text("收起");
                if collapse.clicked() {
                    fm.toggle_tree(&path);
                }
            } else {
                ui.allocate_exact_size(vec2(12.0, 14.0), egui::Sense::hover());
            }
            // 文件夹图标:已加载=蓝色,未加载=灰蓝色(与右侧文件表同款)
            let (icon_rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::hover());
            let color = if loaded {
                Color32::from_rgb(0x4C, 0x9B, 0xE8)
            } else {
                Color32::from_rgb(0x9A, 0xB0, 0xC8)
            };
            draw_folder_icon(&ui.painter_at(icon_rect), icon_rect, color);
            let text = if path == "/" {
                "/".to_string()
            } else {
                path.rsplit('/').next().unwrap_or(&path).to_string()
            };
            let label = if is_cwd {
                RichText::new(text).strong()
            } else {
                RichText::new(text)
            };
            if ui
                .add(
                    egui::Label::new(label)
                        .sense(egui::Sense::click())
                        .truncate(),
                )
                .on_hover_cursor(egui::CursorIcon::Default)
                .on_hover_text(&path)
                .clicked()
            {
                // 点击目录名:展开该节点并进入该目录
                fm.expand_tree(&path);
                fm.navigate(&path);
            }
            if loading {
                ui.label(RichText::new("…").weak().small());
            }
        });
    }
}

// ---------------------------------------------------------------------------
// 文件表格
// ---------------------------------------------------------------------------

fn fm_table(ui: &mut egui::Ui, fm: &mut FileManager) {
    use egui_extras::{Column, TableBuilder};

    // 注意:错误/加载提示不在这里加控件——表格区域是固定矩形,
    // 多出的控件会把 Panel 内容高度顶出去,触发面板高度回弹(整体往上抬);
    // 提示由 fm_body 用 painter 覆盖层绘制,不占布局空间

    let row_h = 20.0;
    TableBuilder::new(ui)
        .striped(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(Column::remainder().at_least(140.0)) // 文件名
        .column(Column::exact(64.0)) // 大小
        .column(Column::exact(52.0)) // 类型
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
                row.set_selected(selected);

                row.col(|ui| {
                    draw_file_icon(ui, &entry);
                    ui.add(egui::Label::new(&entry.name).truncate());
                });
                row.col(|ui| {
                    ui.label(if entry.is_dir {
                        String::new()
                    } else {
                        fmt_bytes(entry.size)
                    });
                });
                row.col(|ui| {
                    ui.label(if entry.is_dir { "文件夹" } else { "文件" });
                });
                row.col(|ui| {
                    ui.label(entry.mtime_string());
                });
                row.col(|ui| {
                    ui.label(entry.perm_string());
                });
                row.col(|ui| {
                    let owner = if entry.owner.is_empty() {
                        format!("{}/{}", entry.uid, entry.gid)
                    } else {
                        format!("{}/{}", entry.owner, entry.group)
                    };
                    ui.label(owner);
                });

                let resp = row.response();
                if resp.clicked() {
                    fm.selected = Some(i);
                }
                if resp.double_clicked() {
                    if entry.is_dir {
                        let p = entry.full_path(&fm.cwd);
                        fm.navigate(&p);
                    } else if let Some(path) = rfd::FileDialog::new()
                        .set_file_name(&entry.name)
                        .save_file()
                    {
                        fm.start_download(&entry, path.to_string_lossy().into_owned());
                    }
                }
                resp.context_menu(|ui| {
                    if entry.is_dir && ui.button("打开").clicked() {
                        let p = entry.full_path(&fm.cwd);
                        fm.navigate(&p);
                        ui.close();
                    }
                    if ui.button("下载").clicked() {
                        if entry.is_dir {
                            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                fm.start_download_dir(&entry, path.to_string_lossy().into_owned());
                            }
                        } else if let Some(path) = rfd::FileDialog::new()
                            .set_file_name(&entry.name)
                            .save_file()
                        {
                            fm.start_download(&entry, path.to_string_lossy().into_owned());
                        }
                        ui.close();
                    }
                    if ui.button("重命名").clicked() {
                        fm.dialogs.rename_old = Some(entry.full_path(&fm.cwd));
                        fm.dialogs.rename_name = entry.name.clone();
                        ui.close();
                    }
                    if ui.button("复制路径").clicked() {
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
                        fm.dialogs.delete_target =
                            Some((entry.full_path(&fm.cwd), entry.is_dir, false));
                        ui.close();
                    }
                    if entry.is_dir
                        && ui
                            .button(
                                RichText::new("递归删除")
                                    .color(Color32::from_rgb(0xD8, 0x3A, 0x3A)),
                            )
                            .clicked()
                    {
                        fm.dialogs.delete_target = Some((entry.full_path(&fm.cwd), true, true));
                        ui.close();
                    }
                });
            });
        });
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

/// 文件夹图标(主体 + 顶部 tab),颜色由调用方给定
pub(crate) fn draw_folder_icon(painter: &egui::Painter, rect: egui::Rect, c: Color32) {
    let body = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.top() + 3.0),
        rect.right_bottom(),
    );
    let tab = egui::Rect::from_min_size(egui::pos2(rect.left(), rect.top()), vec2(6.0, 4.0));
    painter.rect_filled(body, 1.5, c);
    painter.rect_filled(tab, 1.0, c);
}

/// 小图标:文件夹蓝色/文件灰色
fn draw_file_icon(ui: &mut egui::Ui, entry: &FileEntry) {
    let (rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    if entry.is_dir {
        draw_folder_icon(&p, rect, Color32::from_rgb(0x4C, 0x9B, 0xE8));
    } else {
        let body = egui::Rect::from_min_max(
            egui::pos2(rect.left() + 1.0, rect.top()),
            egui::pos2(rect.right() - 1.0, rect.bottom()),
        );
        p.rect_filled(body, 1.0, Color32::from_rgb(0xC8, 0xD0, 0xD8));
    }
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
            format!("⇧ 松开鼠标,上传 {n_hovered} 个文件到 {}", fm.cwd),
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
                if ui.small_button("清除已完成").clicked() {
                    clear_finished = true;
                }
            });
            for t in &fm.transfers {
                if compact {
                    // 右侧窄窗:文件名 + 细条进度
                    ui.horizontal(|ui| {
                        ui.label(match t.op {
                            "download" | "download_dir" => "⇩",
                            _ => "⇧",
                        });
                        ui.add(
                            egui::Label::new(RichText::new(short_name(&t.path)).small()).truncate(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(if let Some(err) = &t.error {
                                RichText::new(format!("失败:{err}"))
                                    .color(Color32::from_rgb(0xD8, 0x3A, 0x3A))
                                    .small()
                            } else if t.finished {
                                RichText::new("完成")
                                    .color(Color32::from_rgb(0x35, 0xA8, 0x60))
                                    .small()
                            } else {
                                RichText::new(format!(
                                    "{}/{}",
                                    fmt_bytes(t.done),
                                    fmt_bytes(t.total)
                                ))
                                .small()
                            });
                        });
                    });
                    if !t.finished && t.total > 0 {
                        let frac = (t.done as f32 / t.total as f32).clamp(0.0, 1.0);
                        let bar_h = 3.0;
                        let (rect, _) = ui.allocate_exact_size(
                            vec2(ui.available_width(), bar_h),
                            egui::Sense::hover(),
                        );
                        ui.painter()
                            .rect_filled(rect, 0.0, Color32::from_rgb(0xDD, 0xDD, 0xD8));
                        let fill_w = rect.width() * frac;
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(rect.min, vec2(fill_w, bar_h)),
                            0.0,
                            Color32::from_rgb(0x4C, 0x9B, 0xE8),
                        );
                    }
                } else {
                    // 底部宽窗:横向 ProgressBar
                    ui.horizontal(|ui| {
                        ui.label(match t.op {
                            "download" | "download_dir" => "⇩",
                            _ => "⇧",
                        });
                        ui.add_sized(
                            [200.0, 16.0],
                            egui::Label::new(short_name(&t.path)).truncate(),
                        );
                        let frac = if t.total > 0 {
                            (t.done as f32 / t.total as f32).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        if !t.finished {
                            ui.add_sized(
                                [140.0, 12.0],
                                egui::ProgressBar::new(frac).show_percentage(),
                            );
                        }
                        ui.label(if let Some(err) = &t.error {
                            RichText::new(format!("失败:{err}"))
                                .color(Color32::from_rgb(0xD8, 0x3A, 0x3A))
                        } else if t.finished {
                            RichText::new("完成").color(Color32::from_rgb(0x35, 0xA8, 0x60))
                        } else {
                            RichText::new(format!("{}/{}", fmt_bytes(t.done), fmt_bytes(t.total)))
                        });
                    });
                }
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

// ---------------------------------------------------------------------------
// 弹窗:新建文件夹 / 重命名 / 删除确认
// ---------------------------------------------------------------------------

fn fm_dialogs(ui: &mut egui::Ui, fm: &mut FileManager) {
    let ctx = ui.ctx().clone();

    // 新建文件夹
    if fm.dialogs.mkdir_open {
        let mut open = true;
        egui::Window::new("新建文件夹")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(&ctx))
            .show(&ctx, |ui| {
                crate::ui::dialog::polish(ui);
                ui.horizontal(|ui| {
                    ui.label("名称:");
                    ui.text_edit_singleline(&mut fm.dialogs.mkdir_name);
                });
                ui.horizontal(|ui| {
                    let name_ok = !fm.dialogs.mkdir_name.trim().is_empty();
                    if crate::ui::dialog::primary_button_enabled(ui, name_ok, "创建").clicked() {
                        let path = join_path(&fm.cwd, fm.dialogs.mkdir_name.trim());
                        if let Err(error) = fm.send(SftpCmd::Mkdir(path)) {
                            fm.error = Some(error);
                        }
                        fm.dialogs.mkdir_open = false;
                    }
                    if ui.button("取消").clicked() {
                        fm.dialogs.mkdir_open = false;
                    }
                });
            });
        if !open {
            fm.dialogs.mkdir_open = false;
        }
    }

    // 重命名
    if let Some(old_path) = fm.dialogs.rename_old.clone() {
        let mut open = true;
        egui::Window::new("重命名")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(&ctx))
            .show(&ctx, |ui| {
                crate::ui::dialog::polish(ui);
                ui.label(format!("原路径:{old_path}"));
                ui.horizontal(|ui| {
                    ui.label("新名称:");
                    ui.text_edit_singleline(&mut fm.dialogs.rename_name);
                });
                ui.horizontal(|ui| {
                    if crate::ui::dialog::primary_button(ui, "确定").clicked() {
                        let new_path = join_path(&fm.cwd, fm.dialogs.rename_name.trim());
                        if let Err(error) = fm.send(SftpCmd::Rename {
                            from: old_path.clone(),
                            to: new_path,
                        }) {
                            fm.error = Some(error);
                        }
                        fm.dialogs.rename_old = None;
                    }
                    if ui.button("取消").clicked() {
                        fm.dialogs.rename_old = None;
                    }
                });
            });
        if !open {
            fm.dialogs.rename_old = None;
        }
    }

    // 删除确认
    if let Some((path, is_dir, recursive)) = fm.dialogs.delete_target.clone() {
        let mut open = true;
        egui::Window::new("确认删除")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(&ctx))
            .show(&ctx, |ui| {
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
                ui.horizontal(|ui| {
                    if crate::ui::dialog::destructive_button(ui, "删除").clicked() {
                        if recursive {
                            if let Err(error) = fm.send(SftpCmd::RemoveRecursive { path }) {
                                fm.error = Some(error);
                            }
                        } else {
                            if let Err(error) = fm.send(SftpCmd::Remove { path, is_dir }) {
                                fm.error = Some(error);
                            }
                        }
                        fm.dialogs.delete_target = None;
                    }
                    if ui.button("取消").clicked() {
                        fm.dialogs.delete_target = None;
                    }
                });
            });
        if !open {
            fm.dialogs.delete_target = None;
        }
    }

    // 权限设置
    if let Some(path) = fm.dialogs.perm_target.clone() {
        let mut open = true;
        egui::Window::new("设置权限")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(&ctx))
            .show(&ctx, |ui| {
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
                ui.horizontal(|ui| {
                    if crate::ui::dialog::primary_button(ui, "应用").clicked() {
                        if let Err(error) = fm.send(SftpCmd::SetPermissions {
                            path,
                            mode: preview_mode,
                        }) {
                            fm.error = Some(error);
                        }
                        fm.dialogs.perm_target = None;
                    }
                    if ui.button("取消").clicked() {
                        fm.dialogs.perm_target = None;
                    }
                });
            });
        if !open {
            fm.dialogs.perm_target = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_manager::FileManager;

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
        }
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
                    fm_toolbar(ui, fm);
                    ui.separator();
                    let transfers_h = transfers_height(fm, TransferPos::Bottom);
                    let body_h = (ui.available_height() - transfers_h).max(40.0);
                    fm_body(ui, fm, body_h);
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
                            fm_toolbar(ui, fm);
                            ui.separator();
                            let body_h = (ui.available_height()
                                - transfers_height(fm, TransferPos::Bottom))
                            .max(40.0);
                            fm_body(ui, fm, body_h);
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
