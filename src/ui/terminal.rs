// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

use crate::app::RShellApp;
use crate::config::TerminalBackgroundKind;
use crate::ui::icons::{Icon, icon_button};

fn no_term_alloc_debug() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| std::env::var_os("RSHELL_NO_TERMALLOC").is_some())
}

fn no_cmd_bar_debug() -> bool {
    static VALUE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *VALUE.get_or_init(|| std::env::var_os("RSHELL_NO_CMDBAR").is_some())
}
use crate::term_view::TermBackground;
use egui::{Color32, RichText};

impl RShellApp {
    pub(crate) fn show_terminal(&mut self, ui: &mut egui::Ui) {
        let is_launcher = self.active_is_launcher();
        self.ensure_terminal_background_texture(ui.ctx());
        let configured_color = Color32::from_rgb(
            self.config.terminal_background.color[0],
            self.config.terminal_background.color[1],
            self.config.terminal_background.color[2],
        );
        let terminal_background = TermBackground {
            color: configured_color,
            image: if self.config.terminal_background.kind == TerminalBackgroundKind::Image {
                self.terminal_background_texture
                    .as_ref()
                    .map(|cached| cached.handle.clone())
            } else {
                None
            },
            image_mode: self.config.terminal_background.image_mode,
            image_darkness: self.config.terminal_background.image_darkness,
        };
        let terminal_font_size = self.config.ssh_terminal.font_size as f32;
        let right_click_paste = self.config.shortcuts.right_click_paste;
        let background = if is_launcher {
            ui.visuals().panel_fill
        } else {
            configured_color
        };
        egui::CentralPanel::default()
            .frame(egui::Frame::default().fill(background))
            .show(ui, |ui| {
                if is_launcher {
                    self.show_ssh_launcher(ui);
                    return;
                }
                // 系统信息页标签:整页渲染,无终端和命令条
                let is_sysinfo = matches!(
                    self.sessions.get(self.active).map(|s| s.kind),
                    Some(crate::app::SessionKind::SysInfo(_))
                );
                if is_sysinfo {
                    self.show_sysinfo_page(ui);
                    return;
                }
                // 底部固定高度留给命令输入条,其余全给终端
                let bar_h = 30.0;
                let term_h = (ui.available_height() - bar_h).max(0.0);
                let w = ui.available_width();

                if !no_term_alloc_debug() {
                    ui.allocate_ui(egui::vec2(w, term_h), |ui| {
                        let idx = self.active;
                        if let Some(session) = self.sessions.get_mut(idx) {
                            match session.term.as_mut() {
                                Some(term) => term.show(
                                    ui,
                                    &terminal_background,
                                    terminal_font_size,
                                    right_click_paste,
                                ),
                                None => {
                                    let msg = session
                                        .error
                                        .clone()
                                        .unwrap_or_else(|| "会话不可用".into());
                                    ui.centered_and_justified(|ui| {
                                        ui.label(
                                            RichText::new(msg)
                                                .color(Color32::from_rgb(0xE0, 0x6C, 0x75)),
                                        );
                                    });
                                }
                            }
                        } else {
                            ui.centered_and_justified(|ui| {
                                ui.label(
                                    RichText::new("＋ 新建 SSH 连接或本机终端")
                                        .color(Color32::from_rgb(0x8F, 0xA8, 0xBF)),
                                );
                            });
                        }
                    });
                }

                if !no_cmd_bar_debug() {
                    self.show_cmd_bar(ui);
                }
            });
    }

