// SPDX-FileCopyrightText: 2026 hiunivs
// SPDX-License-Identifier: AGPL-3.0-only
// Alternative commercial licensing and AGPL section 7 terms: see NOTICE.

use crate::app::{RShellApp, SessionKind};
use crate::config::SavedConnection;
use crate::session::ssh::{SshAuth, SshParams};
use zeroize::Zeroize as _;

/// 由表单字段构建连接参数(新建对话框与设置窗口共用)
fn build_params(
    host: &str,
    port: &str,
    user: &str,
    pass: crate::secrets::SecretString,
    key: &str,
) -> Result<SshParams, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("主机不能为空".into());
    }
    let port: u16 = port
        .trim()
        .parse()
        .map_err(|_| "端口必须是 1-65535 的数字".to_string())?;
    let user = user.trim();
    if user.is_empty() {
        return Err("用户名不能为空".into());
    }
    let key_path = key.trim();
    let auth = if !key_path.is_empty() {
        let path = key_path
            .strip_prefix("~/")
            .map(|rest| {
                std::env::var("HOME")
                    .map(|home| format!("{home}/{rest}"))
                    .unwrap_or_else(|_| key_path.to_string())
            })
            .unwrap_or_else(|| key_path.to_string());
        SshAuth::KeyFile {
            path,
            passphrase: if pass.is_empty() { None } else { Some(pass) },
        }
    } else {
        SshAuth::Password(pass)
    };
    Ok(SshParams {
        host: host.to_string(),
        port,
        user: user.to_string(),
        auth,
    })
}

/// 由已保存连接构建连接参数:
/// - 有 key_id → 用托管密钥(路径 + keyring 口令)
/// - 否则用 key_path;密码认证时口令从 keyring 取
pub(crate) fn params_from_saved(
    c: &SavedConnection,
    cfg: &crate::config::AppConfig,
) -> Result<SshParams, String> {
    let (key_path, pass) = if let Some(kid) = &c.key_id {
        match crate::keys::load_for_connect(kid, cfg) {
            Some((p, pw)) => (p, pw.unwrap_or_default()),
            None => return Err("引用的密钥已不存在,请在连接库里重新指定".into()),
        }
    } else {
        let pw = crate::secrets::get(&crate::secrets::account_for(&c.user, &c.host, c.port))
            .unwrap_or_default();
        (c.key_path.clone(), pw)
    };
    build_params(&c.host, &c.port.to_string(), &c.user, pass, &key_path)
}

impl RShellApp {
    /// 打开当前会话的连接设置窗口：表单结构与连接库“编辑连接”一致。
    pub(crate) fn open_session_edit(&mut self) {
        let Some(session) = self.sessions.get(self.active) else {
            return;
        };
        let Some(params) = session.ssh_params() else {
            return;
        };
        self.edit_host = params.host.clone();
        self.edit_port = params.port.to_string();
        self.edit_user = params.user.clone();
        let account = crate::secrets::account_for(&params.user, &params.host, params.port);
        let saved_connection = self
            .config
            .connections
            .iter()
            .find(|connection| connection.name == account)
            .cloned();
        self.edit_alias = saved_connection
            .as_ref()
            .map(|connection| connection.alias.clone())
            .unwrap_or_default();
        self.edit_group = saved_connection
            .as_ref()
            .map(|connection| connection.group.clone())
            .unwrap_or_default();
        self.edit_tags = saved_connection
            .as_ref()
            .map(|connection| connection.tags.join(", "))
            .unwrap_or_default();

        if let Some(connection) = saved_connection {
            self.edit_key_mode = if let Some(key_id) = connection.key_id {
                crate::ui::conn_library::LcKeyMode::Key(key_id)
            } else if !connection.key_path.is_empty() {
                crate::ui::conn_library::LcKeyMode::Path
            } else {
                crate::ui::conn_library::LcKeyMode::Password
            };
            self.edit_key_path = connection.key_path;
        } else {
            self.edit_key_mode = match &params.auth {
                SshAuth::Password(_) => crate::ui::conn_library::LcKeyMode::Password,
                SshAuth::KeyFile { path, .. } => self
                    .config
                    .keys
                    .iter()
                    .find(|key| key.path == *path)
                    .map(|key| crate::ui::conn_library::LcKeyMode::Key(key.id.clone()))
                    .unwrap_or(crate::ui::conn_library::LcKeyMode::Path),
            };
            self.edit_key_path = match &params.auth {
                SshAuth::KeyFile { path, .. } => path.clone(),
                SshAuth::Password(_) => String::new(),
            };
        }

        self.edit_pass.zeroize();
        match &params.auth {
            SshAuth::Password(password) => {
                let saved = crate::secrets::get(&account);
                self.edit_remember = saved.is_some();
                self.edit_pass = saved
                    .as_ref()
                    .map(crate::secrets::SecretString::expose)
                    .unwrap_or_else(|| password.expose())
                    .to_owned();
            }
            SshAuth::KeyFile { passphrase, .. } => {
                // 自定义私钥的当前会话口令继续保留，用于“保存并重连”。
                self.edit_pass = passphrase
                    .as_ref()
                    .map(|value| value.expose().to_owned())
                    .unwrap_or_default();
                self.edit_remember = false;
            }
        };
        self.edit_error = None;
        self.edit_session_id = Some(session.id);
        self.show_session_edit = true;
    }

