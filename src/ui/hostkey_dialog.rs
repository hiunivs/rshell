// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

use crate::app::RShellApp;
use crate::known_hosts::HostKeyStatus;
use crate::session::HostKeyVerdict;
use egui::{Color32, RichText};

impl RShellApp {
    /// 主机指纹确认弹窗(TOFU):任一 session 有待确认请求就弹出
    pub(crate) fn show_hostkey_dialog(&mut self, ui: &mut egui::Ui) {
        // 找到第一个待确认的 session(避免借用冲突,先取索引)
        let idx = self
            .sessions
            .iter()
            .position(|s| s.pending_hostkey.is_some());
        let Some(idx) = idx else { return };

        let ctx = ui.ctx().clone();
        let mut verdict: Option<HostKeyVerdict> = None;
        let prompt = self.sessions[idx].pending_hostkey.as_ref().unwrap();

        let (title, warning) = match &prompt.status {
            HostKeyStatus::New => (
                "新主机指纹".to_string(),
                format!(
                    "首次连接 {}:{}。请确认服务器指纹:\n(可登录服务器执行 ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub 核对)",
                    prompt.host, prompt.port
                ),
            ),
            HostKeyStatus::Changed { old_fingerprint } => (
                "⚠ 主机指纹已变更".to_string(),
                format!(
                    "警告:{}:{} 的主机密钥与记录不一致!\n可能遭遇中间人攻击,也可能是服务器重装或更换了密钥。\n\n旧指纹:{}",
                    prompt.host, prompt.port, old_fingerprint
                ),
            ),
            HostKeyStatus::Match => unreachable!(),
        };
        let is_changed = matches!(prompt.status, HostKeyStatus::Changed { .. });

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            // 初始居中但可拖动:anchor 会把 movable 置 false
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(&ctx))
            .show(&ctx, |ui| {
                crate::ui::dialog::polish(ui);
                if is_changed {
                    ui.colored_label(Color32::from_rgb(0xD8, 0x3A, 0x3A), &warning);
                } else {
                    ui.label(&warning);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label("算法:");
                    ui.label(RichText::new(&prompt.algorithm).strong());
                });
                ui.horizontal(|ui| {
                    ui.label("指纹:");
                    ui.label(RichText::new(&prompt.fingerprint).monospace().strong());
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if crate::ui::dialog::primary_button(ui, "接受并保存").clicked() {
                        verdict = Some(HostKeyVerdict::AcceptAndSave);
                    }
                    if ui.button("仅本次接受").clicked() {
                        verdict = Some(HostKeyVerdict::AcceptOnce);
                    }
                    if crate::ui::dialog::destructive_button(ui, "拒绝").clicked() {
                        verdict = Some(HostKeyVerdict::Reject);
                    }
                });
            });

        if let Some(v) = verdict
            && let Some(prompt) = self.sessions[idx].pending_hostkey.take()
        {
            let _ = prompt.reply.send(v);
        }
    }

    pub(crate) fn show_keyboard_interactive_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(index) = self
            .sessions
            .iter()
            .position(|session| session.pending_keyboard_interactive.is_some())
        else {
            return;
        };
        let ctx = ui.ctx().clone();
        let mut submit = false;
        let mut cancel = false;
        {
            let prompt = self.sessions[index]
                .pending_keyboard_interactive
                .as_mut()
                .expect("interactive prompt exists");
            // 弹窗刚出现的第一帧不接收回车：避免唤起认证的同一个回车
            // 直接把空响应提交出去。
            let just_opened = prompt.focus_first;
            egui::Window::new("SSH 交互式认证")
                .id(egui::Id::new("ssh_keyboard_interactive"))
                .collapsible(false)
                .resizable(false)
                .pivot(egui::Align2::CENTER_CENTER)
                .default_pos(ctx.content_rect().center())
                .frame(crate::ui::dialog::window_frame(&ctx))
                .show(&ctx, |ui| {
                    crate::ui::dialog::polish(ui);
                    ui.set_width(430.0);
                    ui.label(
                        RichText::new(format!("{}@{}", prompt.user, prompt.host))
                            .strong()
                            .monospace(),
                    );
                    if !prompt.name.trim().is_empty() {
                        ui.add_space(5.0);
                        ui.label(RichText::new(&prompt.name).strong());
                    }
                    if !prompt.instructions.trim().is_empty() {
                        ui.label(&prompt.instructions);
                    }
                    ui.add_space(8.0);
                    for (question_index, (question, response)) in prompt
                        .questions
                        .iter()
                        .zip(prompt.responses.iter_mut())
                        .enumerate()
                    {
                        if !question.prompt.is_empty() {
                            ui.label(&question.prompt);
                        }
                        let text = egui::TextEdit::singleline(&mut **response)
                            .password(!question.echo)
                            .desired_width(f32::INFINITY);
                        let response_widget = ui.add(text);
                        if prompt.focus_first && question_index == 0 {
                            response_widget.request_focus();
                        }
                        ui.add_space(5.0);
                    }
                    prompt.focus_first = false;
                    ui.horizontal(|ui| {
                        if crate::ui::dialog::primary_button(ui, "继续").clicked() {
                            submit = true;
                        }
                        if ui.button("取消连接").clicked() {
                            cancel = true;
                        }
                    });
                    if !just_opened && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        submit = true;
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        submit = false;
                        cancel = true;
                    }
                    if submit {
                        crate::ui::dialog::consume_handled_key(ui, egui::Key::Enter);
                    }
                    if cancel {
                        crate::ui::dialog::consume_handled_key(ui, egui::Key::Escape);
                    }
                });
        }

        if submit || cancel {
            let prompt = self.sessions[index]
                .pending_keyboard_interactive
                .take()
                .expect("interactive prompt exists");
            let verdict = if cancel {
                crate::session::KeyboardInteractiveVerdict::Cancel
            } else {
                crate::session::KeyboardInteractiveVerdict::Responses(
                    prompt
                        .responses
                        .iter()
                        .map(|response| {
                            crate::secrets::SecretString::new(response.as_str().to_owned())
                        })
                        .collect(),
                )
            };
            let _ = prompt.reply.send(verdict);
        }
    }
}