    fn ensure_terminal_background_texture(&mut self, ctx: &egui::Context) {
        let config = &self.config.terminal_background;
        if config.kind != TerminalBackgroundKind::Image || config.image_path.trim().is_empty() {
            self.terminal_background_texture = None;
            self.terminal_background_load_attempt = None;
            self.terminal_background_error = None;
            return;
        }

        let path = config.image_path.trim().to_owned();
        if self.terminal_background_load_attempt.as_deref() != Some(path.as_str()) {
            self.terminal_background_rx = None;
        }
        if let Some(rx) = &self.terminal_background_rx {
            match rx.try_recv() {
                Ok((loaded_path, result)) => {
                    self.terminal_background_rx = None;
                    if loaded_path == path {
                        match result {
                            Ok(image) => {
                                let handle = ctx.load_texture(
                                    format!("terminal-background:{path}"),
                                    image,
                                    egui::TextureOptions::LINEAR,
                                );
                                self.terminal_background_texture =
                                    Some(crate::app::TerminalBackgroundTexture {
                                        path: path.clone(),
                                        handle,
                                    });
                                self.terminal_background_error = None;
                            }
                            Err(error) => self.terminal_background_error = Some(error),
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.terminal_background_rx = None;
                    self.terminal_background_error = Some("背景图片加载线程意外退出".to_string());
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if self
            .terminal_background_texture
            .as_ref()
            .is_some_and(|cached| cached.path == path)
            || self.terminal_background_load_attempt.as_deref() == Some(path.as_str())
        {
            return;
        }

        self.terminal_background_texture = None;
        self.terminal_background_load_attempt = Some(path.clone());
        self.terminal_background_error = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.terminal_background_rx = Some(rx);
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| {
                let bytes =
                    std::fs::read(&path).map_err(|error| format!("读取背景图片失败：{error}"))?;
                let decoded = image::load_from_memory(&bytes)
                    .map_err(|error| format!("解析背景图片失败：{error}"))?;
                let rgba = decoded.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                Ok(egui::ColorImage::from_rgba_unmultiplied(
                    size,
                    rgba.as_raw(),
                ))
            })();
            let _ = tx.send((path, result));
            repaint.request_repaint();
        });
    }

    /// 命令输入条:回车发送到当前终端,↑↓ 翻阅历史,历史按钮弹出菜单
    fn show_cmd_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            // 有弹窗时命令输入条也不能接收按键：它通常画在弹窗之后，
            // 保留焦点时回车会把命令直接发进 shell。
            let modal_blocked = crate::ui::dialog::modal_keyboard_blocked(ui.ctx());
            // 右侧固定控件(历史/设置/选项/连接图标)预留宽度。
            // TextEdit 用 desired_width(INFINITY) 会把后面的控件挤出可视区
            let reserved = 194.0;
            let input_w = (ui.available_width() - reserved).max(80.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.cmd_input)
                    .hint_text("命令输入(回车发送,↑↓ 历史)")
                    .desired_width(input_w)
                    .interactive(!modal_blocked),
            );

            // ↑↓ 翻阅命令历史
            if !modal_blocked && resp.has_focus() {
                let up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
                let down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));
                if up && !self.cmd_history.is_empty() {
                    history_up(
                        &mut self.cmd_input,
                        &self.cmd_history,
                        &mut self.cmd_hist_idx,
                        &mut self.cmd_history_draft,
                    );
                } else if down && self.cmd_hist_idx.is_some() {
                    history_down(
                        &mut self.cmd_input,
                        &self.cmd_history,
                        &mut self.cmd_hist_idx,
                        &mut self.cmd_history_draft,
                    );
                }
            }

            // 回车:发送到当前会话终端
            if !modal_blocked && resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
            {
                let cmd = std::mem::take(&mut self.cmd_input);
                if !cmd.is_empty() {
                    self.push_cmd_history(&cmd);
                    if let Some(session) = self.sessions.get_mut(self.active) {
                        session.send_str(&cmd);
                        session.send_str("\r");
                    }
                }
            }

            // 历史菜单
            ui.menu_button("历史", |ui| {
                crate::ui::dialog::mark_modal(ui.ctx());
                if self.cmd_history.is_empty() {
                    ui.label(RichText::new("（空）").weak());
                }
                let mut picked: Option<String> = None;
                for h in self.cmd_history.iter().rev().take(20) {
                    if ui.button(h).clicked() {
                        picked = Some(h.clone());
                        ui.close();
                    }
                }
                if let Some(h) = picked {
                    self.cmd_input = h;
                    self.cmd_hist_idx = None;
                    self.cmd_history_draft = None;
                }
                ui.separator();
                if ui.button("清空历史").clicked() {
                    self.cmd_history.clear();
                    self.cmd_hist_idx = None;
                    self.cmd_history_draft = None;
                    ui.close();
                }
            });
            // 设置齿轮:编辑当前会话的连接信息(仅 SSH 会话可用)
            {
                let editable = self
                    .sessions
                    .get(self.active)
                    .is_some_and(|s| s.kind == crate::app::SessionKind::Ssh);
                let resp = icon_button(
                    ui,
                    Icon::Settings,
                    if editable {
                        "连接设置"
                    } else {
                        "本机会话无连接设置"
                    },
                    egui::vec2(22.0, 20.0),
                    15.0,
                    if editable {
                        egui::Color32::WHITE
                    } else {
                        ui.visuals().weak_text_color()
                    },
                );
                if resp.clicked() && editable {
                    self.open_session_edit();
                }
            }
            // 重连/断开:连接中显示电源图标(点击断开),
            // 断开/失败后显示循环箭头(点击重连)
            if let Some(session) = self.sessions.get(self.active) {
                let connected = session.is_connected();
                let (icon, tip, color) = if connected {
                    (Icon::Power, "断开连接", Color32::from_rgb(0xE8, 0x4C, 0x3D))
                } else {
                    (
                        Icon::RefreshCw,
                        "重新连接",
                        Color32::from_rgb(0x35, 0xC4, 0x50),
                    )
                };
                if icon_button(ui, icon, tip, egui::vec2(22.0, 20.0), 15.0, color).clicked() {
                    let ctx = ui.ctx().clone();
                    let session = &mut self.sessions[self.active];
                    if connected {
                        session.disconnect();
                    } else {
                        session.reconnect(&ctx);
                    }
                }
            }
            // 文件窗展开/收起。这里只改变 egui 的区域划分，不发送原生窗口
            // resize；隐藏后 CentralPanel 会自然扩展到主窗口底部。
            let (file_panel_icon, file_panel_tip) = if self.file_manager_visible {
                (Icon::ChevronDown, "收起文件窗")
            } else {
                (Icon::ChevronUp, "展开文件窗")
            };
            if icon_button(
                ui,
                file_panel_icon,
                file_panel_tip,
                egui::vec2(22.0, 20.0),
                15.0,
                ui.visuals().text_color(),
            )
            .clicked()
            {
                self.file_manager_visible = !self.file_manager_visible;
                if let Some(term) = self
                    .sessions
                    .get_mut(self.active)
                    .and_then(|session| session.term.as_mut())
                {
                    term.request_focus();
                }
            }
        });
    }
}