    /// 当前会话的连接设置窗口(编辑后保存/保存并重连)
    pub(crate) fn show_session_edit_dialog(&mut self, ui: &mut egui::Ui) {
        if !self.show_session_edit {
            return;
        }
        // 始终绑定打开窗口时的会话；切换标签不会改变编辑目标。
        let Some(edit_session_id) = self.edit_session_id else {
            self.show_session_edit = false;
            self.edit_pass.zeroize();
            return;
        };
        let Some(session) = self
            .sessions
            .iter()
            .find(|session| session.id == edit_session_id)
        else {
            self.show_session_edit = false;
            self.edit_session_id = None;
            self.edit_pass.zeroize();
            return;
        };
        if session.kind != SessionKind::Ssh {
            self.show_session_edit = false;
            self.edit_session_id = None;
            self.edit_pass.zeroize();
            return;
        }
        let ctx = ui.ctx().clone();
        let mut open = self.show_session_edit;
        egui::Window::new("连接设置")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            // 初始居中但可拖动(同"新建连接")
            .pivot(egui::Align2::CENTER_CENTER)
            .default_pos(ctx.content_rect().center())
            .frame(crate::ui::dialog::window_frame(&ctx))
            .show(&ctx, |ui| {
                crate::ui::dialog::polish(ui);
                ui.set_width(340.0);
                egui::Grid::new("session_edit_grid")
                    .num_columns(2)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("显示名");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.edit_alias)
                                .hint_text("可选，用于列表和标签页"),
                        );
                        ui.end_row();
                        ui.label("分组");
                        crate::ui::conn_library::connection_group_combo(
                            ui,
                            "session_edit_group",
                            &mut self.edit_group,
                            &self.config.folders,
                        );
                        ui.end_row();
                        ui.label("标签");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.edit_tags)
                                .hint_text("逗号分隔,如 web, prod"),
                        );
                        ui.end_row();
                        ui.label("主机");
                        ui.text_edit_singleline(&mut self.edit_host);
                        ui.end_row();
                        ui.label("端口");
                        ui.add(egui::TextEdit::singleline(&mut self.edit_port).desired_width(80.0));
                        ui.end_row();
                        ui.label("用户名");
                        ui.text_edit_singleline(&mut self.edit_user);
                        ui.end_row();
                        ui.label("认证");
                        ui.horizontal(|ui| {
                            crate::ui::conn_library::connection_key_combo(
                                ui,
                                "session_edit_key_mode",
                                &mut self.edit_key_mode,
                                &self.config.keys,
                            );
                            ui.menu_button("＋ 新建密钥", |ui| {
                                if ui.button("生成新密钥…").clicked() {
                                    self.open_key_form(
                                        crate::ui::conn_library::KeyFormMode::Generate,
                                    );
                                    self.key_form_assign_to_edit = true;
                                    ui.close();
                                }
                                if ui.button("登记已有私钥…").clicked() {
                                    self.open_key_form(
                                        crate::ui::conn_library::KeyFormMode::Register,
                                    );
                                    self.key_form_assign_to_edit = true;
                                    ui.close();
                                }
                            })
                            .response
                            .on_hover_text("在这里添加密钥，完成后自动用于当前连接");
                        });
                        ui.end_row();
                    });

                match &self.edit_key_mode {
                    crate::ui::conn_library::LcKeyMode::Password => {
                        egui::Grid::new("session_edit_password_grid")
                            .num_columns(2)
                            .spacing([10.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("密码");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.edit_pass).password(true),
                                );
                                ui.end_row();
                                ui.label("");
                                ui.checkbox(
                                    &mut self.edit_remember,
                                    self.config.password_storage.remember_password_label(),
                                );
                                ui.end_row();
                            });
                    }
                    crate::ui::conn_library::LcKeyMode::Path => {
                        ui.horizontal(|ui| {
                            ui.label("私钥");
                            ui.text_edit_singleline(&mut self.edit_key_path);
                            if ui.button("…").clicked()
                                && let Some(path) = rfd::FileDialog::new().pick_file()
                            {
                                self.edit_key_path = path.to_string_lossy().into_owned();
                            }
                        });
                    }
                    crate::ui::conn_library::LcKeyMode::Key(_) => {}
                }

                if let Some(error) = &self.edit_error {
                    ui.add_space(6.0);
                    ui.colored_label(egui::Color32::from_rgb(0xE8, 0x4C, 0x3D), error);
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let action_size = egui::vec2(56.0, 28.0);
                    if crate::ui::dialog::compact_primary_button(ui, "保存").clicked() {
                        self.apply_session_edit(false, &ctx);
                    }
                    if crate::ui::dialog::primary_button(ui, "保存并重连")
                        .on_hover_text("保存参数并立即用新参数重新连接")
                        .clicked()
                    {
                        self.apply_session_edit(true, &ctx);
                    }
                    if ui
                        .add_sized(action_size, egui::Button::new("取消"))
                        .clicked()
                    {
                        self.show_session_edit = false;
                        self.edit_session_id = None;
                    }
                });
            });
        self.show_session_edit = open && self.show_session_edit;
        if !self.show_session_edit {
            self.edit_session_id = None;
            self.edit_pass.zeroize();
        }
    }

    /// 应用编辑结果：更新会话参数和对应的连接库条目；可选立即重连。
    fn apply_session_edit(&mut self, reconnect: bool, ctx: &egui::Context) {
        let Some(edit_session_id) = self.edit_session_id else {
            self.edit_error = Some("编辑目标会话已关闭".into());
            return;
        };
        let Some(session_index) = self
            .sessions
            .iter()
            .position(|session| session.id == edit_session_id && session.kind == SessionKind::Ssh)
        else {
            self.edit_error = Some("编辑目标会话已关闭".into());
            return;
        };
        let managed_auth = match &self.edit_key_mode {
            crate::ui::conn_library::LcKeyMode::Key(key_id) => {
                let Some(key) = self.config.keys.iter().find(|key| key.id == *key_id) else {
                    self.edit_error = Some("选择的管理密钥已不存在".into());
                    return;
                };
                Some((
                    key_id.clone(),
                    key.path.clone(),
                    crate::secrets::get(&crate::keys::key_account(key_id)).unwrap_or_default(),
                ))
            }
            _ => None,
        };
        let (key_id, key_path, auth_password) = match &self.edit_key_mode {
            crate::ui::conn_library::LcKeyMode::Password => (
                None,
                String::new(),
                crate::secrets::SecretString::from(self.edit_pass.clone()),
            ),
            crate::ui::conn_library::LcKeyMode::Path => (
                None,
                self.edit_key_path.trim().to_string(),
                crate::secrets::SecretString::from(self.edit_pass.clone()),
            ),
            crate::ui::conn_library::LcKeyMode::Key(_) => {
                let (key_id, path, passphrase) = managed_auth.expect("上方已验证管理密钥");
                (Some(key_id), path, passphrase)
            }
        };
        let params = match build_params(
            &self.edit_host,
            &self.edit_port,
            &self.edit_user,
            auth_password,
            &key_path,
        ) {
            Ok(p) => p,
            Err(msg) => {
                self.edit_error = Some(msg);
                return;
            }
        };
        let new_account = crate::secrets::account_for(&params.user, &params.host, params.port);
        let old_account = self
            .sessions
            .get(session_index)
            .and_then(|s| s.ssh_params().cloned())
            .map(|p| crate::secrets::account_for(&p.user, &p.host, p.port));
        let was_saved = old_account.as_ref().is_some_and(|old| {
            self.config
                .connections
                .iter()
                .any(|connection| &connection.name == old)
        });

        // 密码认证按当前安全设置写入；密钥认证不保留连接密码。
        if matches!(params.auth, SshAuth::Password(_)) {
            if self.edit_remember && !self.edit_pass.is_empty() {
                if let Err(e) = crate::secrets::set(&new_account, &self.edit_pass) {
                    self.edit_error = Some(format!("密码保存失败：{e}"));
                    return;
                }
            } else {
                crate::secrets::delete(&new_account);
            }
        } else {
            crate::secrets::delete(&new_account);
        }
        if let Some(old) = old_account
            && old != new_account
        {
            self.config.rename_connection_usage(&old, &new_account);
            crate::secrets::delete(&old);
            self.config.connections.retain(|c| c.name != old);
        }

        // 延续原行为：只有原本位于连接库中的会话才同步连接库。
        if was_saved {
            let connection = crate::config::SavedConnection {
                name: new_account.clone(),
                host: params.host.clone(),
                port: params.port,
                user: params.user.clone(),
                key_path: if key_id.is_none() {
                    key_path.clone()
                } else {
                    String::new()
                },
                alias: self.edit_alias.trim().to_string(),
                group: self.edit_group.trim().to_string(),
                tags: crate::ui::conn_library::parse_tags(&self.edit_tags),
                key_id,
            };
            if let Some(existing) = self
                .config
                .connections
                .iter_mut()
                .find(|existing| existing.name == new_account)
            {
                *existing = connection;
            } else {
                self.config.connections.push(connection);
            }
            let group = self.edit_group.trim();
            if !group.is_empty() && !self.config.folders.iter().any(|folder| folder == group) {
                self.config.folders.push(group.to_string());
            }
            if let Err(error) = crate::config::save(&self.config) {
                self.edit_error = Some(format!("保存连接失败：{error:#}"));
                return;
            }
        }

        let session = &mut self.sessions[session_index];
        session.update_ssh_params(params);
        let target = if self.edit_alias.trim().is_empty() {
            self.edit_host.trim()
        } else {
            self.edit_alias.trim()
        };
        session.title = format!("{}@{target}", self.edit_user.trim());
        if reconnect {
            session.reconnect(ctx);
        }
        self.show_session_edit = false;
        self.edit_session_id = None;
        self.edit_error = None;
    }
}