fn history_up(
    input: &mut String,
    history: &[String],
    index: &mut Option<usize>,
    draft: &mut Option<String>,
) {
    if history.is_empty() {
        return;
    }
    if index.is_none() {
        *draft = Some(input.clone());
    }
    let next = index.map_or(history.len() - 1, |current| current.saturating_sub(1));
    *index = Some(next);
    *input = history[next].clone();
}

fn history_down(
    input: &mut String,
    history: &[String],
    index: &mut Option<usize>,
    draft: &mut Option<String>,
) {
    let Some(current) = *index else {
        return;
    };
    if current + 1 < history.len() {
        *index = Some(current + 1);
        *input = history[current + 1].clone();
    } else {
        *index = None;
        *input = draft.take().unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::{history_down, history_up};

    #[test]
    fn history_restores_draft_and_down_is_noop_when_idle() {
        let history = vec!["one".to_string(), "two".to_string()];
        let mut input = "draft".to_string();
        let mut index = None;
        let mut draft = None;

        history_down(&mut input, &history, &mut index, &mut draft);
        assert_eq!(input, "draft");

        history_up(&mut input, &history, &mut index, &mut draft);
        assert_eq!(input, "two");
        history_up(&mut input, &history, &mut index, &mut draft);
        assert_eq!(input, "one");
        history_down(&mut input, &history, &mut index, &mut draft);
        assert_eq!(input, "two");
        history_down(&mut input, &history, &mut index, &mut draft);
        assert_eq!(input, "draft");
        assert!(index.is_none());
    }
}
